//! Built-in Candle and Ollama embeddings with an immutable, filtered HawDB cosine projection.
mod exact;
pub(crate) mod mean;
mod parts;
mod payloads;
pub(crate) mod status;
mod text;
mod tier;
use tier::Tier;

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{Datelike, NaiveDate, Utc};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use hawdb_vector_projection::{
    FileProjection, ProjectionBuildConfig, ProjectionIdentity, ProjectionMetric, ProjectionWriter,
    RaBitQBitWidth,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::store::{self, Db, Filter, Repo};
use payloads::Payloads;

diesel::table! {
    embeddings (owner, id) {
        owner -> Text,
        id -> BigInt,
        fingerprint -> Binary,
        vector -> Binary,
        checksum -> Binary,
    }
}

#[cfg(test)]
const DEFAULT_MODEL: &str = "qwen3-embedding:0.6b";
const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const BATCH: usize = 16;
diesel::table! {
    embedding_checkpoints (owner, id) {
        owner -> Text,
        id -> BigInt,
        fingerprint -> Binary,
        vector -> Binary,
        checksum -> Binary,
    }
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS embeddings (
    owner TEXT NOT NULL, id INTEGER NOT NULL, fingerprint BLOB NOT NULL, vector BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(owner,id)
);
CREATE TABLE IF NOT EXISTS embedding_parts (
    owner TEXT NOT NULL, id INTEGER NOT NULL, part INTEGER NOT NULL, fingerprint BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(owner,id,part)
);
CREATE TABLE IF NOT EXISTS embedding_checkpoints (
    owner TEXT NOT NULL, id INTEGER NOT NULL, fingerprint BLOB NOT NULL, vector BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(owner,id)
)";

pub(crate) fn backend(db: &mut Db) -> Result<String> {
    if let Some(explicit) = store::meta(db, "embedding_backend")? {
        ensure!(
            matches!(explicit.as_str(), "builtin" | "ollama"),
            "无效的嵌入后端，请重新选择模型"
        );
        return Ok(explicit);
    }
    if crate::ollama_settings::inherited() {
        return Ok("ollama".into());
    }
    if let Some(saved) = store::meta(db, "index_backend")? {
        return Ok(saved);
    }
    Ok(if store::meta(db, "index_model")?.is_some() {
        "ollama"
    } else {
        "builtin"
    }
    .into())
}

pub(crate) fn set_backend(db: &mut Db, name: &str) -> Result<()> {
    match name {
        "builtin" => {
            let model = builtin_id(db)?;
            set_builtin(db, &model)
        }
        "ollama" => {
            crate::ollama_settings::selected(db)?;
            store::set_meta(db, "embedding_backend", "ollama")
        }
        _ => anyhow::bail!("请选择 builtin 或 ollama 后端"),
    }
}

fn builtin_id(db: &mut Db) -> Result<String> {
    Ok(store::meta(db, "embedding_builtin_model")?
        .unwrap_or_else(|| crate::models::DEFAULT_ID.to_owned()))
}

pub(crate) fn set_builtin(db: &mut Db, id: &str) -> Result<()> {
    let home = store::data_dir(db)?;
    let _lease = crate::models::open_installed(&home, id)?;
    crate::models::descriptor(&home, id)?;
    db.immediate_transaction(|db| {
        store::set_meta(db, "embedding_builtin_model", id)?;
        store::set_meta(db, "embedding_backend", "builtin")?;
        store::delete_meta(db, "index_issue")
    })
}

pub(crate) fn ensure_model_unused(db: &mut Db, id: &str) -> Result<()> {
    use diesel::sql_types::{Bool, Text};
    #[derive(QueryableByName)]
    struct Used {
        #[diesel(sql_type = Bool)]
        used: bool,
    }
    // Older built-in choices stored only their backend. Their implicit model is MiniLM.
    let used=diesel::sql_query("SELECT EXISTS (
      SELECT 1 FROM metadata_values b LEFT JOIN metadata_values m
        ON m.owner_id=b.owner_id AND m.key=CASE b.key WHEN 'embedding_backend' THEN 'embedding_builtin_model' ELSE 'index_model' END
      WHERE b.key IN ('embedding_backend','index_backend') AND b.value='builtin' AND COALESCE(m.value,?)=?
    ) AS used").bind::<Text,_>(crate::models::DEFAULT_ID).bind::<Text,_>(id).get_result::<Used>(db)?.used;
    ensure!(
        !used,
        "此内置模型仍被资料库选择或索引使用，请先切换模型并重建相关索引"
    );
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Provider {
    Builtin {
        id: String,
        family: &'static str,
        fingerprint: String,
    },
    Ollama {
        url: String,
        model: String,
    },
}

impl Provider {
    fn selected(db: &mut Db) -> Result<Self> {
        match backend(db)?.as_str() {
            "builtin" => {
                let id = builtin_id(db)?;
                let crate::models::Descriptor {
                    family,
                    fingerprint,
                } = crate::models::descriptor(&store::data_dir(db)?, &id)?;
                Ok(Self::Builtin {
                    id,
                    family,
                    fingerprint,
                })
            }
            "ollama" => {
                let (url, model) = crate::ollama_settings::selected(db)?;
                Ok(Self::Ollama { url, model })
            }
            _ => anyhow::bail!("不支持的嵌入后端"),
        }
    }
    fn name(&self) -> &str {
        match self {
            Self::Builtin { .. } => "builtin",
            Self::Ollama { .. } => "ollama",
        }
    }
    fn model(&self) -> &str {
        match self {
            Self::Builtin { id, .. } => id,
            Self::Ollama { model, .. } => model,
        }
    }
}

pub(crate) fn configured_model(db: &mut Db) -> Result<String> {
    match backend(db)?.as_str() {
        "builtin" => builtin_id(db),
        _ => crate::ollama_settings::configured_model(db),
    }
}

#[derive(PartialEq, Eq)]
struct Selection {
    provider: Provider,
    policy: crate::chunking::Policy,
}

impl Selection {
    fn read(db: &mut Db) -> Result<Self> {
        let provider = Provider::selected(db)?;
        let policy = crate::chunking::selected(db, provider.name())?;
        Ok(Self { provider, policy })
    }

    fn model_identity(&self) -> String {
        match &self.provider {
            // Preserve the first MiniLM index format only for the exact original
            // config, tokenizer and weights. A repinned package must get a new key.
            Provider::Builtin {
                id, fingerprint, ..
            } if id == crate::models::DEFAULT_ID
                && fingerprint
                    == "253df9140f5abfa7533637cc3718ac079a2ad3f30dbe5f8728a3717761570ff5" =>
            {
                crate::native_embedding::IDENTITY.to_owned()
            }
            Provider::Builtin {
                id,
                family,
                fingerprint,
            } => {
                let protocol = if *family == "qwen3-gguf" {
                    crate::native_embedding::QWEN_PROTOCOL
                } else {
                    "masked-mean-128-stride24-v3"
                };
                format!("builtin:{id}:{fingerprint}:{protocol}")
            }
            // Old indexes only recorded the model name for the default Ollama endpoint.
            Provider::Ollama { url, model } if url == DEFAULT_OLLAMA_URL => model.clone(),
            Provider::Ollama { url, model } => {
                format!("ollama:{model}@{:x}", Sha256::digest(url.as_bytes()))
            }
        }
    }

    fn identity(&self) -> String {
        let base = self.model_identity();
        if self.policy.legacy() {
            base
        } else {
            format!("{base}|{}", self.policy.identity())
        }
    }

    fn mismatches(&self, db: &mut Db) -> Result<bool> {
        let Some(model) = store::meta(db, "index_model")? else {
            return Ok(false);
        };
        if model != self.provider.model() {
            return Ok(true);
        }
        let identity = store::meta(db, "index_identity")?.unwrap_or(model);
        let backend = store::meta(db, "index_backend")?.unwrap_or_else(|| "ollama".into());
        Ok(identity != self.identity() || backend != self.provider.name())
    }
}

pub(crate) fn needs_rebuild(db: &mut Db) -> Result<bool> {
    db.transaction(|db| Selection::read(db)?.mismatches(db))
}

/// Work measured inside a provider. Remote inference includes the HTTP round trip.
pub(crate) struct EmbeddingWork {
    pub tokens: Option<usize>,
    pub tokenizer: Duration,
    pub inference: Duration,
}

impl EmbeddingWork {
    pub fn tokenized(time: Duration) -> Self {
        Self {
            tokens: Some(0),
            tokenizer: time,
            inference: Duration::ZERO,
        }
    }
}

// Keep sub-millisecond work across batches and round only the reported total.
fn accumulate_ms(milliseconds: &mut u64, fraction: &mut Duration, delta: Duration) {
    let total = *fraction + delta;
    let whole = total.as_millis() as u64;
    *milliseconds = milliseconds.saturating_add(whole);
    *fraction = total - Duration::from_millis(whole);
}

impl Progress {
    fn add_embedding(&mut self, work: EmbeddingWork) {
        self.tokens = self
            .tokens
            .zip(work.tokens)
            .map(|(a, b)| a.saturating_add(b));
        accumulate_ms(
            &mut self.tokenizer_ms,
            &mut self.tokenizer_fraction,
            work.tokenizer,
        );
        accumulate_ms(
            &mut self.inference_ms,
            &mut self.inference_fraction,
            work.inference,
        );
    }
}

#[derive(Clone, Copy)]
struct RemoteBatches {
    size: usize,
    concurrent: usize,
}

impl RemoteBatches {
    fn configured() -> Result<Self> {
        fn read(key: &str, default: usize, max: usize) -> Result<usize> {
            let value = std::env::var(key)
                .map_or(Ok(default), |v| v.parse())
                .with_context(|| format!("{key} 应为 1 至 {max} 的整数"))?;
            ensure!((1..=max).contains(&value), "{key} 应为 1 至 {max} 的整数");
            Ok(value)
        }
        Ok(Self {
            size: read("STARSTACK_OLLAMA_BATCH_SIZE", 64, 128)?,
            concurrent: read("STARSTACK_OLLAMA_CONCURRENCY", 2, 4)?,
        })
    }
}

enum Runtime {
    Ollama(Client, RemoteBatches),
    Builtin {
        home: PathBuf,
        files: OnceCell<crate::models::ModelFiles>,
    },
}

struct Engine {
    selection: Selection,
    runtime: Runtime,
    managed: bool,
    profile: Option<crate::index_worker::Profile>,
}

#[derive(Deserialize)]
struct Embeddings {
    model: Option<String>,
    embeddings: Vec<Vec<f32>>,
    prompt_eval_count: Option<usize>,
}

impl Engine {
    fn configured(db: &mut Db, timeout: Duration) -> Result<Self> {
        let selection = db.transaction(Selection::read)?;
        let runtime = match &selection.provider {
            Provider::Builtin { .. } => Runtime::Builtin {
                home: store::data_dir(db)?,
                files: OnceCell::new(),
            },
            Provider::Ollama { .. } => Runtime::Ollama(
                Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(timeout)
                    .build()?,
                RemoteBatches::configured()?,
            ),
        };
        Ok(Self {
            selection,
            runtime,
            managed: true,
            profile: None,
        })
    }

    fn group_size(&self) -> usize {
        match &self.runtime {
            Runtime::Ollama(_, batches) => batches.size * batches.concurrent,
            Runtime::Builtin { .. } => 64,
        }
    }

    // Keep the shared filesystem lease through publication or the last result check.
    fn acquire(&self) -> Result<()> {
        if let Runtime::Builtin { home, files } = &self.runtime
            && files.get().is_none()
        {
            let installed = crate::models::open_installed(home, self.selection.provider.model())?;
            let _ = files.set(installed);
        }
        Ok(())
    }

    fn check_current(&self, db: &mut Db) -> Result<()> {
        if self.managed {
            ensure!(Selection::read(db)? == self.selection, BuildConflict);
        }
        Ok(())
    }

    fn check_profile(&self, db: &mut Db) -> Result<()> {
        if let Some(profile) = self.profile {
            ensure!(
                crate::index_worker::Profile::load(db)? == profile,
                crate::index_worker::ProfileChanged
            );
        }
        Ok(())
    }

    #[cfg(test)]
    fn fixture(url: &str, model: &str) -> Result<Self> {
        Ok(Self {
            selection: Selection {
                provider: Provider::Ollama {
                    model: model.to_owned(),
                    url: url.to_owned(),
                },
                policy: crate::chunking::Policy::whole_document(),
            },
            runtime: Runtime::Ollama(
                Client::builder().no_proxy().build()?,
                RemoteBatches {
                    size: BATCH,
                    concurrent: 1,
                },
            ),
            managed: false,
            profile: None,
        })
    }

    fn embed(
        &self,
        db: &mut Db,
        texts: &[&str],
        observed: &mut dyn FnMut(EmbeddingWork),
    ) -> Result<Vec<Vec<f32>>> {
        // Re-check before sending each batch. A settings change during one request
        // must stop later batches from sending repository text to the former server.
        db.transaction(|db| self.check_current(db))?;
        self.acquire()?;
        match (&self.runtime, &self.selection.provider) {
            (Runtime::Builtin { files, .. }, Provider::Builtin { family, .. }) => {
                crate::native_embedding::embed(
                    files.get().context("内置模型不可用")?.path(),
                    family,
                    texts,
                    observed,
                )
            }
            (Runtime::Ollama(client, batches), Provider::Ollama { url, model }) => {
                let started = std::time::Instant::now();
                let (vectors, tokens) = if texts.len() <= batches.size {
                    ollama_embed(client, url, model, texts)?
                } else {
                    // Dispatch one bounded wave. The handles retain input order even if
                    // a later request finishes first. Another wave revalidates the owner settings.
                    ensure!(texts.len() <= self.group_size(), "Ollama 批次超出并发上限");
                    std::thread::scope(|scope| -> Result<_> {
                        let handles: Vec<_> = texts
                            .chunks(batches.size)
                            .map(|chunk| {
                                scope.spawn(move || ollama_embed(client, url, model, chunk))
                            })
                            .collect();
                        let mut rows = Vec::with_capacity(texts.len());
                        let mut tokens = Some(0usize);
                        for handle in handles {
                            let (vectors, count) = handle
                                .join()
                                .map_err(|_| anyhow::anyhow!("Ollama 请求线程中断"))??;
                            rows.extend(vectors);
                            tokens = tokens.zip(count).map(|(a, b)| a.saturating_add(b));
                        }
                        Ok((rows, tokens))
                    })?
                };
                observed(EmbeddingWork {
                    tokens,
                    tokenizer: Duration::ZERO,
                    inference: started.elapsed(),
                });
                Ok(vectors)
            }
            _ => anyhow::bail!("嵌入配置不一致"),
        }
    }
}

fn ollama_embed(
    client: &Client,
    url: &str,
    model: &str,
    texts: &[&str],
) -> Result<(Vec<Vec<f32>>, Option<usize>)> {
    let response = client
        .post(format!("{url}/api/embed"))
        .json(&serde_json::json!({"model": model, "input": texts}))
        .send()
        .with_context(|| format!("无法连接 Ollama（{}），请确认服务正在运行且地址可达", url))?;
    ensure!(
        response.status().is_success(),
        "Ollama 返回 {}。请确认模型 {} 可用，必要时运行 ollama pull {}",
        response.status(),
        model,
        model
    );
    let payload = response
        .json::<Embeddings>()
        .context("读取 Ollama /api/embed 响应失败")?;
    if let Some(returned) = payload.model {
        ensure!(
            returned == model
                || returned.strip_suffix(":latest") == Some(model)
                || model.strip_suffix(":latest") == Some(returned.as_str()),
            "请求的嵌入模型为 {}，Ollama 响应模型为 {returned}，请检查服务配置",
            model
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
    Ok((vectors, payload.prompt_eval_count))
}
fn valid(vector: &[f32]) -> bool {
    !vector.is_empty() && vector.iter().any(|v| *v != 0.0) && vector.iter().all(|v| v.is_finite())
}

fn document(repo: &Repo) -> String {
    let mut result = text::metadata(repo);
    if !repo.code_context.is_empty() {
        result.push_str(&format!("\nSaved code: {}", repo.code_context));
    }
    if !repo.commit_index.is_empty() {
        result.push_str(&format!("\nRecent commits: {}", repo.commit_index));
    }
    result.push_str("\nREADME: ");
    result.push_str(&repo.readme);
    result
}

fn structured_hash(key: &str, meta: &str, body: &str) -> Vec<u8> {
    Sha256::new()
        .chain_update(key.as_bytes())
        .chain_update([0])
        .chain_update((meta.len() as u64).to_le_bytes())
        .chain_update(meta.as_bytes())
        .chain_update(body.as_bytes())
        .finalize()
        .to_vec()
}

fn document_hash(selection: &Selection, key: &str, repo: &Repo, tier: Tier) -> Vec<u8> {
    if let crate::chunking::Policy::Structured { body_cap, .. } = selection.policy {
        structured_hash(key, &text::metadata(repo), &tier.body(repo, body_cap))
    } else {
        fingerprint(key, &document(repo))
    }
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

fn check_model(db: &mut Db, selection: &Selection, rebuild: bool) -> Result<()> {
    ensure!(
        rebuild || !selection.mismatches(db)?,
        "后端、模型、服务地址或分块配置与现有索引不同，请运行 starstack index --rebuild"
    );
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
    hash: Vec<u8>,
}

fn documents(
    conn: &mut Db,
    owner: &str,
    selection: &Selection,
    model: &str,
    rebuild: bool,
    tier: Tier,
) -> Result<Vec<Document>> {
    use diesel::sql_types::{BigInt, Binary, Nullable, Text};
    #[derive(QueryableByName)]
    struct Source {
        #[diesel(sql_type = BigInt)]
        id: i64,
        #[diesel(sql_type = Nullable<Binary>)]
        fingerprint: Option<Vec<u8>>,
    }
    if !rebuild
        && matches!(tier, Tier::Full)
        && store::meta(conn, "index_body_pending")?.is_none()
        && store::meta(conn, "index_identity")?.as_deref() == Some(model)
        && store::meta(conn, "index_source_versions")?.is_some()
    {
        // A published version covers all embedding inputs. Decode repository JSON
        // and parse Markdown only for new or changed sources in this snapshot.
        let sources = diesel::sql_query(
            "SELECT r.id, e.fingerprint FROM repositories r
             LEFT JOIN index_source_versions v ON v.owner=r.owner_id AND v.id=r.id AND v.version=r.source_version
             LEFT JOIN embeddings e ON e.owner=v.owner AND e.id=v.id
             WHERE r.owner_id=? AND r.active=1 ORDER BY r.id"
        ).bind::<Text, _>(owner).load::<Source>(conn)?;
        return sources
            .into_iter()
            .map(|source| {
                ensure!(source.id >= 0, "invalid repository id {}", source.id);
                let hash = match source.fingerprint {
                    Some(hash) => hash,
                    None => document_hash(selection, model, &store::by_id(conn, source.id)?, tier),
                };
                Ok(Document {
                    id: source.id,
                    hash,
                })
            })
            .collect();
    }
    let mut docs = Vec::new();
    store::visit_repos(conn, |repo| {
        ensure!(repo.id >= 0, "invalid repository id {}", repo.id);
        docs.push(Document {
            id: repo.id,
            hash: document_hash(selection, model, &repo, tier),
        });
        Ok(())
    })?;
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

type StoredVector = (i64, Vec<u8>, Vec<u8>, Vec<u8>);

fn matching_vectors(
    rows: impl Iterator<Item = diesel::QueryResult<StoredVector>>,
    docs: &[Document],
    payloads: &mut Payloads,
) -> Result<BTreeMap<i64, Vec<f32>>> {
    let mut vectors = BTreeMap::new();
    for row in rows {
        let (id, hash, blob, checksum) = row?;
        let Ok(idx) = docs.binary_search_by_key(&id, |doc| doc.id) else {
            continue;
        };
        if hash == docs[idx].hash
            && let Ok(bytes) = payloads.read(blob, &checksum)
            && let Ok(vector) = decode(&bytes)
        {
            vectors.insert(id, vector);
        }
    }
    Ok(vectors)
}

fn cached_vectors(
    conn: &mut Db,
    owner: &str,
    docs: &[Document],
    payloads: &mut Payloads,
) -> Result<BTreeMap<i64, Vec<f32>>> {
    use embeddings as t;
    matching_vectors(
        t::table
            .filter(t::owner.eq(owner))
            .select((t::id, t::fingerprint, t::vector, t::checksum))
            .load_iter::<StoredVector, _>(conn)?,
        docs,
        payloads,
    )
}

fn checkpoint_vectors(
    conn: &mut Db,
    owner: &str,
    docs: &[Document],
    payloads: &mut Payloads,
) -> Result<BTreeMap<i64, Vec<f32>>> {
    use embedding_checkpoints as t;
    matching_vectors(
        t::table
            .filter(t::owner.eq(owner))
            .select((t::id, t::fingerprint, t::vector, t::checksum))
            .load_iter::<StoredVector, _>(conn)?,
        docs,
        payloads,
    )
}

fn write_checkpoint(
    db: &mut Db,
    payloads: &mut Payloads,
    owner: &str,
    doc: &Document,
    vector: &[f32],
) -> Result<()> {
    use embedding_checkpoints as t;
    let reference = payloads.write(&vector_bytes(vector))?;
    diesel::replace_into(t::table)
        .values((
            t::owner.eq(owner),
            t::id.eq(doc.id),
            t::fingerprint.eq(&doc.hash),
            t::vector.eq(Vec::<u8>::new()),
            t::checksum.eq(reference),
        ))
        .execute(db)?;
    Ok(())
}

fn clear_checkpoints(db: &mut Db, owner: &str) -> Result<()> {
    diesel::delete(embedding_checkpoints::table.filter(embedding_checkpoints::owner.eq(owner)))
        .execute(db)?;
    Ok(())
}

fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn migrate_payloads(db: &mut Db, owner: &str, payloads: &mut Payloads) -> Result<()> {
    if store::meta(db, "index_payload_storage")?.is_some() {
        return Ok(());
    }
    // Moving one bounded batch at a time lets existing searches keep using either format.
    macro_rules! migrate {
        ($table:ident) => {{
            use $table as t;
            while db.immediate_transaction::<_, anyhow::Error, _>(|db| {
                let rows = t::table
                    .filter(t::owner.eq(owner).and(t::vector.ne(Vec::<u8>::new())))
                    .select((t::id, t::vector, t::checksum))
                    .limit(128)
                    .load::<(i64, Vec<u8>, Vec<u8>)>(db)?;
                let moved = !rows.is_empty();
                for (id, bytes, checksum) in rows {
                    if Sha256::digest(&bytes)[..] == checksum && decode(&bytes).is_ok() {
                        // HawDB publishes durably before SQLite changes the reference.
                        let reference = payloads.write(&bytes)?;
                        diesel::update(t::table.find((owner, id)))
                            .set((t::vector.eq(Vec::<u8>::new()), t::checksum.eq(reference)))
                            .execute(db)?;
                    } else {
                        diesel::delete(t::table.find((owner, id))).execute(db)?;
                    }
                }
                Ok(moved)
            })? {}
        }};
    }
    migrate!(embeddings);
    migrate!(embedding_checkpoints);
    store::set_meta(db, "index_payload_storage", "hawdb-objects-v1")
}

fn collect_payloads(db: &mut Db, owner: &str, payloads: &mut Payloads) {
    let result = db.immediate_transaction::<_, anyhow::Error, _>(|db| {
        let published = embeddings::table
            .filter(embeddings::owner.eq(owner))
            .select(embeddings::checksum)
            .load::<Vec<u8>>(db)?;
        let pending = embedding_checkpoints::table
            .filter(embedding_checkpoints::owner.eq(owner))
            .select(embedding_checkpoints::checksum)
            .load::<Vec<u8>>(db)?;
        use parts::embedding_parts as t;
        let segments = t::table
            .filter(t::owner.eq(owner))
            .select(t::checksum)
            .load::<Vec<u8>>(db)?;
        payloads.collect_orphans(published.into_iter().chain(pending).chain(segments))
    });
    if let Err(error) = result {
        eprintln!("清理旧向量文件失败：{error}");
    }
}

pub(crate) fn prepare_build(db: &mut Db, rebuild: bool) -> Result<bool> {
    db.batch_execute(SCHEMA)?;
    let owner = store::owner(db)?;
    let data_dir = store::data_dir(db)?;
    migrate_payloads(db, &owner, &mut Payloads::new(&data_dir, &owner))?;
    db.immediate_transaction(|db| {
        let identity = Selection::read(db)?.identity();
        let previous = store::meta(db, "index_rebuild")?;
        let pending_rebuild = previous.as_deref() == Some(identity.as_str());
        let restart_failed = rebuild && store::meta(db, "index_issue")?.is_some();
        if rebuild && (!pending_rebuild || restart_failed) {
            let owner = store::owner(db)?;
            clear_checkpoints(db, &owner)?;
            // A fresh rebuild of the published model also repairs a remote model whose
            // weights changed under the same name. Policy-only changes can reuse parts.
            if restart_failed
                || store::meta(db, "index_identity")?.is_none_or(|published| published == identity)
            {
                use parts::embedding_parts as t;
                diesel::delete(t::table.filter(t::owner.eq(&owner))).execute(db)?;
            }
            store::set_meta(db, "index_rebuild", &identity)?;
        } else if !pending_rebuild && previous.is_some() {
            store::delete_meta(db, "index_rebuild")?;
        }
        store::set_meta(db, "index_pending", "1")?;
        store::delete_meta(db, "index_issue")?;
        Ok(rebuild || pending_rebuild)
    })
}

pub(crate) fn pending_rebuild(db: &mut Db) -> Result<bool> {
    let identity = Selection::read(db)?.identity();
    Ok(store::meta(db, "index_rebuild")?.as_deref() == Some(identity.as_str()))
}

#[derive(Debug)]
pub(crate) struct BuildConflict;

impl std::fmt::Display for BuildConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("建索引期间收藏资料或嵌入配置已更新，请重试 starstack index")
    }
}

impl std::error::Error for BuildConflict {}

#[derive(Clone, Copy, Default, serde::Deserialize, serde::Serialize)]
pub(crate) struct Progress {
    pub repositories: usize,
    pub total: usize,
    pub reused: usize,
    pub resumed: usize,
    pub chunks: usize,
    pub tokens: Option<usize>,
    pub elapsed_seconds: u64,
    #[serde(default)]
    pub tokenizer_ms: u64,
    #[serde(default)]
    pub inference_ms: u64,
    #[serde(default)]
    pub checkpoint_ms: u64,
    #[serde(skip)]
    tokenizer_fraction: Duration,
    #[serde(skip)]
    inference_fraction: Duration,
}

pub(crate) fn build(conn: &mut Db, rebuild: bool, progress: &dyn Fn(&str)) -> Result<String> {
    crate::index_worker::build(conn, rebuild, progress, &|counts| {
        let tokens = counts
            .tokens
            .map_or_else(|| "未报告 token 数".into(), |n| format!("{n} token"));
        progress(&format!(
            "仓库 {}/{}，已接续 {} 个，已处理 {} 段文本，{}，用时 {} 秒",
            counts.repositories,
            counts.total,
            counts.resumed,
            counts.chunks,
            tokens,
            counts.elapsed_seconds
        ));
    })
}

pub(crate) fn build_observed(
    conn: &mut Db,
    rebuild: bool,
    progress: &dyn Fn(&str),
    observe: &dyn Fn(Progress),
) -> Result<String> {
    build_with_profile(conn, rebuild, None, progress, observe)
}

pub(crate) fn build_with_profile(
    conn: &mut Db,
    rebuild: bool,
    profile: Option<crate::index_worker::Profile>,
    progress: &dyn Fn(&str),
    observe: &dyn Fn(Progress),
) -> Result<String> {
    progress("正在检查索引模型和待处理仓库");
    let mut engine = Engine::configured(conn, Duration::from_secs(180))?;
    engine.profile = profile;
    engine.check_profile(conn)?;
    engine.acquire()?;
    let dir = store::data_dir(conn)?;
    build_observed_with(conn, &dir, rebuild, &engine, progress, observe)
}

#[cfg(test)]
fn build_with(
    conn: &mut Db,
    dir: &Path,
    rebuild: bool,
    engine: &Engine,
    progress: &dyn Fn(&str),
) -> Result<String> {
    build_observed_with(conn, dir, rebuild, engine, progress, &|_| {})
}

fn build_observed_with(
    conn: &mut Db,
    dir: &Path,
    rebuild: bool,
    engine: &Engine,
    progress: &dyn Fn(&str),
    observe: &dyn Fn(Progress),
) -> Result<String> {
    if let Some(metadata) = Tier::initial(conn, &engine.selection, rebuild)? {
        progress("正在建立仓库元数据索引");
        let initial = std::cell::Cell::new(Progress::default());
        build_stage(conn, dir, rebuild, metadata, engine, progress, &|counts| {
            initial.set(counts);
            observe(counts);
        })?;
        progress("元数据索引已发布，正在补全正文");
        engine.check_profile(conn)?;
        let first = initial.get();
        return build_stage(
            conn,
            dir,
            false,
            Tier::Full,
            engine,
            progress,
            &|mut counts| {
                counts.tokenizer_ms += first.tokenizer_ms;
                counts.inference_ms += first.inference_ms;
                accumulate_ms(
                    &mut counts.tokenizer_ms,
                    &mut counts.tokenizer_fraction,
                    first.tokenizer_fraction,
                );
                accumulate_ms(
                    &mut counts.inference_ms,
                    &mut counts.inference_fraction,
                    first.inference_fraction,
                );
                counts.checkpoint_ms += first.checkpoint_ms;
                counts.elapsed_seconds += first.elapsed_seconds;
                counts.chunks += first.chunks;
                counts.tokens = counts
                    .tokens
                    .zip(first.tokens)
                    .map(|(a, b)| a.saturating_add(b));
                observe(counts);
            },
        );
    }
    build_stage(conn, dir, rebuild, Tier::Full, engine, progress, observe)
}

fn build_stage(
    conn: &mut Db,
    dir: &Path,
    rebuild: bool,
    tier: Tier,
    engine: &Engine,
    progress: &dyn Fn(&str),
    observe: &dyn Fn(Progress),
) -> Result<String> {
    let key = engine.selection.identity();
    let owner = store::owner(conn)?;
    conn.batch_execute(SCHEMA)?;
    let mut payloads = Payloads::new(dir, &owner);
    migrate_payloads(conn, &owner, &mut payloads)?;
    let (current, revision, docs, mut vectors, reuse_count, mut computed, existing_count) =
        conn.transaction::<_, anyhow::Error, _>(|db| {
            engine.check_current(db)?;
            let current = build_generation(db, rebuild)?;
            check_model(db, &engine.selection, rebuild)?;
            if let Tier::Metadata(expected) = tier {
                ensure!(current == expected, BuildConflict);
            }
            let docs = documents(db, &owner, &engine.selection, &key, rebuild, tier)?;
            let mut vectors = if rebuild || current == 0 {
                BTreeMap::new()
            } else {
                cached_vectors(db, &owner, &docs, &mut payloads)
                    .context("无法读取嵌入缓存，请运行 starstack index --rebuild")?
            };
            let reuse_count = vectors.len();
            let mut computed = std::collections::BTreeSet::new();
            for (id, vector) in checkpoint_vectors(db, &owner, &docs, &mut payloads)? {
                if let std::collections::btree_map::Entry::Vacant(entry) = vectors.entry(id) {
                    computed.insert(id);
                    entry.insert(vector);
                }
            }
            let existing_count =
                open_projection(&artifact_path(dir, &owner, current), current, &owner, &key)
                    .ok()
                    .map(|p| p.manifest().document_count);
            Ok((
                current,
                library_revision(db)?,
                docs,
                vectors,
                reuse_count,
                computed,
                existing_count,
            ))
        })?;
    let resumed = computed.len();
    let mut missing: Vec<_> = docs
        .iter()
        .filter(|doc| !vectors.contains_key(&doc.id))
        .collect();
    if matches!(tier, Tier::Full)
        && matches!(
            engine.selection.policy,
            crate::chunking::Policy::Structured { .. }
        )
    {
        tier::prioritize_bodies(conn, &owner, &mut missing)?;
    }
    if !rebuild && missing.is_empty() && resumed == 0 && existing_count == Some(docs.len()) {
        conn.immediate_transaction::<_, anyhow::Error, _>(|db| {
            engine.check_current(db)?;
            ensure!(
                build_generation(db, rebuild)? == current && library_revision(db)? == revision,
                BuildConflict
            );
            finish_index(db, &owner, &revision, rebuild, tier)
        })?;
        collect_payloads(conn, &owner, &mut payloads);
        observe(Progress {
            repositories: 0,
            total: 0,
            reused: docs.len(),
            resumed: 0,
            chunks: 0,
            tokens: Some(0),
            ..Progress::default()
        });
        return Ok(format!("索引已是最新，共 {} 个仓库", docs.len()));
    }

    let start = std::time::Instant::now();
    let mut counts = Progress {
        repositories: 0,
        total: missing.len(),
        reused: reuse_count,
        resumed,
        chunks: 0,
        tokens: Some(0),
        ..Progress::default()
    };
    let mut checkpoint_time = Duration::ZERO;
    progress("正在计算仓库向量");
    observe(counts);
    for group in missing.chunks(engine.group_size()) {
        if matches!(
            engine.selection.policy,
            crate::chunking::Policy::Structured { .. }
        ) {
            let mut builder = parts::Build {
                engine,
                owner: &owner,
                generation: current,
                revision: &revision,
                tier,
                checkpoint_time: &mut checkpoint_time,
                rebuild,
                payloads: &mut payloads,
                started: start,
                observe,
            };
            let built = builder.run(conn, group, &mut counts)?;
            computed.extend(built.keys().copied());
            vectors.extend(built);
            continue;
        }
        // Do not hold a SQLite transaction during inference.
        let mut means: Vec<_> = (0..group.len()).map(|_| mean::Mean::default()).collect();
        let mut batch = Vec::with_capacity(BATCH);
        let mut finished = Vec::with_capacity(BATCH);
        let mut flush = |db: &mut Db,
                         batch: &mut Vec<(usize, String)>,
                         means: &mut [mean::Mean],
                         finished: &mut Vec<bool>|
         -> Result<()> {
            embed_fragments(db, engine, batch, means, &mut |tokens| {
                counts.add_embedding(tokens);
                counts.elapsed_seconds = start.elapsed().as_secs();
                observe(counts);
            })?;
            let completed: Vec<_> = batch
                .iter()
                .zip(finished.iter())
                .filter(|&(_, done)| *done)
                .map(|((idx, _), _)| Ok((group[*idx], std::mem::take(&mut means[*idx]).finish()?)))
                .collect::<Result<_>>()?;
            if !completed.is_empty() {
                let writing = std::time::Instant::now();
                // Checkpoints are invisible to search until the entire projection commits.
                db.immediate_transaction::<_, anyhow::Error, _>(|db| {
                    engine.check_current(db)?;
                    ensure!(build_generation(db, rebuild)? == current, BuildConflict);
                    for (doc, vector) in &completed {
                        write_checkpoint(db, &mut payloads, &owner, doc, vector)?;
                    }
                    Ok(())
                })?;
                checkpoint_time += writing.elapsed();
                counts.checkpoint_ms = checkpoint_time.as_millis() as u64;
            }
            counts.chunks += batch.len();
            counts.repositories += completed.len();
            for (doc, vector) in completed {
                computed.insert(doc.id);
                vectors.insert(doc.id, vector);
            }
            counts.elapsed_seconds = start.elapsed().as_secs();
            observe(counts);
            batch.clear();
            finished.clear();
            engine.check_profile(db)
        };
        for (idx, doc) in group.iter().enumerate() {
            let repo = match store::by_id(conn, doc.id) {
                Ok(repo) => repo,
                Err(error) => {
                    ensure!(library_revision(conn)? == revision, BuildConflict);
                    return Err(error);
                }
            };
            let text = document(&repo);
            ensure!(fingerprint(&key, &text) == doc.hash, BuildConflict);
            let mut fragments = engine.selection.policy.split(&text).into_iter().peekable();
            while let Some(fragment) = fragments.next() {
                batch.push((idx, fragment));
                finished.push(fragments.peek().is_none());
                if batch.len() == BATCH {
                    flush(conn, &mut batch, &mut means, &mut finished)?;
                }
            }
        }
        if !batch.is_empty() {
            flush(conn, &mut batch, &mut means, &mut finished)?;
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
    identity.embedding_model = Some(key.clone());
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
    let report = conn.immediate_transaction::<_, anyhow::Error, _>(|db| {
        engine.check_current(db)?;
        ensure!(build_generation(db, rebuild)? == current, BuildConflict);
        ensure!(library_revision(db)? == revision, BuildConflict);
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
        for doc in &docs {
            if !computed.contains(&doc.id) {
                continue;
            }
            let bytes = vector_bytes(&vectors[&doc.id]);
            diesel::replace_into(embeddings::table)
                .values((
                    embeddings::owner.eq(&owner),
                    embeddings::id.eq(doc.id),
                    embeddings::fingerprint.eq(&doc.hash),
                    embeddings::vector.eq(Vec::<u8>::new()),
                    embeddings::checksum.eq(Payloads::reference_bytes(&bytes)),
                ))
                .execute(db)?;
        }
        diesel::sql_query("DELETE FROM embedding_parts WHERE owner = ? AND NOT EXISTS (
            SELECT 1 FROM repositories r WHERE r.owner_id = embedding_parts.owner AND r.id = embedding_parts.id AND r.active = 1)")
            .bind::<diesel::sql_types::Text, _>(&owner).execute(db)?;
        store::set_meta(db, "index_generation", &next.to_string())?;
        store::set_meta(db, "index_model", engine.selection.provider.model())?;
        store::set_meta(db, "index_backend", engine.selection.provider.name())?;
        store::set_meta(db, "index_identity", &key)?;
        store::set_meta(
            db,
            "index_chunk_policy",
            &engine.selection.policy.identity(),
        )?;
        if vectors.is_empty() {
            store::delete_meta(db, "index_dimension")?;
        } else {
            store::set_meta(db, "index_dimension", &dimension.to_string())?;
        }
        store::set_meta(db, "index_published_at", &Utc::now().to_rfc3339())?;
        finish_index(db, &owner, &revision, rebuild, tier)?;
        Ok(format!(
            "索引已更新，共 {} 个仓库，新计算 {} 个，复用 {} 个",
            docs.len(),
            missing.len(),
            docs.len() - missing.len()
        ))
    })?;
    collect_payloads(conn, &owner, &mut payloads);
    exact::prime(
        &artifact_path(dir, &owner, next),
        &owner,
        next,
        &key,
        &vectors,
        &mut payloads,
    );
    Ok(report)
}

fn finish_index(db: &mut Db, owner: &str, revision: &str, rebuild: bool, tier: Tier) -> Result<()> {
    use diesel::sql_types::Text;
    diesel::sql_query(
        "DELETE FROM index_source_versions WHERE owner=? AND NOT EXISTS (
        SELECT 1 FROM repositories r WHERE r.owner_id=index_source_versions.owner
        AND r.id=index_source_versions.id AND r.active=1)",
    )
    .bind::<Text, _>(owner)
    .execute(db)?;
    diesel::sql_query(
        "INSERT INTO index_source_versions(owner,id,version)
        SELECT owner_id,id,source_version FROM repositories WHERE owner_id = ? AND active=1
        ON CONFLICT(owner,id) DO UPDATE SET version=excluded.version WHERE version<>excluded.version",
    )
    .bind::<Text, _>(owner)
    .execute(db)?;
    store::set_meta(db, "index_source_versions", "1")?;
    store::set_meta(db, "indexed_library_revision", revision)?;
    clear_checkpoints(db, owner)?;
    if rebuild {
        store::delete_meta(db, "index_rebuild")?;
    }
    if matches!(tier, Tier::Metadata(_)) {
        store::set_meta(db, "index_body_pending", "1")?;
        store::set_meta(db, "index_pending", "1")?;
    } else if store::meta(db, "index_rebuild")?.is_none() {
        store::delete_meta(db, "index_body_pending")?;
        store::delete_meta(db, "index_pending")?;
        store::delete_meta(db, "index_issue")?;
    }
    Ok(())
}

fn embed_fragments(
    db: &mut Db,
    engine: &Engine,
    batch: &[(usize, String)],
    means: &mut [mean::Mean],
    observed: &mut dyn FnMut(EmbeddingWork),
) -> Result<()> {
    let texts: Vec<_> = batch.iter().map(|(_, text)| text.as_str()).collect();
    let vectors = engine.embed(db, &texts, observed)?;
    ensure!(vectors.len() == texts.len(), "嵌入数量与文本分块数量不一致");
    for ((idx, _), vector) in batch.iter().zip(vectors) {
        means[*idx].push(&vector)?;
    }
    Ok(())
}

pub(crate) fn inferred_filter(query: &str, explicit: &Filter, today: NaiveDate) -> Filter {
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

#[cfg(test)]
fn search(conn: &mut Db, query: &str, filter: &Filter, limit: usize) -> Result<Vec<Repo>> {
    let dir = store::data_dir(conn)?;
    let engine = Engine::configured(conn, Duration::from_secs(180))?;
    search_with(conn, &dir, query, filter, limit, &engine)
}

pub(crate) fn hybrid_search(
    conn: &mut Db,
    query: &str,
    filter: &Filter,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<Repo>> {
    let dir = store::data_dir(conn)?;
    let engine = Engine::configured(conn, timeout)?;
    let depth = limit.max(50);
    search_ranked(
        conn,
        &dir,
        query,
        filter,
        depth,
        &engine,
        |db, semantic, filter| {
            let keywords = store::keyword_search(db, query, filter, depth)?;
            Ok(crate::library::fuse(semantic, keywords, limit))
        },
    )
}

pub(crate) fn has_published_generation(db: &mut Db) -> Result<bool> {
    Ok(generation(db)? > 0)
}

#[cfg(test)]
fn search_with(
    conn: &mut Db,
    dir: &Path,
    query: &str,
    filter: &Filter,
    limit: usize,
    engine: &Engine,
) -> Result<Vec<Repo>> {
    search_ranked(conn, dir, query, filter, limit, engine, |_, rows, _| {
        Ok(rows)
    })
}

fn search_ranked(
    conn: &mut Db,
    dir: &Path,
    query: &str,
    filter: &Filter,
    limit: usize,
    engine: &Engine,
    finish: impl FnOnce(&mut Db, Vec<Repo>, &Filter) -> Result<Vec<Repo>>,
) -> Result<Vec<Repo>> {
    let query = query.trim();
    ensure!(!query.is_empty(), "请输入检索词");
    if limit == 0 {
        return Ok(Vec::new());
    }
    let filter = inferred_filter(query, filter, Utc::now().date_naive());
    filter.validate()?;
    let key = engine.selection.identity();
    let owner = store::owner(conn)?;
    let snapshot = conn.transaction::<_, anyhow::Error, _>(|db| {
        engine.check_current(db)?;
        let allowed = allowed_ids(db, &owner, &filter)?;
        if allowed.is_empty() {
            return Ok(None);
        }
        let current = generation(db)?;
        check_model(db, &engine.selection, false)?;
        ensure!(current > 0, "尚未建立语义索引，请运行 starstack index");
        let search = exact::Search::open(dir, &owner, current, &key, &allowed)?;
        let revision = library_revision(db)?;
        if store::meta(db, "index_source_versions")?.is_some() {
            check_source_versions(db, &owner, &allowed)?;
        } else if revision == "0"
            || store::meta(db, "indexed_library_revision")?.as_deref() != Some(&revision)
        {
            check_fingerprints(db, &owner, &engine.selection, &key, &allowed)?;
        }
        let lists_revision = if filter.list.is_some() {
            store::meta(db, "lists_revision")?
        } else {
            None
        };
        Ok(Some((allowed, search, current, revision, lists_revision)))
    })?;
    let Some((allowed, search, current, revision, lists_revision)) = snapshot else {
        return Ok(Vec::new());
    };

    let instruct = match &engine.selection.provider {
        Provider::Ollama { model, .. } => model.starts_with("qwen3-embedding"),
        Provider::Builtin { family, .. } => *family == "qwen3-gguf",
    };
    let prompt = if instruct {
        format!(
            "Instruct: Given a search request, retrieve GitHub repositories that match the request.\nQuery: {query}"
        )
    } else {
        query.to_owned()
    };
    let vector = engine
        .embed(conn, &[&prompt], &mut |_| {})?
        .pop()
        .context("missing query embedding")?;
    conn.transaction(|db| {
        engine.check_current(db)?;
        ensure!(
            generation(db)? == current
                && library_revision(db)? == revision
                && (filter.list.is_none() || store::meta(db, "lists_revision")? == lists_revision),
            "查询期间收藏资料已更新，请重试查询"
        );
        let ranked = search.rank(db, dir, &allowed, &vector, limit)?;
        let semantic = ranked
            .into_iter()
            .map(|(id, score)| {
                let mut repo = store::by_id(db, id)?;
                repo.score = Some(score);
                Ok(repo)
            })
            .collect::<Result<_>>()?;
        finish(db, semantic, &filter)
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

fn check_source_versions(db: &mut Db, owner: &str, allowed: &[u64]) -> Result<()> {
    use diesel::sql_types::{BigInt, Text};
    #[derive(QueryableByName)]
    struct Changed {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    let changes = diesel::sql_query(
        "SELECT r.id AS id FROM repositories r
         LEFT JOIN index_source_versions v ON v.owner=r.owner_id AND v.id=r.id
         WHERE r.owner_id=? AND r.active=1 AND (v.version IS NULL OR v.version<>r.source_version)",
    )
    .bind::<Text, _>(owner)
    .load_iter::<Changed, _>(db)?;
    for row in changes {
        if allowed.binary_search(&u64::try_from(row?.id)?).is_ok() {
            return Err(Stale.into());
        }
    }
    Ok(())
}

fn check_fingerprints(
    db: &mut Db,
    owner: &str,
    selection: &Selection,
    model: &str,
    allowed: &[u64],
) -> Result<()> {
    let hashes = embeddings::table
        .filter(embeddings::owner.eq(owner))
        .select((embeddings::id, embeddings::fingerprint))
        .load::<(i64, Vec<u8>)>(db)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    for &id in allowed {
        let id = i64::try_from(id)?;
        let repo = store::by_id(db, id)?;
        if hashes.get(&id) != Some(&document_hash(selection, model, &repo, Tier::Full)) {
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
