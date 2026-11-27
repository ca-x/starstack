//! Embedding-only Qwen3 forward over GGUF quantized weights. Candle's generative
//! Qwen3 forward projects the same final hidden state to vocabulary logits.
use super::Mean;
use anyhow::{Context, Result, anyhow, ensure};
use candle_core::{
    DType, Device, Tensor,
    quantized::{QTensor, gguf_file},
};
use candle_nn::{
    Activation, Module,
    attention::{AttnMask, flash_attn},
};
use candle_transformers::{
    models::{
        quantized_qwen3::{Gguf, RotaryEmbedding},
        with_tracing::QMatMul,
    },
    quantized_nn::RmsNorm,
};
use std::{fs::File, io::BufReader, path::Path};
use tokenizers::{Tokenizer, TruncationParams};

const WIDTH: usize = 1024;
const HEADS: usize = 16;
const KV_HEADS: usize = 8;
const HEAD_DIM: usize = 128;
const WINDOW: usize = 512;
pub(crate) const PROTOCOL: &str = "qwen3-causal-eot-last-512-stride64-v2";

struct Block {
    attention_norm: RmsNorm,
    feedforward_norm: RmsNorm,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    q: QMatMul,
    k: QMatMul,
    v: QMatMul,
    out: QMatMul,
    gate: QMatMul,
    up: QMatMul,
    down: QMatMul,
}

impl Block {
    fn forward(&self, x: &Tensor, rotary: &RotaryEmbedding) -> candle_core::Result<Tensor> {
        let (_, len, _) = x.dims3()?;
        let h = self.attention_norm.forward(x)?;
        let q = self
            .q
            .forward(&h)?
            .reshape((1, len, HEADS, HEAD_DIM))?
            .apply(&self.q_norm)?
            .transpose(1, 2)?;
        let k = self
            .k
            .forward(&h)?
            .reshape((1, len, KV_HEADS, HEAD_DIM))?
            .apply(&self.k_norm)?
            .transpose(1, 2)?;
        let v = self.v.forward(&h)?.reshape((1, len, KV_HEADS, HEAD_DIM))?;
        let (q, k) = rotary.apply(&q, &k, 0)?;
        let context = flash_attn::<f32>(
            &q.transpose(1, 2)?.contiguous()?,
            &k.transpose(1, 2)?.contiguous()?,
            &v.contiguous()?,
            1.0 / (HEAD_DIM as f32).sqrt(),
            AttnMask::causal(),
            None,
            None,
        )?;
        let context = context
            .transpose(1, 2)?
            .reshape((1, len, HEADS * HEAD_DIM))?
            .apply(&self.out)?;
        let residual = (x + context)?;
        let h = self.feedforward_norm.forward(&residual)?;
        let ff = (self.gate.forward(&h)?.apply(&Activation::Silu)? * self.up.forward(&h)?)?
            .apply(&self.down)?;
        residual + ff
    }
}

pub(super) struct Encoder {
    tokenizer: Tokenizer,
    table: QTensor,
    rotary: RotaryEmbedding,
    blocks: Vec<Block>,
    norm: RmsNorm,
}

