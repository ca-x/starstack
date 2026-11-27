//! Tenant-scoped collection storage. GitHub writes never own annotation columns.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use chrono::NaiveDate;
use diesel::{connection::SimpleConnection, prelude::*};
use serde::{Deserialize, Serialize};

pub(crate) type Db = SqliteConnection;
pub(crate) const PRIMARY: &str = "primary";

diesel::table! { repositories (owner_id, id) {
    owner_id -> Text, id -> BigInt, metadata -> Text, tags -> Text, note -> Text, pinned -> Bool, active -> Bool, summary -> Text, code_context -> Text, commit_index -> Text,
}}
diesel::table! { repository_fts (rowid) {
    rowid -> BigInt, owner -> Text, repo_id -> BigInt, name -> Text, body -> Text,
}}
diesel::table! { metadata_values (owner_id, key) {owner_id -> Text, key -> Text, value -> Text,}}
diesel::table! { current_library (owner_id) { owner_id -> Text, directory -> Text, }}

pub(crate) fn home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("STARSTACK_HOME").filter(|d| !d.is_empty()) {
        return Ok(dir.into());
    }
    Ok(dirs::data_local_dir()
        .context("无法定位本地数据目录，请设置 STARSTACK_HOME")?
        .join("starstack"))
}

#[cfg(test)]
pub(crate) fn connect(dir: &Path) -> Result<Db> {
    connect_as(dir, PRIMARY)
}

pub(crate) fn connect_as(dir: &Path, user: &str) -> Result<Db> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .context("无法创建 STARSTACK_HOME，请检查目录权限")?;
    let path = dir.join("starstack.sqlite3");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&path)?;
    let mut conn = Db::establish(path.to_str().context("数据目录路径需要使用 UTF-8")?)?;
    conn.batch_execute(
        "PRAGMA busy_timeout=10000; PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;",
    )?;
    #[derive(QueryableByName)]
    struct SchemaVersion {
        #[diesel(sql_type=diesel::sql_types::Integer)]
        user_version: i32,
    }
    let version = diesel::sql_query("PRAGMA user_version")
        .get_result::<SchemaVersion>(&mut conn)?
        .user_version;
    ensure!(
        (0..=4).contains(&version),
        "数据库版本 {version} 高于此程序支持的版本，请使用较新的星栈"
    );
    if version < 4 {
        conn.immediate_transaction::<_, diesel::result::Error, _>(|db| {
            let version = diesel::sql_query("PRAGMA user_version").get_result::<SchemaVersion>(db)?.user_version;
            if version == 0 { db.batch_execute(include_str!("schema.sql"))?; }
            if version < 2 {
                db.batch_execute(include_str!("lists_schema.sql"))?;
                db.batch_execute("CREATE TABLE IF NOT EXISTS github_windows(scope TEXT NOT NULL,resource TEXT NOT NULL,observations TEXT NOT NULL,PRIMARY KEY(scope,resource))")?;
            }
            if version < 3 {
                db.batch_execute(include_str!("commits_schema.sql"))?;
                db.batch_execute("ALTER TABLE repositories ADD COLUMN summary TEXT NOT NULL DEFAULT ''; ALTER TABLE repositories ADD COLUMN code_context TEXT NOT NULL DEFAULT ''; ALTER TABLE repositories ADD COLUMN commit_index TEXT NOT NULL DEFAULT ''; PRAGMA user_version=3")?;
            }
            if version < 4 { db.batch_execute(include_str!("search_schema.sql"))?; }
            Ok(())
        })?;
    }
    conn.batch_execute(
        "CREATE TEMP TABLE current_library(owner_id TEXT PRIMARY KEY,directory TEXT NOT NULL)",
    )?;
    diesel::insert_into(current_library::table)
        .values((
            current_library::owner_id.eq(user),
            current_library::directory.eq(dir.to_str().context("无效目录名")?),
        ))
        .execute(&mut conn)?;
    Ok(conn)
}
pub(crate) fn owner(db: &mut Db) -> Result<String> {
    Ok(current_library::table
        .select(current_library::owner_id)
        .first(db)?)
}
pub(crate) fn data_dir(db: &mut Db) -> Result<PathBuf> {
    Ok(current_library::table
        .select(current_library::directory)
        .first::<String>(db)?
        .into())
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub(crate) struct Repo {
    pub id: i64,
    pub full_name: String,
    pub description: Option<String>,
    pub language: Option<String>,
    pub topics: Vec<String>,
    pub stargazers_count: u64,
    pub starred_at: String,
    pub html_url: String,
    pub archived: bool,
    #[serde(default)]
    pub private: Option<bool>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub readme: String,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(skip)]
    pub code_context: String,
    #[serde(skip)]
    pub commit_index: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Debug, Default, Clone, PartialEq, clap::Args, Deserialize, schemars::JsonSchema)]
