//! Explicit instance-wide configuration for generative AI, separate from embeddings.
use crate::store::{self, Db};
use anyhow::{Context, Result, ensure};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiType {
    #[default]
    Auto,
    Responses,
    ChatCompletions,
}

impl ApiType {
    pub fn model(
        self,
        provider: &dyn aimux_core::Provider,
        id: &str,
    ) -> Result<std::sync::Arc<dyn aimux_core::LanguageModel>> {
        let api = match self {
            Self::Auto => return Ok(provider.language_model(id)?),
            Self::Responses => aimux_core::LanguageApi::Responses,
            Self::ChatCompletions => aimux_core::LanguageApi::Chat,
        };
        Ok(provider
            .language_model_with_api(api, id)
            .context("此聊天服务商不支持所选的 OpenAI 请求类型")??)
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Config {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_type: ApiType,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_endpoint()?;
        if !self.provider.is_empty() {
            ensure!(
                !self.model.trim().is_empty() && self.model.len() <= 256,
                "请填写聊天模型名称"
            );
        }
        Ok(())
    }

    fn provider(&self) -> Result<std::sync::Arc<dyn aimux_core::provider::Provider>> {
        Ok(aimux_providers::create_provider(
            &self.provider,
            aimux_providers::PresetSettings {
                api_key: self.api_key.clone().map(Into::into),
                base_url: self.base_url.clone(),
                ..Default::default()
            },
        )?)
    }

    fn validate_endpoint(&self) -> Result<()> {
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
            self.provider != "ollama" || self.base_url.is_some(),
            "请明确设置 Ollama 聊天地址（例如 https://ollama.example.com/v1）"
        );
        ensure!(
            self.api_type == ApiType::Auto || self.provider == "openai",
            "OpenAI 请求类型仅适用于 OpenAI 和 OpenAI 兼容服务"
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
                api_type: match env("STARSTACK_AI_API_TYPE").as_deref() {
                    None | Some("auto") => ApiType::Auto,
                    Some("responses") => ApiType::Responses,
                    Some("chat_completions") => ApiType::ChatCompletions,
                    _ => anyhow::bail!(
                        "STARSTACK_AI_API_TYPE 支持 auto、responses、chat_completions"
                    ),
                },
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
      "api_type":cfg.api_type,"base_url":if admin{cfg.base_url}else{None},"has_key":admin && cfg.api_key.is_some()}),
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
    api_type: Option<ApiType>,
    #[serde(default)]
    clear_key: bool,
}
pub(crate) fn draft(db: &mut Db, input: Input) -> Result<Config> {
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
    Ok(if provider.is_empty() {
        Config::default()
    } else {
        Config {
            api_type: input.api_type.unwrap_or_else(|| {
                if provider == old.provider {
                    old.api_type
                } else {
                    ApiType::Auto
                }
            }),
            provider,
            model: input.model.trim().to_owned(),
            base_url,
            api_key: key,
        }
    })
}

pub(crate) async fn models(cfg: Config) -> Result<Vec<String>> {
    ensure!(!cfg.provider.is_empty(), "请选择聊天服务商");
    cfg.validate_endpoint()?;
    let provider = cfg
        .provider()
        .map_err(|_| anyhow::anyhow!("无法连接聊天服务，请检查服务商、地址和 API 密钥"))?;
    let discovery = provider.discovery().context("该聊天服务不提供模型列表")?;
    let models = tokio::time::timeout(std::time::Duration::from_secs(12), discovery.list_models())
        .await
        .context("获取模型列表超时，请重试")?
        .map_err(|_| anyhow::anyhow!("无法获取模型列表，请检查服务地址和 API 密钥"))?;
    let mut ids: Vec<_> = models
        .into_iter()
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty() && id.len() <= 256)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

pub(crate) fn update(db: &mut Db, input: Input) -> Result<()> {
    use store::metadata_values as m;
    ensure!(
        env("STARSTACK_AI_PROVIDER").is_none(),
        "聊天服务已由环境变量配置"
    );
    let cfg = draft(db, input)?;
    cfg.validate()?;
    if !cfg.provider.is_empty() {
        cfg.provider()?;
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
