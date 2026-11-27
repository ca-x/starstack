//! An optional process-local exact scan for small, already-read immutable generations.
use super::{Payloads, artifact_path, cosine, decode, embeddings, open_projection, vector_bytes};
use crate::store::Db;
use anyhow::{Context, Result, ensure};
use diesel::prelude::*;
use hawdb_vector_projection::{FileProjection, ProjectionSearchOptions};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_BYTES: usize = 48 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
fn fits(count: usize, dim: usize) -> bool {
    count > 0
        && dim > 0
        && count <= MAX_ROWS
        && dim
            .checked_mul(4)
            .and_then(|size| size.checked_add(256))
            .and_then(|row| count.checked_mul(row))
            .is_some_and(|total| total <= MAX_BYTES)
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    #[cfg(unix)]
    identity: (u64, u64, u64, i64, i64, i64, i64),
}
impl Stamp {
    pub(super) fn read(path: &Path) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::symlink_metadata(path).ok()?;
            m.is_file().then_some(Self {
                identity: (
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                ),
            })
        }
        // Without a trusted inode/change stamp, use HawDB's per-read integrity checks.
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Key {
    path: PathBuf,
    owner: String,
    generation: u64,
    model: String,
    stamp: Option<Stamp>,
}
impl Key {
    pub fn new(path: &Path, owner: &str, generation: u64, model: &str) -> Self {
        Self {
            path: path.into(),
            owner: owner.into(),
            generation,
            model: model.into(),
            stamp: Stamp::read(path),
        }
    }
    fn unchanged(&self) -> bool {
        self.stamp.is_some() && Stamp::read(&self.path) == self.stamp
    }
}
#[derive(Clone, Copy)]
struct Source {
    reference: [u8; 40],
    stamp: Stamp,
}
impl Source {
    pub fn capture(payloads: &mut Payloads, reference: &[u8]) -> Option<Self> {
        let reference: [u8; 40] = reference.try_into().ok()?;
        let path = payloads.object_path(&reference).ok()?;
        Some(Self {
            reference,
            stamp: Stamp::read(&path)?,
        })
    }
}
struct Row {
    offset: usize,
    norm: f64,
    source: Source,
}
struct Dense {
    key: Key,
    count: usize,
    dim: usize,
    rows: HashMap<i64, Row>,
    vectors: Vec<f32>,
}
static HOT: Mutex<Option<Dense>> = Mutex::new(None);
impl Dense {
    fn new(key: &Key, count: usize, dim: usize) -> Self {
        Self {
            key: key.clone(),
            count,
            dim,
            rows: HashMap::with_capacity(count),
            vectors: Vec::with_capacity(count * dim),
        }
    }
    fn push(&mut self, id: i64, v: &[f32], source: Source) {
        if self.dim != v.len() {
            return;
        }
        if let Some(row) = self.rows.get_mut(&id) {
            if row.source.reference != source.reference {
                self.vectors[row.offset..row.offset + self.dim].copy_from_slice(v);
                row.norm = dot(v, v).sqrt();
            }
            row.source = source;
            return;
        }
        if self.rows.len() >= self.count {
            return;
        }
        let offset = self.vectors.len();
        self.vectors.extend_from_slice(v);
        self.rows.insert(
            id,
            Row {
                offset,
                norm: dot(v, v).sqrt(),
                source,
            },
        );
    }
}
fn dot(a: &[f32], b: &[f32]) -> f64 {
    // Independent lanes let LLVM vectorize while retaining double-precision cosine scores.
    let mut sums = [0.0; 8];
    let (a_blocks, tail_a) = a.as_chunks::<8>();
    let (b_blocks, tail_b) = b.as_chunks::<8>();
    for (x, y) in a_blocks.iter().zip(b_blocks) {
        for i in 0..8 {
            sums[i] += f64::from(x[i]) * f64::from(y[i]);
        }
    }
    sums.iter().sum::<f64>()
        + tail_a
            .iter()
            .zip(tail_b)
            .map(|(&x, &y)| f64::from(x) * f64::from(y))
            .sum::<f64>()
}
fn covers(key: &Key, allowed: &[u64]) -> bool {
    HOT.try_lock()
        .ok()
        .and_then(|slot| {
            slot.as_ref().map(|c| {
                c.key == *key && allowed.iter().all(|id| c.rows.contains_key(&(*id as i64)))
            })
        })
        .unwrap_or(false)
}
fn learn_rows(key: &Key, count: usize, dim: usize, rows: Vec<(i64, Vec<f32>, Source)>) {
    if rows.is_empty() || !fits(count, dim) || !key.unchanged() {
        return;
    }
    if let Ok(mut slot) = HOT.try_lock() {
        if slot.as_ref().is_none_or(|c| c.key != *key) {
            slot.take();
            *slot = Some(Dense::new(key, count, dim));
        }
        if let Some(c) = slot.as_mut() {
            for (id, v, source) in rows {
                c.push(id, &v, source);
            }
        }
    }
}
pub(super) fn prime(
    path: &Path,
    owner: &str,
    generation: u64,
    model: &str,
    vectors: &BTreeMap<i64, Vec<f32>>,
    payloads: &mut Payloads,
) {
    let dim = vectors.first_key_value().map_or(0, |(_, v)| v.len());
    if !fits(vectors.len(), dim) {
        return;
    }
    let key = Key::new(path, owner, generation, model);
    if key.stamp.is_none() {
        return;
    }
    if open_projection(path, generation, owner, model).is_err() {
        return;
    }
    // Admit only a fully verified projection. Populate from vectors already prepared by the writer.
    let Ok(mut slot) = HOT.try_lock() else {
        return;
    };
    *slot = None;
    let mut dense = Dense::new(&key, vectors.len(), dim);
    for (id, v) in vectors {
        let reference = Payloads::reference_bytes(&vector_bytes(v));
        let Some(source) = Source::capture(payloads, &reference) else {
            return;
        };
        if payloads.read(Vec::new(), &reference).is_err() {
            return;
        }
        dense.push(*id, v, source);
    }
    if key.unchanged() {
        *slot = Some(dense);
    }
}

