//! Instance-wide outbound proxy for GitHub and public README requests.
use crate::{
    auth,
    store::{self, Db},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use diesel::{connection::SimpleConnection, prelude::*};
use reqwest::{Proxy, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;

diesel::table! { proxy_configuration(id) { id -> Integer, setting -> Text, } }

const INVALID_URL: &str = "代理地址无效，请填写 HTTP、HTTPS、SOCKS5 或 SOCKS5h 代理 URL";

pub(crate) enum ProxyConfig {
    System,
    Direct,
    Custom(Url),
}

impl ProxyConfig {
    fn custom(raw: &str) -> Result<Self> {
        ensure!(
            raw.len() <= 4096 && raw.contains("://") && !raw.chars().any(char::is_whitespace),
            INVALID_URL
        );
        let url = Url::parse(raw).map_err(|_| anyhow!(INVALID_URL))?;
        ensure!(
            matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                && url.host().is_some()
                && url.port() != Some(0)
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none(),
            INVALID_URL
        );
        Self::proxy(&url)?;
        Ok(Self::Custom(url))
    }

    fn proxy(url: &Url) -> Result<Proxy> {
        // A proxy URL can include credentials. Keep errors and status payloads redacted.
        Proxy::all(url.clone()).map_err(|_| anyhow!(INVALID_URL))
    }

    fn blocking_builder(&self) -> Result<reqwest::blocking::ClientBuilder> {
        let client = reqwest::blocking::Client::builder();
        Ok(match self {
            Self::System => client,
            Self::Direct => client.no_proxy(),
            Self::Custom(url) => client.proxy(Self::proxy(url)?),
        })
    }

    #[cfg(any(test, feature = "firecrawl-local"))]
    pub(crate) fn async_builder(&self) -> Result<reqwest::ClientBuilder> {
        let client = reqwest::Client::builder();
        Ok(match self {
            Self::System => client,
            Self::Direct => client.no_proxy(),
            Self::Custom(url) => client.proxy(Self::proxy(url)?),
        })
    }

    fn status(&self, source: &str) -> Value {
        let (mode, address, credentials) = match self {
            Self::System => ("system", None, false),
            Self::Direct => ("direct", None, false),
            Self::Custom(url) => {
                // Slice from the host onward; userinfo is never part of the returned address.
                let mut address =
                    format!("{}://{}", url.scheme(), url.host().expect("validated host"));
                if let Some(port) = url.port() {
                    address.push_str(&format!(":{port}"));
                }
                (
                    "custom",
                    Some(address),
                    !url.username().is_empty() || url.password().is_some(),
                )
            }
        };
        json!({"mode": mode, "source": source, "address": address, "has_credentials": credentials})
    }
}

fn schema(db: &mut Db) -> Result<()> {
    db.batch_execute("CREATE TABLE IF NOT EXISTS proxy_configuration (id INTEGER PRIMARY KEY CHECK(id=1), setting TEXT NOT NULL)")?;
    Ok(())
}

fn operator_url() -> Result<Option<String>> {
    match env::var("STARSTACK_PROXY_URL") {
        Ok(s) => Ok((!s.trim().is_empty()).then_some(s)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("STARSTACK_PROXY_URL 需要 UTF-8 代理地址"),
    }
}

fn effective(db: &mut Db, operator: Option<String>) -> Result<(ProxyConfig, &'static str)> {
    if let Some(raw) = operator {
        return Ok((
            ProxyConfig::custom(raw.trim()).context("STARSTACK_PROXY_URL 无效")?,
            "environment",
        ));
    }
    schema(db)?;
    let saved: Option<String> = proxy_configuration::table
        .find(1)
        .select(proxy_configuration::setting)
        .first(db)
        .optional()?;
    match saved.as_deref() {
        None => Ok((ProxyConfig::System, "system")),
        Some("direct") => Ok((ProxyConfig::Direct, "database")),
        Some(raw) => Ok((ProxyConfig::custom(raw)?, "database")),
    }
}

pub(crate) fn config(db: &mut Db) -> Result<ProxyConfig> {
    Ok(effective(db, operator_url()?)?.0)
}

pub(crate) fn builder(db: &mut Db) -> Result<reqwest::blocking::ClientBuilder> {
    config(db)?.blocking_builder()
}

fn require_admin(db: &mut Db, revision: Option<i32>) -> Result<()> {
    let owner = store::owner(db)?;
    let user = auth::account(db, &owner)?;
    ensure!(user.admin && !user.disabled, "需要管理员权限");
    ensure!(
        revision.is_none_or(|value| user.revision == value),
        "会话已失效，请重新登录"
    );
    Ok(())
}

pub(crate) fn read_settings(db: &mut Db) -> Result<Value> {
    require_admin(db, None)?;
    let (config, source) = effective(db, operator_url()?)?;
    Ok(config.status(source))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", tag = "mode", deny_unknown_fields)]
enum Update {
    System,
    Direct,
    Custom { url: String },
}

pub(crate) fn test_connection(db: &mut Db, input: Value, revision: i32) -> Result<Value> {
    require_admin(db, Some(revision))?;
    let operator = operator_url()?;
    let config = if input.is_null() || operator.is_some() {
        effective(db, operator)?.0
    } else {
        let update: Update =
            serde_json::from_value(input).map_err(|_| anyhow!("代理设置格式无效"))?;
        match update {
            Update::System => ProxyConfig::System,
            Update::Direct => ProxyConfig::Direct,
            Update::Custom { url } => ProxyConfig::custom(url.trim())?,
        }
    };
    let start = std::time::Instant::now();
    let response = config
        .blocking_builder()?
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .and_then(|client| client.head("https://www.cloudflare.com/").send())
        .map_err(|e| {
            if e.is_timeout() {
                anyhow!("连接 Cloudflare 超时，请检查代理地址和网络")
            } else {
                anyhow!("无法访问 Cloudflare，请检查代理地址、认证信息和网络")
            }
        })?;
    ensure!(
        response.status().is_success() || response.status().is_redirection(),
        "Cloudflare 返回 HTTP {}，请检查代理或等待后重试",
        response.status().as_u16()
    );
    Ok(json!({"elapsed_ms": start.elapsed().as_millis()}))
}

pub(crate) fn save_settings(db: &mut Db, input: Value, revision: i32) -> Result<Value> {
    persist(db, input, revision, operator_url()?)
}

fn persist(db: &mut Db, input: Value, revision: i32, operator: Option<String>) -> Result<Value> {
    let update: Update = serde_json::from_value(input).map_err(|_| anyhow!("代理设置格式无效"))?;
    db.immediate_transaction(|db| {
        require_admin(db, Some(revision))?;
        ensure!(operator.is_none(), "代理由 STARSTACK_PROXY_URL 管理");
        schema(db)?;
        let selected = match update {
            Update::System => ProxyConfig::System,
            Update::Direct => ProxyConfig::Direct,
            Update::Custom { url } => ProxyConfig::custom(url.trim())?,
        };
        let value = match &selected {
            ProxyConfig::System => None,
            ProxyConfig::Direct => Some("direct"),
            ProxyConfig::Custom(url) => Some(url.as_str()),
        };
        if let Some(value) = value {
            diesel::insert_into(proxy_configuration::table)
                .values((
                    proxy_configuration::id.eq(1),
                    proxy_configuration::setting.eq(value),
                ))
                .on_conflict(proxy_configuration::id)
                .do_update()
                .set(proxy_configuration::setting.eq(value))
                .execute(db)?;
            Ok(selected.status("database"))
        } else {
            diesel::delete(proxy_configuration::table.find(1)).execute(db)?;
            Ok(selected.status("system"))
        }
    })
}

#[cfg(test)]
mod tests;
