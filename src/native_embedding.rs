//! CPU embeddings for BERT sentence encoders and quantized Qwen3-Embedding.
use crate::index::mean::Mean;
use anyhow::{Context, Result, anyhow, ensure};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod qwen;
pub(crate) use qwen::PROTOCOL as QWEN_PROTOCOL;
use tokenizers::{Encoding, PaddingParams, PostProcessor, Tokenizer, TruncationParams};

/// Includes the pinned weights, document processor and pooling protocol in the index identity.
pub(crate) const IDENTITY: &str =
    "builtin:multilingual-minilm-l12-v2:eaa086f0ffee582a:masked-mean-128-stride24-v2";

const DIMENSION: usize = 384;
const MAX_TOKENS: usize = 128;

struct Encoder {
    tokenizer: Tokenizer,
    padding: PaddingParams,
    model: BertModel,
}

enum Model {
    Bert(Box<Encoder>),
    Qwen(Box<qwen::Encoder>),
}

// Keep one immutable revision warm and bound simultaneous CPU activation buffers.
static CACHE: Mutex<Option<(PathBuf, Model)>> = Mutex::new(None);

pub(crate) fn evict(dir: &Path) {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.as_ref().is_some_and(|(path, _)| path == dir) {
        cache.take();
    }
}

fn load_bert(dir: &Path) -> Result<Encoder> {
    for (name, limit) in [
        ("config.json", 1_048_576),
        ("tokenizer.json", 64 * 1024 * 1024),
        ("model.safetensors", 1024 * 1024 * 1024),
    ] {
        ensure!(
            fs::metadata(dir.join(name))?.len() <= limit,
            "BERT 模型文件过大：{name}"
        );
    }
    let config: Config = serde_json::from_slice(&fs::read(dir.join("config.json"))?)?;
    ensure!(
        config.model_type.as_deref() == Some("bert")
            && config.hidden_size == DIMENSION
            && (MAX_TOKENS..=8192).contains(&config.max_position_embeddings)
            && (1..=12).contains(&config.num_hidden_layers)
            && (1..=32).contains(&config.num_attention_heads)
            && DIMENSION.is_multiple_of(config.num_attention_heads)
            && (1..=4096).contains(&config.intermediate_size)
            && (1..=300_000).contains(&config.vocab_size)
            && (1..=16).contains(&config.type_vocab_size)
            && config.pad_token_id < config.vocab_size
            && config.layer_norm_eps.is_finite()
            && config.layer_norm_eps > 0.0,
        "内置模型配置与多语言 MiniLM 不兼容，请校验模型"
    );
    let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| anyhow!("加载内置模型分词器失败（{e}）"))?;
    ensure!(
        tokenizer
            .get_post_processor()
            .is_none_or(|p| p.added_tokens(false) < MAX_TOKENS - 24),
        "BERT 分词器添加的特殊标记过多"
    );
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: MAX_TOKENS,
            stride: 24,
            ..Default::default()
        }))
        .map_err(|e| anyhow!("设置分词器长度失败（{e}）"))?;
    let padding = tokenizer
        .get_padding()
        .cloned()
        .unwrap_or_else(|| PaddingParams {
            pad_id: tokenizer
                .token_to_id("<pad>")
                .unwrap_or(config.pad_token_id as u32),
            ..Default::default()
        });
    tokenizer.with_padding(None);
    ensure!(
        padding.pad_id < config.vocab_size as u32
            && (padding.pad_type_id as usize) < config.type_vocab_size
            && tokenizer
                .get_vocab(true)
                .values()
                .all(|&id| (id as usize) < config.vocab_size),
        "分词器词表超出 BERT 词表范围"
    );
    // The safe buffered loader owns its bytes, including on platforms that lock mapped files.
    let bytes = fs::read(dir.join("model.safetensors")).context("读取内置模型权重失败")?;
    let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, &Device::Cpu)?;
    let model = BertModel::load(vb, &config).context("载入 BERT 权重失败，请校验模型")?;
    Ok(Encoder {
        model,
        tokenizer,
        padding,
    })
}

pub(crate) fn validate_custom_bert(dir: &Path) -> Result<()> {
    let encoder = load_bert(dir)?;
    embed_bert(&encoder, &["StarStack model check / 本地模型校验"])?;
    Ok(())
}

