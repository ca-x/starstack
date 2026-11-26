//! Local Ollama embeddings with an immutable, filtered HawDB cosine projection.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{Datelike, NaiveDate, Utc};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use hawdb_vector_projection::{
    FileProjection, ProjectionBuildConfig, ProjectionIdentity, ProjectionMetric,
    ProjectionSearchOptions, ProjectionWriter, RaBitQBitWidth,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::store::{self, Db, Filter, Repo};

diesel::table! {
    embeddings (owner, id) {
        owner -> Text,
        id -> BigInt,
        fingerprint -> Binary,
        vector -> Binary,
        checksum -> Binary,
    }
}

const DEFAULT_MODEL: &str = "qwen3-embedding:0.6b";
const BATCH: usize = 16;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS embeddings (
    owner TEXT NOT NULL, id INTEGER NOT NULL, fingerprint BLOB NOT NULL, vector BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(owner,id)
)";

pub(crate) fn model() -> String {
    std::env::var("STARSTACK_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

struct Ollama {
    client: Client,
    url: String,
    model: String,
}

#[derive(Deserialize)]
struct Embeddings {
    model: Option<String>,
    embeddings: Vec<Vec<f32>>,
}

impl Ollama {
    fn configured() -> Result<Self> {
        let url = std::env::var("STARSTACK_OLLAMA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".to_owned());
        Ok(Self {
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(180))
                .build()?,
            url: format!("{}/api/embed", url.trim_end_matches('/')),
            model: model(),
        })
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let response = self
            .client
            .post(&self.url)
            .json(&serde_json::json!({"model": self.model, "input": texts}))
            .send()
            .with_context(|| {
                format!(
                    "无法连接 Ollama（{}），请确认服务正在运行且地址可达",
                    self.url
                )
            })?;
        ensure!(
            response.status().is_success(),
            "Ollama 返回 {}。请确认模型 {} 可用，必要时运行 ollama pull {}",
            response.status(),
            self.model,
            self.model
        );
        let payload = response
            .json::<Embeddings>()
            .context("读取 Ollama /api/embed 响应失败")?;
        if let Some(returned) = payload.model {
            ensure!(
                returned == self.model
                    || returned.strip_suffix(":latest") == Some(self.model.as_str())
                    || self.model.strip_suffix(":latest") == Some(returned.as_str()),
                "请求的嵌入模型为 {}，Ollama 响应模型为 {returned}，请检查服务配置",
                self.model
            );
        }
        let vectors = payload.embeddings;
        ensure!(
            vectors.len() == texts.len(),
            "Ollama 返回 {} 个向量，预期与 {} 段文本逐一对应",
            vectors.len(),
            texts.len()
        );
        ensure!(
            vectors.iter().all(|v| valid(v)),
            "Ollama 返回了空向量、全零向量或非有限值，请检查模型"
        );
        Ok(vectors)
    }
}

fn valid(vector: &[f32]) -> bool {
    !vector.is_empty() && vector.iter().any(|v| *v != 0.0) && vector.iter().all(|v| v.is_finite())
}

fn document(repo: &Repo) -> String {
    format!(
        "Repository: {}\nDescription: {}\nLanguage: {}\nTopics: {}\nLicense: {}\nTags: {}\nNote: {}\nREADME: {}",
        repo.full_name,
        repo.description.as_deref().unwrap_or(""),
        repo.language.as_deref().unwrap_or(""),
        repo.topics.join(", "),
        repo.license.as_deref().unwrap_or(""),
        repo.tags.join(", "),
        repo.note,
        repo.readme
    )
}

fn fingerprint(model: &str, text: &str) -> Vec<u8> {
    Sha256::new()
        .chain_update(model.as_bytes())
        .chain_update([0])
        .chain_update(text.as_bytes())
        .finalize()
        .to_vec()
}

fn artifact_path(dir: &Path, owner: &str, generation: u64) -> PathBuf {
    dir.join(format!(
        "vectors-{:x}-{}.hawdb",
        Sha256::digest(owner.as_bytes()),
        generation % 2
    ))
}

fn generation(conn: &mut Db) -> Result<u64> {
    build_generation(conn, false)
}

fn build_generation(conn: &mut Db, rebuild: bool) -> Result<u64> {
    let raw = store::meta(conn, "index_generation")?;
    match raw.as_deref().unwrap_or("0").parse() {
        Ok(value) => Ok(value),
        Err(_) if rebuild => Ok(0),
        Err(e) => Err(e).context("索引版本无效，请运行 starstack index --rebuild"),
    }
}

fn check_model(db: &mut Db, selected: &str, rebuild: bool) -> Result<()> {
    if !rebuild && let Some(saved) = store::meta(db, "index_model")? {
        ensure!(
            saved == selected,
            "现有索引使用 {saved}，当前配置使用 {selected}。请切回旧模型，或运行 starstack index --rebuild 切换模型"
        );
    }
    Ok(())
}

fn open_projection(
    path: &Path,
    generation: u64,
    owner: &str,
    model: &str,
) -> Result<FileProjection> {
    let projection = FileProjection::open(path)?;
    let identity = &projection.manifest().identity;
    ensure!(
        identity.generation == generation
            && identity.embedding_version.as_deref() == Some(owner)
            && identity.embedding_model.as_deref() == Some(model),
        "index generation or model mismatch"
    );
    Ok(projection)
}

struct Document {
    id: i64,
    text: String,
    hash: Vec<u8>,
}

fn documents(conn: &mut Db, model: &str) -> Result<Vec<Document>> {
    let mut docs = Vec::new();
    for repo in store::repos(conn)? {
        ensure!(repo.id >= 0, "invalid repository id {}", repo.id);
        let text = document(&repo);
        docs.push(Document {
            id: repo.id,
            hash: fingerprint(model, &text),
            text,
        });
    }
    docs.sort_unstable_by_key(|doc| doc.id);
    Ok(docs)
}

fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    let (words, trailing) = bytes.as_chunks::<4>();
    ensure!(trailing.is_empty(), "invalid stored embedding size");
    let vector: Vec<_> = words.iter().copied().map(f32::from_le_bytes).collect();
    ensure!(valid(&vector), "invalid cached embedding values");
    Ok(vector)
}

