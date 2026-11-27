//! Synchronize the authenticated GitHub account's stars, preserving local annotations.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, LINK, RETRY_AFTER};
use reqwest::{StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};

use crate::quota::{Policy as QuotaPolicy, ReadmeSource, Resource, Window};
use crate::store::{self, Change, Db, Repo};

const GITHUB_API: &str = "https://api.github.com";
const MAX_ATTEMPTS: u32 = 4;
mod blobs;
const INITIAL_READMES: &str = "initial_readme_backfill_v1";
const REST_FALLBACK: &str = "pending_readme_rest_fallback_v1";
const PENDING: &str = "pending_readme_ids_v1";
const LAST_README_ATTEMPT: &str = "last_readme_attempt_id_v1";

#[derive(Default, Debug, Serialize)]
pub(crate) struct Summary {
    added: usize,
    updated: usize,
    skipped: usize,
    removed: usize,
    pub failed: usize,
    pub readmes_fetched: usize,
    readme_failures: usize,
    pub pending_readmes: usize,
    readme_pause: Option<String>,
    failure_examples: Vec<String>,
    retry_after_seconds: Option<u64>,
    #[serde(skip)]
    retry_minimum: Option<Duration>,
}

impl Summary {
    fn remember_retry(&mut self, error: &anyhow::Error) {
        if let Some(delay) = retry_minimum(error) {
            self.retry_minimum = Some(self.retry_minimum.unwrap_or_default().max(delay));
        }
    }

    pub(crate) fn retry_minimum(&self) -> Option<Duration> {
        self.retry_minimum
    }

    fn record_failure(&mut self, message: String, progress: &dyn Fn(&str)) {
        self.failed += 1;
        self.record_example(message, progress);
    }

    fn readme_failed(&mut self, name: &str, error: &anyhow::Error, progress: &dyn Fn(&str)) {
        self.readme_failures += 1;
        self.remember_retry(error);
        self.record_example(format!("{name} 的 README 待重试（{error}）"), progress);
    }

    fn record_example(&mut self, message: String, progress: &dyn Fn(&str)) {
        progress(&message);
        if self.failure_examples.len() < 3 {
            self.failure_examples.push(message);
        }
    }

    pub(crate) fn web_report(&self, catalog: bool) -> (String, Vec<String>, Vec<String>) {
        let mut warnings = Vec::new();
        let message = if catalog {
            format!(
                "新增 {}，更新 {}，取消收藏 {}，无需变更 {}",
                self.added, self.updated, self.removed, self.skipped
            )
        } else {
            format!(
                "本轮已补全 {} 个 README，待补全 {}",
                self.readmes_fetched, self.pending_readmes
            )
        };
        if self.failed > 0 {
            warnings.push(format!("{} 项同步失败。可运行全量同步重试。", self.failed));
        }
        if self.readme_failures > 0 {
            warnings.push(format!(
                "{} 个 README 抓取失败，将在后续任务中重试。",
                self.readme_failures
            ));
        }
        let mut details = self.failure_examples.clone();
        if let Some(reason) = &self.readme_pause {
            details.insert(0, reason.clone());
        }
        (message, warnings, details)
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "新增 {}，更新 {}，无需变更 {}，取消收藏 {}，失败 {}",
            self.added, self.updated, self.skipped, self.removed, self.failed
        )?;
        if self.readmes_fetched > 0 || self.pending_readmes > 0 {
            write!(
                f,
                "，README 已获取 {}，待补抓 {}",
                self.readmes_fetched, self.pending_readmes
            )?;
        }
        if let Some(reason) = &self.readme_pause {
            write!(f, "。{reason}")?;
        }
        if self.readme_failures > 0 {
            write!(
                f,
                "，README 抓取失败 {} 个，将在后续同步重试",
                self.readme_failures
            )?;
        }
        if !self.failure_examples.is_empty() {
            write!(f, "。{}", self.failure_examples.join("。"))?;
        }
        if self.failed > 0 {
            if let Some(seconds) = self.retry_after_seconds {
                write!(f, "。请等待 {seconds} 秒后运行 starstack sync --full")?;
            } else {
                write!(f, "。请运行 starstack sync --full 重试")?;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct User {
    id: u64,
}

#[derive(Deserialize)]
struct Star {
    starred_at: DateTime<Utc>,
    repo: RemoteRepo,
}

#[derive(Deserialize)]
struct License {
    spdx_id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct RemoteRepo {
    private: Option<bool>,
    pushed_at: Option<String>,
    default_branch: Option<String>,
    license: Option<License>,
    id: i64,
    full_name: String,
    description: Option<String>,
    language: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    stargazers_count: u64,
    html_url: String,
    archived: bool,
}

impl Star {
    fn into_repo(self) -> Repo {
        let r = self.repo;
        Repo {
            id: r.id,
            full_name: r.full_name,
            description: r.description,
            language: r.language,
            topics: r.topics,
            stargazers_count: r.stargazers_count,
            starred_at: self
                .starred_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            html_url: r.html_url,
            archived: r.archived,
            private: r.private,
            pushed_at: r.pushed_at,
            default_branch: r.default_branch,
            license: r
                .license
                .and_then(|l| l.spdx_id.filter(|id| id != "NOASSERTION").or(l.name)),
            readme: String::new(),
            tags: Vec::new(),
            note: String::new(),
            summary: String::new(),
            code_context: String::new(),
            commit_index: String::new(),
            pinned: false,
            score: None,
        }
    }
}

fn retry_delay(response: &Response, attempt: u32) -> Option<Duration> {
    let headers = response.headers();
    let value = |key: &str| headers.get(key)?.to_str().ok()?.parse::<u64>().ok();
    let wait = value(RETRY_AFTER.as_str());
    let limited = headers
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v == "0");
    let status = response.status();
    if !status.is_server_error()
        && status != StatusCode::TOO_MANY_REQUESTS
        && !(status == StatusCode::FORBIDDEN && (wait.is_some() || limited))
    {
        return None;
    }
    let reset = value("x-ratelimit-reset")
        .filter(|_| limited)
        .map(|v| v.saturating_sub(Utc::now().timestamp().max(0) as u64));
    Some(Duration::from_secs(wait.or(reset).unwrap_or(1 << attempt)))
}

#[derive(Debug)]
struct RateLimit {
    delay: Duration,
}

impl fmt::Display for RateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GitHub 请求达到限额，请在 {} 秒后重试",
            self.delay.as_secs()
        )
    }
}

