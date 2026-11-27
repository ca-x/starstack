//! Mirror the authenticated user's GitHub Star Lists independently from private annotations.
pub(crate) mod organize;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use diesel::prelude::*;
use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::quota::{Policy, Resource, Window};
use crate::store::{self, Db};

diesel::table! {
    github_lists (owner_id, list_id) {
        owner_id -> Text, list_id -> Text, name -> Text, updated_at -> Text, last_added_at -> Text, item_count -> BigInt,
    }
}
diesel::table! {
    github_list_members (owner_id, list_id, repo_id) {
        owner_id -> Text, list_id -> Text, repo_id -> BigInt,
    }
}

const LISTS: &str = "query StarStackLists($after: String) {
  viewer { databaseId lists(first: 12, after: $after) {
    totalCount pageInfo { hasNextPage endCursor }
    nodes { id name updatedAt lastAddedAt items(first: 1) { totalCount } }
  } }
}";
const FIRST_ITEMS: &str = "query StarStackListItems($ids: [ID!]!) {
  nodes(ids: $ids) { ... on UserList { id updatedAt lastAddedAt
    items(first: 100) { totalCount pageInfo { hasNextPage endCursor }
    nodes { ... on Repository { fullDatabaseId } } }
  } }
}";
const NEXT_ITEMS: &str = "query StarStackMoreListItems($id: ID!, $after: String!) {
  node(id: $id) { ... on UserList { id updatedAt lastAddedAt
    items(first: 100, after: $after) { totalCount pageInfo { hasNextPage endCursor }
    nodes { ... on Repository { fullDatabaseId } } }
  } }
}";

#[derive(Debug, Clone, PartialEq, Eq)]
struct List {
    name: String,
    updated: String,
    added: String,
    count: i64,
}

fn cached(db: &mut Db, owner: &str) -> Result<BTreeMap<String, List>> {
    use github_lists as l;
    Ok(l::table
        .filter(l::owner_id.eq(owner))
        .select((
            l::list_id,
            l::name,
            l::updated_at,
            l::last_added_at,
            l::item_count,
        ))
        .load::<(String, String, String, String, i64)>(db)?
        .into_iter()
        .map(|(id, name, updated, added, count)| {
            (
                id,
                List {
                    name,
                    updated,
                    added,
                    count,
                },
            )
        })
        .collect())
}

pub(crate) fn counts(db: &mut Db) -> Result<BTreeMap<String, usize>> {
    let owner = store::owner(db)?;
    let mut results = BTreeMap::new();
    for list in cached(db, &owner)?.into_values() {
        *results.entry(list.name).or_default() += usize::try_from(list.count)?;
    }
    Ok(results)
}

#[cfg(test)]
pub(crate) fn members(db: &mut Db, name: &str) -> Result<BTreeSet<i64>> {
    use github_list_members as m;
    use github_lists as l;
    let owner = store::owner(db)?;
    let ids = l::table
        .filter(l::owner_id.eq(&owner).and(l::name.eq(name)))
        .select(l::list_id)
        .load::<String>(db)?;
    Ok(m::table
        .filter(m::owner_id.eq(owner).and(m::list_id.eq_any(ids)))
        .select(m::repo_id)
        .load::<i64>(db)?
        .into_iter()
        .collect())
}

pub(crate) fn for_repo(db: &mut Db, id: i64) -> Result<Vec<String>> {
    use github_list_members as m;
    use github_lists as l;
    let owner = store::owner(db)?;
    let ids = m::table
        .filter(m::owner_id.eq(&owner).and(m::repo_id.eq(id)))
        .select(m::list_id)
        .load::<String>(db)?;
    Ok(l::table
        .filter(l::owner_id.eq(owner).and(l::list_id.eq_any(ids)))
        .select(l::name)
        .distinct()
        .order(l::name)
        .load(db)?)
}

struct GitHub<'a> {
    client: Client,
    url: Url,
    token: &'a str,
    legacy_ids: std::cell::Cell<bool>,
}

#[derive(Deserialize)]
struct GraphError {
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    extensions: Value,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphError>,
}