fn cached_vectors(
    conn: &mut Db,
    owner: &str,
    docs: &[Document],
) -> Result<BTreeMap<i64, Vec<f32>>> {
    let mut vectors = BTreeMap::new();
    let rows = embeddings::table
        .filter(embeddings::owner.eq(owner))
        .select((
            embeddings::id,
            embeddings::fingerprint,
            embeddings::vector,
            embeddings::checksum,
        ))
        .load_iter::<(i64, Vec<u8>, Vec<u8>, Vec<u8>), _>(conn)?;
    for row in rows {
        let (id, hash, blob, checksum) = row?;
        let Ok(idx) = docs.binary_search_by_key(&id, |doc| doc.id) else {
            continue;
        };
        if hash != docs[idx].hash {
            continue;
        }
        if Sha256::digest(&blob)[..] == checksum
            && let Ok(vector) = decode(&blob)
        {
            vectors.insert(id, vector);
        }
    }
    Ok(vectors)
}

pub(crate) fn build(conn: &mut Db, rebuild: bool, progress: &dyn Fn(&str)) -> Result<String> {
    let ollama = Ollama::configured()?;
    let dir = store::data_dir(conn)?;
    build_with(conn, &dir, rebuild, &ollama, progress)
}

fn build_with(
    conn: &mut Db,
    dir: &Path,
    rebuild: bool,
    ollama: &Ollama,
    progress: &dyn Fn(&str),
) -> Result<String> {
    let owner = store::owner(conn)?;
    let (current, revision, docs, mut vectors, existing_count) = conn
        .transaction::<_, anyhow::Error, _>(|db| {
            let current = build_generation(db, rebuild)?;
            check_model(db, &ollama.model, rebuild)?;
            let docs = documents(db, &ollama.model)?;
            let vectors = if rebuild || current == 0 {
                BTreeMap::new()
            } else {
                cached_vectors(db, &owner, &docs)
                    .context("无法读取嵌入缓存，请运行 starstack index --rebuild")?
            };
            let existing_count = open_projection(
                &artifact_path(dir, &owner, current),
                current,
                &owner,
                &ollama.model,
            )
            .ok()
            .map(|p| p.manifest().document_count);
            Ok((
                current,
                library_revision(db)?,
                docs,
                vectors,
                existing_count,
            ))
        })?;
    let missing: Vec<_> = docs
        .iter()
        .filter(|doc| !vectors.contains_key(&doc.id))
        .collect();
    if !rebuild && missing.is_empty() && existing_count == Some(docs.len()) {
        conn.immediate_transaction::<_, anyhow::Error, _>(|db| {
            ensure!(
                build_generation(db, rebuild)? == current && library_revision(db)? == revision,
                "收藏资料已更新，请重试 starstack index"
            );
            store::set_meta(db, "indexed_library_revision", &revision)
        })?;
        return Ok(format!("索引已是最新，共 {} 个仓库", docs.len()));
    }

    for (i, chunk) in missing.chunks(BATCH).enumerate() {
        progress(&format!(
            "正在为仓库计算向量（第 {}–{} 个，共 {} 个）",
            i * BATCH + 1,
            i * BATCH + chunk.len(),
            missing.len()
        ));
        let texts: Vec<_> = chunk.iter().map(|d| d.text.as_str()).collect();
        for (doc, vector) in chunk.iter().zip(ollama.embed(&texts)?) {
            vectors.insert(doc.id, vector);
        }
    }

    let next = current
        .checked_add(1)
        .context("index generation overflow")?;
    let stage = tempfile::Builder::new()
        .prefix(".starstack-index-")
        .tempdir_in(dir)?;
    let staged = stage.path().join("projection.hawdb");
    progress(&format!("正在写入 {} 个仓库的 HawDB 索引", docs.len()));
    let mut identity = ProjectionIdentity::new(next);
    identity.embedding_model = Some(ollama.model.clone());
    identity.embedding_version = Some(owner.clone());
    let dimension = vectors.values().next().map_or(1, Vec::len);
    if !rebuild
        && !vectors.is_empty()
        && let Some(saved) = store::meta(conn, "index_dimension")?
    {
        ensure!(
            saved == dimension.to_string(),
            "模型的向量维度由 {saved} 变成 {dimension}，请检查模型并运行 starstack index --rebuild"
        );
    }
    let config = ProjectionBuildConfig::new(dimension, identity)
        .with_metric(ProjectionMetric::Cosine)
        .with_bit_width(RaBitQBitWidth::Four);
    let mut writer = ProjectionWriter::create(&staged, config)?;
    for (&id, vector) in &vectors {
        ensure!(
            vector.len() == dimension,
            "模型返回的向量维度不一致，请检查模型并运行 starstack index --rebuild"
        );
        writer.push(u64::try_from(id)?, vector)?;
    }
    writer.finish()?;
    // Publish into the inactive slot before committing its generation in SQLite. A failed
    // write or crash leaves the previously committed projection and its matching cache intact.
    conn.immediate_transaction(|db| {
        ensure!(
            build_generation(db, rebuild)? == current,
            "另一个进程已更新索引，请重试 starstack index"
        );
        ensure!(
            library_revision(db)? == revision,
            "建索引期间收藏资料已更新，请重试 starstack index"
        );
        let destination = artifact_path(dir, &owner, next);
        if destination.try_exists()? {
            fs::remove_file(&destination)?;
        }
        fs::rename(&staged, &destination)?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        db.batch_execute(SCHEMA)?;
        let indexed_ids = embeddings::table
            .filter(embeddings::owner.eq(&owner))
            .select(embeddings::id)
            .load::<i64>(db)?;
        for id in indexed_ids
            .into_iter()
            .filter(|id| !vectors.contains_key(id))
        {
            diesel::delete(embeddings::table.find((&owner, id))).execute(db)?;
        }
        for doc in &missing {
            let bytes: Vec<_> = vectors[&doc.id]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            diesel::replace_into(embeddings::table)
                .values((
                    embeddings::owner.eq(&owner),
                    embeddings::id.eq(doc.id),
                    embeddings::fingerprint.eq(&doc.hash),
                    embeddings::vector.eq(&bytes),
                    embeddings::checksum.eq(Sha256::digest(&bytes).to_vec()),
                ))
                .execute(db)?;
        }
        store::set_meta(db, "index_generation", &next.to_string())?;
        store::set_meta(db, "index_model", &ollama.model)?;
        if vectors.is_empty() {
            store::delete_meta(db, "index_dimension")?;
        } else {
            store::set_meta(db, "index_dimension", &dimension.to_string())?;
        }
        store::set_meta(db, "indexed_library_revision", &revision)?;
        Ok(format!(
            "索引已更新，共 {} 个仓库，新计算 {} 个，复用 {} 个",
            docs.len(),
            missing.len(),
            docs.len() - missing.len()
        ))
    })
}