pub(crate) fn embed(dir: &Path, family: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let mut cache = CACHE.lock().map_err(|_| anyhow!("内置模型推理锁不可用"))?;
    if cache.as_ref().is_none_or(|(path, _)| path != dir) {
        cache.take();
        let model = match family {
            "minilm" => Model::Bert(Box::new(load_bert(dir)?)),
            "qwen3-gguf" => Model::Qwen(Box::new(qwen::Encoder::load(dir)?)),
            _ => anyhow::bail!("不支持的内置模型架构"),
        };
        *cache = Some((dir.to_owned(), model));
    }
    match &cache.as_ref().context("内置模型不可用")?.1 {
        Model::Bert(encoder) => embed_bert(encoder, texts),
        Model::Qwen(encoder) => encoder.embed(texts),
    }
}

fn embed_bert(encoder: &Encoder, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let encodings = encoder
        .tokenizer
        .encode_batch(texts.to_vec(), true)
        .map_err(|e| anyhow!("本地分词失败（{e}）"))?;
    let mut means: Vec<_> = (0..texts.len()).map(|_| Mean::default()).collect();
    let mut batch = Vec::with_capacity(16);
    for (i, mut first) in encodings.into_iter().enumerate() {
        let rest = first.take_overflowing();
        for window in std::iter::once(first).chain(rest) {
            batch.push((i, window));
            if batch.len() == 16 {
                infer_batch(encoder, &mut batch, &mut means)?;
                batch.clear();
            }
        }
    }
    if !batch.is_empty() {
        infer_batch(encoder, &mut batch, &mut means)?;
    }
    means.into_iter().map(Mean::finish).collect()
}

fn infer_batch(
    encoder: &Encoder,
    encodings: &mut [(usize, Encoding)],
    means: &mut [Mean],
) -> Result<()> {
    let width = encodings
        .iter()
        .map(|(_, window)| window.len())
        .max()
        .unwrap_or(0);
    ensure!(width > 0 && width <= MAX_TOKENS, "内置模型分词结果长度无效");
    let padding = &encoder.padding;
    for (_, window) in encodings.iter_mut() {
        window.pad(
            width,
            padding.pad_id,
            padding.pad_type_id,
            &padding.pad_token,
            padding.direction,
        );
    }
    let shape = (encodings.len(), width);
    let tokens = encodings
        .iter()
        .flat_map(|(_, e)| e.get_ids().iter().copied())
        .collect();
    let mask = encodings
        .iter()
        .flat_map(|(_, e)| e.get_attention_mask().iter().copied())
        .collect();
    let types = encodings
        .iter()
        .flat_map(|(_, e)| e.get_type_ids().iter().copied())
        .collect();
    let tokens = Tensor::from_vec(tokens, shape, &Device::Cpu)?;
    let types = Tensor::from_vec(types, shape, &Device::Cpu)?;
    let mask = Tensor::from_vec(mask, shape, &Device::Cpu)?;
    let hidden = encoder
        .model
        .forward(&tokens, &types, Some(&mask))
        .context("内置模型 CPU 推理失败")?;
    let weights = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
    let sums = hidden.broadcast_mul(&weights)?.sum(1)?;
    let pooled = sums.broadcast_div(&weights.sum(1)?.clamp(1e-9, f32::MAX)?)?;
    let vectors = pooled.to_vec2::<f32>()?;
    ensure!(
        vectors.len() == encodings.len() && vectors.iter().all(|v| v.len() == DIMENSION),
        "内置模型返回无效的向量形状"
    );
    for ((owner, _), vector) in encodings.iter().zip(vectors) {
        means[*owner].push(&vector)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "set STARSTACK_TEST_MODEL_DIR to the pinned, verified MiniLM revision directory"]
    fn multilingual_search_preserves_text_after_the_first_token_window() -> Result<()> {
        let dir = PathBuf::from(
            std::env::var_os("STARSTACK_TEST_MODEL_DIR")
                .context("missing STARSTACK_TEST_MODEL_DIR")?,
        );
        let prefix = "发布说明与安装要求以及平台支持和使用方法。".repeat(18);
        let photos = format!(
            "{prefix}{}",
            "功能是查找相册中的重复照片并清理相似图片。".repeat(6)
        );
        let memory = format!(
            "{prefix}{}",
            "功能是统计内存分配和检测进程的性能瓶颈。".repeat(6)
        );
        let rows = embed(
            &dir,
            "minilm",
            &[
                &photos,
                &memory,
                "如何清理相似照片？",
                "查找内存分配性能瓶颈",
            ],
        )?;
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        for i in 0..2 {
            assert!(dot(&rows[i], &rows[i + 2]) > dot(&rows[1 - i], &rows[i + 2]) + 0.025);
        }
        let solo = embed(&dir, "minilm", &["如何清理相似照片？"])?;
        assert!(dot(&rows[2], &solo[0]) > 0.999);
        Ok(())
    }
}
