//! Add a starred repository to existing viewer-owned lists. GitHub replaces the
//! entire membership set and provides no compare-and-swap for external writers.
use super::*;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};

const REPOSITORY: &str = "query StarStackListRepository($owner:String!,$name:String!) {
  viewer { databaseId } repository(owner:$owner,name:$name) {
    id fullDatabaseId viewerHasStarred
  }
}";
const UPDATE: &str = "mutation StarStackAppendLists($input:UpdateUserListsForItemInput!) {
  updateUserListsForItem(input:$input) { item { ... on Repository { id } } user { databaseId } lists { id name updatedAt lastAddedAt items(first:1) { totalCount } } }
}";

#[derive(Deserialize)]
struct Actor {
    #[serde(rename = "databaseId")]
    id: i64,
}

#[derive(Deserialize)]
struct RepoNode {
    id: String,
    #[serde(flatten)]
    numeric: Item,
    #[serde(rename = "viewerHasStarred")]
    starred: bool,
}

#[derive(Deserialize)]
struct Target {
    viewer: Actor,
    repository: RepoNode,
}

#[derive(Deserialize)]
struct Node {
    id: String,
}

#[derive(Deserialize)]
struct Written {
    item: Node,
    user: Actor,
    lists: Vec<ListNode>,
}

#[derive(Deserialize)]
struct Mutation {
    #[serde(rename = "updateUserListsForItem")]
    written: Written,
}

fn identify(db: &mut Db, gh: &GitHub<'_>, repo_id: i64) -> Result<(String, String)> {
    let repo = store::by_id(db, repo_id)?;
    let (owner, name) = repo.full_name.split_once('/').context("无效的仓库名称")?;
    let target = gh.query::<Target>(db, REPOSITORY, json!({"owner":owner,"name":name}))?;
    let node = target.repository;
    ensure!(
        target.viewer.id > 0 && !node.id.is_empty() && node.numeric.id()? == repo_id,
        "GitHub 返回了不同的账户或仓库，请重新同步"
    );
    ensure!(
        node.starred,
        "此仓库已不在当前 GitHub 用户的收藏中，请重新同步"
    );
    Ok((target.viewer.id.to_string(), node.id))
}

fn write_lock(db: &mut Db, api: &str, account: &str) -> Result<File> {
    let key = Sha256::new()
        .chain_update(api)
        .chain_update([0])
        .chain_update(account)
        .finalize();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store::data_dir(db)?.join(format!(".github-lists-{key:x}.lock")))?;
    lock.try_lock()
        .context("此 GitHub 账户正在更新收藏分类，请稍后重试")?;
    Ok(lock)
}

fn mark_dirty(db: &mut Db, ids: &BTreeSet<String>) -> Result<()> {
    let mut dirty = dirty_lists(db)?;
    dirty.extend(ids.iter().cloned());
    let rev = store::meta(db, "lists_revision")?
        .unwrap_or_else(|| "0".to_owned())
        .parse::<u64>()?;
    store::set_meta(
        db,
        "lists_revision",
        &rev.checked_add(1).context("分类版本超出范围")?.to_string(),
    )?;
    store::set_meta(db, "lists_dirty", &serde_json::to_string(&dirty)?)
}

// Only add locally after the remote response identifies the same viewer, item and exact list set.
// Reuse the returned server version only when the complete cached set advanced by exactly this item.
fn record_outcome(
    db: &mut Db,
    added: &BTreeSet<String>,
    repo_id: i64,
    pending_revision: Option<&str>,
    confirmed: Option<&BTreeMap<String, ListNode>>,
) -> Result<()> {
    use github_list_members as m;
    use github_lists as l;
    db.immediate_transaction(|db| {
        let stable = store::meta(db, "lists_revision")?.as_deref() == pending_revision;
        mark_dirty(db, added)?;
        if let Some(confirmed) = confirmed {
            let mut dirty = dirty_lists(db)?;
            let owner = store::owner(db)?;
            for id in added {
                let list = l::table.find((&owner, id));
                let Some(count) = list.select(l::item_count).first::<i64>(db).optional()? else {
                    continue;
                };
                let inserted = diesel::insert_or_ignore_into(m::table)
                    .values((
                        m::owner_id.eq(&owner),
                        m::list_id.eq(id),
                        m::repo_id.eq(repo_id),
                    ))
                    .execute(db)?;
                let next = count
                    .checked_add(i64::try_from(inserted)?)
                    .context("分类成员数超出范围")?;
                let returned = &confirmed[id];
                if stable && next == returned.items.total_count {
                    diesel::update(list)
                        .set((
                            l::name.eq(&returned.name),
                            l::item_count.eq(next),
                            l::updated_at.eq(&returned.updated_at),
                            l::last_added_at.eq(&returned.last_added_at),
                        ))
                        .execute(db)?;
                    dirty.remove(id);
                } else {
                    diesel::update(list)
                        .set(l::item_count.eq(next))
                        .execute(db)?;
                }
            }
            store::set_meta(db, "lists_dirty", &serde_json::to_string(&dirty)?)?;
        }
        Ok(())
    })
}