fn rank(
    db: &mut Db,
    payloads: &mut Payloads,
    key: &Key,
    allowed: &[u64],
    q: &[f32],
    limit: usize,
) -> Option<Vec<(i64, f32)>> {
    let (mut scores, witnesses) = {
        let slot = HOT.try_lock().ok()?;
        slot.as_ref()?.shortlist(key, allowed, q, limit)?
    };
    unchanged_sources(db, payloads, &key.owner, &witnesses)?;
    scores.truncate(limit);
    Some(scores)
}

type Scores = Vec<(i64, f32)>;
type Sources = HashMap<i64, Source>;
impl Dense {
    fn shortlist(
        &self,
        key: &Key,
        allowed: &[u64],
        q: &[f32],
        limit: usize,
    ) -> Option<(Scores, Sources)> {
        if self.key != *key || self.dim != q.len() || !key.unchanged() {
            return None;
        }
        let norm = dot(q, q).sqrt();
        let mut scores = Vec::with_capacity(allowed.len());
        for id in allowed {
            let id = i64::try_from(*id).ok()?;
            let r = self.rows.get(&id)?;
            let score = (dot(q, &self.vectors[r.offset..r.offset + self.dim]) / (norm * r.norm))
                .clamp(-1., 1.) as f32;
            scores.push((id, score));
        }
        let cmp =
            |(a_id, a): &(i64, f32), (b_id, b): &(i64, f32)| b.total_cmp(a).then(a_id.cmp(b_id));
        let keep = limit.saturating_mul(4);
        if scores.len() > keep {
            scores.select_nth_unstable_by(keep, cmp);
            scores.truncate(keep);
        }
        scores.sort_unstable_by(cmp);
        let witnesses = scores
            .iter()
            .map(|(id, _)| (*id, self.rows[id].source))
            .collect::<HashMap<_, _>>();

        Some((scores, witnesses))
    }
}

