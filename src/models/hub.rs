//! Import files from an immutable Hub commit, then run the same checks used for uploads.
use super::{ModelStatus, download, imports, registry};
use crate::{
    model_hub,
    store::{self, Db},
};
use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client};
use std::{fs::File, io::Read, path::Path};

fn response(client: &Client, url: Url) -> Result<reqwest::blocking::Response> {
    Ok(client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?)
}
fn head(client: &Client, base: &Url, ms: bool, repo: &str) -> Result<String> {
    let path = if ms {
        format!("{repo}.git/info/refs?service=git-upload-pack")
    } else {
        format!("api/models/{repo}")
    };
    let mut raw = Vec::new();
    response(client, base.join(&path)?)?
        .take(2_097_153)
        .read_to_end(&mut raw)?;
    ensure!(raw.len() <= 2_097_152, "模型仓库元数据过大");
    let rev = if ms {
        // Git smart HTTP v0 identifies HEAD with the 40 hex bytes before the ref name.
        let end = raw
            .windows(6)
            .position(|s| s == b" HEAD\0")
            .context("ModelScope 未返回仓库 HEAD")?;
        let start = end.checked_sub(40).context("模型版本无效")?;
        std::str::from_utf8(&raw[start..end])?.to_owned()
    } else {
        let metadata: serde_json::Value = serde_json::from_slice(&raw)?;
        metadata["sha"]
            .as_str()
            .context("Hugging Face 未返回固定版本")?
            .to_owned()
    };
    ensure!(
        rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
        "模型版本无效"
    );
    Ok(rev)
}
fn get_file(client: &Client, url: Url, out: &Path, limit: u64) -> Result<()> {
    let r = response(client, url)?;
    ensure!(
        r.content_length().is_none_or(|n| n > 0 && n <= limit),
        "模型文件超出大小限制"
    );
    let mut file = File::create_new(out)?;
    let n = std::io::copy(&mut r.take(limit + 1), &mut file)?;
    ensure!((1..=limit).contains(&n), "模型文件超出大小限制");
    Ok(())
}
pub(crate) fn download_custom(
    db: &mut Db,
    source: &str,
    repo: &str,
    progress: &dyn Fn(&str),
) -> Result<ModelStatus> {
    ensure!(
        repo.len() <= 200 && model_hub::valid_repo(repo),
        "请输入 owner/repository 格式的模型仓库"
    );
    for id in [registry::DEFAULT_ID, registry::QWEN_ID] {
        if registry::builtin(id).is_some_and(|p| p.repository.eq_ignore_ascii_case(repo)) {
            return super::install(db, id, source, progress);
        }
    }
    let base = download::endpoint(source)?;
    let client = download::client(db)?;
    let ms = source == "modelscope";
    let rev = head(&client, &base, ms, repo)?;
    let prefix = if ms { "models/" } else { "" };
    let base = base.join(&format!("{prefix}{repo}/resolve/{rev}/"))?;
    let stage = tempfile::tempdir_in(store::data_dir(db)?)?;
    for index in [0, 3, 1, 2] {
        let name = registry::IMPORT_NAMES[index];
        let remote = if name == "pooling.json" {
            "1_Pooling/config.json"
        } else {
            name
        };
        progress(&format!("下载 {repo}/{remote}"));
        let out = stage.path().join(name);
        get_file(&client, base.join(remote)?, &out, imports::LIMITS[index])
            .with_context(|| format!("无法获取兼容的 BERT 模型文件 {remote}"))?;
        if name == "config.json" {
            let v: serde_json::Value = serde_json::from_reader(File::open(out)?)?;
            ensure!(
                v["model_type"] == "bert" && v["hidden_size"] == 384,
                "该仓库不是受支持的 384 维 BERT 嵌入模型"
            );
        }
    }
    imports::register(db, stage.path(), source, repo, &rev, progress)
}