impl std::error::Error for RateLimit {}

pub(crate) fn retry_minimum(error: &anyhow::Error) -> Option<Duration> {
    error
        .downcast_ref::<RateLimit>()
        .map(|e| e.delay)
        .or_else(|| crate::retry::minimum(error))
}

struct GitHub<'a> {
    client: Client,
    token: &'a str,
    base: &'a str,
}

struct Requests<'a> {
    gh: &'a GitHub<'a>,
    quota: Window,
    observed_this_run: bool,
    policy: QuotaPolicy,
    proxy: crate::proxy::ProxyConfig,
    budget_left: usize,
}

#[derive(Debug)]
struct DeferredReadme(String);
impl fmt::Display for DeferredReadme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for DeferredReadme {}

impl Requests<'_> {
    fn reserve_message(&self) -> String {
        let mut message = format!(
            "已为收藏列表预留 {}% GitHub REST 配额",
            self.policy.reserve_percent
        );
        if let (Some(left), Some(limit), Some(reset)) =
            (self.quota.remaining, self.quota.limit, self.quota.reset_at)
        {
            let wait = (reset - Utc::now().timestamp()).max(0);
            message.push_str(&format!(
                "，当前剩余 {left}/{limit}，约 {wait} 秒后可继续补抓 README"
            ));
        }
        message
    }
    fn reserve_readme(&mut self, github: bool) -> Result<()> {
        if self.budget_left == 0 {
            return Err(DeferredReadme("本轮 README 预算已用完，下次同步将继续补抓".into()).into());
        }
        if github {
            if let Err(error) = self.quota.check() {
                return Err(DeferredReadme(error.to_string()).into());
            }
            if !self.quota.has_headroom(self.policy.reserve_percent) {
                return Err(DeferredReadme(self.reserve_message()).into());
            }
        }
        self.budget_left -= 1;
        Ok(())
    }
    fn get(&mut self, path: &str, progress: &dyn Fn(&str)) -> Result<Response> {
        for attempt in 0..MAX_ATTEMPTS {
            if path.ends_with("/readme") {
                self.reserve_readme(true)?;
            } else {
                self.quota.check()?;
            }
            let response = self
                .gh
                .client
                .get(format!("{}{path}", self.gh.base))
                .bearer_auth(self.gh.token)
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header(
                    ACCEPT,
                    if path.ends_with("/readme") {
                        "application/vnd.github.raw+json"
                    } else if path == "/user" {
                        "application/vnd.github+json"
                    } else {
                        "application/vnd.github.star+json"
                    },
                )
                .send();
            if let Ok(response) = &response {
                self.observed_this_run |= self.quota.observe(response.headers());
            }
            let delay = match response {
                Ok(r)
                    if r.status().is_success()
                        || (path.ends_with("/readme") && r.status() == StatusCode::NOT_FOUND) =>
                {
                    return Ok(r);
                }
                Ok(r) => {
                    let status = r.status();
                    let suggestion = match status {
                        StatusCode::UNAUTHORIZED => "请重新授权 GitHub 或检查访问令牌",
                        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                            "请检查令牌权限和 GitHub API 限额"
                        }
                        _ => "请重试 starstack sync",
                    };
                    let mut retry = retry_delay(&r, attempt);
                    if status == StatusCode::FORBIDDEN
                        && retry.is_none()
                        && r.text()
                            .unwrap_or_default()
                            .contains("secondary rate limit")
                    {
                        retry = Some(Duration::from_secs(60));
                    }
                    let Some(delay) = retry else {
                        bail!("GitHub 请求 {path} 返回 {status}，{suggestion}")
                    };
                    if matches!(
                        status,
                        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
                    ) {
                        self.quota.defer(delay.as_secs());
                    }
                    if delay > Duration::from_secs(30) || attempt + 1 == MAX_ATTEMPTS {
                        if matches!(
                            status,
                            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
                        ) {
                            return Err(RateLimit { delay }.into());
                        }
                        return Err(crate::retry::Transient {
                            message: format!("GitHub 请求 {path} 返回 {status}，{suggestion}"),
                            after: delay,
                        }
                        .into());
                    }
                    delay
                }
                Err(e) if attempt + 1 == MAX_ATTEMPTS => {
                    return Err(e).context("无法连接 GitHub，请检查网络后重试");
                }
                Err(_) => Duration::from_secs(1 << attempt),
            };
            progress(&format!(
                "{} 秒后重试 GitHub 请求（第 {}/{MAX_ATTEMPTS} 次）",
                delay.as_secs(),
                attempt + 2
            ));
            std::thread::sleep(delay);
        }
        unreachable!("final request always returns")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Catalog,
    Readmes,
    PendingOnly,
}

