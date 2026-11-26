//! Instance-wide, revision-pinned CPU embedding models.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use crate::store::{self, Db};

mod download;
pub(crate) use download::endpoint as source_endpoint;
#[cfg(test)]
mod tests;

mod hub;
mod imports;
mod registry;
pub(crate) use hub::download_custom;
pub(crate) use imports::{import_custom, import_files};
use registry::{Artifact, Package};
pub(crate) use registry::{DEFAULT_ID, Descriptor, QWEN_ID, descriptor, known_id};

#[derive(Debug, Serialize)]
pub(crate) struct ModelStatus {
    pub id: String,
    pub name: String,
    pub repository: String,
    pub revision: String,
    pub modelscope_revision: String,
    pub dimension: u32,
    pub expected_bytes: u64,
    pub downloaded_bytes: u64,
    pub installed: bool,
    pub state: &'static str,
    pub sources: Vec<&'static str>,
    pub files: Vec<String>,
    pub source: Option<String>,
    pub error: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct LastAttempt {
    source: String,
    error: Option<String>,
}

fn directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_dir(),
                "模型目录包含非目录或符号链接：{}",
                path.display()
            );
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn bytes(path: &Path) -> Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "模型路径包含非普通文件：{}", path.display());
            Ok(Some(meta.len()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

struct Location {
    id: String,
    base: PathBuf,
    ready: PathBuf,
    stage: PathBuf,
}

impl Location {
    fn new(home: &Path, id: &str, create: bool) -> Result<Self> {
        ensure!(known_id(id), "未知的检索模型 {id}");
        let rev = registry::builtin(id)
            .map(|p| p.revision)
            .unwrap_or_else(|| id.to_owned());
        let root = home.join("models");
        let base = root.join(id);
        for dir in [&root, &base] {
            if !directory(dir)? && create {
                if let Err(error) = fs::create_dir(dir)
                    && error.kind() != ErrorKind::AlreadyExists
                {
                    return Err(error.into());
                }
                ensure!(directory(dir)?, "无法创建模型目录");
            }
        }
        Ok(Self {
            id: id.to_owned(),
            ready: base.join(&rev),
            stage: base.join(format!(".{rev}.partial")),
            base,
        })
    }

    // The lock's inode remains at this path across installs and removals.
    fn lockfile(&self) -> Result<File> {
        let path = self.base.join(".lock");
        bytes(&path)?;
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?)
    }

    fn lock_exclusive(&self) -> Result<File> {
        let lock = self.lockfile()?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(TryLockError::WouldBlock) => bail!("模型正在使用或下载中，请在当前操作完成后重试"),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    fn record(&self, source: &str, error: Option<String>) -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&self.base)?;
        serde_json::to_writer(
            &mut tmp,
            &LastAttempt {
                source: source.to_owned(),
                error,
            },
        )?;
        tmp.persist(self.base.join("download.json"))?;
        Ok(())
    }

    fn status(&self, pkg: &Package, busy: bool) -> Result<ModelStatus> {
        let ready = directory(&self.ready)?
            && pkg.files.iter().try_fold(true, |ok, a| -> Result<bool> {
                Ok(ok && bytes(&self.ready.join(&a.name))? == Some(a.bytes))
            })?;
        let size = pkg.files.iter().map(|f| f.bytes).sum();
        let downloaded = if ready {
            size
        } else if directory(&self.stage)? {
            pkg.files.iter().try_fold(0, |sum, a| -> Result<u64> {
                Ok(sum + bytes(&self.stage.join(&a.name))?.unwrap_or(0).min(a.bytes))
            })?
        } else {
            0
        };
        let record = self.base.join("download.json");
        bytes(&record)?;
        let last = match File::open(record) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(e) if e.kind() == ErrorKind::NotFound => LastAttempt::default(),
            Err(e) => return Err(e.into()),
        };
        let state = if busy {
            "downloading"
        } else if ready {
            "ready"
        } else if last.error.is_some() {
            "error"
        } else if downloaded > 0 {
            "partial"
        } else {
            "missing"
        };
        Ok(ModelStatus {
            id: pkg.id.clone(),
            name: pkg.name.clone(),
            repository: pkg.repository.clone(),
            revision: pkg.revision.clone(),
            modelscope_revision: pkg.modelscope_revision.clone(),
            dimension: pkg.dimension,
            expected_bytes: size,
            downloaded_bytes: downloaded,
            installed: ready,
            state,
            sources: if registry::builtin(&self.id).is_some() {
                vec!["modelscope", "huggingface"]
            } else {
                vec![]
            },
            files: pkg
                .files
                .iter()
                .map(|f| {
                    if pkg.id == QWEN_ID && f.name == "model.gguf" {
                        registry::QWEN_FILENAME.to_owned()
                    } else {
                        f.name.clone()
                    }
                })
                .collect(),
            source: (!last.source.is_empty()).then_some(last.source),
            error: last.error,
        })
    }
}

