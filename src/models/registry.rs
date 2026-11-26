//! Catalog of pinned snapshots and content-addressed imports.
use super::{Location, bytes, directory, download};
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::path::Path;

pub(crate) const DEFAULT_ID: &str = "multilingual-minilm-l12-v2";
pub(crate) const QWEN_ID: &str = "qwen3-embedding-0.6b-q4-k-m";
pub(super) const HF_REVISION: &str = "e8f8c211226b894fcb81acc59f3b34ba3efd5f42";
pub(super) const MS_REVISION: &str = "5f5b1cee6aeb8bd07d715a425dbe588f6c29d7df";
pub(super) const QWEN_FILENAME: &str = "Qwen3-Embedding-0.6B-q4_k_m.gguf";
pub(super) const IMPORT_NAMES: [&str; 4] = [
    "config.json",
    "tokenizer.json",
    "model.safetensors",
    "pooling.json",
];

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Artifact {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}
impl Artifact {
    pub fn new(name: &str, bytes: u64, sha256: &str) -> Self {
        Self {
            name: name.to_owned(),
            bytes,
            sha256: sha256.to_owned(),
        }
    }
}
#[derive(Serialize, Deserialize)]
pub(super) struct Package {
    pub id: String,
    pub name: String,
    pub repository: String,
    pub revision: String,
    pub modelscope_revision: String,
    pub dimension: u32,
    pub files: Vec<Artifact>,
    pub origin: String,
}

pub(super) fn builtin(id: &str) -> Option<Package> {
    let (name, repo, hf, ms, dimension, files) = match id {
        DEFAULT_ID => (
            "Multilingual MiniLM-L12-v2",
            "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2",
            HF_REVISION,
            MS_REVISION,
            384,
            vec![
                Artifact::new(
                    "config.json",
                    645,
                    "6300193cb75e01cf80c96decef7187dfb33094d97cc1490b7ead6ff134476e4e",
                ),
                Artifact::new(
                    "tokenizer.json",
                    9081518,
                    "2c3387be76557bd40970cec13153b3bbf80407865484b209e655e5e4729076b8",
                ),
                Artifact::new(
                    "model.safetensors",
                    470641600,
                    "eaa086f0ffee582aeb45b36e34cdd1fe2d6de2bef61f8a559a1bbc9bd955917b",
                ),
            ],
        ),
        QWEN_ID => (
            "Qwen3-Embedding-0.6B Q4_K_M",
            "Mungert/Qwen3-Embedding-0.6B-GGUF",
            "48ccc6bc9fd9edd966b973ea8b588f59c1f1904a",
            "0f751794ee0d67dbfb2e3d875026af707599b741",
            1024,
            vec![
                Artifact::new(
                    "tokenizer.json",
                    11423705,
                    "def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a",
                ),
                Artifact::new(
                    "model.gguf",
                    394704832,
                    "c608745221a03d45ee7328aab5ae180ef5db54c9a47eda43ef05f73156ba824b",
                ),
            ],
        ),
        _ => return None,
    };
    Some(Package {
        id: id.into(),
        name: name.into(),
        repository: repo.into(),
        revision: hf.into(),
        modelscope_revision: ms.into(),
        dimension,
        files,
        origin: "catalog".into(),
    })
}

pub(super) fn file_url(p: &Package, source: &str, f: &Artifact) -> Result<Url> {
    let base = download::endpoint(source)?;
    let ms = source == "modelscope";
    let prefix = if ms { "models/" } else { "" };
    let tokenizer = p.id == QWEN_ID && f.name == "tokenizer.json";
    let repo = if tokenizer {
        "Qwen/Qwen3-Embedding-0.6B"
    } else {
        &p.repository
    };
    let rev = match (tokenizer, ms) {
        (true, true) => "dd3e37eb506b3addbd930037796e25d349ae9010",
        (true, false) => "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3",
        (false, true) => &p.modelscope_revision,
        (false, false) => &p.revision,
    };
    let filename = if p.id == QWEN_ID && f.name == "model.gguf" {
        QWEN_FILENAME
    } else {
        &f.name
    };
    Ok(base.join(&format!("{prefix}{repo}/resolve/{rev}/{filename}"))?)
}
pub(super) fn fingerprint(files: &[Artifact]) -> String {
    let mut h = Sha256::new();
    for f in files {
        h.update(f.name.as_bytes());
        h.update([0]);
        h.update(f.bytes.to_be_bytes());
        h.update(f.sha256.as_bytes());
    }
    format!("{:x}", h.finalize())
}
pub(super) fn custom_id(id: &str) -> bool {
    id.strip_prefix("bert-").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub(crate) fn known_id(id: &str) -> bool {
    matches!(id, DEFAULT_ID | QWEN_ID) || custom_id(id)
}
pub(super) fn load(loc: &Location) -> Result<Option<Package>> {
    if let Some(p) = builtin(&loc.id) {
        return Ok(Some(p));
    }
    if !directory(&loc.ready)? {
        return Ok(None);
    }
    let manifest = loc.ready.join("registry.json");
    ensure!(
        bytes(&manifest)?.is_some_and(|n| n <= 32768),
        "导入模型缺少注册信息"
    );
    let p: Package = serde_json::from_reader(File::open(manifest)?)?;
    ensure!(
        p.id == loc.id
            && p.dimension == 384
            && p.files.iter().map(|f| f.name.as_str()).eq(IMPORT_NAMES)
            && p.id == format!("bert-{}", fingerprint(&p.files)),
        "模型注册信息与文件身份不符"
    );
    Ok(Some(p))
}
pub(crate) struct Descriptor {
    pub family: &'static str,
    pub fingerprint: String,
}
pub(crate) fn descriptor(home: &Path, id: &str) -> Result<Descriptor> {
    let loc = Location::new(home, id, false)?;
    let p = load(&loc)?.context("自定义模型尚未导入")?;
    Ok(Descriptor {
        family: if id == QWEN_ID {
            "qwen3-gguf"
        } else {
            "minilm"
        },
        fingerprint: fingerprint(&p.files),
    })
}