pub(crate) fn append(db: &mut Db, repo_id: i64, list_names: &[String]) -> Result<usize> {
    let api = api_base();
    let token = crate::auth::github_token(db)?;
    let gh = github_client(db, &api, &token)?;
    append_with(db, &api, &gh, repo_id, list_names)
}

fn append_with(
    db: &mut Db,
    api: &str,
    gh: &GitHub<'_>,
    repo_id: i64,
    list_names: &[String],
) -> Result<usize> {
    store::by_id(db, repo_id)?;
    if list_names.is_empty() {
        return Ok(0);
    }
    ensure!(
        store::meta(db, "github_api")?.is_none_or(|saved| saved == api),
        "此资料库已绑定其他 GitHub API 地址"
    );
    let (account, item) = identify(db, gh, repo_id)?;
    bind_account(db, api, &account)?;
    let _lock = write_lock(db, api, &account)?;
    // Re-read the viewer's paginated catalog and update its complete member snapshot.
    // The shared sync logic reuses stable lists and verifies changed lists to their last page.
    refresh_with(db, api, gh, &|_| {})?;
    let (revision, remote, mut union) = db.transaction::<_, anyhow::Error, _>(|db| {
        use github_list_members as m;
        let owner = store::owner(db)?;
        check_account(db, api, &account)?;
        let current = m::table
            .filter(m::owner_id.eq(&owner).and(m::repo_id.eq(repo_id)))
            .select(m::list_id)
            .load::<String>(db)?;
        Ok((
            store::meta(db, "lists_revision")?,
            cached(db, &owner)?,
            current.into_iter().collect::<BTreeSet<_>>(),
        ))
    })?;
    let mut requested = BTreeSet::new();
    for name in list_names {
        let mut matches = remote.iter().filter(|(_, l)| l.name == *name);
        let (id, _) = matches
            .next()
            .with_context(|| format!("GitHub 分类「{name}」不存在，请刷新分类"))?;
        ensure!(matches.next().is_none(), "GitHub 分类「{name}」存在重名");
        requested.insert(id.clone());
    }
    let added: BTreeSet<_> = requested.difference(&union).cloned().collect();
    if added.is_empty() {
        return Ok(0);
    }
    union.extend(added.iter().cloned());
    ensure!(
        catalog(db, api, gh)? == (account.clone(), remote),
        "读取期间 GitHub 分类发生变化，请重试"
    );
    ensure!(
        identify(db, gh, repo_id)? == (account.clone(), item.clone()),
        "GitHub 账户或仓库发生变化，请重试"
    );
    // A timeout/crash can happen after GitHub applied the replacement. Keep a
    // durable refresh marker, and invalidate any local refresh started in flight.
    let pending_revision = db.immediate_transaction::<_, anyhow::Error, _>(|db| {
        ensure!(
            store::meta(db, "lists_revision")? == revision,
            "本地分类在操作期间发生变化，请重试"
        );
        mark_dirty(db, &added)?;
        store::meta(db, "lists_revision")
    })?;
    let confirmation = gh
        .query::<Mutation>(db, UPDATE, json!({"input":{"itemId":item,"listIds":union}}))
        .context("GitHub 未确认分类写入，请刷新后核对")
        .and_then(|reply| {
            let written = reply.written;
            let count = written.lists.len();
            let lists = written
                .lists
                .into_iter()
                .map(|list| (list.id.clone(), list))
                .collect::<BTreeMap<_, _>>();
            ensure!(
                written.user.id.to_string() == account
                    && written.item.id == item
                    && count == union.len()
                    && lists.keys().eq(union.iter())
                    && lists
                        .values()
                        .all(|l| l.items.total_count >= 1 && !l.name.is_empty()),
                "GitHub 返回的分类与请求不一致，请刷新后核对"
            );
            Ok(lists)
        });
    record_outcome(
        db,
        &added,
        repo_id,
        pending_revision.as_deref(),
        confirmation.as_ref().ok(),
    )?;
    confirmation?;
    Ok(added.len())
}

#[cfg(test)]
mod tests;
