//! Search the selected Hub's public catalog through the instance proxy.
use crate::{models, proxy, store::Db};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, time::Duration};

#[derive(Deserialize)]
pub(crate) struct Search {
    source: String,
    query: String,
}
pub(crate) fn valid_repo(id: &str) -> bool {
    let mut pair = id.split('/');
    let safe = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    pair.next().is_some_and(safe) && pair.next().is_some_and(safe) && pair.next().is_none()
}
pub(crate) fn search(db: &mut Db, input: Search) -> Result<Value> {
    let query = input.query.trim();
    ensure!(
        (1..=160).contains(&query.len()),
        "请输入 1 至 160 字节的模型搜索词"
    );
    let base = models::source_endpoint(&input.source)?;
    let ms = input.source == "modelscope";
    let mut url = base.join(if ms {
        "openapi/v1/models"
    } else {
        "api/models"
    })?;
    url.query_pairs_mut().append_pair("search", query);
    if ms {
        url.query_pairs_mut().append_pair("page_size", "40");
    } else {
        url.query_pairs_mut()
            .append_pair("limit", "40")
            .append_pair("sort", "downloads")
            .append_pair("direction", "-1");
    }
    let client = proxy::builder(db)?
        .timeout(Duration::from_secs(25))
        .build()?;
    let response = client
        .get(url)
        .send()
        .context("无法访问模型库，请检查实例代理或切换来源")?
        .error_for_status()?;
    let mut bytes = Vec::new();
    response.take(2_000_001).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 2_000_000, "模型库响应过大，请缩小搜索范围");
    let data: Value = serde_json::from_slice(&bytes)?;
    let rows = if ms { &data["data"]["models"] } else { &data };
    let catalog = models::list(&crate::store::data_dir(db)?)?;
    let mut results = Vec::new();
    for row in rows.as_array().context("模型库响应格式异常")? {
        let Some(repo) = row["id"].as_str().filter(|x| valid_repo(x)) else {
            continue;
        };
        let registered = catalog.iter().find(|m| {
            matches!(m.id.as_str(), models::DEFAULT_ID | models::QWEN_ID)
                && m.repository.eq_ignore_ascii_case(repo)
        });
        let gguf = repo.to_ascii_lowercase().contains("gguf")
            || row["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|t| t == "gguf"));
        let format = if gguf {
            Some("GGUF")
        } else if registered.is_some()
            || row["tags"]
                .as_array()
                .is_some_and(|t| t.iter().any(|x| x == "safetensors"))
        {
            Some("Safetensors")
        } else {
            None
        };
        let link = if ms {
            format!("https://modelscope.cn/models/{repo}")
        } else {
            format!("https://huggingface.co/{repo}")
        };
        results.push(json!({"repository":repo,"name":row["display_name"].as_str().unwrap_or(repo),"downloads":row["downloads"].as_u64(),
            "source":input.source,"url":link,"format":format,"model_id":registered.map(|m|&m.id)}));
    }
    Ok(json!({"results":results}))
}