pub(crate) struct Filter {
    /// Pre-encoded ID array for the SQLite exclusion predicate on subsequent search pages.
    #[arg(skip)]
    #[serde(skip)]
    pub seen_json: Option<String>,
    /// Primary programming language, e.g. Rust
    #[arg(long)]
    pub language: Option<String>,
    /// Exact GitHub topic
    #[arg(long)]
    pub topic: Option<String>,
    /// Exact name of a manually curated GitHub Star List
    #[arg(long)]
    pub list: Option<String>,
    /// Exact private tag
    #[arg(long)]
    pub tag: Option<String>,
    /// Restrict to archived or unarchived repositories
    #[arg(long)]
    pub archived: Option<bool>,
    /// Restrict to pinned or unpinned repositories
    #[arg(long)]
    pub pinned: Option<bool>,
    /// Earliest starred date, inclusive (YYYY-MM-DD, UTC)
    #[arg(long)]
    pub since: Option<NaiveDate>,
    /// Latest starred date, inclusive (YYYY-MM-DD, UTC)
    #[arg(long)]
    pub until: Option<NaiveDate>,
}
impl Filter {
    pub fn exclude_seen(&mut self, mut ids: Vec<i64>) -> Result<()> {
        ensure!(ids.iter().all(|&id| id >= 0), "已展示的仓库 ID 不能为负数");
        ids.sort_unstable();
        ids.dedup();
        self.seen_json = (!ids.is_empty())
            .then(|| serde_json::to_string(&ids))
            .transpose()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !matches!((self.since,self.until),(Some(a),Some(b)) if a>b),
            "起始日期应早于或等于结束日期"
        );
        Ok(())
    }
}
#[derive(Debug, PartialEq)]
pub(crate) enum Change {
    Added,
    Updated,
    Unchanged,
}

pub(crate) fn upsert(db: &mut Db, r: &Repo) -> Result<Change> {
    use repositories as t;
    let owner = owner(db)?;
    let mut val = serde_json::to_value(r)?;
    if let Some(o) = val.as_object_mut() {
        for key in ["tags", "note", "pinned", "score", "summary"] {
            o.remove(key);
        }
    }
    let raw = serde_json::to_string(&val)?;
    let row = t::table.find((&owner, r.id));
    match row
        .select((t::metadata, t::active))
        .first::<(String, bool)>(db)
        .optional()?
    {
        Some((old, true)) if old == raw => Ok(Change::Unchanged),
        Some(_) => {
            diesel::update(row)
                .set((t::metadata.eq(raw), t::active.eq(true)))
                .execute(db)?;
            bump_revision(db)?;
            Ok(Change::Updated)
        }
        None => {
            diesel::insert_into(t::table)
                .values((t::owner_id.eq(owner), t::id.eq(r.id), t::metadata.eq(raw)))
                .execute(db)?;
            bump_revision(db)?;
            Ok(Change::Added)
        }
    }
}
type Snapshot = (String, String, String, bool, String, String, String);
fn decode(
    (raw, tags, note, pinned, summary, code_context, commit_index): Snapshot,
) -> Result<Repo> {
    let mut r: Repo = serde_json::from_str(&raw).context("仓库数据损坏，请运行 sync --full")?;
    r.tags = serde_json::from_str(&tags)?;
    r.note = note;
    r.pinned = pinned;
    r.summary = summary;
    r.code_context = code_context;
    r.commit_index = commit_index;
    Ok(r)
}
#[cfg(test)]
pub(crate) fn repos(db: &mut Db) -> Result<Vec<Repo>> {
    let mut result = Vec::new();
    visit_repos(db, |repo| {
        result.push(repo);
        Ok(())
    })?;
    Ok(result)
}