fn pending_ids(db: &mut Db) -> Result<BTreeSet<i64>> {
    store::meta(db, PENDING)?
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .context("读取 README 待补抓队列失败")
        .map(Option::unwrap_or_default)
}

pub(crate) fn enrich(db: &mut Db, progress: &dyn Fn(&str)) -> Result<Summary> {
    if pending_ids(db)?.is_empty() {
        return Ok(Summary::default());
    }
    run_mode(db, false, Mode::PendingOnly, progress)
}

pub(crate) fn catalog(conn: &mut Db, full: bool, progress: &dyn Fn(&str)) -> Result<Summary> {
    run_mode(conn, full, Mode::Catalog, progress)
}

pub(crate) fn run(conn: &mut Db, full: bool, progress: &dyn Fn(&str)) -> Result<Summary> {
    run_mode(conn, full, Mode::Readmes, progress)
}

fn run_mode(conn: &mut Db, full: bool, mode: Mode, progress: &dyn Fn(&str)) -> Result<Summary> {
    let token = crate::auth::github_token(conn)?;
    let api = std::env::var("STARSTACK_GITHUB_API_URL").unwrap_or_else(|_| GITHUB_API.into());
    let gh = GitHub {
        client: crate::proxy::builder(conn)?
            .user_agent("starstack")
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?,
        token: token.trim(),
        base: api.trim_end_matches('/'),
    };
    synchronize(conn, full, mode, &gh, progress)
}

fn synchronize(
    conn: &mut Db,
    full: bool,
    mode: Mode,
    gh: &GitHub<'_>,
    progress: &dyn Fn(&str),
) -> Result<Summary> {
    let quota = Window::load(conn, Resource::Rest)?;
    let policy = QuotaPolicy::load(conn)?;
    let mut requests = Requests {
        gh,
        quota,
        observed_this_run: false,
        proxy: crate::proxy::config(conn)?,
        budget_left: policy.readme_budget,
        policy,
    };
    let result = synchronize_account(conn, full, mode, &mut requests, progress);
    requests.quota.save(conn, Resource::Rest)?;
    result
}

