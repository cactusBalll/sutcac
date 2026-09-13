//! Embedding layer wrapping fastembed.
//!
//! The active model is fixed per index and recorded in index metadata
//! (`metadata.json`), so switching models or MRL dims requires a rebuild
//! (design §7). Encoding is asymmetric: docs are embedded without prefix,
//! queries with [`QUERY_PREFIX`].

use std::path::PathBuf;
use std::sync::Mutex;

use crate::config::EmbeddingConfig;
use crate::error::RagError;

/// Query-side prefix; must match the EmbeddingGemma model card verbatim.
pub const RETRIEVAL_QUERY_PREFIX: &str = "task: search result | query:";

/// Embedding backend abstraction. Kept as a trait so tests can substitute a
/// deterministic hash-based embedder without touching the model.
pub trait EmbedBackend: Send {
    /// Embed `texts`; when `is_query` the query prefix is prepended.
    fn embed_texts(&self, texts: &[String], is_query: bool) -> crate::RagResult<Vec<Vec<f32>>>;
    /// Full dimension produced by the model (before MRL truncation).
    fn dim(&self) -> usize;
}

/// fastembed-backed embedder for `EmbeddingGemma300MQ4`.
pub struct FastembedEmbedder {
    cfg: EmbeddingConfig,
    cache_dir: PathBuf,
    inner: Mutex<Option<fastembed::TextEmbedding>>,
}

impl FastembedEmbedder {
    /// Create the embedder; the model is loaded (and downloaded on first
    /// use, ~190MB) on the first [`EmbedBackend::embed_texts`] call.
    pub fn new(cfg: EmbeddingConfig, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cfg,
            cache_dir: cache_dir.into(),
            inner: Mutex::new(None),
        }
    }

    fn load_inner(&self) -> crate::RagResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| RagError::Embed("embedder mutex poisoned".to_string()))?;
        if guard.is_none() {
            let model = parse_model(&self.cfg.model)?;
            if self.cfg.query_prefix != RETRIEVAL_QUERY_PREFIX {
                return Err(RagError::Config(format!(
                    "embedding.query_prefix must be {:?}",
                    RETRIEVAL_QUERY_PREFIX
                )));
            }
            let opts = fastembed::InitOptions::new(model)
                .with_cache_dir(self.cache_dir.display().to_string().into())
                .with_show_download_progress(false);
            let engine = fastembed::TextEmbedding::try_new(opts)
                .map_err(|e| RagError::Embed(e.to_string()))?;
            *guard = Some(engine);
        }
        Ok(())
    }
}

impl EmbedBackend for FastembedEmbedder {
    fn dim(&self) -> usize {
        768
    }

    fn embed_texts(&self, texts: &[String], is_query: bool) -> crate::RagResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.load_inner()?;
        let inputs: Vec<String> = texts
            .iter()
            .map(|t| {
                if is_query {
                    format!("{}{}", self.cfg.query_prefix, t)
                } else {
                    t.clone()
                }
            })
            .collect();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| RagError::Embed("embedder mutex poisoned".to_string()))?;
        let engine = guard.as_mut().expect("engine loaded above");
        let raw = engine
            .embed(inputs, None)
            .map_err(|e| RagError::Embed(e.to_string()))?;
        let mut out = Vec::with_capacity(raw.len());
        for vec in raw {
            out.push(apply_mrl(vec, self.cfg.mrl_dim));
        }
        Ok(out)
    }
}

/// Map a configured model name onto the supported fastembed variant.
fn parse_model(name: &str) -> crate::RagResult<fastembed::EmbeddingModel> {
    match name {
        "EmbeddingGemma300MQ4" => Ok(fastembed::EmbeddingModel::EmbeddingGemma300MQ4),
        other => Err(RagError::Config(format!(
            "unsupported embedding model: {} (supported: EmbeddingGemma300MQ4)",
            other
        ))),
    }
}

/// L2-normalize `vec` in place.
pub fn l2_normalize(vec: &mut Vec<f32>) {
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
}

/// MRL truncation + renormalization to `mrl_dim`.
pub fn apply_mrl(mut vec: Vec<f32>, mrl_dim: usize) -> Vec<f32> {
    if mrl_dim > 0 && mrl_dim < vec.len() {
        vec.truncate(mrl_dim);
    }
    l2_normalize(&mut vec);
    vec
}

/// Deterministic fake embedder for tests (hash-based unit vectors). Provides
/// semantic ordering for word overlap: shared tokens pull vectors together.
pub struct FakeEmbedder {
    pub dim: usize,
}

impl EmbedBackend for FakeEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_texts(&self, texts: &[String], _is_query: bool) -> crate::RagResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_vec(t)).collect())
    }
}

fn hash_vec(text: &str) -> Vec<f32> {
    let mut acc = vec![0.0f32; 64];
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        let h = fxhash_str(tok);
        for j in 0..acc.len() {
            let bit = ((h >> ((j * 7 + h as usize % 13) % 61)) & 1) == 1;
            acc[j] += if bit { 1.0 } else { -0.5 };
        }
    }
    apply_mrl(acc, 64)
}

fn fxhash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mrl_truncates_and_normalizes() {
        let v = apply_mrl(vec![3.0, 0.0, 4.0], 2);
        assert_eq!(v.len(), 2);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fake_is_deterministic() {
        let e = FakeEmbedder { dim: 64 };
        let a = e
            .embed_texts(&["parse_context_window".into()], false)
            .unwrap();
        let b = e
            .embed_texts(&["parse_context_window".into()], false)
            .unwrap();
        assert_eq!(a[0], b[0]);
    }
}
