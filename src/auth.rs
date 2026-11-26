//! Account credentials, revocable MCP tokens, and GitHub device authorization.
use crate::store::{self, Db, PRIMARY};
use anyhow::{Context, Result, bail, ensure};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use diesel::prelude::*;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    time::{Duration, Instant},
};
use uuid::Uuid;

diesel::table! {accounts(id){id->Text,username->Text,password_hash->Text,admin->Bool,disabled->Bool,revision->Integer,}}
diesel::table! {access_tokens(id){id->Text,owner_id->Text,name->Text,digest->Text,created_at->Text,}}
diesel::allow_tables_to_appear_in_same_query!(accounts, access_tokens);
#[derive(Clone, Serialize, Queryable, Selectable)]
#[diesel(table_name=accounts)]
pub(crate) struct Account {
    #[serde(skip)]
    pub id: String,
    pub username: String,
    pub admin: bool,
    pub disabled: bool,
    #[serde(skip)]
    pub revision: i32,
}
pub(crate) fn users(db: &mut Db) -> Result<Vec<Account>> {
    Ok(accounts::table
        .select(Account::as_select())
        .order(accounts::username)
        .load(db)?)
}
pub(crate) fn account(db: &mut Db, id: &str) -> Result<Account> {
    Ok(accounts::table
        .find(id)
        .select(Account::as_select())
        .first(db)?)
}
fn password_hash(password: &str) -> Result<String> {
    ensure!(
        (8..=1024).contains(&password.len()),
        "密码长度应为 8 至 1024 字节"
    );
    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())
        .map_err(|_| anyhow::anyhow!("生成密码盐失败"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| anyhow::anyhow!("计算密码摘要失败"))
}
pub(crate) fn bootstrap(db: &mut Db) -> Result<()> {
    if accounts::table
        .find(PRIMARY)
        .count()
        .get_result::<i64>(db)?
        > 0
    {
        return Ok(());
    }
    let user = env::var("STARSTACK_ADMIN_USER").context("首次启动请设置 STARSTACK_ADMIN_USER")?;
    let pass = env::var("STARSTACK_ADMIN_PASSWORD")
        .context("首次启动请设置 STARSTACK_ADMIN_PASSWORD（至少 8 字节）")?;
    new_account(db, PRIMARY, &user, &pass, true)
}
fn new_account(db: &mut Db, id: &str, user: &str, password: &str, admin: bool) -> Result<()> {
    use accounts as a;
    ensure!(
        !user.trim().is_empty() && user.trim() == user && user.len() <= 128,
        "用户名应为 1–128 字节，首尾不能有空格"
    );
    let hash = password_hash(password)?;
    diesel::insert_into(a::table)
        .values((
            a::id.eq(id),
            a::username.eq(user),
            a::password_hash.eq(hash),
            a::admin.eq(admin),
        ))
        .execute(db)
        .context("无法创建用户，请检查用户名是否重复")?;
    Ok(())
}
#[derive(Deserialize)]
pub(crate) struct Create {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub admin: bool,
}
/// Recheck privileges inside the same transaction as the administrative change.
pub(crate) fn create(db: &mut Db, actor: &str, input: Create) -> Result<()> {
    db.immediate_transaction(|db| {
        let actor = account(db, actor)?;
        ensure!(actor.admin && !actor.disabled, "需要管理员权限");
        new_account(
            db,
            &Uuid::new_v4().to_string(),
            &input.username,
            &input.password,
            input.admin,
        )
    })
}
#[derive(Deserialize)]
pub(crate) struct Update {
    pub username: String,
    pub password: Option<String>,
    pub admin: Option<bool>,
    pub disabled: Option<bool>,
}
pub(crate) fn update(db: &mut Db, actor: &str, input: Update) -> Result<()> {
    use accounts as a;
    db.immediate_transaction(|db| {
        let actor = account(db, actor)?;
        ensure!(actor.admin && !actor.disabled, "需要管理员权限");
        let user = a::table
            .filter(a::username.eq(&input.username))
            .select(Account::as_select())
            .first::<Account>(db)
            .optional()?
            .context("用户不存在")?;
        ensure!(
            user.id != PRIMARY || (input.admin != Some(false) && input.disabled != Some(true)),
            "初始管理员必须保持启用和管理员角色"
        );
        let row = a::table.find(&user.id);
        if let Some(p) = input.password {
            diesel::update(row)
                .set(a::password_hash.eq(password_hash(&p)?))
                .execute(db)?;
        }
        diesel::update(row)
            .set((
                a::admin.eq(input.admin.unwrap_or(user.admin)),
                a::disabled.eq(input.disabled.unwrap_or(user.disabled)),
                a::revision.eq(a::revision + 1),
            ))
            .execute(db)?;
        diesel::delete(access_tokens::table.filter(access_tokens::owner_id.eq(user.id)))
            .execute(db)?;
        Ok(())
    })
}
pub(crate) fn verify(db: &mut Db, username: &str, password: &str) -> Result<Account> {
    let (user, hash) = accounts::table
        .filter(
            accounts::username
                .eq(username)
                .and(accounts::disabled.eq(false)),
        )
        .select((Account::as_select(), accounts::password_hash))
        .first::<(Account, String)>(db)
        .optional()?
        .context("账号或密码不正确")?;
    let parsed =
        PasswordHash::new(&hash).map_err(|_| anyhow::anyhow!("密码记录损坏，请管理员重置密码"))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| anyhow::anyhow!("账号或密码不正确"))?;
    Ok(user)
}
pub(crate) fn secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
#[derive(Serialize, Queryable)]
pub(crate) struct Token {
    id: String,
    name: String,
    created_at: String,
}
pub(crate) fn tokens(db: &mut Db) -> Result<Vec<Token>> {
    use access_tokens as t;
    let user = store::owner(db)?;
    Ok(t::table
        .filter(t::owner_id.eq(user))
        .select((t::id, t::name, t::created_at))
        .order(t::created_at.desc())
        .load(db)?)
}
pub(crate) fn issue(db: &mut Db, label: &str, revision: i32) -> Result<String> {
    db.immediate_transaction(|db| {
        let user = store::owner(db)?;
        let account = account(db, &user)?;
        ensure!(
            !account.disabled && account.revision == revision,
            "会话已失效，请重新登录"
        );
        issue_current(db, label)
    })
}
fn issue_current(db: &mut Db, label: &str) -> Result<String> {
    use access_tokens as t;
    ensure!(
        !label.trim().is_empty() && label.len() <= 128,
        "请为 token 填写 1 至 128 字节的名称"
    );
    let token = format!("sst_{}", secret());
    let user = store::owner(db)?;
    diesel::insert_into(t::table)
        .values((
            t::id.eq(Uuid::new_v4().to_string()),
            t::owner_id.eq(user),
            t::name.eq(label.trim()),
            t::digest.eq(digest(&token)),
            t::created_at.eq(chrono::Utc::now().to_rfc3339()),
        ))
        .execute(db)?;
    Ok(token)
}
pub(crate) fn revoke(db: &mut Db, id: &str) -> Result<()> {
    use access_tokens as t;
    let user = store::owner(db)?;
    ensure!(
        diesel::delete(t::table.filter(t::owner_id.eq(user).and(t::id.eq(id)))).execute(db)? == 1,
        "未找到该 token"
    );
    Ok(())
}
pub(crate) fn bearer_owner(db: &mut Db, bearer: &str) -> Result<String> {
    use access_tokens as t;
    ensure!(bearer.starts_with("sst_"), "无效 token");
    Ok(t::table
        .inner_join(accounts::table.on(accounts::id.eq(t::owner_id)))
        .filter(
            t::digest
                .eq(digest(bearer))
                .and(accounts::disabled.eq(false)),
        )
        .select(t::owner_id)
        .first(db)?)
}
fn environment_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .find(|v| !v.trim().is_empty())
}