pub(crate) fn visit_repos(db: &mut Db, mut visit: impl FnMut(Repo) -> Result<()>) -> Result<()> {
    use repositories as t;
    let owner = owner(db)?;
    let rows = t::table
        .filter(t::owner_id.eq(owner).and(t::active.eq(true)))
        .select((
            t::metadata,
            t::tags,
            t::note,
            t::pinned,
            t::summary,
            t::code_context,
            t::commit_index,
        ))
        .load_iter::<Snapshot, _>(db)?;
    for row in rows {
        visit(decode(row?)?)?;
    }
    Ok(())
}
pub(crate) fn by_id(db: &mut Db, id: i64) -> Result<Repo> {
    use repositories as t;
    let owner = owner(db)?;
    decode(
        t::table
            .find((owner, id))
            .filter(t::active.eq(true))
            .select((
                t::metadata,
                t::tags,
                t::note,
                t::pinned,
                t::summary,
                t::code_context,
                t::commit_index,
            ))
            .first::<Snapshot>(db)
            .optional()?
            .context("当前收藏中没有这个仓库")?,
    )
}
pub(crate) fn by_name(db: &mut Db, name: &str) -> Result<Repo> {
    use repositories as t;
    let user = owner(db)?;
    let name_filter =
        diesel::dsl::sql::<diesel::sql_types::Bool>("json_extract(metadata, '$.full_name') = ")
            .bind::<diesel::sql_types::Text, _>(name)
            .sql(" COLLATE NOCASE");
    decode(
        t::table
            .filter(
                t::owner_id
                    .eq(user)
                    .and(t::active.eq(true))
                    .and(name_filter),
            )
            .select((
                t::metadata,
                t::tags,
                t::note,
                t::pinned,
                t::summary,
                t::code_context,
                t::commit_index,
            ))
            .first(db)
            .optional()?
            .context("未找到该仓库，请使用 list 查看 owner/repo 全名")?,
    )
}
pub(crate) fn deactivate_missing(db: &mut Db, seen: &BTreeSet<i64>) -> Result<usize> {
    use repositories as t;
    let user = owner(db)?;
    let active = t::table.filter(t::owner_id.eq(&user).and(t::active.eq(true)));
    let missing: Vec<_> = active
        .select(t::id)
        .load::<i64>(db)?
        .into_iter()
        .filter(|id| !seen.contains(id))
        .collect();
    for ids in missing.chunks(400) {
        diesel::update(active.filter(t::id.eq_any(ids)))
            .set(t::active.eq(false))
            .execute(db)?;
    }
    if !missing.is_empty() {
        bump_revision(db)?
    }
    Ok(missing.len())
}
#[derive(Default, Deserialize)]
pub(crate) struct Edit {
    pub id: i64,
    pub tags: Option<Vec<String>>,
    pub note: Option<String>,
    pub summary: Option<String>,
    pub pinned: Option<bool>,
}
pub(crate) fn edit(db: &mut Db, patch: Edit) -> Result<Repo> {
    db.immediate_transaction(|db| edit_in_transaction(db, patch))
}
pub(crate) fn edit_in_transaction(db: &mut Db, patch: Edit) -> Result<Repo> {
    use repositories as t;
    let mut r = by_id(db, patch.id)?;
    if let Some(tags) = patch.tags {
        let mut seen = BTreeSet::new();
        r.tags = tags
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
            .collect();
    }
    if let Some(note) = patch.note {
        r.note = note;
    }
    if let Some(summary) = patch.summary {
        ensure!(summary.len() <= 16 * 1024, "仓库总结或代码片段超过长度限制");
        r.summary = summary.trim().to_owned();
    }
    if let Some(pin) = patch.pinned {
        r.pinned = pin;
    }
    ensure!(
        r.tags.len() <= 100 && r.tags.iter().all(|s| s.len() <= 256) && r.note.len() <= 32768,
        "批注过长，最多 100 个标签、32 KiB 备注"
    );
    let user = owner(db)?;
    diesel::update(t::table.find((user, r.id)))
        .set((
            t::tags.eq(serde_json::to_string(&r.tags)?),
            t::note.eq(&r.note),
            t::summary.eq(&r.summary),
            t::pinned.eq(r.pinned),
        ))
        .execute(db)?;
    bump_revision(db)?;
    Ok(r)
}
pub(crate) fn save_repository_code(db: &mut Db, id: i64, content: &str) -> Result<()> {
    use repositories as r;
    ensure!(content.len() <= 48 * 1024, "仓库总结或代码片段超过长度限制");
    db.immediate_transaction(|db| {
        let owner = owner(db)?;
        let changed = diesel::update(
            r::table
                .find((owner, id))
                .filter(r::active.eq(true))
                .filter(r::code_context.ne(content)),
        )
        .set(r::code_context.eq(content))
        .execute(db)?;
        if changed > 0 {
            bump_revision(db)?;
        } else {
            by_id(db, id)?;
        }
        Ok(())
    })
}
pub(crate) fn meta(db: &mut Db, key: &str) -> Result<Option<String>> {
    let user = owner(db)?;
    Ok(metadata_values::table
        .find((user, key))
        .select(metadata_values::value)
        .first(db)
        .optional()?)
}
pub(crate) fn set_meta(db: &mut Db, key: &str, value: &str) -> Result<()> {
    use metadata_values as t;
    let user = owner(db)?;
    diesel::insert_into(t::table)
        .values((t::owner_id.eq(user), t::key.eq(key), t::value.eq(value)))
        .on_conflict((t::owner_id, t::key))
        .do_update()
        .set(t::value.eq(value))
        .execute(db)?;
    Ok(())
}
pub(crate) fn delete_meta(db: &mut Db, key: &str) -> Result<()> {
    let user = owner(db)?;
    diesel::delete(metadata_values::table.find((user, key))).execute(db)?;
    Ok(())
}