fn inferred_filter(query: &str, explicit: &Filter, today: NaiveDate) -> Filter {
    let mut filter = explicit.clone();
    let q = query.to_lowercase();
    let year = if q.contains("去年") || q.contains("last year") {
        Some(today.year() - 1)
    } else if q.contains("今年") || q.contains("this year") {
        Some(today.year())
    } else {
        None
    };
    if filter.since.is_none()
        && filter.until.is_none()
        && let Some(year) = year
    {
        filter.since = NaiveDate::from_ymd_opt(year, 1, 1);
        filter.until = NaiveDate::from_ymd_opt(year, 12, 31);
    }
    if filter.language.is_none() {
        let tokens = q.split(|c: char| !(c.is_ascii_alphanumeric() || "+#".contains(c)));
        for lang in [
            "Rust",
            "Python",
            "Go",
            "TypeScript",
            "JavaScript",
            "C++",
            "C#",
            "Java",
            "Kotlin",
            "Swift",
            "Ruby",
            "Scala",
            "PHP",
        ] {
            if tokens.clone().any(|t| t.eq_ignore_ascii_case(lang)) {
                filter.language = Some(lang.to_owned());
                break;
            }
        }
    }
    filter
}

pub(crate) fn search(
    conn: &mut Db,
    query: &str,
    filter: &Filter,
    limit: usize,
) -> Result<Vec<Repo>> {
    let dir = store::data_dir(conn)?;
    search_with(conn, &dir, query, filter, limit, &Ollama::configured()?)
}