fn synchronize_account(
    conn: &mut Db,
    full: bool,
    mode: Mode,
    gh: &mut Requests<'_>,
    progress: &dyn Fn(&str),
) -> Result<Summary> {
    if mode == Mode::PendingOnly {
        let pending = pending_ids(conn)?;
        let pause = if gh.budget_left == 0 {
            Some("本轮 README 预算为 0".into())
        } else if let Err(error) = gh.quota.check() {
            Some(error.to_string())
        } else if gh.policy.readme_source == ReadmeSource::Github
            && !gh.quota.has_headroom(gh.policy.reserve_percent)
        {
            Some(gh.reserve_message())
        } else {
            None
        };
        if pending.is_empty() || pause.is_some() {
            return Ok(Summary {
                pending_readmes: pending.len(),
                readme_pause: pause,
                ..Default::default()
            });
        }
    }
    if let Some(origin) = store::meta(conn, "github_api")? {
        ensure!(
            origin == gh.gh.base,
            "当前资料库已绑定其他 GitHub API 地址，请恢复原地址，或切换星栈用户"
        );
    }
    let scope_before_auth = gh.quota.clone();
    let account = gh
        .get("/user", progress)?
        .json::<User>()
        .context("无法读取当前 GitHub 账户 ID")?
        .id
        .to_string();
    let (mut revision, cutoff, mut pending) = conn.transaction::<_, anyhow::Error, _>(|db| {
        let owner = store::meta(db, "github_account")?;
        ensure!(
            mode != Mode::PendingOnly || owner.is_some(),
            "尚未绑定 GitHub 账号，请先同步收藏目录"
        );
        if owner.as_ref().is_some_and(|owner| owner != &account) {
            // The authenticated token belongs to a different upstream account and quota window.
            gh.quota = scope_before_auth;
            bail!("当前资料库已绑定其他 GitHub 账户，请重新授权原账户，或切换星栈用户");
        }
        let retry_full = store::meta(db, "retry_full_sync")?.is_some();
        let cutoff: Option<DateTime<Utc>> = if full || retry_full || mode == Mode::PendingOnly {
            None
        } else {
            store::meta(db, "star_cursor")?
                .map(|s| s.parse())
                .transpose()
                .context("同步游标无效，请运行 starstack sync --full")?
        };
        let pending = pending_ids(db)?;
        Ok((store::meta(db, "sync_revision")?, cutoff, pending))
    })?;
    let mut summary = Summary::default();
    if mode == Mode::PendingOnly {
        enrich_queue(
            conn,
            gh,
            &mut revision,
            &mut pending,
            &mut summary,
            mode,
            progress,
        )?;
        return Ok(summary);
    }
    let (mut repos, newest) = fetch_catalog(gh, cutoff, &mut summary, progress);

    // Commit membership before slow network enrichment. A stopped process can resume using the queue.
    let user = store::owner(conn)?;
    conn.immediate_transaction::<_, anyhow::Error, _>(|db| {
        advance_revision(db, &mut revision)?;
        if store::meta(db, "last_sync")?.is_none() {
            store::set_meta(db, INITIAL_READMES, "1")?;
        }
        for repo in repos.values_mut() {
            let saved = db.transaction::<_, anyhow::Error, _>(|tx| save_metadata(tx, &user, repo));
            match saved {
                Ok((change, needs_readme)) => {
                    match change {
                        Change::Added => summary.added += 1,
                        Change::Updated => summary.updated += 1,
                        Change::Unchanged => summary.skipped += 1,
                    }
                    if needs_readme {
                        pending.insert(repo.id);
                    }
                }
                Err(error) => summary
                    .record_failure(format!("保存 {} 失败（{error}）", repo.full_name), progress),
            }
        }
        store::set_meta(db, "github_account", &account)?;
        store::set_meta(db, "github_api", gh.gh.base)?;
        if summary.failed == 0 {
            let completed = Utc::now().to_rfc3339();
            if cutoff.is_none() {
                store::set_meta(db, "last_full_sync", &completed)?;
                let seen = repos.keys().copied().collect();
                summary.removed = store::deactivate_missing(db, &seen)?;
                pending.retain(|id| seen.contains(id));
            }
            if let Some(t) = newest {
                store::set_meta(db, "star_cursor", &t.to_rfc3339())?;
            } else if cutoff.is_none() {
                store::delete_meta(db, "star_cursor")?;
            }
            store::set_meta(db, "last_sync", &completed)?;
            store::delete_meta(db, "retry_full_sync")?;
        } else if cutoff.is_none() {
            store::set_meta(db, "retry_full_sync", "1")?;
        }
        let indexed = store::meta(db, "indexed_library_revision")?;
        if indexed.is_none() || indexed != store::meta(db, "library_revision")? {
            store::set_meta(db, "index_pending", "1")?;
        }
        checkpoint(db, &pending, &mut summary, mode)
    })?;
    gh.quota.save(conn, Resource::Rest)?;
    drop(repos);
    if mode == Mode::Readmes {
        enrich_queue(
            conn,
            gh,
            &mut revision,
            &mut pending,
            &mut summary,
            mode,
            progress,
        )?;
    }
    Ok(summary)
}

