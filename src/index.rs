//! Built-in Candle and Ollama embeddings with an immutable, filtered HawDB cosine projection.
pub(crate) mod mean;

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

#[cfg(test)]
const DEFAULT_MODEL: &str = "qwen3-embedding:0.6b";
const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const BATCH: usize = 16;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS embeddings (
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
        store::set_meta(db, "embedding_backend", "builtin")
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
    Ok(Provider::selected(db)?.model().to_owned())
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

    fn identity(&self) -> String {
        let base = match &self.provider {
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
                    "masked-mean-128-stride24-v2"
                };
                format!("builtin:{id}:{fingerprint}:{protocol}")
            }
            // Old indexes only recorded the model name for the default Ollama endpoint.
            Provider::Ollama { url, model } if url == DEFAULT_OLLAMA_URL => model.clone(),
            Provider::Ollama { url, model } => {
                format!("ollama:{model}@{:x}", Sha256::digest(url.as_bytes()))
            }
        };
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

enum Runtime {
    Ollama(Client),
    Builtin {
        home: PathBuf,
        files: OnceCell<crate::models::ModelFiles>,
    },
}

struct Engine {
    selection: Selection,
    runtime: Runtime,
    managed: bool,
}

#[derive(Deserialize)]
struct Embeddings {
    model: Option<String>,
    embeddings: Vec<Vec<f32>>,
}

impl Engine {
    fn configured(db: &mut Db) -> Result<Self> {
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
                    .timeout(Duration::from_secs(180))
                    .build()?,
            ),
        };
        Ok(Self {
            selection,
            runtime,
            managed: true,
        })
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
            ensure!(
                Selection::read(db)? == self.selection,
                "嵌入后端、服务地址、模型或分块配置已变化，请重新执行"
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
            runtime: Runtime::Ollama(Client::builder().no_proxy().build()?),
            managed: false,
        })
    }

    fn embed(&self, db: &mut Db, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
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
                )
            }
            (Runtime::Ollama(client), Provider::Ollama { url, model }) => {
                ollama_embed(client, url, model, texts)
            }
            _ => anyhow::bail!("嵌入配置不一致"),
        }
    }
}

fn ollama_embed(client: &Client, url: &str, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
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
    Ok(vectors)
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

fn documents(conn: &mut Db, model: &str) -> Result<Vec<Document>> {
    let mut docs = Vec::new();
    store::visit_repos(conn, |repo| {
        ensure!(repo.id >= 0, "invalid repository id {}", repo.id);
        let text = document(&repo);
        docs.push(Document {
            id: repo.id,
            hash: fingerprint(model, &text),
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
    let engine = Engine::configured(conn)?;
    engine.acquire()?;
    let dir = store::data_dir(conn)?;
    build_with(conn, &dir, rebuild, &engine, progress)
}

fn build_with(
    conn: &mut Db,
    dir: &Path,
    rebuild: bool,
    engine: &Engine,
    progress: &dyn Fn(&str),
) -> Result<String> {
    let key = engine.selection.identity();
    let owner = store::owner(conn)?;
    let (current, revision, docs, mut vectors, existing_count) = conn
        .transaction::<_, anyhow::Error, _>(|db| {
            engine.check_current(db)?;
            let current = build_generation(db, rebuild)?;
            check_model(db, &engine.selection, rebuild)?;
            let docs = documents(db, &key)?;
            let vectors = if rebuild || current == 0 {
                BTreeMap::new()
            } else {
                cached_vectors(db, &owner, &docs)
                    .context("无法读取嵌入缓存，请运行 starstack index --rebuild")?
            };
            let existing_count =
                open_projection(&artifact_path(dir, &owner, current), current, &owner, &key)
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
            engine.check_current(db)?;
            ensure!(
                build_generation(db, rebuild)? == current && library_revision(db)? == revision,
                "收藏资料已更新，请重试 starstack index"
            );
            store::set_meta(db, "indexed_library_revision", &revision)
        })?;
        return Ok(format!("索引已是最新，共 {} 个仓库", docs.len()));
    }

    for (i, group) in missing.chunks(BATCH).enumerate() {
        progress(&format!(
            "正在为仓库计算向量（第 {}–{} 个，共 {} 个）",
            i * BATCH + 1,
            i * BATCH + group.len(),
            missing.len()
        ));
        // Read README bodies for the changed IDs a group at a time, outside the
        // transaction during inference. Recheck fingerprints and revision before publishing.
        let mut means: Vec<_> = (0..group.len()).map(|_| mean::Mean::default()).collect();
        let mut batch = Vec::with_capacity(BATCH);
        for (idx, doc) in group.iter().enumerate() {
            let text = document(&store::by_id(conn, doc.id)?);
            ensure!(
                fingerprint(&key, &text) == doc.hash,
                "建索引期间收藏资料已更新，请重试 starstack index"
            );
            for fragment in engine.selection.policy.split(&text) {
                batch.push((idx, fragment));
                if batch.len() == BATCH {
                    embed_fragments(conn, engine, &batch, &mut means)?;
                    batch.clear();
                }
            }
        }
        if !batch.is_empty() {
            embed_fragments(conn, engine, &batch, &mut means)?;
        }
        for (doc, mean) in group.iter().zip(means) {
            vectors.insert(doc.id, mean.finish()?);
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
    conn.immediate_transaction(|db| {
        engine.check_current(db)?;
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
        store::set_meta(db, "indexed_library_revision", &revision)?;
        Ok(format!(
            "索引已更新，共 {} 个仓库，新计算 {} 个，复用 {} 个",
            docs.len(),
            missing.len(),
            docs.len() - missing.len()
        ))
    })
}

fn embed_fragments(
    db: &mut Db,
    engine: &Engine,
    batch: &[(usize, String)],
    means: &mut [mean::Mean],
) -> Result<()> {
    let texts: Vec<_> = batch.iter().map(|(_, text)| text.as_str()).collect();
    let vectors = engine.embed(db, &texts)?;
    ensure!(vectors.len() == texts.len(), "嵌入数量与文本分块数量不一致");
    for ((idx, _), vector) in batch.iter().zip(vectors) {
        means[*idx].push(&vector)?;
    }
    Ok(())
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
    let engine = Engine::configured(conn)?;
    search_with(conn, &dir, query, filter, limit, &engine)
}

fn search_with(
    conn: &mut Db,
    dir: &Path,
    query: &str,
    filter: &Filter,
    limit: usize,
    engine: &Engine,
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
        let projection = open_projection(
            &artifact_path(dir, &owner, current),
            current,
            &owner,
            &key,
        )
        .context(
            "无法打开语义索引，请运行 starstack index 修复，或运行 starstack index --rebuild 重建",
        )?;
        let revision = library_revision(db)?;
        if revision == "0"
            || store::meta(db, "indexed_library_revision")?.as_deref() != Some(&revision)
        {
            check_fingerprints(db, &owner, &key, &allowed)?;
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
        .embed(conn, &[&prompt])?
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
        engine.check_current(db)?;
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