fn search_with(
    conn: &mut Db,
    dir: &Path,
    query: &str,
    filter: &Filter,
    limit: usize,
    ollama: &Ollama,
) -> Result<Vec<Repo>> {
    let query = query.trim();
    ensure!(!query.is_empty(), "请输入检索词");
    if limit == 0 {
        return Ok(Vec::new());
    }
    let filter = inferred_filter(query, filter, Utc::now().date_naive());
    filter.validate()?;
    let owner = store::owner(conn)?;
    let snapshot = conn.transaction::<_, anyhow::Error, _>(|db| {
        let allowed = allowed_ids(db, &owner, &filter)?;
        if allowed.is_empty() {
            return Ok(None);
        }
        let current = generation(db)?;
        check_model(db, &ollama.model, false)?;
        ensure!(current > 0, "尚未建立语义索引，请运行 starstack index");
        let projection = open_projection(
            &artifact_path(dir, &owner, current),
            current,
            &owner,
            &ollama.model,
        )
        .context(
            "无法打开语义索引，请运行 starstack index 修复，或运行 starstack index --rebuild 重建",
        )?;
        let revision = library_revision(db)?;
        if revision == "0"
            || store::meta(db, "indexed_library_revision")?.as_deref() != Some(&revision)
        {
            check_fingerprints(db, &owner, &ollama.model, &allowed)?;
        }
        let lists_revision = if filter.list.is_some() {
            store::meta(db, "lists_revision")?
        } else {
            None
        };
        Ok(Some((
            allowed,
            projection,
            current,
            revision,
            lists_revision,
        )))
    })?;
    let Some((allowed, projection, current, revision, lists_revision)) = snapshot else {
        return Ok(Vec::new());
    };

    let prompt = if ollama.model.starts_with("qwen3-embedding") {
        format!(
            "Instruct: Given a search request, retrieve GitHub repositories that match the request.\nQuery: {query}"
        )
    } else {
        query.to_owned()
    };
    let vector = ollama
        .embed(&[&prompt])?
        .pop()
        .context("missing query embedding")?;
    ensure!(
        projection.manifest().dimension == vector.len(),
        "查询向量的维度与索引不同，请检查模型并运行 starstack index --rebuild"
    );
    let candidates = projection
        .search(
            &vector,
            limit.saturating_mul(4).min(allowed.len()),
            ProjectionSearchOptions::default().with_allowed_ids(&allowed),
        )
        .context("查找向量失败，请检查嵌入模型或运行 starstack index --rebuild")?;

    conn.transaction(|db| {
        ensure!(
            generation(db)? == current
                && library_revision(db)? == revision
                && (filter.list.is_none() || store::meta(db, "lists_revision")? == lists_revision),
            "查询期间收藏资料已更新，请重试查询"
        );
        let mut ranked = Vec::with_capacity(candidates.hits.len());
        for hit in candidates.hits {
            let id = i64::try_from(hit.id)?;
            let (bytes, checksum) = embeddings::table
                .find((&owner, id))
                .select((embeddings::vector, embeddings::checksum))
                .first::<(Vec<u8>, Vec<u8>)>(db)?;
            ensure!(
                Sha256::digest(&bytes)[..] == checksum,
                "向量校验失败，请运行 starstack index 修复"
            );
            let stored = decode(&bytes)?;
            ensure!(
                vector.len() == stored.len(),
                "缓存向量维度不一致，请运行 starstack index --rebuild"
            );
            ranked.push((id, cosine(&vector, &stored)));
        }
        ranked.sort_unstable_by(|(a_id, a), (b_id, b)| b.total_cmp(a).then(a_id.cmp(b_id)));
        ranked.truncate(limit);
        ranked
            .into_iter()
            .map(|(id, score)| {
                let mut repo = store::by_id(db, id)?;
                repo.score = Some(score);
                Ok(repo)
            })
            .collect()
    })
}