fn fetch_catalog(
    gh: &mut Requests<'_>,
    cutoff: Option<DateTime<Utc>>,
    summary: &mut Summary,
    progress: &dyn Fn(&str),
) -> (BTreeMap<i64, Repo>, Option<DateTime<Utc>>) {
    let mut repos = BTreeMap::new();
    let mut newest = None;
    let mut last_page = None;
    let mut consecutive_failures = 0;
    for page in 1_u64.. {
        progress(&format!("正在读取 GitHub 收藏列表第 {page} 页"));
        let path = format!("/user/starred?sort=created&direction=desc&per_page=100&page={page}");
        let response = match gh.get(&path, progress) {
            Ok(response) => response,
            Err(error) => {
                consecutive_failures += 1;
                summary.remember_retry(&error);
                summary.retry_after_seconds =
                    error.downcast_ref::<RateLimit>().map(|r| r.delay.as_secs());
                summary
                    .record_failure(format!("读取收藏列表第 {page} 页失败（{error}）"), progress);
                if summary.retry_after_seconds.is_none()
                    && last_page.is_some_and(|last| page < last)
                    && consecutive_failures < 2
                {
                    continue;
                }
                break;
            }
        };
        // Only page numbers are consumed. Authenticated requests stay on the configured origin.
        let (has_next, last) = pagination(response.headers());
        last_page = last.or(last_page);
        let values = match response.json::<Vec<serde_json::Value>>() {
            Ok(values) => values,
            Err(error) => {
                consecutive_failures += 1;
                summary
                    .record_failure(format!("收藏列表第 {page} 页格式无效（{error}）"), progress);
                if has_next && consecutive_failures < 2 {
                    continue;
                }
                break;
            }
        };
        consecutive_failures = 0;
        let mut crossed_cursor = false;
        for (offset, value) in values.into_iter().enumerate() {
            match serde_json::from_value::<Star>(value) {
                Ok(star) => {
                    // GitHub timestamps have second precision. Include every record at the boundary.
                    crossed_cursor |= cutoff.is_some_and(|cursor| star.starred_at < cursor);
                    newest = Some(
                        newest.map_or(star.starred_at, |t: DateTime<Utc>| t.max(star.starred_at)),
                    );
                    repos
                        .entry(star.repo.id)
                        .or_insert_with(|| star.into_repo());
                }
                Err(e) => {
                    summary.record_failure(
                        format!("第 {page} 页的第 {} 条收藏格式无效（{e}）", offset + 1),
                        progress,
                    );
                }
            }
        }
        if !has_next || crossed_cursor {
            break;
        }
    }

    (repos, newest)
}

fn advance_revision(db: &mut Db, revision: &mut Option<String>) -> Result<()> {
    ensure!(
        store::meta(db, "sync_revision")? == *revision,
        "另一个同步任务已完成，请重试 starstack sync"
    );
    let next: u64 = revision.as_deref().unwrap_or("0").parse()?;
    let next = next.checked_add(1).context("同步版本超出范围")?.to_string();
    store::set_meta(db, "sync_revision", &next)?;
    *revision = Some(next);
    Ok(())
}