pub(crate) fn list(home: &Path) -> Result<Vec<ModelStatus>> {
    let mut ids = vec![DEFAULT_ID.to_owned(), QWEN_ID.to_owned()];
    let root = home.join("models");
    if directory(&root)? {
        for ent in fs::read_dir(root)? {
            let ent = ent?;
            let Some(id) = ent.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if registry::custom_id(&id) {
                ids.push(id);
            }
        }
    }
    ids[2..].sort();
    let mut rows = Vec::new();
    for id in ids {
        let loc = Location::new(home, &id, false)?;
        if let Some(item) = status(&loc)? {
            rows.push(item);
        }
    }
    Ok(rows)
}
fn status(loc: &Location) -> Result<Option<ModelStatus>> {
    let lock_path = loc.base.join(".lock");
    let guard = if bytes(&lock_path)?.is_some() {
        Some(File::open(lock_path)?)
    } else {
        None
    };
    let busy = match guard.as_ref().map(File::try_lock_shared) {
        Some(Err(TryLockError::WouldBlock)) => true,
        Some(Err(TryLockError::Error(err))) => return Err(err.into()),
        _ => false,
    };
    let Some(pkg) = registry::load(loc)? else {
        return Ok(None);
    };
    Ok(Some(loc.status(&pkg, busy)?))
}

pub(crate) fn installed_path(home: &Path, id: &str) -> Result<PathBuf> {
    let loc = Location::new(home, id, false)?;
    ensure!(directory(&loc.ready)?, "请先下载内置模型 {id}");
    let pkg = registry::load(&loc)?.context("模型未安装")?;
    for file in &pkg.files {
        ensure!(
            bytes(&loc.ready.join(&file.name))? == Some(file.bytes),
            "模型文件 {} 不完整，请重新下载 {id}",
            file.name
        );
    }
    Ok(loc.ready)
}

pub(crate) struct ModelFiles {
    path: PathBuf,
    _lock: File,
}
impl ModelFiles {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn open_installed(home: &Path, id: &str) -> Result<ModelFiles> {
    let loc = Location::new(home, id, false)?;
    ensure!(directory(&loc.base)?, "请先下载内置模型 {id}");
    let lock = loc.lockfile()?;
    lock.try_lock_shared()
        .context("模型正在下载或移除，请稍后再试")?;
    Ok(ModelFiles {
        path: installed_path(home, id)?,
        _lock: lock,
    })
}

pub(crate) fn install(
    db: &mut Db,
    id: &str,
    source: &str,
    progress: &dyn Fn(&str),
) -> Result<ModelStatus> {
    ensure!(
        matches!(source, "modelscope" | "huggingface"),
        "未知下载源，请选择 modelscope 或 huggingface"
    );
    let pkg = registry::builtin(id).context("请选择目录中的预设模型")?;
    let loc = Location::new(&store::data_dir(db)?, id, true)?;
    let _guard = loc.lock_exclusive()?;
    loc.record(source, None)?;
    let result = (|| {
        let client = download::client(db)?;
        download::install_files(
            &client,
            &|f| registry::file_url(&pkg, source, f),
            &loc,
            &pkg.files,
            progress,
        )
    })();
    if let Err(error) = result {
        let message = format!("{error:#}。可重试或切换模型源，已下载的数据会保留");
        loc.record(source, Some(message.clone()))?;
        bail!("{message}");
    }
    loc.status(&pkg, false)
}

pub(crate) fn remove(db: &mut Db, id: &str) -> Result<ModelStatus> {
    let home = store::data_dir(db)?;
    let loc = Location::new(&home, id, false)?;
    let pkg = registry::load(&loc)?.context("模型未安装")?;
    if !directory(&loc.base)? {
        return loc.status(&pkg, false);
    }
    let _guard = loc.lock_exclusive()?;
    crate::index::ensure_model_unused(db, id)?;
    crate::native_embedding::evict(&loc.ready);
    let removing = loc.base.join(".removing");
    if directory(&removing)? {
        fs::remove_dir_all(&removing)?;
    }
    if directory(&loc.ready)? {
        fs::rename(&loc.ready, &removing)?;
    }
    for path in [&removing, &loc.stage] {
        if directory(path)? {
            fs::remove_dir_all(path)?;
        }
    }
    let record = loc.base.join("download.json");
    if bytes(&record)?.is_some() {
        fs::remove_file(record)?;
    }
    loc.status(&pkg, false)
}
