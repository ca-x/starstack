//! Both sources serve identical pinned bytes. Check size and digest independently of response metadata.
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{StatusCode, Url, blocking::Client, header};
use sha2::{Digest, Sha256};

use super::{Artifact, Location, bytes, directory};
use crate::store::Db;

pub(super) fn client(db: &mut Db) -> Result<Client> {
    Ok(crate::proxy::instance_builder(db)?
        .user_agent("StarStack embedding model downloader")
        .no_gzip()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .build()?)
}
pub(crate) fn endpoint(name: &str) -> Result<Url> {
    let (env, default) = match name {
        "modelscope" => ("STARSTACK_MODELSCOPE_ENDPOINT", "https://modelscope.cn"),
        "huggingface" => ("STARSTACK_HF_ENDPOINT", "https://huggingface.co"),
        _ => bail!("未知下载源，请选择 modelscope 或 huggingface"),
    };
    let base = std::env::var(env).unwrap_or_else(|_| default.to_owned());
    let mut url = Url::parse(&base).with_context(|| format!("{env} 地址无效"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.query().is_none(),
        "{env} 需要有效的 HTTP(S) 服务根地址"
    );
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

pub(super) fn valid_file(path: &Path, artifact: &Artifact) -> Result<bool> {
    if bytes(path)? != Some(artifact.bytes) {
        return Ok(false);
    }
    Ok(digest(path)? == artifact.sha256)
}
pub(super) fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn install_files(
    client: &Client,
    url: &dyn Fn(&Artifact) -> Result<Url>,
    loc: &Location,
    files: &[Artifact],
    progress: &dyn Fn(&str),
) -> Result<()> {
    if directory(&loc.ready)? {
        let mut intact = true;
        for f in files {
            if !valid_file(&loc.ready.join(&f.name), f)? {
                intact = false;
                break;
            }
        }
        if intact {
            return Ok(());
        }
        // Under the write lock a damaged installation cannot be mistaken for a valid snapshot.
        fs::remove_dir_all(&loc.ready)?;
    }
    if !directory(&loc.stage)? {
        fs::create_dir(&loc.stage)?;
    }
    let total = files.iter().map(|f| f.bytes).sum::<u64>();
    let mut prior = 0;
    for f in files {
        let path = loc.stage.join(&f.name);
        let file_url = url(f)?;
        if !valid_file(&path, f)? {
            if bytes(&path)?.is_some_and(|n| n >= f.bytes) {
                fs::remove_file(&path)?;
            }
            let mut attempt = 1;
            loop {
                let resumed = bytes(&path)?.is_some_and(|size| size > 0);
                let result = fetch(client, &file_url, &path, f.bytes, &|size| {
                    progress(&format!(
                        "{}：{:.1} / {:.1} MiB",
                        f.name,
                        (prior + size) as f64 / 1_048_576.0,
                        total as f64 / 1_048_576.0
                    ));
                });
                match result {
                    Ok(()) if valid_file(&path, f)? => break,
                    Ok(()) => {
                        fs::remove_file(&path)?;
                        if !resumed || attempt == 3 {
                            bail!("{} 的 SHA-256 与固定模型版本不符", f.name);
                        }
                        progress(&format!("{} 断点数据校验失败，重新下载", f.name));
                        attempt += 1;
                    }
                    Err(error) if attempt < 3 && !error.is::<Permanent>() => {
                        progress(&format!(
                            "{} 下载中断，正在重试（{}/3）",
                            f.name,
                            attempt + 1
                        ));
                        std::thread::sleep(Duration::from_secs(attempt));
                        attempt += 1;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("下载 {} 失败", f.name));
                    }
                }
            }
        }
        prior += f.bytes;
    }
    fs::rename(&loc.stage, &loc.ready).context("原子安装已验证的模型文件失败")?;
    Ok(())
}

#[derive(Debug)]
struct Permanent(String);
impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Permanent {}

fn fetch(
    client: &Client,
    url: &Url,
    path: &Path,
    expected: u64,
    progress: &dyn Fn(u64),
) -> Result<()> {
    let start = bytes(path)?.unwrap_or(0);
    if start == expected {
        return Ok(());
    }
    let mut request = client
        .get(url.clone())
        .header(header::ACCEPT_ENCODING, "identity");
    if start > 0 {
        request = request.header(header::RANGE, format!("bytes={start}-"));
    }
    let mut response = request.send().map_err(reqwest::Error::without_url)?;
    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE && start > 0 {
        fs::remove_file(path)?;
        bail!("源站拒绝断点范围，将重新下载");
    }
    if !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        let error = format!("源站返回 HTTP {status}");
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            bail!("{error}");
        }
        return Err(Permanent(error).into());
    }
    let append = status == StatusCode::PARTIAL_CONTENT;
    let limit = if append {
        let range = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|s| s.to_str().ok());
        let prefix = format!("bytes {start}-");
        let suffix = format!("/{expected}");
        let end = range
            .and_then(|s| s.strip_prefix(&prefix))
            .and_then(|s| s.strip_suffix(&suffix))
            .and_then(|s| s.parse::<u64>().ok());
        end.filter(|v| *v >= start && *v < expected)
            .context("模型源返回了不一致的断点范围")?
            + 1
    } else {
        expected
    };
    let mut written = if append { start } else { 0 };
    if response
        .content_length()
        .is_some_and(|len| len > limit - written)
    {
        return Err(Permanent("源站文件比固定版本大".into()).into());
    }
    // Refuse to mutate an unrelated file if a local user hard-linked it into the staging directory.
    #[cfg(unix)]
    if let Ok(meta) = fs::symlink_metadata(path) {
        use std::os::unix::fs::MetadataExt;
        ensure!(meta.nlink() == 1, "断点文件引用了其他路径");
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)?;
    let mut buffer = vec![0; 128 * 1024];
    let mut last = written;
    progress(written);
    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if n as u64 > limit - written {
            return Err(Permanent("模型下载内容超过预期大小".into()).into());
        }
        file.write_all(&buffer[..n])?;
        written += n as u64;
        if written - last >= 8 * 1024 * 1024 {
            progress(written);
            last = written;
        }
    }
    ensure!(
        written == expected,
        "文件传输不完整（{written}/{expected} 字节）"
    );
    file.sync_all()?;
    progress(written);
    Ok(())
}