fn save_metadata(db: &mut Db, user: &str, repo: &mut Repo) -> Result<(Change, bool)> {
    use store::repositories as t;
    let prior = t::table
        .find((user, repo.id))
        .select((t::metadata, t::active))
        .first::<(String, bool)>(db)
        .optional()?;
    let mut needs_readme = true;
    if let Some((raw, active)) = prior {
        let old: Repo = serde_json::from_str(&raw)?;
        needs_readme =
            !active || old.pushed_at != repo.pushed_at || old.default_branch != repo.default_branch;
        repo.readme = old.readme;
    }
    let change = store::upsert(db, repo)?;
    // Discard the old text after writing each row, keeping catalogue memory proportional to metadata.
    repo.readme = String::new();
    Ok((change, needs_readme))
}

fn checkpoint(
    db: &mut Db,
    pending: &BTreeSet<i64>,
    summary: &mut Summary,
    mode: Mode,
) -> Result<()> {
    summary.pending_readmes = pending.len();
    if pending.is_empty() {
        store::delete_meta(db, INITIAL_READMES)?;
        store::delete_meta(db, REST_FALLBACK)?;
    }
    store::set_meta(db, PENDING, &serde_json::to_string(pending)?)?;
    let key = if mode == Mode::PendingOnly {
        "last_readme_summary"
    } else {
        "last_sync_summary"
    };
    store::set_meta(db, key, &serde_json::to_string(summary)?)
}

fn enrich_queue(
    db: &mut Db,
    gh: &mut Requests<'_>,
    revision: &mut Option<String>,
    pending: &mut BTreeSet<i64>,
    summary: &mut Summary,
    mode: Mode,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let mut queue: Vec<_> = pending.iter().copied().collect();
    // Move past the last attempted repo so permanent per-repo errors cannot starve the queue.
    if let Some(last) = store::meta(db, LAST_README_ATTEMPT)? {
        let last: i64 = last.parse().context("读取 README 队列游标失败")?;
        let next = queue.partition_point(|id| *id <= last);
        queue.rotate_left(next);
    }
    let initial = gh.policy.initial_readme_boost
        && gh.observed_this_run
        && gh.budget_left > 0
        && store::meta(db, INITIAL_READMES)?.is_some();
    let spare = gh.quota.observed_headroom(gh.policy.reserve_percent);
    if initial && let Some(spare) = spare {
        gh.budget_left = gh.budget_left.max(spare.min(1000));
    }
    let mut blob_enabled =
        initial && spare.is_some_and(|n| n > 0) && gh.policy.readme_source == ReadmeSource::Github;
    let mut fallback: BTreeSet<i64> = store::meta(db, REST_FALLBACK)?
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default();
    fallback.retain(|id| pending.contains(id));
    let mut exhausted = false;
    for ids in queue.chunks(if blob_enabled { 3 } else { 8 }) {
        let mut fetched = Vec::new();
        let mut public_batch = Vec::new();
        let mut last_attempt = None;
        let repos = ids
            .iter()
            .map(|&id| store::by_id(db, id))
            .collect::<Result<Vec<_>>>()?;
        let mut blobs = if blob_enabled && gh.budget_left > 0 {
            blobs::read(db, gh, &repos, &mut blob_enabled)?
        } else {
            BTreeMap::new()
        };
        let mut remaining = Vec::new();
        for mut repo in repos {
            let id = repo.id;
            if let Some(text) = blobs.remove(&id) {
                last_attempt = Some(id);
                repo.readme = text;
                fetched.push(repo);
            } else {
                remaining.push(repo);
            }
        }
        for mut repo in remaining {
            let id = repo.id;
            if gh.budget_left == 0 {
                summary.readme_pause = Some("本轮 README 预算已用完，下次同步将继续补抓".into());
                exhausted = true;
                break;
            }
            progress(&format!(
                "正在读取 README，待处理 {}（{}）",
                pending.len() - fetched.len() - public_batch.len(),
                repo.full_name
            ));
            // The scraper reads github.com; enterprise repositories with the same name can differ.
            let use_firecrawl = !fallback.contains(&id)
                && gh.policy.readme_source == ReadmeSource::Firecrawl
                && repo.private == Some(false)
                && repo
                    .html_url
                    .eq_ignore_ascii_case(&format!("https://github.com/{}", repo.full_name));
            if use_firecrawl {
                gh.reserve_readme(false)?;
                last_attempt = Some(id);
                public_batch.push(repo);
                continue;
            }
            let previous_budget = gh.budget_left;
            let result = readme(gh, &repo.full_name, progress);
            if gh.budget_left < previous_budget {
                last_attempt = Some(id);
            }
            match result {
                Ok(text) => {
                    repo.readme = text;
                    fetched.push(repo);
                }
                Err(error) if error.is::<RateLimit>() || error.is::<DeferredReadme>() => {
                    summary.remember_retry(&error);
                    if let Some(limit) = error.downcast_ref::<RateLimit>() {
                        summary.retry_after_seconds = Some(limit.delay.as_secs());
                    }
                    summary.readme_pause = Some(format!("README 暂缓：{error}"));
                    if gh.policy.readme_source == ReadmeSource::Github || gh.budget_left == 0 {
                        exhausted = true;
                        break;
                    }
                }
                Err(error) => summary.readme_failed(&repo.full_name, &error, progress),
            }
        }
        if !public_batch.is_empty() {
            let names: Vec<_> = public_batch
                .iter()
                .map(|repo| repo.full_name.clone())
                .collect();
            let results = crate::firecrawl::readmes(&names, &gh.proxy);
            ensure!(
                results.len() == public_batch.len(),
                "Firecrawl 返回的结果数量异常"
            );
            for (mut repo, result) in public_batch.into_iter().zip(results) {
                let result = result.and_then(|text| {
                    ensure!(!text.trim().is_empty(), "公开页面没有可识别的 README");
                    Ok(text)
                });
                match result {
                    Ok(text) => {
                        repo.readme = text;
                        fetched.push(repo);
                    }
                    Err(error) => {
                        fallback.insert(repo.id);
                        if gh.budget_left == 0 || !gh.quota.has_headroom(gh.policy.reserve_percent)
                        {
                            summary.readme_failed(&repo.full_name, &error, progress);
                            continue;
                        }
                        match readme(gh, &repo.full_name, progress) {
                            Ok(text) => {
                                repo.readme = text;
                                fetched.push(repo);
                            }
                            Err(failure)
                                if failure.is::<RateLimit>() || failure.is::<DeferredReadme>() =>
                            {
                                summary.remember_retry(&failure);
                                summary.readme_pause = Some(format!("README 暂缓：{failure}"));
                            }
                            Err(failure) => {
                                summary.readme_failed(&repo.full_name, &failure, progress)
                            }
                        }
                    }
                }
            }
        }
        db.immediate_transaction::<_, anyhow::Error, _>(|db| {
            advance_revision(db, revision)?;
            if !fetched.is_empty() {
                store::set_meta(db, "index_pending", "1")?;
            }
            for repo in fetched {
                store::upsert(db, &repo)?;
                pending.remove(&repo.id);
                fallback.remove(&repo.id);
                summary.readmes_fetched += 1;
            }
            if fallback.is_empty() {
                store::delete_meta(db, REST_FALLBACK)?;
            } else {
                store::set_meta(db, REST_FALLBACK, &serde_json::to_string(&fallback)?)?;
            }
            if let Some(last) = last_attempt {
                store::set_meta(db, LAST_README_ATTEMPT, &last.to_string())?;
            }
            checkpoint(db, pending, summary, mode)
        })?;
        gh.quota.save(db, Resource::Rest)?;
        if exhausted {
            break;
        }
    }
    Ok(())
}