impl GitHub<'_> {
    fn query<T: DeserializeOwned>(&self, db: &mut Db, query: &str, variables: Value) -> Result<T> {
        let legacy = self.legacy_ids.get() && query.contains("fullDatabaseId");
        let request = if legacy {
            std::borrow::Cow::Owned(query.replace("fullDatabaseId", "databaseId"))
        } else {
            std::borrow::Cow::Borrowed(query)
        };
        let (body, delay) = self.post(
            db,
            &json!({"query":request.as_ref(),"variables":&variables}),
            !query.trim_start().starts_with("mutation "),
        )?;
        // GitHub may return HTTP 200 with partial data and errors. Do not publish such snapshots.
        let envelope: Envelope<Value> =
            serde_json::from_slice(&body).context("读取 GitHub 分类响应失败")?;
        if envelope.errors.iter().any(|e| {
            e.kind.as_deref() == Some("RATE_LIMITED")
                || e.extensions.get("code").and_then(Value::as_str) == Some("RATE_LIMITED")
                || e.message.to_ascii_lowercase().contains("rate limit")
        }) {
            let mut window = Window::load(db, Resource::Graphql)?;
            window.defer(delay);
            window.save(db, Resource::Graphql)?;
            bail!("GitHub GraphQL 请求达到额度，请在 {delay} 秒后重试");
        }
        // Older Enterprise schemas expose only databaseId. Avoid selecting that 32-bit
        // field on servers that support fullDatabaseId, since it can overflow independently.
        if !legacy
            && query.contains("fullDatabaseId")
            && envelope.errors.iter().any(|e| {
                e.message.contains("fullDatabaseId")
                    && (e.extensions.get("code").and_then(Value::as_str) == Some("undefinedField")
                        || e.message.contains("doesn't exist on type")
                        || e.message.contains("Cannot query field"))
            })
        {
            self.legacy_ids.set(true);
            return self.query(db, query, variables);
        }
        if let Some(e) = envelope.errors.first() {
            bail!(
                "GitHub 分类查询失败（{}{}）",
                e.kind
                    .as_ref()
                    .map(|k| format!("{k} · "))
                    .unwrap_or_default(),
                e.message
            );
        }
        serde_json::from_value(envelope.data.context("GitHub 未返回分类数据")?)
            .context("GitHub 分类数据不完整")
    }

    fn post(&self, db: &mut Db, payload: &Value, retry_safe: bool) -> Result<(Vec<u8>, u64)> {
        let attempts: u8 = if retry_safe { 3 } else { 1 };
        let reserve = Policy::load(db)?.reserve_percent;
        for attempt in 1..=attempts {
            let mut window = Window::load(db, Resource::Graphql)?;
            window.check()?;
            ensure!(
                window.has_headroom(reserve),
                "GraphQL 额度已降至保留线，请在重置后刷新"
            );
            let retry = attempt < attempts;
            let backoff = Duration::from_millis(250 * u64::from(attempt));
            let response = match self
                .client
                .post(self.url.clone())
                .bearer_auth(self.token)
                .json(payload)
                .send()
            {
                Ok(r) => r,
                Err(e) if retry && (e.is_connect() || e.is_timeout() || e.is_request()) => {
                    std::thread::sleep(backoff);
                    continue;
                }
                Err(e) => return Err(e).context("无法连接 GitHub GraphQL"),
            };
            let status = response.status();
            let headers = response.headers();
            window.observe(headers);
            let number = |key: &str| headers.get(key)?.to_str().ok()?.parse::<u64>().ok();
            let retry_after = number("retry-after");
            let delay = retry_after
                .or_else(|| {
                    number("x-ratelimit-reset")
                        .and_then(|t| t.checked_sub(chrono::Utc::now().timestamp().max(0) as u64))
                })
                .unwrap_or(60)
                .max(1);
            let limited = status.as_u16() == 429
                || (status.as_u16() == 403
                    && (retry_after.is_some() || number("x-ratelimit-remaining") == Some(0)));
            let transient = matches!(status.as_u16(), 502..=504);
            if limited || (transient && retry_after.is_some_and(|v| v > 2)) {
                window.defer(delay);
            }
            window.save(db, Resource::Graphql)?;
            if transient && retry && retry_after.is_none_or(|v| v <= 2) {
                std::thread::sleep(backoff.max(Duration::from_secs(retry_after.unwrap_or(0))));
                continue;
            }
            if !status.is_success() {
                let secondary = status.as_u16() == 403
                    && response
                        .text()
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                        .contains("rate limit");
                if limited || secondary {
                    window.defer(delay);
                    window.save(db, Resource::Graphql)?;
                    bail!("GitHub GraphQL 请求达到额度，请在 {delay} 秒后重试");
                }
                bail!("GitHub 分类请求返回 {status}，请稍后重试或检查授权");
            }
            match response.bytes() {
                Ok(body) => return Ok((body.to_vec(), delay)),
                Err(e) if retry && (e.is_body() || e.is_timeout() || e.is_decode()) => {
                    std::thread::sleep(backoff);
                }
                Err(e) => return Err(e).context("读取 GitHub GraphQL 响应失败"),
            }
        }
        unreachable!("the final attempt always returns its result")
    }
}