impl Encoder {
    pub(super) fn load(dir: &Path) -> Result<Self> {
        let mut reader = BufReader::new(File::open(dir.join("model.gguf"))?);
        let content = gguf_file::Content::read(&mut reader)?;
        let get = |key: &str| {
            content
                .metadata
                .get(key)
                .with_context(|| format!("GGUF 缺少 {key}"))
        };
        ensure!(
            get("general.architecture")?.to_string()? == "qwen3"
                && get("general.basename")?.to_string()? == "Qwen3-Embedding"
                && get("general.file_type")?.to_u32()? == 15
                && get("qwen3.embedding_length")?.to_u32()? == WIDTH as u32
                && get("qwen3.block_count")?.to_u32()? == 28
                && get("qwen3.attention.head_count")?.to_u32()? == HEADS as u32
                && get("qwen3.attention.head_count_kv")?.to_u32()? == KV_HEADS as u32
                && get("qwen3.attention.key_length")?.to_u32()? == HEAD_DIM as u32
                && get("qwen3.attention.value_length")?.to_u32()? == HEAD_DIM as u32
                && get("qwen3.context_length")?.to_u32()? >= WINDOW as u32,
            "文件与 Qwen3-Embedding-0.6B Q4_K_M 不兼容"
        );
        let eps = f64::from(get("qwen3.attention.layer_norm_rms_epsilon")?.to_f32()?);
        let theta = f64::from(get("qwen3.rope.freq_base")?.to_f32()?);
        let rotary = RotaryEmbedding::new(DType::F32, HEAD_DIM, WINDOW, theta, &Device::Cpu)?;
        let mut gg = Gguf::new(content, reader, Device::Cpu);
        let table = gg.tensor("token_embd.weight")?;
        let mut blocks = Vec::with_capacity(28);
        for i in 0..28 {
            let w = |name: &str| format!("blk.{i}.{name}.weight");
            blocks.push(Block {
                attention_norm: gg.rms_norm(&w("attn_norm"), eps)?,
                feedforward_norm: gg.rms_norm(&w("ffn_norm"), eps)?,
                q_norm: gg.rms_norm(&w("attn_q_norm"), eps)?,
                k_norm: gg.rms_norm(&w("attn_k_norm"), eps)?,
                q: gg.qmatmul(&w("attn_q"))?,
                k: gg.qmatmul(&w("attn_k"))?,
                v: gg.qmatmul(&w("attn_v"))?,
                out: gg.qmatmul(&w("attn_output"))?,
                gate: gg.qmatmul(&w("ffn_gate"))?,
                up: gg.qmatmul(&w("ffn_up"))?,
                down: gg.qmatmul(&w("ffn_down"))?,
            });
        }
        let norm = gg.rms_norm("output_norm.weight", eps)?;
        let mut tokenizer =
            Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow!("{e}"))?;
        tokenizer
            .with_padding(None)
            .with_truncation(Some(TruncationParams {
                max_length: WINDOW,
                stride: 64,
                ..Default::default()
            }))
            .map_err(|e| anyhow!("{e}"))?;
        Ok(Self {
            tokenizer,
            table,
            rotary,
            blocks,
            norm,
        })
    }

    pub(super) fn embed(
        &self,
        texts: &[&str],
        observed: &mut dyn FnMut(crate::index::EmbeddingWork),
    ) -> Result<Vec<Vec<f32>>> {
        texts
            .iter()
            .map(|text| {
                // The official embedding tokenizer appends <|endoftext|> (151643) to
                // every window. The generative EOS in some GGUF exports is different.
                let started = std::time::Instant::now();
                let mut first = self
                    .tokenizer
                    .encode(*text, true)
                    .map_err(|e| anyhow!("{e}"))?;
                observed(crate::index::EmbeddingWork::tokenized(started.elapsed()));
                let rest = first.take_overflowing();
                let mut mean = Mean::default();
                for window in std::iter::once(first).chain(rest) {
                    let ids = window.get_ids();
                    ensure!(
                        ids.len() <= WINDOW && ids.last() == Some(&151643),
                        "Qwen3 分词器未添加正确的结束标记"
                    );
                    let started = std::time::Instant::now();
                    let input = Tensor::new(ids, &Device::Cpu)?.unsqueeze(0)?;
                    let mut h = self.table.embedding(&input)?;
                    for block in &self.blocks {
                        h = block.forward(&h, &self.rotary)?;
                    }
                    let last = h
                        .narrow(1, ids.len() - 1, 1)?
                        .apply(&self.norm)?
                        .reshape(WIDTH)?
                        .to_vec1::<f32>()?;
                    mean.push_weighted(&last, ids.len() as f64)?;
                    observed(crate::index::EmbeddingWork {
                        tokens: Some(ids.len()),
                        tokenizer: std::time::Duration::ZERO,
                        inference: started.elapsed(),
                    });
                }
                mean.finish()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "set STARSTACK_TEST_QWEN_DIR to the verified Q4_K_M GGUF and official tokenizer"]
    fn quantized_last_token_embeddings_rank_cross_language_matches() -> Result<()> {
        let path = std::path::PathBuf::from(
            std::env::var_os("STARSTACK_TEST_QWEN_DIR")
                .context("missing STARSTACK_TEST_QWEN_DIR")?,
        );
        let encoder = Encoder::load(&path)?;
        let docs = encoder.embed(
            &[
                "Find duplicate photos in a local image collection.",
                "Profile process CPU and memory bottlenecks.",
            ],
            &mut |_| {},
        )?;
        let queries = encoder.embed(
            &[
                "Instruct: Retrieve matching GitHub repositories.\nQuery: 清理相册中重复照片",
                "Instruct: Retrieve matching GitHub repositories.\nQuery: 进程内存与 CPU 性能瓶颈",
            ],
            &mut |_| {},
        )?;
        for (idx, q) in queries.iter().enumerate() {
            let dot = |d: &[f32]| q.iter().zip(d).map(|(a, b)| a * b).sum::<f32>();
            assert_eq!(q.len(), WIDTH);
            assert!((dot(q) - 1.0).abs() < 1e-4);
            assert!(dot(&docs[idx]) > dot(&docs[1 - idx]) + 0.1);
        }
        Ok(())
    }
}
