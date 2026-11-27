//! An opt-in commit observation log. SHA-addressed rows are insert-only.
use crate::{
    repo_code,
    store::{self, Db},
};
use anyhow::{Context, Result, ensure};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

diesel::table! { commit_tracking (owner_id, repo_id) { owner_id -> Text, repo_id -> BigInt, state -> Text, } }
diesel::table! { repository_commits (owner_id, repo_id, sha) {
    owner_id -> Text, repo_id -> BigInt, sha -> Text, committed_at -> Text, message -> Text,
} }

#[derive(Clone, Serialize, Deserialize)]
struct State {
    enabled: bool,
    limit: usize,
    baseline: Option<String>,
    pending: Option<Crawl>,
    warning: Option<String>,
    #[serde(default)]
    backfill: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            enabled: false,
            limit: 20,
            baseline: None,
            pending: None,
            warning: None,
            backfill: false,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Crawl {
    anchor: String,
    page: usize,
}

fn read(db: &mut Db, owner: &str, id: i64) -> Result<(Option<String>, State)> {
    let raw: Option<String> = commit_tracking::table
        .find((owner, id))
        .select(commit_tracking::state)
        .first(db)
        .optional()?;
    let state = raw
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    Ok((raw, state))
}

fn write(db: &mut Db, owner: &str, id: i64, state: &State) -> Result<()> {
    use commit_tracking as t;
    let raw = serde_json::to_string(state)?;
    diesel::insert_into(t::table)
        .values((t::owner_id.eq(owner), t::repo_id.eq(id), t::state.eq(&raw)))
        .on_conflict((t::owner_id, t::repo_id))
        .do_update()
        .set(t::state.eq(&raw))
        .execute(db)?;
    Ok(())
}

pub(crate) fn configure(db: &mut Db, id: i64, enabled: bool, limit: usize) -> Result<()> {
    ensure!(limit <= 10_000, "提交条数可设为 1 至 10000，或选全部");
    let owner = store::owner(db)?;
    db.immediate_transaction(|db| {
        store::by_id(db, id)?;
        let (_, mut state) = read(db, &owner, id)?;
        if state.enabled != enabled || state.limit != limit {
            state.pending = None;
            if state.limit != limit {
                state.backfill = true;
            }
        }
        state.enabled = enabled;
        state.limit = limit;
        write(db, &owner, id, &state)?;
        project(db, &owner, id, enabled)
    })
}

#[derive(Serialize, Queryable)]
struct Record {
    sha: String,
    committed_at: String,
    message: String,
}

fn records(db: &mut Db, owner: &str, id: i64, offset: i64, limit: i64) -> Result<Vec<Record>> {
    use repository_commits as r;
    Ok(r::table
        .filter(r::owner_id.eq(owner).and(r::repo_id.eq(id)))
        .order((r::committed_at.desc(), r::sha.desc()))
        .offset(offset)
        .limit(limit)
        .select((r::sha, r::committed_at, r::message))
        .load(db)?)
}

pub(crate) fn status(db: &mut Db, id: i64, offset: u32) -> Result<Value> {
    use repository_commits as r;
    let repo = store::by_id(db, id)?;
    let owner = store::owner(db)?;
    let (_, state) = read(db, &owner, id)?;
    let total: i64 = r::table
        .filter(r::owner_id.eq(&owner).and(r::repo_id.eq(id)))
        .count()
        .first(db)?;
    Ok(
        json!({"enabled":state.enabled,"limit":state.limit,"baseline":state.baseline,"incomplete":state.pending.is_some(),
        "warning":state.warning,"total":total,"entries":records(db,&owner,id,i64::from(offset),25)?,"repository":repo.full_name}),
    )
}

fn project(db: &mut Db, owner: &str, id: i64, enabled: bool) -> Result<()> {
    use store::repositories as r;
    let text = if enabled {
        records(db, owner, id, 0, 100)?
            .into_iter()
            .map(|commit| {
                format!(
                    "{} {}",
                    commit.committed_at,
                    commit.message.chars().take(350).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    let changed = diesel::update(r::table.find((owner, id)).filter(r::commit_index.ne(&text)))
        .set(r::commit_index.eq(&text))
        .execute(db)?;
    if changed > 0 {
        store::bump_revision(db)?;
    }
    Ok(())
}

pub(crate) fn refresh(db: &mut Db, id: i64) -> Result<()> {
    use repository_commits as r;
    let owner = store::owner(db)?;
    let repo = store::by_id(db, id)?;
    for _ in 0..3 {
        let (raw, mut state) = read(db, &owner, id)?;
        ensure!(state.enabled, "请先为该仓库启用提交记录");
        let page = state.pending.as_ref().map_or(1, |crawl| crawl.page);
        let mut rows = repo_code::commits(
            db,
            &repo,
            state.pending.as_ref().map(|c| c.anchor.as_str()),
            page,
        )?;
        if page == 1 {
            let Some(first) = rows.first() else {
                return Ok(());
            };
            ensure!(valid_sha(&first.sha), "GitHub 返回的提交 SHA 无效");
            state.pending = Some(Crawl {
                anchor: first.sha.clone(),
                page: 1,
            });
            if let Some(old) = &state.baseline
                && old != &first.sha
            {
                match repo_code::compare_commits(db, &repo, old, &first.sha) {
                    Ok(true) => {}
                    Ok(false) => {
                        state.warning = Some(format!(
                            "默认分支历史已改写。旧基线 {old}，新基线 {}。已保留旧记录。",
                            first.sha
                        ))
                    }
                    Err(_) => {
                        state.warning = Some(format!(
                            "无法核对旧基线 {old} 与新提交 {} 的关系。已保留旧记录。",
                            first.sha
                        ))
                    }
                }
            }
        }
        let observed = rows.len();
        let same_head = if state.backfill {
            None
        } else {
            rows.iter()
                .position(|row| state.baseline.as_ref() == Some(&row.sha))
        };
        if let Some(index) = same_head {
            rows.truncate(index + 1);
        }
        let remaining = if state.limit == 0 {
            usize::MAX
        } else {
            state.limit.saturating_sub((page - 1).saturating_mul(100))
        };
        rows.truncate(remaining);
        let complete = same_head.is_some() || observed < 100 || observed >= remaining;
        let crawl = state.pending.as_mut().context("提交分页缺少基线")?;
        if complete {
            state.baseline = Some(crawl.anchor.clone());
            state.pending = None;
            state.backfill = false;
        } else {
            crawl.page += 1;
        }
        db.immediate_transaction::<_, anyhow::Error, _>(|db| {
            store::by_id(db, id)?;
            ensure!(
                read(db, &owner, id)?.0 == raw,
                "提交记录已在其他页面更新，请重新打开仓库详情"
            );
            for row in rows {
                ensure!(valid_sha(&row.sha), "GitHub 返回的提交 SHA 无效");
                let timestamp = chrono::DateTime::parse_from_rfc3339(&row.commit.committer.date)?
                    .to_utc()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let message = row.commit.message;
                let key = (&owner, id, &row.sha);
                let existing: Option<(String, String)> = r::table
                    .find(key)
                    .select((r::committed_at, r::message))
                    .first(db)
                    .optional()?;
                if let Some(existing) = existing {
                    ensure!(
                        existing == (timestamp, message),
                        "同一提交 SHA 的内容与本地记录不一致。已保留旧记录。"
                    );
                } else {
                    diesel::insert_into(r::table)
                        .values((
                            r::owner_id.eq(&owner),
                            r::repo_id.eq(id),
                            r::sha.eq(row.sha),
                            r::committed_at.eq(timestamp),
                            r::message.eq(message),
                        ))
                        .execute(db)?;
                }
            }
            write(db, &owner, id, &state)?;
            project(db, &owner, id, true)
        })?;
        if complete {
            return Ok(());
        }
    }
    Ok(())
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}
