//! Shared application operations used by the three entry points.
use crate::{
    index,
    store::{self, Db, Filter, Repo},
    sync,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fmt::Write as _, path::PathBuf};
#[derive(Clone)]
pub(crate) struct Library {
    pub dir: PathBuf,
    pub user: String,
}
impl Library {
    pub fn open(&self) -> Result<Db> {
        store::connect_as(&self.dir, &self.user)
    }
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T> + Send + 'static,
    {
        let lib = self.clone();
        tokio::task::spawn_blocking(move || f(&mut lib.open()?))
            .await
            .context("后台任务中断")?
    }
}
#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct Query {
    /// Describe what you need, in Chinese or English.
    pub query: String,
    #[serde(flatten)]
    pub filter: Filter,
    /// Maximum number of returned repositories (1 to 100).
    pub limit: Option<usize>,
}
impl Query {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.query.trim().is_empty() && self.query.len() <= 8192,
            "检索词应为 1 至 8192 字节"
        );
        ensure!(
            (1..=100).contains(&self.limit.unwrap_or(10)),
            "检索条数应在 1 至 100 之间"
        );
        self.filter.validate()
    }
}
pub(crate) fn search(db: &mut Db, q: &Query) -> Result<Vec<Repo>> {
    q.validate()?;
    let limit = q.limit.unwrap_or(10);
    match index::search(db, &q.query, &q.filter, limit) {
        Err(error) if error.is::<index::Stale>() => {
            index::build(db, false, &|_| {})?;
            index::search(db, &q.query, &q.filter, limit)
        }
        result => result,
    }
}
/// Keep long READMEs in the collection; cards and AI sources only need their metadata.
pub(crate) fn cards(rows: Vec<Repo>) -> Vec<serde_json::Value> {
    rows.into_iter()
        .map(|mut r| {
            r.readme.clear();
            let mut v = serde_json::json!(r);
            if let Some(o) = v.as_object_mut() {
                o.remove("readme");
            }
            v
        })
        .collect()
}
#[derive(Serialize)]
pub(crate) struct SyncResult {
    pub sync: sync::Summary,
    pub lists: String,
    pub index: String,
    pub index_ready: bool,
}
impl std::fmt::Display for SyncResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}。{}。{}", self.sync, self.lists, self.index)
    }
}
pub(crate) fn refresh(db: &mut Db, full: bool, progress: &dyn Fn(&str)) -> Result<SyncResult> {
    let sync = sync::run(db, full, progress)?;
    progress(&sync.to_string());
    let lists =
        crate::lists::refresh(db, progress).unwrap_or_else(|e| format!("分类未刷新：{e:#}"));
    let (index, index_ready) = match index::build(db, false, progress) {
        Ok(s) => (s, true),
        Err(e) => (format!("同步数据已保存。索引待处理：{e:#}"), false),
    };
    Ok(SyncResult {
        sync,
        lists,
        index,
        index_ready,
    })
}
#[derive(Clone, Copy, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    Markdown,
    Json,
}
pub(crate) fn export(rows: &[Repo], format: Format) -> Result<String> {
    if matches!(format, Format::Json) {
        return Ok(serde_json::to_string_pretty(rows)? + "\n");
    }
    fn plain(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace(['\r', '\n'], " ")
            .replace('[', "\\[")
            .replace(']', "\\]")
    }
    let mut out = format!("# 星栈精选\n\n共 {} 个仓库。\n", rows.len());
    for r in rows {
        writeln!(
            out,
            "\n## [{}]({})\n",
            plain(&r.full_name),
            r.html_url.replace(')', "%29")
        )?;
        if let Some(d) = &r.description {
            writeln!(out, "{}\n", plain(d))?;
        }
        writeln!(
            out,
            "{} · ☆ {} · 收藏于 {}{}",
            r.language.as_deref().unwrap_or("未标注语言"),
            r.stargazers_count,
            r.starred_at.get(..10).unwrap_or(&r.starred_at),
            if r.pinned { " · 已置顶" } else { "" }
        )?;
        if !r.tags.is_empty() {
            writeln!(out, "\n标签　{}", plain(&r.tags.join("、")))?;
        }
        if !r.summary.is_empty() {
            writeln!(out, "\n{}", plain(&r.summary))?;
        }
        if !r.note.is_empty() {
            writeln!(out, "\n> {}", plain(&r.note))?;
        }
    }
    Ok(out)
}
pub(crate) fn interval(db: &mut Db) -> Result<u64> {
    let value = store::meta(db, "sync_interval_minutes")?
        .or_else(|| std::env::var("STARSTACK_SYNC_INTERVAL_MINUTES").ok())
        .unwrap_or_else(|| "360".into());
    let n = value
        .parse::<u64>()
        .context("同步周期必须是分钟数，0 表示关闭")?;
    ensure!(n <= 10080, "同步周期范围为 0 至 10080 分钟");
    Ok(n)
}
pub(crate) fn approve(db: &mut Db, id: &str) -> Result<()> {
    db.immediate_transaction(|db| {
        ensure!(
            store::meta(db, "pending_sync")?.as_deref() == Some(id),
            "同步请求已失效，请重新请求"
        );
        store::delete_meta(db, "pending_sync")
    })
}
