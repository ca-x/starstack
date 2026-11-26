//! Offline installation and verified, content-addressed BERT imports.
use super::{Artifact, Location, ModelStatus, Package, bytes, directory, download, registry};
use crate::store::{self, Db};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(super) const LIMITS: [u64; 4] = [65536, 32 * 1024 * 1024, 512 * 1024 * 1024, 65536];

fn copy_bounded(src: &Path, dst: &Path, max: u64) -> Result<u64> {
    let n = bytes(src)?.context("模型文件缺失")?;
    ensure!(
        n > 0 && n <= max,
        "模型文件大小超出限制：{}",
        src.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut out = File::create_new(dst)?;
    let written = std::io::copy(&mut File::open(src)?.take(max + 1), &mut out)?;
    ensure!(written == n, "复制期间模型文件大小发生变化");
    out.sync_all()?;
    Ok(n)
}
fn incoming(dir: &Path, name: &str) -> Result<PathBuf> {
    if name == "pooling.json" && directory(&dir.join("1_Pooling"))? {
        return Ok(dir.join("1_Pooling/config.json"));
    }
    if name == "model.gguf" && bytes(&dir.join(name))?.is_none() {
        return Ok(dir.join(registry::QWEN_FILENAME));
    }
    Ok(dir.join(name))
}
fn already_installed(loc: &Location, p: &Package) -> Result<bool> {
    if !directory(&loc.ready)? {
        return Ok(false);
    }
    for f in &p.files {
        if !download::valid_file(&loc.ready.join(&f.name), f)? {
            return Ok(false);
        }
    }
    Ok(true)
}
fn publish(dir: &Path, loc: &Location) -> Result<()> {
    if directory(&loc.ready)? {
        crate::native_embedding::evict(&loc.ready);
        fs::remove_dir_all(&loc.ready)?;
    }
    fs::rename(dir, &loc.ready)?;
    if directory(&loc.stage)? {
        fs::remove_dir_all(&loc.stage)?;
    }
    Ok(())
}
pub(crate) fn import_files(
    db: &mut Db,
    id: &str,
    dir: &Path,
    progress: &dyn Fn(&str),
) -> Result<ModelStatus> {
    let p = registry::builtin(id).context("只能按预设导入目录中的模型")?;
    let loc = Location::new(&store::data_dir(db)?, id, true)?;
    let _guard = loc.lock_exclusive()?;
    let stage = tempfile::tempdir_in(&loc.base)?;
    ensure!(directory(dir)?, "模型文件目录不存在");
    for f in &p.files {
        progress(&format!("校验 {}", f.name));
        let path = stage.path().join(&f.name);
        copy_bounded(&incoming(dir, &f.name)?, &path, f.bytes)?;
        ensure!(
            download::valid_file(&path, f)?,
            "{} 与预设的 SHA-256 或大小不符",
            f.name
        );
    }
    if !already_installed(&loc, &p)? {
        publish(stage.path(), &loc)?;
    }
    loc.record("upload", None)?;
    loc.status(&p, false)
}
pub(crate) fn import_custom(
    db: &mut Db,
    dir: &Path,
    progress: &dyn Fn(&str),
) -> Result<ModelStatus> {
    register(db, dir, "upload", "", "", progress)
}
pub(super) fn register(
    db: &mut Db,
    dir: &Path,
    origin: &str,
    repo: &str,
    revision: &str,
    progress: &dyn Fn(&str),
) -> Result<ModelStatus> {
    let home = store::data_dir(db)?;
    ensure!(directory(dir)?, "模型文件目录不存在");
    let stage = tempfile::tempdir_in(&home)?;
    let mut files = Vec::new();
    for (name, limit) in registry::IMPORT_NAMES.into_iter().zip(LIMITS) {
        progress(&format!("校验 {name}"));
        let dst = stage.path().join(name);
        let n = copy_bounded(&incoming(dir, name)?, &dst, limit)?;
        files.push(Artifact::new(name, n, &download::digest(&dst)?));
    }
    let pooling: serde_json::Value =
        serde_json::from_reader(File::open(stage.path().join("pooling.json"))?)?;
    let modes = pooling
        .as_object()
        .context("缺少 Sentence Transformers 池化配置")?;
    ensure!(
        pooling["word_embedding_dimension"] == 384
            && pooling["pooling_mode_mean_tokens"] == true
            && modes
                .iter()
                .filter(|(k, _)| k.starts_with("pooling_mode_")
                    && k.as_str() != "pooling_mode_mean_tokens")
                .all(|(_, v)| v.as_bool() == Some(false)),
        "仅支持 384 维 BERT 均值池化嵌入模型"
    );
    crate::native_embedding::validate_custom_bert(stage.path())?;
    let hash = registry::fingerprint(&files);
    let id = format!("bert-{hash}");
    let loc = Location::new(&home, &id, true)?;
    let _guard = loc.lock_exclusive()?;
    if let Some(existing) = registry::load(&loc)?
        && already_installed(&loc, &existing)?
    {
        return loc.status(&existing, false);
    }
    let p = Package {
        id,
        name: if repo.is_empty() {
            format!("BERT 384 · {}", &hash[..12])
        } else {
            repo.to_owned()
        },
        repository: repo.to_owned(),
        revision: revision.to_owned(),
        modelscope_revision: if origin == "modelscope" {
            revision.to_owned()
        } else {
            String::new()
        },
        dimension: 384,
        files,
        origin: origin.to_owned(),
    };
    let mut manifest = File::create_new(stage.path().join("registry.json"))?;
    serde_json::to_writer(&mut manifest, &p)?;
    manifest.flush()?;
    manifest.sync_all()?;
    drop(manifest);
    publish(stage.path(), &loc)?;
    loc.record(origin, None)?;
    loc.status(&p, false)
}