fn pagination(headers: &reqwest::header::HeaderMap) -> (bool, Option<u64>) {
    let mut has_next = false;
    let mut last = None;
    for link in headers
        .get_all(LINK)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(','))
    {
        let mut parts = link.split(';');
        let url = parts
            .next()
            .unwrap_or_default()
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        for relation in parts.map(str::trim) {
            if relation == "rel=\"next\"" {
                has_next = true;
            }
            if relation == "rel=\"last\""
                && let Ok(url) = reqwest::Url::parse(url)
            {
                last = url
                    .query_pairs()
                    .find(|(k, _)| k == "page")
                    .and_then(|(_, v)| v.parse().ok());
            }
        }
    }
    (has_next, last)
}

fn readme(gh: &mut Requests<'_>, full_name: &str, progress: &dyn Fn(&str)) -> Result<String> {
    let (owner, name) = full_name.split_once('/').context("GitHub 仓库名称无效")?;
    ensure!(
        [owner, name].into_iter().all(|s| !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "GitHub 仓库名称无效"
    );
    let response = gh.get(&format!("/repos/{full_name}/readme"), progress)?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(String::new());
    }
    let mut bytes = Vec::new();
    response
        .take(64 * 1024)
        .read_to_end(&mut bytes)
        .context("读取 GitHub README 失败")?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests;
