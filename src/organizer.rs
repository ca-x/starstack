//! Durable per-account bulk organization. A file lease fences parallel workers across processes.
use crate::{
    ai, auth,
    library::Library,
    lists,
    organize::{self, ApplyInput, ApplyItem, Batch, PreviewInput},
    store::{self, Db, Filter},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions, TryLockError},
    path::Path,
};

#[derive(Default, Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    #[default]
    Tags,
    GithubLists,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Suggestion {
    pub id: i64,
    pub full_name: String,
    pub html_url: String,
    pub tags: Vec<String>,
    pub reason: String,
}

#[derive(Deserialize)]
struct Start {
    #[serde(default)]
    filter: Filter,
    target: Target,
    confirmed: bool,
}

#[derive(Serialize, Deserialize)]
struct Job {
    status: String,
    target: Target,
    total: usize,
    processed: usize,
    added: usize,
    error: Option<String>,
    #[serde(default)]
    pause_requested: bool,
}
impl Default for Job {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            target: Target::Tags,
            total: 0,
            processed: 0,
            added: 0,
            error: None,
            pause_requested: false,
        }
    }
}

fn state(db: &mut Db) -> Result<Job> {
    Ok(store::meta(db, "organize_job")?
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}
fn save(db: &mut Db, job: &Job) -> Result<()> {
    store::set_meta(db, "organize_job", &serde_json::to_string(job)?)
}
fn lock(dir: &Path, owner: &str) -> Result<Option<File>> {
    let path = dir.join(format!(
        "organize-{:x}.lock",
        Sha256::digest(owner.as_bytes())
    ));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn categories(db: &mut Db, target: Target, batch: &Batch) -> Result<Vec<String>> {
    match target {
        Target::Tags => Ok(batch.known_tags.clone()),
        Target::GithubLists => Ok(lists::counts(db)?.into_keys().collect()),
    }
}
fn parse_target(body: &mut Value) -> Result<Target> {
    let value = body.as_object_mut().and_then(|v| v.remove("target"));
    Ok(value
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}
pub(crate) async fn preview(lib: Library, mut body: Value) -> Result<Value> {
    let target = parse_target(&mut body)?;
    let input: PreviewInput = serde_json::from_value(body)?;
    let (batch, cats) = lib
        .run(move |db| {
            let b = organize::prepare(db, input)?;
            let cats = categories(db, target, &b)?;
            Ok((b, cats))
        })
        .await?;
    Ok(
        json!({"suggestions":ai::classify(&batch,target,&cats).await?,"scanned":batch.repos.len(),"next_offset":batch.next_offset,"total":batch.total}),
    )
}
fn apply(db: &mut Db, target: Target, input: ApplyInput) -> Result<Value> {
    if target == Target::Tags {
        return organize::apply(db, input);
    }
    ensure!(
        input.items.len() <= organize::BATCH_SIZE,
        "每批最多整理 20 个仓库"
    );
    let mut added = 0;
    let mut updated = 0;
    for item in input.items {
        ensure!(item.tags.len() <= 5, "每仓库每批最多添加 5 个分类");
        let n = lists::organize::append(db, item.id, &item.tags)?;
        updated += usize::from(n > 0);
        added += n;
    }
    Ok(json!({"updated":updated,"added":added}))
}
pub(crate) fn manual(db: &mut Db, mut body: Value) -> Result<Value> {
    let target = parse_target(&mut body)?;
    apply(db, target, serde_json::from_value(body)?)
}
pub(crate) fn progress(db: &mut Db) -> Result<Value> {
    db.immediate_transaction(|db| {
        let mut job = state(db)?;
        if job.status == "running"
            && let Some(_lease) = lock(&store::data_dir(db)?, &store::owner(db)?)?
        {
            job.status = "paused".into();
            job.pause_requested = false;
            save(db, &job)?;
        }
        Ok(json!(job))
    })
}
pub(crate) fn pause(db: &mut Db) -> Result<Value> {
    db.immediate_transaction(|db| {
        let mut job = state(db)?;
        if job.status == "running" {
            job.pause_requested = true;
            save(db, &job)?
        }
        Ok(json!({"message":"已请求暂停"}))
    })
}
pub(crate) async fn start(lib: Library, input: Option<Value>) -> Result<Value> {
    let task = input.map(serde_json::from_value::<Start>).transpose()?;
    let lease = lock(&lib.dir, &lib.user)?.context("已有整理任务正在运行")?;
    lib.run(move |db| {
        db.immediate_transaction(|db| {
            let mut job = state(db)?;
            if let Some(task) = task {
                ensure!(task.confirmed, "请先确认自动整理范围和目标");
                if task.target == Target::GithubLists {
                    ensure!(!lists::counts(db)?.is_empty(), "请先同步 GitHub 已有分类");
                }
                let ids = organize::snapshot_ids(db, &task.filter)?;
                store::set_meta(db, "organize_ids", &serde_json::to_string(&ids)?)?;
                job = Job {
                    target: task.target,
                    total: ids.len(),
                    ..Default::default()
                };
            } else {
                ensure!(
                    matches!(job.status.as_str(), "paused" | "failed" | "running"),
                    "没有可继续的任务"
                )
            }
            job.status = "running".into();
            job.error = None;
            job.pause_requested = false;
            save(db, &job)
        })
    })
    .await?;
    tokio::spawn(async move {
        let _lease = lease;
        if let Err(error) = run(&lib).await {
            let message = format!("{error:#}");
            let _ = lib
                .run(move |db| {
                    db.immediate_transaction(|db| {
                        let mut job = state(db)?;
                        job.status = "failed".into();
                        job.error = Some(message);
                        save(db, &job)
                    })
                })
                .await;
        }
    });
    Ok(json!({"message":"自动整理已启动"}))
}
fn proceed(db: &mut Db) -> Result<bool> {
    let mut job = state(db)?;
    let owner = store::owner(db)?;
    if job.pause_requested || auth::account(db, &owner)?.disabled {
        job.status = "paused".into();
        job.pause_requested = false;
        save(db, &job)?;
        return Ok(false);
    }
    Ok(job.status == "running")
}
fn advance(db: &mut Db, added: usize) -> Result<()> {
    let mut job = state(db)?;
    job.processed += 1;
    job.added += added;
    save(db, &job)
}
fn active(db: &mut Db, id: i64) -> Result<bool> {
    use diesel::prelude::*;
    use store::repositories as r;
    let owner = store::owner(db)?;
    Ok(r::table
        .find((owner, id))
        .select(r::active)
        .first::<bool>(db)
        .optional()?
        .unwrap_or(false))
}
async fn run(lib: &Library) -> Result<()> {
    let ids: Vec<i64> = lib
        .run(|db| {
            Ok(serde_json::from_str(
                &store::meta(db, "organize_ids")?.context("缺少整理任务快照")?,
            )?)
        })
        .await?;
    loop {
        let j = lib.run(state).await?;
        if !lib.run(proceed).await? {
            return Ok(());
        }
        if j.processed >= ids.len() {
            return lib
                .run(|db| {
                    let mut j = state(db)?;
                    j.status = "done".into();
                    save(db, &j)?;
                    store::delete_meta(db, "organize_ids")
                })
                .await;
        }
        let slice = ids[j.processed..]
            .iter()
            .take(organize::BATCH_SIZE)
            .copied()
            .collect::<Vec<_>>();
        let target = j.target;
        let (batch, cats) = lib
            .run({
                let slice = slice.clone();
                move |db| {
                    let b = organize::by_ids(db, &slice)?;
                    let cats = categories(db, target, &b)?;
                    Ok((b, cats))
                }
            })
            .await?;
        let mut suggestions = ai::classify(&batch, target, &cats).await?;
        for id in slice {
            if !lib.run(proceed).await? {
                return Ok(());
            }
            let item = suggestions.iter().position(|s| s.id == id).map(|i| {
                let s = suggestions.swap_remove(i);
                ApplyItem { id, tags: s.tags }
            });
            lib.run(move |db| {
                if target==Target::Tags {
                    return db.immediate_transaction(|db|{
                        let count=if let Some(item)=item && active(db,id)? {
                            organize::apply_in_transaction(db,ApplyInput{items:vec![item]})?["added"].as_u64().unwrap_or(0) as usize
                        }else{0};
                        advance(db,count)
                    })
                }
                let count=if let Some(item)=item && active(db,id)?{
                    lists::organize::append(db,id,&item.tags)?
                }else{0};
                db.immediate_transaction(|db|advance(db,count))
            })
            .await?;
        }
    }
}