fn library_revision(db: &mut Db) -> Result<String> {
    Ok(store::meta(db, "library_revision")?.unwrap_or_else(|| "0".to_owned()))
}

// The common unfiltered query can read only the owner's keys. Optional predicates extract
// narrow JSON fields in SQLite so searching does not deserialize thousands of README bodies.
fn allowed_ids(db: &mut Db, owner: &str, filter: &Filter) -> Result<Vec<u64>> {
    use store::repositories as r;
    let q = store::eligible(owner.to_owned(), filter).select(r::id);
    q.order(r::id)
        .load::<i64>(db)?
        .into_iter()
        .map(|id| Ok(u64::try_from(id)?))
        .collect()
}

#[derive(Debug)]
pub(crate) struct Stale;

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("索引中的仓库资料已过期，请运行 starstack index")
    }
}

impl std::error::Error for Stale {}

fn check_fingerprints(db: &mut Db, owner: &str, model: &str, allowed: &[u64]) -> Result<()> {
    let hashes = embeddings::table
        .filter(embeddings::owner.eq(owner))
        .select((embeddings::id, embeddings::fingerprint))
        .load::<(i64, Vec<u8>)>(db)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    for &id in allowed {
        let id = i64::try_from(id)?;
        let repo = store::by_id(db, id)?;
        if hashes.get(&id) != Some(&fingerprint(model, &document(&repo))) {
            return Err(Stale.into());
        }
    }
    Ok(())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (dot, l2a, l2b) = a
        .iter()
        .zip(b)
        .fold((0.0, 0.0, 0.0), |(dot, l2a, l2b), (&a, &b)| {
            let (a, b) = (f64::from(a), f64::from(b));
            (a.mul_add(b, dot), a.mul_add(a, l2a), b.mul_add(b, l2b))
        });
    (dot / (l2a * l2b).sqrt()).clamp(-1.0, 1.0) as f32
}

#[cfg(test)]
mod tests;
