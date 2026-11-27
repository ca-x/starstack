//! A read-only inventory of the current account's published index and saved work.
use super::{Payloads, artifact_path, generation, library_revision};
use crate::store::{self, Db, repositories};
use anyhow::Result;
use diesel::{prelude::*, sql_types};
use serde::Serialize;
use std::{fs, io::ErrorKind, path::Path};

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Unbuilt,
    Pending,
    Ready,
    NeedsAttention,
}

#[derive(Serialize)]
pub(crate) struct Status {
    state: State,
    repositories: i64,
    indexed: i64,
    checkpoints: i64,
    body_pending: bool,
    projection_bytes: u64,
    vector_payload_bytes: u64,
    legacy_payload_bytes: i64,
    last_published_at: Option<String>,
}

#[derive(Default, QueryableByName)]
struct Usage {
    #[diesel(sql_type = sql_types::BigInt)]
    active: i64,
    #[diesel(sql_type = sql_types::BigInt)]
    bytes: i64,
}

fn table_exists(db: &mut Db, name: &str) -> Result<bool> {
    #[derive(QueryableByName)]
    struct Present {
        #[diesel(sql_type = sql_types::Bool)]
        present: bool,
    }
    Ok(diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?) AS present",
    )
    .bind::<sql_types::Text, _>(name)
    .get_result::<Present>(db)?
    .present)
}

fn file_info(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn read(db: &mut Db) -> Result<Status> {
    db.transaction(|db| {
        let owner = store::owner(db)?;
        let dir = store::data_dir(db)?;
        let generation = generation(db);
        let active_file = match generation.as_ref() {
            Ok(generation) if *generation > 0 => file_info(&artifact_path(&dir, &owner, *generation))?,
            _ => None,
        };
        let projection_bytes = [0, 1].iter().try_fold(0_u64, |size, slot| {
            Ok::<_, anyhow::Error>(
                size + file_info(&artifact_path(&dir, &owner, *slot))?.map_or(0, |file| file.len()),
            )
        })?;

        let total = repositories::table
            .filter(repositories::owner_id.eq(&owner).and(repositories::active.eq(true)))
            .count()
            .get_result::<i64>(db)?;
        let has_embeddings = table_exists(db, "embeddings")?;
        let vectors = if has_embeddings {
            diesel::sql_query(
                "SELECT COUNT(r.id) AS active, COALESCE(SUM(length(e.vector)), 0) AS bytes
                 FROM embeddings e LEFT JOIN repositories r ON e.owner=r.owner_id AND e.id=r.id AND r.active=1
                 WHERE e.owner = ?",
            )
            .bind::<sql_types::Text, _>(&owner)
            .get_result::<Usage>(db)?
        } else {
            Usage::default()
        };
        let checkpoint = if table_exists(db, "embedding_checkpoints")? {
            diesel::sql_query(
                "SELECT COUNT(r.id) AS active, COALESCE(SUM(length(c.vector)), 0) AS bytes
                 FROM embedding_checkpoints c LEFT JOIN repositories r ON c.owner=r.owner_id AND c.id=r.id AND r.active=1
                 WHERE c.owner = ?",
            )
            .bind::<sql_types::Text, _>(&owner)
            .get_result::<Usage>(db)?
        } else {
            Usage::default()
        };
        let published = active_file.is_some() && has_embeddings;
        let incompatible = published && super::needs_rebuild(db).unwrap_or(true);
        let body_pending = published && !incompatible && store::meta(db, "index_body_pending")?.is_some();
        let index_issue = incompatible || store::meta(db, "index_issue")?.is_some()
            || generation.as_ref().is_err()
            || generation.is_ok_and(|id| id > 0 && !published);
        let state = if index_issue {
            State::NeedsAttention
        } else if store::meta(db, "index_pending")?.is_some()
            || (published
                && store::meta(db, "indexed_library_revision")?.as_deref()
                    != Some(library_revision(db)?.as_str()))
        {
            State::Pending
        } else if published {
            State::Ready
        } else {
            State::Unbuilt
        };
        let last_published_at = store::meta(db, "index_published_at")?.or_else(|| {
            active_file.and_then(|file| file.modified().ok())
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
        });
        Ok(Status {
            state,
            repositories: total,
            indexed: if published { vectors.active } else { 0 },
            checkpoints: checkpoint.active,
            body_pending,
            projection_bytes,
            vector_payload_bytes: Payloads::new(&dir, &owner).disk_bytes()?,
            legacy_payload_bytes: vectors.bytes + checkpoint.bytes,
            last_published_at,
        })
    })
}
