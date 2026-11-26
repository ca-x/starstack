//! Account-specific document splitting, measured in Unicode scalar values.
use crate::store::{self, Db};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Policy {
    WholeDocument,
    Window { size: usize, overlap: usize },
}

impl Policy {
    pub(crate) fn whole_document() -> Self {
        Self::WholeDocument
    }

    pub(crate) fn legacy(&self) -> bool {
        matches!(self, Self::WholeDocument)
    }

    pub(crate) fn identity(&self) -> String {
        match self {
            Self::WholeDocument => "whole-document".into(),
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
        let &Self::Window { size, overlap } = self else {
            return vec![text.to_owned()];
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

pub(crate) fn selected(db: &mut Db, backend: &str) -> Result<Policy> {
    if let Some(value) = store::meta(db, &format!("chunking_policy:{backend}"))? {
        let policy: Policy = serde_json::from_str(&value)?;
        policy.validate()?;
        return Ok(policy);
    }
    if backend == "ollama"
        && store::meta(db, "index_model")?.is_some()
        && store::meta(db, "index_chunk_policy")?.is_none_or(|s| s == "whole-document")
        && store::meta(db, "index_backend")?.is_none_or(|s| s == "ollama")
    {
        return Ok(Policy::whole_document());
    }
    Ok(Preset::Balanced.policy())
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
    let policy = selected(db, backend)?;
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
