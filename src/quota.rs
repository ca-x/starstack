//! Durable observations are shared by upstream account and resource. Budgets are personal.
use crate::store::{self, Db};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use diesel::prelude::*;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

diesel::table! {github_windows(scope,resource){scope->Text,resource->Text,observations->Text,}}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resource {
    Rest,
    Graphql,
}
impl Resource {
    fn name(self) -> &'static str {
        match self {
            Self::Rest => "core",
            Self::Graphql => "graphql",
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct Window {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_at: Option<i64>,
    pub blocked_until: Option<i64>,
    #[serde(skip)]
    resource: Option<Resource>,
}
fn scope(db: &mut Db) -> Result<String> {
    if let Some(account) = store::meta(db, "github_account")? {
        let api = std::env::var("STARSTACK_GITHUB_API_URL")
            .unwrap_or_else(|_| "https://api.github.com".to_owned());
        Ok(format!("{api}/{account}"))
    } else {
        Ok(format!("local/{}", store::owner(db)?))
    }
}
impl Window {
    pub fn load(db: &mut Db, resource: Resource) -> Result<Self> {
        let raw = github_windows::table
            .find((scope(db)?, resource.name()))
            .select(github_windows::observations)
            .first::<String>(db)
            .optional()?;
        let mut w: Self = raw
            .map(|v| serde_json::from_str(&v))
            .transpose()?
            .unwrap_or_default();
        w.resource = Some(resource);
        Ok(w)
    }
    pub fn observe(&mut self, headers: &HeaderMap) {
        if let (Some(expected), Some(actual)) = (self.resource, headers.get("x-ratelimit-resource"))
            && actual != expected.name()
        {
            return;
        }
        let number = |key: &str| headers.get(key)?.to_str().ok()?.parse::<u64>().ok();
        if let (Some(limit), Some(left), Some(reset)) = (
            number("x-ratelimit-limit"),
            number("x-ratelimit-remaining"),
            number("x-ratelimit-reset"),
        ) {
            self.limit = Some(limit);
            self.remaining = Some(left);
            self.reset_at = i64::try_from(reset).ok();
        }
    }
    pub fn defer(&mut self, seconds: u64) {
        let until = Utc::now()
            .timestamp()
            .saturating_add(seconds.min(i64::MAX as u64) as i64);
        self.blocked_until = Some(self.blocked_until.unwrap_or(0).max(until));
    }
    pub fn check(&self) -> Result<()> {
        let now = Utc::now().timestamp();
        let stop = self
            .blocked_until
            .unwrap_or(0)
            .max(if self.remaining == Some(0) {
                self.reset_at.unwrap_or(0)
            } else {
                0
            });
        ensure!(
            stop <= now,
            "GitHub 额度正在冷却，请在 {} 秒后重试；已保存的收藏仍可使用",
            stop - now
        );
        Ok(())
    }
    pub fn has_headroom(&self, percent: u8) -> bool {
        if self.check().is_err() {
            return false;
        }
        match (self.limit, self.remaining, self.reset_at) {
            (Some(limit), Some(left), Some(reset)) if reset > Utc::now().timestamp() => {
                left > limit.saturating_mul(u64::from(percent)).div_ceil(100)
            }
            _ => true,
        }
    }
    pub fn save(&self, db: &mut Db, resource: Resource) -> Result<()> {
        use github_windows as w;
        db.immediate_transaction(|db| {
            let existing = Self::load(db, resource)?;
            let mut next = self.clone();
            if existing.reset_at > next.reset_at {
                next = existing.clone();
            } else if existing.reset_at == next.reset_at {
                next.remaining = match (next.remaining, existing.remaining) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
            next.blocked_until = existing.blocked_until.max(self.blocked_until);
            let key = scope(db)?;
            let raw = serde_json::to_string(&next)?;
            diesel::insert_into(w::table)
                .values((
                    w::scope.eq(key),
                    w::resource.eq(resource.name()),
                    w::observations.eq(&raw),
                ))
                .on_conflict((w::scope, w::resource))
                .do_update()
                .set(w::observations.eq(&raw))
                .execute(db)?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReadmeSource {
    #[default]
    Github,
    Firecrawl,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct Policy {
    pub reserve_percent: u8,
    pub readme_budget: usize,
    pub readme_source: ReadmeSource,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            reserve_percent: 5,
            readme_budget: 200,
            readme_source: ReadmeSource::Github,
        }
    }
}
impl Policy {
    pub fn load(db: &mut Db) -> Result<Self> {
        Ok(store::meta(db, "quota_policy")?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default())
    }
    pub fn save(self, db: &mut Db) -> Result<()> {
        ensure!(
            self.reserve_percent <= 50 && self.readme_budget <= 5000,
            "额度预留范围为 0–50%，README 预算范围为 0–5000"
        );
        ensure!(
            self.readme_source != ReadmeSource::Firecrawl || cfg!(feature = "firecrawl-local"),
            "请使用启用 firecrawl-local 的星栈构建"
        );
        store::set_meta(db, "quota_policy", &serde_json::to_string(&self)?)
    }
}
pub(crate) fn readme_interval(db: &mut Db) -> Result<u64> {
    let input = store::meta(db, "readme_interval_minutes")?
        .or_else(|| std::env::var("STARSTACK_README_INTERVAL_MINUTES").ok())
        .unwrap_or_else(|| "30".into());
    let n = input
        .parse()
        .context("README 自动补全周期应为 0–10080 分钟")?;
    ensure!(n <= 10080, "README 自动补全周期不能超过 10080 分钟");
    Ok(n)
}
pub(crate) fn pending(db: &mut Db) -> Result<usize> {
    let raw = store::meta(db, "pending_readme_ids_v1")?;
    Ok(raw
        .map(|s| serde_json::from_str::<Vec<i64>>(&s))
        .transpose()?
        .map_or(0, |ids| ids.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merges_observations_for_a_shared_account_and_keeps_policies_personal() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut a = store::connect_as(dir.path(), "a")?;
        let mut b = store::connect_as(dir.path(), "b")?;
        let mut other = store::connect_as(dir.path(), "other")?;
        for db in [&mut a, &mut b] {
            store::set_meta(db, "github_account", "42")?;
        }
        store::set_meta(&mut other, "github_account", "43")?;
        let w = Window {
            limit: Some(1000),
            remaining: Some(40),
            reset_at: Some(Utc::now().timestamp() + 3600),
            ..Window::default()
        };
        w.save(&mut a, Resource::Rest)?;
        let mut slow = w.clone();
        slow.remaining = Some(45);
        slow.save(&mut b, Resource::Rest)?;
        let observed = Window::load(&mut b, Resource::Rest)?;
        assert_eq!(
            observed.remaining,
            Some(40),
            "late responses cannot restore spent quota"
        );
        assert!(!observed.has_headroom(5));
        assert!(observed.has_headroom(2));
        assert_eq!(Window::load(&mut other, Resource::Rest)?.remaining, None);
        assert_eq!(Window::load(&mut a, Resource::Graphql)?.remaining, None);
        Policy {
            reserve_percent: 9,
            ..Policy::default()
        }
        .save(&mut b)?;
        assert_eq!(Policy::load(&mut a)?.reserve_percent, 5);
        let mut throttled = observed;
        throttled.defer(30);
        throttled.save(&mut a, Resource::Rest)?;
        assert!(
            Window::load(&mut b, Resource::Rest)?.check().is_err(),
            "cooldown applies to the account sharing this budget"
        );
        assert!(Window::load(&mut other, Resource::Rest)?.check().is_ok());
        Ok(())
    }
}
