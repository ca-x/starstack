//! Account-specific document splitting, measured in Unicode scalar values.
use crate::store::{self, Db};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Policy {
    WholeDocument,
    Structured {
        body_cap: usize,
        alpha: u8,
        window: Option<(usize, usize)>,
    },
    Window {
        size: usize,
        overlap: usize,
    },
}

impl Policy {
    #[cfg(test)]
    pub(crate) fn whole_document() -> Self {
        Self::WholeDocument
    }

    pub(crate) fn legacy(&self) -> bool {
        matches!(self, Self::WholeDocument)
    }

    pub(crate) fn identity(&self) -> String {
        match self {
            Self::WholeDocument => "whole-document".into(),
            Self::Structured {
                body_cap,
                alpha,
                window,
            } => match window {
                Some((size, overlap)) => {
                    format!("structured-v1-{body_cap}-{alpha}-{size}-{overlap}")
                }
                None => format!("structured-v1-{body_cap}-{alpha}"),
            },
            Self::Window { size, overlap } => format!("unicode-window-v1-{size}-{overlap}"),
        }
    }

    fn validate(&self) -> Result<()> {
        if let &Self::Window { size, overlap } = self {
            ensure!(
                (32..=4096).contains(&size) && overlap <= size / 2,
                "分块长度应在 32 至 4096 个字符之间，重叠长度不能超过分块的一半"
            );
        }
        Ok(())
    }

    pub(crate) fn split(&self, text: &str) -> Vec<String> {
        let (size, overlap) = match *self {
            Self::Window { size, overlap }
            | Self::Structured {
                window: Some((size, overlap)),
                ..
            } => (size, overlap),
            _ => return vec![text.to_owned()],
        };
        let boundaries: Vec<_> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut chunks = Vec::new();
        let mut start = 0;
        loop {
            let end = (start + size).min(boundaries.len() - 1);
            chunks.push(text[boundaries[start]..boundaries[end]].to_owned());
            if end == boundaries.len() - 1 {
                break;
            }
            start = end - overlap;
        }
        chunks
    }
}

fn configured(db: &mut Db, backend: &str) -> Result<Option<Policy>> {
    store::meta(db, &format!("chunking_policy:{backend}"))?
        .map(|value| {
            let policy: Policy = serde_json::from_str(&value)?;
            policy.validate()?;
            Ok(policy)
        })
        .transpose()
}

pub(crate) fn selected(db: &mut Db, backend: &str) -> Result<Policy> {
    let configured = configured(db, backend)?;
    let body_cap = match std::env::var("STARSTACK_README_INDEX_CHARS") {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("STARSTACK_README_INDEX_CHARS 应为 0 至 65536"))?,
        Err(std::env::VarError::NotPresent) => 4000,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        body_cap <= 65536,
        "STARSTACK_README_INDEX_CHARS 应为 0 至 65536"
    );
    let window = match configured.unwrap_or_else(|| Preset::Balanced.policy()) {
        Policy::Window { size, overlap } if backend == "ollama" => Some((size, overlap)),
        _ => None,
    };
    Ok(Policy::Structured {
        body_cap,
        alpha: 50,
        window,
    })
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Preset {
    Short,
    Balanced,
    Long,
    Whole,
}

impl Preset {
    fn policy(self) -> Policy {
        match self {
            Self::Whole => Policy::WholeDocument,
            Self::Short => Policy::Window {
                size: 192,
                overlap: 24,
            },
            Self::Balanced => Policy::Window {
                size: 512,
                overlap: 64,
            },
            Self::Long => Policy::Window {
                size: 1024,
                overlap: 128,
            },
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Parameters {
    preset: Preset,
}

pub(crate) fn parameters(db: &mut Db, backend: &str) -> Result<Parameters> {
    let policy = configured(db, backend)?.unwrap_or_else(|| Preset::Balanced.policy());
    let preset = [Preset::Balanced, Preset::Short, Preset::Long, Preset::Whole]
        .into_iter()
        .find(|p| p.policy() == policy)
        .ok_or_else(|| anyhow::anyhow!("分块参数未知，请重新选择预设"))?;
    Ok(Parameters { preset })
}

pub(crate) fn save(db: &mut Db, input: Parameters) -> Result<()> {
    let policy = input.preset.policy();
    db.immediate_transaction(|db| {
        let backend = crate::index::backend(db)?;
        store::set_meta(
            db,
            &format!("chunking_policy:{backend}"),
            &serde_json::to_string(&policy)?,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_keep_unicode_overlap_and_tail() {
        let policy = Policy::Window {
            size: 3,
            overlap: 1,
        };
        assert_eq!(policy.split("甲乙🌟丁戊己"), ["甲乙🌟", "🌟丁戊", "戊己"]);
        assert_eq!(policy.split(""), [""]);
    }
}
