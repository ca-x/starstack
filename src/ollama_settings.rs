//! Instance-approved inference servers and account-specific model selection.
use crate::store::{self, Db};
use anyhow::{Context, Result, ensure};
use diesel::prelude::*;
use reqwest::Url;
use std::env;

const FALLBACK_MODEL: &str = "qwen3-embedding:0.6b";
const ENDPOINTS_KEY: &str = "allowed_ollama_urls";
const INSTANCE: &str = "@instance";

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.trim().is_empty())
}

pub(crate) fn inherited() -> bool {
    var("STARSTACK_OLLAMA_URL").is_some() || var("STARSTACK_MODEL").is_some()
}

pub(crate) fn canonical_url(raw: &str) -> Result<String> {
    let mut parsed = Url::parse(raw.trim()).context("请输入完整的 Ollama HTTP(S) 服务地址")?;
    ensure!(
        raw.len() <= 2048
            && matches!(parsed.scheme(), "http" | "https")
            && parsed.host().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "Ollama 地址需使用 HTTP 或 HTTPS，且不能含凭证、查询参数或片段"
    );
    let path = parsed.path().trim_end_matches('/').to_owned();
    parsed.set_path(&path);
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

pub(crate) fn endpoints(db: &mut Db) -> Result<Vec<String>> {
    use store::metadata_values as m;
    if let Some(saved) = m::table
        .find((INSTANCE, ENDPOINTS_KEY))
        .select(m::value)
        .first::<String>(db)
        .optional()?
    {
        return Ok(serde_json::from_str(&saved)?);
    }
    let default = var("STARSTACK_OLLAMA_URL")
        .or_else(|| var("STARSTACK_OLLAMA_DEFAULT_URL"))
        .unwrap_or_else(|| "http://127.0.0.1:11434".into());
    Ok(vec![canonical_url(&default)?])
}

pub(crate) fn set_endpoints(db: &mut Db, urls: &[String]) -> Result<()> {
    use store::metadata_values as m;
    ensure!(
        !urls.is_empty() && urls.len() <= 32,
        "请保留 1 至 32 个 Ollama 服务地址"
    );
    let mut normalized = Vec::new();
    for raw in urls {
        let url = canonical_url(raw)?;
        if !normalized.contains(&url) {
            normalized.push(url);
        }
    }
    let encoded = serde_json::to_string(&normalized)?;
    db.transaction(|db| {
        let used = m::table
            .filter(m::key.eq("ollama_url"))
            .select(m::value)
            .distinct()
            .load::<String>(db)?;
        ensure!(
            used.iter().all(|u| normalized.contains(u)),
            "有账号仍选择了待移除的 Ollama 地址，请先切换该账号的设置"
        );
        diesel::insert_into(m::table)
            .values((
                m::owner_id.eq(INSTANCE),
                m::key.eq(ENDPOINTS_KEY),
                m::value.eq(&encoded),
            ))
            .on_conflict((m::owner_id, m::key))
            .do_update()
            .set(m::value.eq(&encoded))
            .execute(db)?;
        Ok(())
    })
}

pub(crate) fn selected(db: &mut Db) -> Result<(String, String)> {
    let urls = endpoints(db)?;
    let url = match store::meta(db, "ollama_url")? {
        Some(url) => {
            ensure!(
                urls.contains(&url),
                "Ollama 地址已停用，请在模型设置中重新选择"
            );
            url
        }
        None => urls.first().context("请先添加 Ollama 服务地址")?.clone(),
    };
    Ok((url, configured_model(db)?))
}

pub(crate) fn configured_model(db: &mut Db) -> Result<String> {
    let legacy = if store::meta(db, "index_backend")?.is_none_or(|s| s == "ollama") {
        store::meta(db, "index_model")?
    } else {
        None
    };
    Ok(store::meta(db, "ollama_model")?
        .or_else(|| var("STARSTACK_MODEL"))
        .or(legacy)
        .unwrap_or_else(|| FALLBACK_MODEL.into()))
}

pub(crate) fn set(db: &mut Db, url: Option<&str>, model: Option<&str>) -> Result<()> {
    if let Some(raw) = url {
        let normalized = canonical_url(raw)?;
        ensure!(
            endpoints(db)?.contains(&normalized),
            "请选择管理员已添加的 Ollama 服务地址"
        );
        store::set_meta(db, "ollama_url", &normalized)?;
    }
    if let Some(raw) = model {
        let model = raw.trim();
        ensure!(
            !model.is_empty()
                && model.len() <= 256
                && model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b)),
            "请输入有效的 Ollama 模型名（可含标签）"
        );
        store::set_meta(db, "ollama_model", model)?;
    }
    Ok(())
}
