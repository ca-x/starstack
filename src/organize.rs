//! Preview a tenant's active stars and append approved private tags in a single transaction.
use anyhow::{Context, Result, ensure};
use diesel::{dsl::sql, prelude::*, sql_types::Text};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::store::{self, Db, Filter, repositories as r};

pub(crate) const BATCH_SIZE: usize = 20;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewInput {
    pub filter: Option<Filter>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Batch {
    pub repos: Vec<Entry>,
    pub known_tags: Vec<String>,
    pub total: usize,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Entry {
    pub id: i64,
    pub full_name: String,
    pub html_url: String,
    pub description: Option<String>,
    pub language: Option<String>,
    pub topics: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub lists: Vec<String>,
}

/// Snapshot matching IDs only. A long-running job can then page without re-evaluating tag filters.
pub(crate) fn snapshot_ids(db: &mut Db, filter: &Filter) -> Result<Vec<i64>> {
    filter.validate()?;
    Ok(store::eligible(store::owner(db)?, filter)
        .select(r::id)
        .order(r::id)
        .load(db)?)
}

pub(crate) fn prepare(db: &mut Db, input: PreviewInput) -> Result<Batch> {
    let filter = input.filter.unwrap_or_default();
    filter.validate()?;
    let limit = input.limit.unwrap_or(BATCH_SIZE);
    ensure!(
        (1..=BATCH_SIZE).contains(&limit),
        "每批仓库数量应为 1 至 20"
    );
    let offset = input.offset.unwrap_or(0);
    db.transaction(|db| {
        let owner = store::owner(db)?;
        let query = || store::eligible(owner.clone(), &filter);
        let total = usize::try_from(query().count().get_result::<i64>(db)?)?;
        let ids = query()
            .select(r::id)
            .order(r::id)
            .limit(i64::try_from(limit)?)
            .offset(i64::try_from(offset)?)
            .load::<i64>(db)?;
        let next = offset + ids.len();
        let mut batch = load_batch(db, &owner, &ids)?;
        batch.total = total;
        batch.next_offset = (next < total).then_some(next);
        Ok(batch)
    })
}

pub(crate) fn by_ids(db: &mut Db, ids: &[i64]) -> Result<Batch> {
    ensure!(ids.len() <= BATCH_SIZE, "每批最多整理 20 个仓库");
    db.transaction(|db| {
        let owner = store::owner(db)?;
        load_batch(db, &owner, ids)
    })
}

fn load_batch(db: &mut Db, owner: &str, ids: &[i64]) -> Result<Batch> {
    let compact = sql::<Text>("json_remove(metadata, '$.readme')");
    let lists = sql::<Text>(
        "(SELECT json_group_array(name) FROM (
        SELECT DISTINCT l.name FROM github_list_members m JOIN github_lists l
        ON l.owner_id = m.owner_id AND l.list_id = m.list_id
        WHERE m.owner_id = repositories.owner_id AND m.repo_id = repositories.id ORDER BY l.name))",
    );
    let rows = r::table
        .filter(
            r::owner_id
                .eq(owner)
                .and(r::active.eq(true))
                .and(r::id.eq_any(ids)),
        )
        .select((compact, r::tags, lists))
        .order(r::id)
        .load::<(String, String, String)>(db)?;
    let mut repos = Vec::with_capacity(rows.len());
    for (metadata, tags, lists) in rows {
        let mut entry: Entry = serde_json::from_str(&metadata)?;
        entry.tags = serde_json::from_str(&tags)?;
        entry.lists = serde_json::from_str(&lists)?;
        repos.push(entry);
    }

    #[derive(QueryableByName)]
    struct KnownTag {
        #[diesel(sql_type = Text)]
        tag: String,
    }
    let known_tags = diesel::sql_query("SELECT DISTINCT t.value AS tag FROM repositories AS r
        JOIN json_each(r.tags) AS t WHERE r.owner_id = ? AND r.active = 1 ORDER BY tag COLLATE NOCASE, tag")
        .bind::<Text,_>(owner)
        .load::<KnownTag>(db)?
        .into_iter().map(|v| v.tag).collect();
    Ok(Batch {
        total: repos.len(),
        next_offset: None,
        repos,
        known_tags,
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyInput {
    pub items: Vec<ApplyItem>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyItem {
    pub id: i64,
    pub tags: Vec<String>,
}

pub(crate) fn apply(db: &mut Db, input: ApplyInput) -> Result<Value> {
    ensure!(input.items.len() <= BATCH_SIZE, "每批最多整理 20 个仓库");
    db.immediate_transaction(|db| apply_in_transaction(db, input))
}

pub(crate) fn apply_in_transaction(db: &mut Db, input: ApplyInput) -> Result<Value> {
    let owner = store::owner(db)?;
    let mut ids = BTreeSet::new();
    let mut updated = 0;
    let mut added = 0;
    for item in input.items {
        ensure!(ids.insert(item.id), "同一批次重复指定仓库");
        ensure!(item.tags.len() <= 5, "每个仓库每次最多追加 5 个标签");
        let row = r::table.find((&owner, item.id)).filter(r::active.eq(true));
        let raw = row
            .select(r::tags)
            .first::<String>(db)
            .optional()?
            .context("当前收藏中没有这个仓库")?;
        let mut tags: Vec<String> = serde_json::from_str(&raw)?;
        let mut seen = tags
            .iter()
            .map(|s| s.to_lowercase())
            .collect::<BTreeSet<_>>();
        let prior = tags.len();
        for tag in item.tags {
            let tag = tag.trim();
            ensure!(
                !tag.is_empty() && tag.len() <= 256 && !tag.chars().any(char::is_control),
                "标签需要 1 至 256 字节且不能包含控制字符"
            );
            if seen.insert(tag.to_lowercase()) {
                tags.push(tag.to_owned());
            }
        }
        if tags.len() != prior {
            ensure!(tags.len() <= 100, "每个仓库最多保存 100 个标签");
            diesel::update(row)
                .set(r::tags.eq(serde_json::to_string(&tags)?))
                .execute(db)?;
            added += tags.len() - prior;
            updated += 1;
        }
    }
    if updated > 0 {
        store::bump_revision(db)?;
    }
    Ok(json!({"updated":updated,"added":added}))
}

#[cfg(test)]
mod tests;