pub(crate) fn endpoint(rest: &str) -> Result<Url> {
    let mut url = Url::parse(rest).context("无效的 GitHub API 地址")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "无效的 GitHub API 地址"
    );
    let base = url.path().trim_end_matches('/');
    let path = format!("{}/graphql", base.strip_suffix("/v3").unwrap_or(base));
    url.set_path(&path);
    Ok(url)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

impl PageInfo {
    fn next(self, seen: &mut BTreeSet<String>) -> Result<Option<String>> {
        if !self.has_next_page {
            return Ok(None);
        }
        let cursor = self
            .end_cursor
            .filter(|s| !s.is_empty())
            .context("GitHub 分类分页缺少游标")?;
        ensure!(
            seen.insert(cursor.clone()),
            "GitHub 分类分页游标重复，请稍后重试"
        );
        Ok(Some(cursor))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    total_count: i64,
    page_info: PageInfo,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListNode {
    id: String,
    name: String,
    updated_at: String,
    last_added_at: String,
    items: Count,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Count {
    total_count: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Viewer {
    database_id: i64,
    lists: Page<ListNode>,
}

#[derive(Deserialize)]
struct Listing {
    viewer: Viewer,
}

fn catalog(db: &mut Db, api: &str, gh: &GitHub<'_>) -> Result<(String, BTreeMap<String, List>)> {
    let mut account = None;
    let mut total = None;
    let mut lists = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut after = None;
    loop {
        let viewer = gh
            .query::<Listing>(db, LISTS, json!({"after":after}))?
            .viewer;
        let page = viewer.lists;
        ensure!(viewer.database_id > 0, "GitHub 返回了无效的账户 ID");
        let id = viewer.database_id.to_string();
        if account.is_none() {
            bind_account(db, api, &id)?;
        }
        ensure!(
            account.get_or_insert(id.clone()) == &id
                && total.get_or_insert(page.total_count) == &page.total_count,
            "获取分类期间 GitHub 账户或分类数量发生变化，请重试"
        );
        ensure!(
            page.total_count >= 0 && (!page.nodes.is_empty() || !page.page_info.has_next_page),
            "GitHub 返回无效的分类分页，请重试"
        );
        for l in page.nodes {
            ensure!(
                l.items.total_count >= 0 && !l.id.is_empty() && !l.name.is_empty(),
                "GitHub 返回了无效的收藏分类"
            );
            let value = List {
                name: l.name,
                updated: l.updated_at,
                added: l.last_added_at,
                count: l.items.total_count,
            };
            ensure!(
                lists.insert(l.id, value).is_none(),
                "GitHub 返回了重复的分类，请重试"
            );
        }
        ensure!(
            !page.page_info.has_next_page || i64::try_from(lists.len())? < page.total_count,
            "GitHub 分类数量与分页信息不符，请重试"
        );
        after = page.page_info.next(&mut seen)?;
        if after.is_none() {
            break;
        }
    }
    ensure!(
        Some(i64::try_from(lists.len())?) == total,
        "GitHub 分类列表不完整，请重试"
    );
    Ok((account.context("缺少 GitHub 账户信息")?, lists))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    full_database_id: Option<DatabaseId>,
    database_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DatabaseId {
    Number(i64),
    String(String),
}

impl Item {
    fn id(self) -> Result<i64> {
        let id = match self.full_database_id {
            Some(DatabaseId::Number(id)) => id,
            Some(DatabaseId::String(id)) => id
                .parse()
                .context("仓库 fullDatabaseId 无效或超出 SQLite 的整数范围")?,
            None => self.database_id.context("GitHub 未返回仓库数据库 ID")?,
        };
        ensure!(id > 0, "GitHub 返回了无效的仓库数据库 ID");
        Ok(id)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Items {
    id: String,
    updated_at: String,
    last_added_at: String,
    items: Page<Item>,
}

#[derive(Deserialize)]
struct Batch {
    nodes: Vec<Items>,
}
#[derive(Deserialize)]
struct Single {
    node: Items,
}

fn collect_items(
    db: &mut Db,
    gh: &GitHub<'_>,
    id: &str,
    list: &List,
    mut page: Items,
) -> Result<BTreeSet<i64>> {
    let mut ids = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    loop {
        ensure!(
            page.id == id
                && page.updated_at == list.updated
                && page.last_added_at == list.added
                && page.items.total_count == list.count,
            "读取「{}」期间分类发生变化，请重试",
            list.name
        );
        ensure!(
            !page.items.nodes.is_empty() || !page.items.page_info.has_next_page,
            "GitHub 返回了空的成员中间页"
        );
        for item in page.items.nodes {
            ensure!(
                ids.insert(item.id()?),
                "「{}」包含无效或重复的仓库 ID，请重试",
                list.name
            );
        }
        ensure!(
            !page.items.page_info.has_next_page || i64::try_from(ids.len())? < list.count,
            "GitHub 分类成员数量与分页信息不符，请重试"
        );
        let Some(after) = page.items.page_info.next(&mut cursors)? else {
            break;
        };
        page = gh
            .query::<Single>(db, NEXT_ITEMS, json!({"id":id,"after":after}))?
            .node;
    }
    ensure!(
        i64::try_from(ids.len())? == list.count,
        "「{}」的仓库列表不完整，请重试",
        list.name
    );
    Ok(ids)
}

pub(crate) fn refresh(db: &mut Db, progress: &dyn Fn(&str)) -> Result<String> {
    let api = api_base();
    let token = crate::auth::github_token(db)?;
    let gh = github_client(db, &api, &token)?;
    refresh_with(db, &api, &gh, progress)
}

fn api_base() -> String {
    std::env::var("STARSTACK_GITHUB_API_URL")
        .unwrap_or_else(|_| "https://api.github.com".to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn github_client<'a>(db: &mut Db, api: &str, token: &'a str) -> Result<GitHub<'a>> {
    Ok(GitHub {
        client: crate::proxy::builder(db)?
            .user_agent("starstack")
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        url: endpoint(api)?,
        token,
        legacy_ids: Default::default(),
    })
}

fn check_account(db: &mut Db, api: &str, id: &str) -> Result<()> {
    ensure!(
        store::meta(db, "github_api")?
            .as_deref()
            .is_none_or(|saved| saved == api)
            && store::meta(db, "github_account")?
                .as_deref()
                .is_none_or(|saved| saved == id),
        "此资料库已绑定其他 GitHub 账户或 API 地址，原有分类已保留"
    );
    Ok(())
}

// The first viewer response identifies the upstream quota bucket even when Star Lists
// is the first operation in this library. Share that observation before any costly pages.
fn bind_account(db: &mut Db, api: &str, account: &str) -> Result<()> {
    let unbound = store::meta(db, "github_account")?.is_none();
    let observed = if unbound {
        Some(Window::load(db, Resource::Graphql)?)
    } else {
        None
    };
    db.immediate_transaction::<_, anyhow::Error, _>(|db| {
        check_account(db, api, account)?;
        if unbound {
            store::set_meta(db, "github_account", account)?;
            store::set_meta(db, "github_api", api)?;
        }
        Ok(())
    })?;
    if let Some(window) = observed {
        window.save(db, Resource::Graphql)?;
    }
    Ok(())
}

fn fetch_members(
    db: &mut Db,
    gh: &GitHub<'_>,
    remote: &BTreeMap<String, List>,
    candidates: &[&str],
    progress: &dyn Fn(&str),
) -> Result<BTreeMap<String, BTreeSet<i64>>> {
    let populated: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|id| remote[*id].count > 0)
        .collect();
    let mut fetched = BTreeMap::new();
    // Batch first pages across lists to reduce GraphQL calls; each subsequent cursor belongs to one list.
    for group in populated.chunks(4) {
        progress(&format!(
            "正在读取 GitHub 分类成员（{} / {}）",
            fetched.len(),
            populated.len()
        ));
        let nodes = gh
            .query::<Batch>(db, FIRST_ITEMS, json!({"ids":group}))?
            .nodes;
        ensure!(nodes.len() == group.len(), "GitHub 分类成员响应不完整");
        for page in nodes {
            let id = page.id.clone();
            ensure!(
                group.contains(&id.as_str()) && !fetched.contains_key(&id),
                "GitHub 返回意外或重复的分类"
            );
            fetched.insert(id.clone(), collect_items(db, gh, &id, &remote[&id], page)?);
        }
    }
    Ok(fetched)
}

fn dirty_lists(db: &mut Db) -> Result<BTreeSet<String>> {
    Ok(store::meta(db, "lists_dirty")?
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}

fn refresh_with(
    db: &mut Db,
    api: &str,
    gh: &GitHub<'_>,
    progress: &dyn Fn(&str),
) -> Result<String> {
    ensure!(
        store::meta(db, "github_api")?
            .as_deref()
            .is_none_or(|saved| saved == api),
        "此资料库已绑定其他 GitHub API 地址，请恢复原地址"
    );
    let owner = store::owner(db)?;
    let (revision, observed, old, dirty) = db.transaction::<_, anyhow::Error, _>(|db| {
        let observed = store::meta(db, "lists_observed_at")?
            .map(|v| v.parse::<i64>())
            .transpose()?;
        Ok((
            store::meta(db, "lists_revision")?,
            observed,
            cached(db, &owner)?,
            dirty_lists(db)?,
        ))
    })?;
    let started = chrono::Utc::now().timestamp();
    progress("正在读取 GitHub 收藏分类");
    let (account, remote) = catalog(db, api, gh)?;
    check_account(db, api, &account)?;
    let candidates: Vec<_> = remote
        .iter()
        .filter(|(id, list)| {
            dirty.contains(*id)
                || old.get(*id) != Some(*list)
                || observed.is_some_and(|t| {
                    // GitHub timestamps can be rounded to seconds. Re-read a list at the previous
                    // snapshot boundary to catch a same-count swap within that second.
                    [&list.added, &list.updated]
                        .into_iter()
                        .filter_map(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .any(|stamp| stamp.timestamp() >= t.saturating_sub(1))
                })
        })
        .map(|(id, _)| id.as_str())
        .collect();
    let fetched = fetch_members(db, gh, &remote, &candidates, progress)?;
    use github_list_members as m;
    use github_lists as l;
    db.immediate_transaction(|db| {
        ensure!(
            store::meta(db, "lists_revision")? == revision,
            "其他进程已更新分类，请重试"
        );
        check_account(db, api, &account)?;
        let mut changed = Vec::new();
        for id in candidates {
            if old.get(id) == remote.get(id) {
                let prior: BTreeSet<_> = m::table
                    .filter(m::owner_id.eq(&owner).and(m::list_id.eq(id)))
                    .select(m::repo_id)
                    .load::<i64>(db)?
                    .into_iter()
                    .collect();
                if fetched.get(id).is_none_or(|current| *current == prior) {
                    continue;
                }
            }
            changed.push(id);
        }
        let removed = old.keys().filter(|id| !remote.contains_key(*id)).count();
        for id in old.keys().filter(|id| !remote.contains_key(*id)) {
            diesel::delete(l::table.find((&owner, id))).execute(db)?;
        }
        for id in &changed {
            let list = &remote[*id];
            diesel::delete(l::table.find((&owner, *id))).execute(db)?;
            diesel::insert_into(l::table)
                .values((
                    l::owner_id.eq(&owner),
                    l::list_id.eq(*id),
                    l::name.eq(&list.name),
                    l::updated_at.eq(&list.updated),
                    l::last_added_at.eq(&list.added),
                    l::item_count.eq(list.count),
                ))
                .execute(db)?;
            if let Some(ids) = fetched.get(*id) {
                for chunk in ids.iter().collect::<Vec<_>>().chunks(200) {
                    let rows: Vec<_> = chunk
                        .iter()
                        .map(|&&repo_id| {
                            (
                                m::owner_id.eq(&owner),
                                m::list_id.eq(*id),
                                m::repo_id.eq(repo_id),
                            )
                        })
                        .collect();
                    diesel::insert_into(m::table).values(&rows).execute(db)?;
                }
            }
        }
        if !changed.is_empty() || removed > 0 || revision.is_none() {
            let next = revision
                .as_deref()
                .unwrap_or("0")
                .parse::<u64>()?
                .checked_add(1)
                .context("分类版本超出范围")?;
            store::set_meta(db, "lists_revision", &next.to_string())?;
        }
        store::set_meta(db, "github_api", api)?;
        store::set_meta(db, "github_account", &account)?;
        store::set_meta(db, "lists_observed_at", &started.to_string())?;
        store::delete_meta(db, "lists_dirty")?;
        store::set_meta(db, "lists_last_sync", &chrono::Utc::now().to_rfc3339())?;
        Ok(format!(
            "GitHub 收藏分类共 {} 个，更新 {} 个，移除 {} 个",
            remote.len(),
            changed.len(),
            removed
        ))
    })
}

#[cfg(test)]
mod tests;
