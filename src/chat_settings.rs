//! Explicit instance-wide configuration for generative AI, separate from embeddings.
use crate::store::{self, Db};
use anyhow::{Result, ensure};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Config {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.provider.is_empty() {
            return Ok(());
        }
        ensure!(
            self.provider.len() <= 80
                && self
                    .provider
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "聊天服务商名称无效"
        );
        ensure!(
            !self.model.trim().is_empty() && self.model.len() <= 256,
            "请填写聊天模型名称"
        );
        ensure!(
            self.provider != "ollama" || self.base_url.is_some(),
            "请明确设置 Ollama 聊天地址（例如 https://ollama.example.com/v1）"
        );
        if let Some(url) = &self.base_url {
            crate::ollama_settings::canonical_url(url)?;
        }
        Ok(())
    }
}
fn saved(db: &mut Db) -> Result<Config> {
    use store::metadata_values as m;
    let raw = m::table
        .find(("@instance", "chat_provider"))
        .select(m::value)
        .first::<String>(db)
        .optional()?;
    Ok(raw
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}
fn effective(db: &mut Db) -> Result<(Config, bool)> {
    if let Some(provider) = env("STARSTACK_AI_PROVIDER") {
        let base_url = env("STARSTACK_AI_BASE_URL").or_else(|| {
            (provider == "ollama")
                .then(|| {
                    env("STARSTACK_OLLAMA_URL").map(|u| format!("{}/v1", u.trim_end_matches('/')))
                })
                .flatten()
        });
        Ok((
            Config {
                provider,
                model: env("STARSTACK_AI_MODEL").unwrap_or_default(),
                base_url,
                api_key: env("STARSTACK_AI_API_KEY"),
            },
            true,
        ))
    } else {
        Ok((saved(db)?, false))
    }
}
pub(crate) fn get(db: &mut Db) -> Result<Config> {
    let (cfg, _) = effective(db)?;
    ensure!(
        !cfg.provider.is_empty(),
        "尚未配置 AI 聊天服务，请管理员在「同步与索引」中选择服务商、地址和聊天模型"
    );
    cfg.validate()?;
    Ok(cfg)
}
pub(crate) fn status(db: &mut Db, admin: bool) -> Result<Value> {
    let (cfg, env) = effective(db)?;
    Ok(
        json!({"configured":!cfg.provider.is_empty() && cfg.validate().is_ok(),"managed_by_env":env,"provider":cfg.provider,"model":cfg.model,
      "base_url":if admin{cfg.base_url}else{None},"has_key":admin && cfg.api_key.is_some()}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    provider: String,
    #[serde(default)]
    model: String,
    base_url: Option<String>,
    api_key: Option<String>,
    #[serde(default)]
    clear_key: bool,
}
pub(crate) fn update(db: &mut Db, input: Input) -> Result<()> {
    use store::metadata_values as m;
    ensure!(
        env("STARSTACK_AI_PROVIDER").is_none(),
        "聊天服务已由环境变量配置"
    );
    let old = saved(db)?;
    let provider = input.provider.trim().to_lowercase();
    let base_url = input
        .base_url
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ollama_settings::canonical_url(&s))
        .transpose()?;
    let mut key = input.api_key.filter(|s| !s.is_empty());
    if !input.clear_key && key.is_none() && old.provider == provider && old.base_url == base_url {
        key = old.api_key;
    }
    ensure!(key.as_ref().is_none_or(|s| s.len() <= 8192), "API 密钥过长");
    let cfg = if provider.is_empty() {
        Config::default()
    } else {
        Config {
            provider,
            model: input.model.trim().to_owned(),
            base_url,
            api_key: key,
        }
    };
    cfg.validate()?;
    if !cfg.provider.is_empty() {
        aimux_providers::create_provider(
            &cfg.provider,
            aimux_providers::PresetSettings {
                api_key: cfg.api_key.clone().map(Into::into),
                base_url: cfg.base_url.clone(),
                ..Default::default()
            },
        )?;
    }
    let value = serde_json::to_string(&cfg)?;
    diesel::insert_into(m::table)
        .values((
            m::owner_id.eq("@instance"),
            m::key.eq("chat_provider"),
            m::value.eq(&value),
        ))
        .on_conflict((m::owner_id, m::key))
        .do_update()
        .set(m::value.eq(&value))
        .execute(db)?;
    Ok(())
}