fn unchanged_sources(
    db: &mut Db,
    payloads: &mut Payloads,
    owner: &str,
    witnesses: &Sources,
) -> Option<()> {
    // Revalidate the same depth as normal reranking. An altered/missing file or SQLite reference
    // takes the existing checksum-verified path; the cached bytes cannot hide a damaged result.
    let ids: Vec<i64> = witnesses.keys().copied().collect();
    for chunk in ids.chunks(400) {
        let rows = embeddings::table
            .filter(
                embeddings::owner
                    .eq(owner)
                    .and(embeddings::id.eq_any(chunk)),
            )
            .select((
                embeddings::id,
                embeddings::checksum,
                diesel::dsl::sql::<diesel::sql_types::Bool>("length(vector) = 0"),
            ))
            .load::<(i64, Vec<u8>, bool)>(db)
            .ok()?;
        if rows.len() != chunk.len() {
            return None;
        }
        for (id, reference, external) in rows {
            let old = witnesses.get(&id)?;
            if !external
                || reference != old.reference
                || Source::capture(payloads, &reference)?.stamp != old.stamp
            {
                return None;
            }
        }
    }
    Some(())
}

pub(super) struct Search {
    key: Key,
    projection: Option<FileProjection>,
}
impl Search {
    pub(super) fn open(
        dir: &Path,
        owner: &str,
        generation: u64,
        model: &str,
        allowed: &[u64],
    ) -> Result<Self> {
        let path = artifact_path(dir, owner, generation);
        let key = Key::new(&path, owner, generation, model);
        let projection = if key.stamp.is_some() && covers(&key, allowed) {
            None
        } else {
            Some(Self::projection(&key)?)
        };
        Ok(Self { key, projection })
    }
    fn projection(key: &Key) -> Result<FileProjection> {
        open_projection(&key.path, key.generation, &key.owner, &key.model).context(
            "无法打开语义索引，请运行 starstack index 修复，或运行 starstack index --rebuild 重建",
        )
    }
    pub(super) fn rank(
        self,
        db: &mut Db,
        dir: &Path,
        allowed: &[u64],
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<(i64, f32)>> {
        let mut payloads = Payloads::new(dir, &self.key.owner);
        if self.projection.is_none()
            && let Some(hits) = rank(db, &mut payloads, &self.key, allowed, vector, limit)
        {
            return Ok(hits);
        }
        let projection = match self.projection {
            Some(p) => p,
            None => Self::projection(&self.key)?,
        };
        ensure!(
            projection.manifest().dimension == vector.len(),
            "查询向量的维度与索引不同，请检查模型并运行 starstack index --rebuild"
        );
        let candidates = projection
            .search(
                vector,
                limit.saturating_mul(4).min(allowed.len()),
                ProjectionSearchOptions::default().with_allowed_ids(allowed),
            )
            .context("查找向量失败，请检查嵌入模型或运行 starstack index --rebuild")?;

        let count = projection.manifest().document_count;
        let learn = self.key.stamp.is_some() && fits(count, vector.len());
        let mut verified = Vec::new();
        let mut ranked = Vec::with_capacity(candidates.hits.len());
        let ids = candidates
            .hits
            .iter()
            .map(|hit| i64::try_from(hit.id))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for batch in ids.chunks(400) {
            let rows = embeddings::table
                .filter(embeddings::owner.eq(&self.key.owner))
                .filter(embeddings::id.eq_any(batch))
                .select((embeddings::id, embeddings::vector, embeddings::checksum))
                .load::<(i64, Vec<u8>, Vec<u8>)>(db)?;
            ensure!(
                rows.len() == batch.len(),
                "索引缺少候选向量，请运行 starstack index"
            );
            for (id, bytes, checksum) in rows {
                let source = if learn && bytes.is_empty() {
                    Source::capture(&mut payloads, &checksum)
                } else {
                    None
                };
                let bytes = payloads
                    .read(bytes, &checksum)
                    .context("无法读取原始向量，请运行 starstack index 修复")?;
                let stored = decode(&bytes)?;
                ensure!(
                    vector.len() == stored.len(),
                    "缓存向量维度不一致，请运行 starstack index --rebuild"
                );
                ranked.push((id, cosine(vector, &stored)));
                if let Some(source) = source {
                    verified.push((id, stored, source));
                }
            }
        }
        ranked.sort_unstable_by(|(a_id, a), (b_id, b)| b.total_cmp(a).then(a_id.cmp(b_id)));
        ranked.truncate(limit);

        if learn {
            learn_rows(&self.key, count, vector.len(), verified);
        }
        Ok(ranked)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store;
    use diesel::connection::SimpleConnection;

    #[test]
    fn exact_scan_requires_the_requested_scope_sources_and_file_identity() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("projection");
        std::fs::write(&file, b"valid snapshot")?;
        let key = Key::new(&file, "tenant-a", 1, "model-a");
        let mut dense = Dense::new(&key, 3, 3);
        let stamp = Stamp::read(&file).unwrap();
        let source = Source {
            reference: [3; 40],
            stamp,
        };
        dense.push(3, &[0.0, 2.0, 0.0], source);
        dense.push(2, &[3.0, 0.0, 0.0], source);
        let query = [2.0, 1.0, 0.0];
        let (rows, _) = dense.shortlist(&key, &[2, 3], &query, 2).unwrap();
        assert_eq!(
            rows,
            vec![
                (2, super::super::cosine(&query, &[3., 0., 0.])),
                (3, super::super::cosine(&query, &[0., 2., 0.]))
            ]
        );
        assert!(dense.shortlist(&key, &[1, 3], &query, 10).is_none());
        assert_eq!(dense.shortlist(&key, &[3], &query, 10).unwrap().0[0].0, 3);
        for other in [
            Key::new(&file, "tenant-b", 1, "model-a"),
            Key::new(&file, "tenant-a", 2, "model-a"),
            Key::new(&file, "tenant-a", 1, "model-b"),
        ] {
            assert!(dense.shortlist(&other, &[3], &query, 10).is_none());
        }
        std::fs::write(&file, b"short")?;
        assert!(dense.shortlist(&key, &[3], &query, 10).is_none());
        assert!(!fits(usize::MAX, 1024));
        assert!(!fits(10000, 8192));
        assert!(fits(10000, 1024));
        Ok(())
    }

    #[test]
    fn changed_sqlite_reference_or_damaged_object_goes_back_to_verified_reads() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut db = store::connect(temp.path())?;
        db.batch_execute(super::super::SCHEMA)?;
        let owner = store::owner(&mut db)?;
        let mut payloads = Payloads::new(temp.path(), &owner);
        let bytes = vector_bytes(&[1., 2., 3.]);
        let reference = payloads.write(&bytes)?;
        let source = Source::capture(&mut payloads, &reference).unwrap();
        let witnesses = HashMap::from([(4, source)]);
        let row = embeddings::table.find((&owner, 4_i64));
        diesel::insert_into(embeddings::table)
            .values((
                embeddings::owner.eq(&owner),
                embeddings::id.eq(4_i64),
                embeddings::fingerprint.eq(Vec::<u8>::new()),
                embeddings::vector.eq(Vec::<u8>::new()),
                embeddings::checksum.eq(&reference),
            ))
            .execute(&mut db)?;
        assert!(unchanged_sources(&mut db, &mut payloads, &owner, &witnesses).is_some());
        assert!(unchanged_sources(&mut db, &mut payloads, "another", &witnesses).is_none());
        diesel::update(row)
            .set(embeddings::checksum.eq(vec![0; 40]))
            .execute(&mut db)?;
        assert!(unchanged_sources(&mut db, &mut payloads, &owner, &witnesses).is_none());
        diesel::update(row)
            .set(embeddings::checksum.eq(&reference))
            .execute(&mut db)?;
        std::fs::write(payloads.object_path(&reference)?, vec![0; bytes.len()])?;
        assert!(unchanged_sources(&mut db, &mut payloads, &owner, &witnesses).is_none());
        assert!(payloads.read(vec![], &reference).is_err());
        Ok(())
    }
}
