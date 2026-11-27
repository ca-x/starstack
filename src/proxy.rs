//! Outbound proxy defaults and account overrides for GitHub and README requests.
use crate::{
    auth,
    store::{self, Db},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use diesel::{connection::SimpleConnection, prelude::*};
use reqwest::{Proxy, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;

diesel::table! { proxy_configuration(id) { id -> Integer, setting -> Text, } }

const ACCOUNT_PROXY: &str = "github_proxy";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Instance,
    Account,
}

const INVALID_URL: &str = "代理地址无效，请填写 HTTP、HTTPS、SOCKS5 或 SOCKS5h 代理 URL";

pub(crate) enum ProxyConfig {
    System,
    Direct,
    Custom(Url),
}

impl ProxyConfig {
    fn custom(raw: &str) -> Result<Self> {
        ensure!(
            raw.len() <= 4096 && raw.contains("://") && !raw.chars().any(char::is_whitespace),
            INVALID_URL
        );
        let url = Url::parse(raw).map_err(|_| anyhow!(INVALID_URL))?;
        ensure!(
            matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                && url.host().is_some()
                && url.port() != Some(0)
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none(),
            INVALID_URL
        );
        Self::proxy(&url)?;
        Ok(Self::Custom(url))
    }

    fn proxy(url: &Url) -> Result<Proxy> {
        // A proxy URL can include credentials. Keep errors and status payloads redacted.
        Proxy::all(url.clone()).map_err(|_| anyhow!(INVALID_URL))
    }

    fn blocking_builder(&self) -> Result<reqwest::blocking::ClientBuilder> {
        let client = reqwest::blocking::Client::builder();
        Ok(match self {
            Self::System => client,
            Self::Direct => client.no_proxy(),
            Self::Custom(url) => client.proxy(Self::proxy(url)?),
        })
    }

    #[cfg(any(test, feature = "firecrawl-local"))]
    pub(crate) fn async_builder(&self) -> Result<reqwest::ClientBuilder> {
        let client = reqwest::Client::builder();
        Ok(match self {
            Self::System => client,
            Self::Direct => client.no_proxy(),
            Self::Custom(url) => client.proxy(Self::proxy(url)?),
        })
    }

    fn status(&self, source: &str) -> Value {
        let (mode, address, credentials) = match self {
            Self::System => ("system", None, false),
            Self::Direct => ("direct", None, false),
            Self::Custom(url) => {
                // Slice from the host onward; userinfo is never part of the returned address.
                let mut address =
                    format!("{}://{}", url.scheme(), url.host().expect("validated host"));
                if let Some(port) = url.port() {
                    address.push_str(&format!(":{port}"));
                }
                (
                    "custom",
                    Some(address),
                    !url.username().is_empty() || url.password().is_some(),
                )
            }
        };
        json!({"mode": mode, "source": source, "address": address, "has_credentials": credentials})
    }
}

fn schema(db: &mut Db) -> Result<()> {
    db.batch_execute("CREATE TABLE IF NOT EXISTS proxy_configuration (id INTEGER PRIMARY KEY CHECK(id=1), setting TEXT NOT NULL)")?;
    Ok(())
}

fn operator_url() -> Result<Option<String>> {
    match env::var("STARSTACK_PROXY_URL") {
        Ok(s) => Ok((!s.trim().is_empty()).then_some(s)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("STARSTACK_PROXY_URL 需要 UTF-8 代理地址"),
    }
}

fn effective(db: &mut Db, operator: Option<String>) -> Result<(ProxyConfig, &'static str)> {
    if let Some(raw) = operator {
        return Ok((
            ProxyConfig::custom(raw.trim()).context("STARSTACK_PROXY_URL 无效")?,
            "environment",
        ));
    }
    schema(db)?;
    let saved: Option<String> = proxy_configuration::table
        .find(1)
        .select(proxy_configuration::setting)
        .first(db)
        .optional()?;
    match saved.as_deref() {
        None => Ok((ProxyConfig::System, "system")),
        Some("direct") => Ok((ProxyConfig::Direct, "database")),
        Some(raw) => Ok((ProxyConfig::custom(raw)?, "database")),
    }
}

fn account_config(db: &mut Db) -> Result<Option<ProxyConfig>> {
    Ok(match store::meta(db, ACCOUNT_PROXY)?.as_deref() {
        None => None,
        Some("system") => Some(ProxyConfig::System),
        Some("direct") => Some(ProxyConfig::Direct),
        Some(raw) => Some(ProxyConfig::custom(raw)?),
    })
}

fn scoped_config(db: &mut Db, scope: Scope, operator: Option<String>) -> Result<ProxyConfig> {
    if scope == Scope::Account
        && operator.is_none()
        && let Some(config) = account_config(db)?
    {
        return Ok(config);
    }
    Ok(effective(db, operator)?.0)
}

pub(crate) fn config(db: &mut Db) -> Result<ProxyConfig> {
    scoped_config(db, Scope::Account, operator_url()?)
}

pub(crate) fn instance_builder(db: &mut Db) -> Result<reqwest::blocking::ClientBuilder> {
    scoped_config(db, Scope::Instance, operator_url()?)?.blocking_builder()
}

pub(crate) fn builder(db: &mut Db) -> Result<reqwest::blocking::ClientBuilder> {
    config(db)?.blocking_builder()
}

fn require_access(db: &mut Db, revision: i32, scope: Scope) -> Result<()> {
    let owner = store::owner(db)?;
    let user = auth::account(db, &owner)?;
    ensure!(
        !user.disabled && user.revision == revision,
        "会话已失效，请重新登录"
    );
    ensure!(scope == Scope::Account || user.admin, "需要管理员权限");
    Ok(())
}

pub(crate) fn read_settings(db: &mut Db, revision: i32, scope: Scope) -> Result<Value> {
    require_access(db, revision, scope)?;
    settings(db, scope, operator_url()?)
}

fn settings(db: &mut Db, scope: Scope, operator: Option<String>) -> Result<Value> {
    if scope == Scope::Instance {
        let (config, source) = effective(db, operator)?;
        return Ok(config.status(source));
    }
    if operator.is_some() {
        return Ok(
            json!({"mode":"inherit","source":"environment","address":null,"has_credentials":false}),
        );
    }
    Ok(match account_config(db)? {
        Some(config) => config.status("database"),
        None => {
            json!({"mode":"inherit","source":"instance","address":null,"has_credentials":false})
        }
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", tag = "mode", deny_unknown_fields)]
enum Update {
    Inherit,
    System,
    Direct,
    Custom { url: String },
}

fn selection(input: Value, scope: Scope) -> Result<Option<ProxyConfig>> {
    let update: Update = serde_json::from_value(input).map_err(|_| anyhow!("代理设置格式无效"))?;
    match update {
        Update::Inherit if scope == Scope::Account => Ok(None),
        Update::Inherit => bail!("实例代理不能继承个人代理"),
        Update::System => Ok(Some(ProxyConfig::System)),
        Update::Direct => Ok(Some(ProxyConfig::Direct)),
        Update::Custom { url } => Ok(Some(ProxyConfig::custom(url.trim())?)),
    }
}

pub(crate) fn test_connection(
    db: &mut Db,
    input: Value,
    revision: i32,
    scope: Scope,
) -> Result<Value> {
    require_access(db, revision, scope)?;
    let operator = operator_url()?;
    let config = if input.is_null() || operator.is_some() {
        scoped_config(db, scope, operator)?
    } else {
        match selection(input, scope)? {
            Some(config) => config,
            None => effective(db, None)?.0,
        }
    };
    let start = std::time::Instant::now();
    let response = config
        .blocking_builder()?
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .and_then(|client| client.head("https://www.cloudflare.com/").send())
        .map_err(|e| {
            if e.is_timeout() {
                anyhow!("连接 Cloudflare 超时，请检查代理地址和网络")
            } else {
                anyhow!("无法访问 Cloudflare，请检查代理地址、认证信息和网络")
            }
        })?;
    ensure!(
        response.status().is_success() || response.status().is_redirection(),
        "Cloudflare 返回 HTTP {}，请检查代理或等待后重试",
        response.status().as_u16()
    );
    Ok(json!({"elapsed_ms": start.elapsed().as_millis()}))
}

pub(crate) fn save_settings(
    db: &mut Db,
    input: Value,
    revision: i32,
    scope: Scope,
) -> Result<Value> {
    persist(db, input, revision, operator_url()?, scope)
}

fn persist(
    db: &mut Db,
    input: Value,
    revision: i32,
    operator: Option<String>,
    scope: Scope,
) -> Result<Value> {
    let selected = selection(input, scope)?;
    db.immediate_transaction(|db| {
        require_access(db, revision, scope)?;
        ensure!(operator.is_none(), "代理由 STARSTACK_PROXY_URL 管理");
        let value = match &selected {
            None => None,
            Some(ProxyConfig::System) if scope == Scope::Instance => None,
            Some(ProxyConfig::System) => Some("system"),
            Some(ProxyConfig::Direct) => Some("direct"),
            Some(ProxyConfig::Custom(url)) => Some(url.as_str()),
        };
        if scope == Scope::Account {
            match value {
                Some(value) => store::set_meta(db, ACCOUNT_PROXY, value)?,
                None => store::delete_meta(db, ACCOUNT_PROXY)?,
            }
            return settings(db, scope, None);
        }
        schema(db)?;
        if let Some(value) = value {
            diesel::insert_into(proxy_configuration::table)
                .values((
                    proxy_configuration::id.eq(1),
                    proxy_configuration::setting.eq(value),
                ))
                .on_conflict(proxy_configuration::id)
                .do_update()
                .set(proxy_configuration::setting.eq(value))
                .execute(db)?;
        } else {
            diesel::delete(proxy_configuration::table.find(1)).execute(db)?;
        }
        settings(db, scope, None)
    })
}

#[cfg(test)]
mod tests;