#[derive(Clone, Copy, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Sort {
    #[default]
    Time,
    Stars,
}
pub(crate) fn eligible<'a>(
    owner: String,
    filter: &'a Filter,
) -> repositories::BoxedQuery<'a, diesel::sqlite::Sqlite> {
    use diesel::{
        dsl::sql,
        sql_types::{Bool, Text},
    };
    use repositories as r;
    let mut q = r::table
        .filter(r::owner_id.eq(owner).and(r::active.eq(true)))
        .into_boxed();
    if let Some(ids) = &filter.seen_json {
        q = q.filter(
            sql::<Bool>("repositories.id NOT IN (SELECT value FROM json_each(")
                .bind::<Text, _>(ids)
                .sql("))"),
        );
    }
    if let Some(name) = &filter.list {
        q = q.filter(
            sql::<Bool>(
                "EXISTS (SELECT 1 FROM github_list_members m
            JOIN github_lists l ON l.owner_id=m.owner_id AND l.list_id=m.list_id
            WHERE m.owner_id=repositories.owner_id AND m.repo_id=repositories.id AND l.name = ",
            )
            .bind::<Text, _>(name)
            .sql(")"),
        );
    }
    if let Some(value) = &filter.language {
        if value == "未标注" {
            q = q.filter(sql::<Bool>("json_extract(metadata,'$.language') IS NULL"));
        } else {
            q = q.filter(
                sql::<Bool>("json_extract(metadata,'$.language') = ")
                    .bind::<Text, _>(value)
                    .sql(" COLLATE NOCASE"),
            );
        }
    }
    if let Some(value) = &filter.topic {
        q = q.filter(
            sql::<Bool>("EXISTS (SELECT 1 FROM json_each(metadata,'$.topics') WHERE value = ")
                .bind::<Text, _>(value)
                .sql(" COLLATE NOCASE)"),
        );
    }
    if let Some(value) = &filter.tag {
        q = q.filter(
            sql::<Bool>("EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ")
                .bind::<Text, _>(value)
                .sql(" COLLATE NOCASE)"),
        );
    }
    if let Some(value) = filter.archived {
        q = q.filter(sql::<Bool>("json_extract(metadata,'$.archived') = ").bind::<Bool, _>(value));
    }
    if let Some(value) = filter.pinned {
        q = q.filter(r::pinned.eq(value));
    }
    if let Some(value) = filter.since {
        q = q.filter(
            sql::<Bool>("substr(json_extract(metadata,'$.starred_at'),1,10) >= ")
                .bind::<Text, _>(value.to_string()),
        );
    }
    if let Some(value) = filter.until {
        q = q.filter(
            sql::<Bool>("substr(json_extract(metadata,'$.starred_at'),1,10) <= ")
                .bind::<Text, _>(value.to_string()),
        );
    }
    q
}
pub(crate) fn has_active_repositories(db: &mut Db) -> Result<bool> {
    use repositories as r;
    let user = owner(db)?;
    Ok(diesel::select(diesel::dsl::exists(
        r::table.filter(r::owner_id.eq(user).and(r::active.eq(true))),
    ))
    .get_result(db)?)
}