fn gh_token() -> Result<String> {
    if let Ok(base) = env::var("STARSTACK_GITHUB_API_URL") {
        ensure!(
            base.trim_end_matches('/') == "https://api.github.com",
            "gh 授权绑定 github.com，请恢复默认 GitHub API 地址"
        );
    }
    let result = std::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("无法运行 gh，请安装 GitHub CLI 并检查 PATH，或使用 Device Flow")?;
    ensure!(
        result.status.success(),
        "gh 尚未授权 github.com，请先运行 gh auth login --hostname github.com"
    );
    let token = String::from_utf8(result.stdout).context("gh 返回的凭证格式无效")?;
    ensure!(
        !token.trim().is_empty(),
        "gh 没有返回凭证，请重新运行 gh auth login"
    );
    Ok(token.trim().into())
}

pub(crate) fn use_gh(db: &mut Db) -> Result<()> {
    ensure!(store::owner(db)? == PRIMARY, "本机 gh 仅可用于初始资料库");
    gh_token()?;
    db.immediate_transaction(|db| {
        store::set_meta(db, "github_source", "gh")?;
        store::delete_meta(db, "github_token")
    })
}
pub(crate) fn has_github_credentials(db: &mut Db) -> Result<bool> {
    let primary = store::owner(db)? == PRIMARY;
    Ok((primary
        && (environment_token().is_some()
            || store::meta(db, "github_source")?.as_deref() == Some("gh")))
        || store::meta(db, "github_token")?.is_some())
}
pub(crate) fn github_token(db: &mut Db) -> Result<String> {
    if store::owner(db)? == PRIMARY {
        if let Some(token) = environment_token() {
            return Ok(token);
        }
        if store::meta(db, "github_source")?.as_deref() == Some("gh") {
            return gh_token();
        }
    }
    store::meta(db, "github_token")?.context("请在后台连接 GitHub，或运行 starstack auth login；本机已登录 gh 可用 starstack auth login --gh")
}
const LOGIN: &str = "https://github.com/login";
pub(crate) struct Device {
    client: Client,
    client_id: String,
    endpoint: String,
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
    expires: Instant,
    next: Instant,
}
#[derive(Deserialize)]
struct Challenge {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}
impl Device {
    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.expires
    }
    pub(crate) fn start(db: &mut Db) -> Result<Self> {
        let client_id=env::var("STARSTACK_GITHUB_CLIENT_ID").ok().filter(|v|!v.is_empty()).context("请设置已启用 Device Flow 的 STARSTACK_GITHUB_CLIENT_ID，再重启星栈；或使用个人 token")?;
        let scope = env::var("STARSTACK_GITHUB_SCOPE").unwrap_or_default();
        Self::start_at(LOGIN, client_id, &scope, crate::proxy::builder(db)?)
    }
    fn start_at(
        endpoint: &str,
        client_id: String,
        scope: &str,
        builder: reqwest::blocking::ClientBuilder,
    ) -> Result<Self> {
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()?;
        let c = client
            .post(format!("{endpoint}/device/code"))
            .header("Accept", "application/json")
            .form(&[("client_id", client_id.as_str()), ("scope", scope)])
            .send()
            .context("无法连接 GitHub，请检查网络后重试")?
            .error_for_status()?
            .json::<Challenge>()
            .context("未能获取设备码，请确认 GitHub 应用已开启 Device Flow")?;
        let interval = c.interval.unwrap_or(5).max(1);
        let now = Instant::now();
        Ok(Self {
            client,
            client_id,
            endpoint: endpoint.into(),
            device_code: c.device_code,
            user_code: c.user_code,
            verification_uri: c.verification_uri,
            interval,
            expires_in: c.expires_in,
            expires: now + Duration::from_secs(c.expires_in),
            next: now + Duration::from_secs(interval),
        })
    }
    pub(crate) fn poll(&mut self, db: &mut Db) -> Result<bool> {
        ensure!(
            Instant::now() < self.expires,
            "设备码已过期，请重新发起授权"
        );
        if Instant::now() < self.next {
            return Ok(false);
        }
        #[derive(Deserialize)]
        struct Answer {
            access_token: Option<String>,
            error: Option<String>,
        }
        let response = self
            .client
            .post(format!("{}/oauth/access_token", self.endpoint))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("device_code", &self.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send();
        self.next = Instant::now() + Duration::from_secs(self.interval);
        let response = match response {
            Err(e) if e.is_connect() || e.is_timeout() => return Ok(false),
            Ok(r) if r.status().is_server_error() => return Ok(false),
            result => result?,
        };
        let answer = response.error_for_status()?.json::<Answer>()?;
        if let Some(token) = answer.access_token {
            db.immediate_transaction::<_, anyhow::Error, _>(|db| {
                store::set_meta(db, "github_token", &token)?;
                store::delete_meta(db, "github_source")
            })?;
            return Ok(true);
        }
        match answer.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => self.interval += 5,
            Some("access_denied") => bail!("已取消 GitHub 授权"),
            Some("expired_token") => bail!("设备码已过期，请重新发起授权"),
            _ => bail!("GitHub 授权失败，请确认 client ID 后重新发起授权"),
        }
        self.next = Instant::now() + Duration::from_secs(self.interval);
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{Reply, Server};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn device_flow_handles_polling_and_scopes_the_persisted_token() -> Result<()> {
        let attempts = AtomicUsize::new(0);
        let server = Server::new(move |r| {
            let params = String::from_utf8(r.body).expect("device requests use URL-encoded UTF-8");
            match r.target.as_str() {
                "/device/code" => {
                    assert!(params.contains("client_id=fixture"));
                    Reply::json(
                        json!({"device_code":"private-challenge","user_code":"ABCD","verification_uri":"https://github.com/login/device","interval":1,"expires_in":300}),
                    )
                }
                "/oauth/access_token" => {
                    assert!(params.contains("device_code=private-challenge"));
                    match attempts.fetch_add(1, Ordering::SeqCst) {
                        0 => Reply::json(json!({"error":"slow_down"})),
                        1 => Reply::status(503),
                        _ => Reply::json(json!({"access_token":"user-scoped-fixture-token"})),
                    }
                }
                path => panic!("unexpected mock request {path}"),
            }
        });
        let dir = tempfile::tempdir()?;
        let mut db = store::connect_as(dir.path(), "reader")?;
        let mut d = Device::start_at(
            &server.url,
            "fixture".into(),
            "",
            Client::builder().no_proxy(),
        )?;
        assert!(
            !d.poll(&mut db)?,
            "the first poll respects the initial interval"
        );
        d.next = Instant::now();
        assert!(!d.poll(&mut db)?);
        assert_eq!(d.interval, 6, "slow_down adds five seconds");
        d.next = Instant::now();
        assert!(
            !d.poll(&mut db)?,
            "temporary failures keep the challenge alive"
        );
        d.next = Instant::now();
        assert!(d.poll(&mut db)?);
        assert_eq!(github_token(&mut db)?, "user-scoped-fixture-token");
        let mut other = store::connect_as(dir.path(), "second")?;
        assert!(github_token(&mut other).is_err());
        Ok(())
    }
}
