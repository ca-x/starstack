//! Settings shared by the CLI and the authenticated web interface.
use crate::{
    index, models, ollama_settings,
    store::{self, Db},
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub backend: String,
    pub model: Option<String>,
    pub url: Option<String>,
}

pub(crate) fn select(db: &mut Db, input: Selection) -> Result<()> {
    match input.backend.as_str() {
        "ollama" => db.immediate_transaction(|db| {
            ollama_settings::set(db, input.url.as_deref(), input.model.as_deref())?;
            index::set_backend(db, "ollama")?;
            store::delete_meta(db, "index_issue")
        }),
        "builtin" => {
            ensure!(input.url.is_none(), "内置模型使用本机文件");
            index::set_builtin(db, input.model.as_deref().unwrap_or(models::DEFAULT_ID))
        }
        _ => anyhow::bail!("请选择 builtin 或 ollama"),
    }
}

pub(crate) fn panel(db: &mut Db) -> Result<Value> {
    let items = models::list(&store::data_dir(db)?)?;
    let endpoints = ollama_settings::endpoints(db)?;
    let url = store::meta(db, "ollama_url")?
        .or_else(|| endpoints.first().cloned())
        .unwrap_or_default();
    let model = ollama_settings::configured_model(db)?;
    let models = items
        .into_iter()
        .map(|item| {
            let status = match item.state {
                "ready" | "downloading" => item.state,
                "error" | "partial" => "failed",
                _ => "missing",
            };
            json!({
                "id": item.id, "label": item.name, "status": status,
                "downloaded_bytes": item.downloaded_bytes, "total_bytes": item.expected_bytes,
                "error": item.error, "sources": item.sources, "files":item.files
            })
        })
        .collect::<Vec<_>>();
    let backend = index::backend(db)?;
    let (needs_rebuild, config_issue) = match index::needs_rebuild(db) {
        Ok(rebuild) => (rebuild, None),
        Err(error) => (false, Some(error.to_string())),
    };
    let index_issue = config_issue.or(store::meta(db, "index_issue")?);
    Ok(
        json!({"backend":backend,"chunking":crate::chunking::parameters(db,&backend)?,"model":index::configured_model(db)?,
        "ollama_url":url,"ollama_model":model,"ollama_urls":endpoints,
        "index_issue":index_issue,"needs_rebuild":needs_rebuild,"models":models}),
    )
}