pub(crate) fn keyword_search(
    db: &mut Db,
    query: &str,
    filter: &Filter,
    limit: usize,
) -> Result<Vec<Repo>> {
    use diesel::sql_types::{BigInt, Text};

    let words: Vec<_> = query
        .split(|c: char| !c.is_alphanumeric() && !"+#_-".contains(c))
        .filter(|word| !word.is_empty())
        .take(12)
        .collect();
    if words.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut terms = Vec::new();
    let mut has_short_word = false;
    for &word in &words {
        if word.chars().count() < 3 {
            has_short_word = true;
        } else if word.starts_with(|c: char| c.is_ascii()) {
            terms.push(format!(r#""{word}""#));
        } else {
            let chars: Vec<_> = word.chars().collect();
            for window in chars.windows(3).take(16) {
                let token: String = window.iter().collect();
                terms.push(format!(r#""{token}""#));
            }
        }
    }
    terms.sort_unstable();
    terms.dedup();
    terms.truncate(64);
    #[derive(QueryableByName)]
    struct Hit {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    db.transaction(|db| {
        let user = owner(db)?;
        let allowed = if *filter == Filter::default() {
            None
        } else {
            Some(eligible(user.clone(), filter).select(repositories::id).load::<i64>(db)?
                .into_iter().collect::<BTreeSet<_>>())
        };
        let is_allowed = |id: i64| allowed.as_ref().is_none_or(|ids| ids.contains(&id));
        if terms.is_empty() {
            return short_keyword_search(db, &user, query, &words, limit, is_allowed);
        }
        let hits = diesel::sql_query(
            "SELECT repo_id AS id FROM repository_fts
             WHERE repository_fts MATCH ? AND owner=?
             ORDER BY CASE WHEN name=? COLLATE NOCASE THEN 0 WHEN instr(lower(name),lower(?))>0 THEN 1 ELSE 2 END,
                      bm25(repository_fts,0,0,6,1), repo_id",
        )
        .bind::<Text, _>(terms.join(" OR "))
        .bind::<Text, _>(&user)
        .bind::<Text, _>(query.trim())
        .bind::<Text, _>(query.trim())
        .load_iter::<Hit, _>(db)?
        .filter_map(|row| match row {
            Ok(hit) if is_allowed(hit.id) => Some(Ok(hit.id)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .take(limit).collect::<diesel::QueryResult<Vec<_>>>()?;
        if hits.is_empty() && has_short_word {
            return short_keyword_search(db, &user, query, &words, limit, is_allowed);
        }
        hits.into_iter().map(|id| by_id(db, id)).collect()
    })
}

fn short_keyword_search(
    db: &mut Db,
    user: &str,
    query: &str,
    words: &[&str],
    limit: usize,
    is_allowed: impl Fn(i64) -> bool,
) -> Result<Vec<Repo>> {
    use repository_fts as f;
    let terms: Vec<_> = words.iter().map(|word| word.to_lowercase()).collect();
    let phrase = query.trim().to_lowercase();
    // Trigrams cannot match one- or two-character words. Scan the compact keyword
    // documents, which exclude README content, for these queries.
    let mut matches = Vec::new();
    for row in f::table
        .filter(f::owner.eq(user))
        .select((f::repo_id, f::name, f::body))
        .load_iter::<(i64, String, String), _>(db)?
    {
        let (id, name, text) = row?;
        if !is_allowed(id) {
            continue;
        }
        let (name, text) = (name.to_lowercase(), text.to_lowercase());
        let mut score = usize::from(name.contains(&phrase)) * 4;
        for term in &terms {
            score += 3 * usize::from(name.contains(term)) + usize::from(text.contains(term));
        }
        if score > 0 {
            matches.push((std::cmp::Reverse(score), id));
        }
    }
    matches.sort_unstable();
    matches
        .into_iter()
        .take(limit)
        .map(|(_, id)| by_id(db, id))
        .collect()
}

fn ordered<'a>(
    owner: String,
    filter: &'a Filter,
    sort: Sort,
) -> repositories::BoxedQuery<'a, diesel::sqlite::Sqlite> {
    use diesel::{
        dsl::sql,
        sql_types::{BigInt, Text},
    };
    let q = eligible(owner, filter).order(repositories::pinned.desc());
    let q = match sort {
        Sort::Time => q.then_order_by(sql::<Text>("json_extract(metadata,'$.starred_at')").desc()),
        Sort::Stars => {
            q.then_order_by(sql::<BigInt>("json_extract(metadata,'$.stargazers_count')").desc())
        }
    };
    q.then_order_by(sql::<Text>("json_extract(metadata,'$.full_name')"))
}
pub(crate) fn list(db: &mut Db, filter: &Filter, sort: Sort) -> Result<Vec<Repo>> {
    use repositories as r;
    filter.validate()?;
    ordered(owner(db)?, filter, sort)
        .select((
            r::metadata,
            r::tags,
            r::note,
            r::pinned,
            r::summary,
            r::code_context,
            r::commit_index,
        ))
        .load::<Snapshot>(db)?
        .into_iter()
        .map(decode)
        .collect()
}
pub(crate) fn page(
    db: &mut Db,
    filter: &Filter,
    sort: Sort,
    offset: usize,
    limit: usize,
) -> Result<(i64, Vec<Repo>)> {
    use repositories as r;
    filter.validate()?;
    db.transaction(|db| {
        let user = owner(db)?;
        let total = eligible(user.clone(), filter).count().first(db)?;
        let compact =
            diesel::dsl::sql::<diesel::sql_types::Text>("json_remove(metadata,'$.readme')");
        let rows = ordered(user, filter, sort)
            .select((
                compact,
                r::tags,
                r::note,
                r::pinned,
                r::summary,
                diesel::dsl::sql::<diesel::sql_types::Text>("''"),
                diesel::dsl::sql::<diesel::sql_types::Text>("''"),
            ))
            .limit(i64::try_from(limit)?)
            .offset(i64::try_from(offset)?)
            .load::<Snapshot>(db)?
            .into_iter()
            .map(decode)
            .collect::<Result<_>>()?;
        Ok((total, rows))
    })
}
#[derive(Serialize)]
pub(crate) struct Stats {
    pub total: usize,
    pub lists: BTreeMap<String, usize>,
    pub languages: BTreeMap<String, usize>,
    pub tags: BTreeMap<String, usize>,
    pub last_sync: Option<String>,
    pub model: String,
}
pub(crate) fn stats(db: &mut Db) -> Result<Stats> {
    use repositories as t;
    let user = owner(db)?;
    let language = diesel::dsl::sql::<diesel::sql_types::Nullable<diesel::sql_types::Text>>(
        "json_extract(metadata, '$.language')",
    );
    let rows = t::table
        .filter(t::owner_id.eq(user).and(t::active.eq(true)))
        .select((language, t::tags))
        .load::<(Option<String>, String)>(db)?;
    let mut languages = BTreeMap::new();
    let mut tags = BTreeMap::new();
    let total = rows.len();
    for (language, raw_tags) in rows {
        *languages
            .entry(language.unwrap_or_else(|| "未标注".into()))
            .or_default() += 1;
        for tag in serde_json::from_str::<Vec<String>>(&raw_tags)? {
            *tags.entry(tag).or_default() += 1;
        }
    }
    Ok(Stats {
        total,
        languages,
        lists: crate::lists::counts(db)?,
        tags,
        last_sync: meta(db, "last_sync")?,
        model: crate::index::configured_model(db)?,
    })
}

pub(crate) fn bump_revision(db: &mut Db) -> Result<()> {
    let v = meta(db, "library_revision")?
        .map(|v| v.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    set_meta(
        db,
        "library_revision",
        &v.checked_add(1)
            .context("收藏数据版本号已用尽")?
            .to_string(),
    )
}
