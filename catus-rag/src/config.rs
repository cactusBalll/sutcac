//! Rag configuration structures.
//!
//! This is the serde form of the `[rag]` config table consumed by catus-core
//! (`AppConfig.rag`) and by [`crate::index::HybridIndex`]. All fields carry
//! defaults, and `AppConfig` itself is `#[serde(default)]`, so a missing
//! `[rag]` table yields the disabled default ([`RagConfig::default`], which
//! has `enabled = false`).

use serde::{Deserialize, Serialize};

/// Root `[rag]` configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RagConfig {
    /// Master switch: when false, no tools are registered and no injection
    /// happens.
    pub enabled: bool,
    /// Expose `rag_search` / `rag_index` as tools to the LLM.
    pub tools_enabled: bool,
    /// Per-turn retrieval injection (append-only system message after the
    /// user message).
    pub auto_inject: bool,
    /// Max chunks injected per turn.
    pub inject_top_k: usize,
    /// Max total characters injected per turn.
    pub inject_max_chars: usize,
    /// Index directory. Empty or unset means the workspace default
    /// `<cwd>/.sutcac/rag`, resolved by the caller.
    pub index_dir: String,
    /// Embedded engine settings.
    pub embedding: EmbeddingConfig,
    /// Dense lane settings.
    pub dense: DenseConfig,
    /// Lexical lane settings.
    pub bm25: Bm25Config,
    /// Fusion settings.
    pub fusion: FusionConfig,
    /// Indexable-file filter.
    pub filter: FilterConfig,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tools_enabled: true,
            auto_inject: false,
            inject_top_k: 3,
            inject_max_chars: 4000,
            index_dir: String::new(),
            embedding: EmbeddingConfig::default(),
            dense: DenseConfig::default(),
            bm25: Bm25Config::default(),
            fusion: FusionConfig::default(),
            filter: FilterConfig::default(),
        }
    }
}

/// The embedding engine. Only the fixed variant catalogue is supported to
/// keep index metadata validation strict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Model name; must be `EmbeddingGemma300MQ4`.
    pub model: String,
    /// Query-side prefix. Must match the model card verbatim; docs are
    /// embedded without prefix (asymmetric encoding, design §4.2).
    pub query_prefix: String,
    /// MRL truncation dimension (768 / 512 / 256 / 128).
    pub mrl_dim: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "EmbeddingGemma300MQ4".to_string(),
            query_prefix: "task: search result | query:".to_string(),
            mrl_dim: 768,
        }
    }
}

/// Dense (HNSW) lane parameters (design §4.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DenseConfig {
    /// HNSW graph connectivity.
    pub m: u16,
    /// Build-time search depth.
    pub ef_construction: u16,
    /// Query-time search depth (recall knob, 32–256).
    pub ef_search: u16,
    /// Candidates fetched per lane before fusion.
    pub recall_top: usize,
}

impl Default for DenseConfig {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 128,
            ef_search: 64,
            recall_top: 50,
        }
    }
}

/// Lexical (BM25) lane parameters (design §4.4). Tantivy's own Okapi
/// parameters (k1=1.2, b=0.75) are used unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bm25Config {
    /// Candidates fetched per lane before fusion.
    pub recall_top: usize,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self { recall_top: 50 }
    }
}

/// Fusion parameters (design §4.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FusionConfig {
    /// `rrf` only in this iteration.
    pub method: String,
    /// RRF constant k (Cormack et al., 2009 recommend 60).
    pub rrf_k: u32,
    /// Weighted-fusion alpha; reserved, unused by `rrf`.
    pub alpha: f32,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            method: "rrf".to_string(),
            rrf_k: 60,
            alpha: 0.4,
        }
    }
}

/// Indexable-file whitelist (extension based) plus concrete excludes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    /// Extra extensions appended to the built-in whitelist (no leading dot).
    pub extra_exts: Vec<String>,
    /// Path components that are always skipped (directory or file names).
    pub exclude: Vec<String>,
}

impl FilterConfig {
    /// Extensions whitelisted out of the box: code plus Markdown documents.
    pub const CODE_EXTS: &'static [&'static str] = &[
        "rs", "ts", "tsx", "js", "jsx", "py", "go", "c", "h", "cc", "cpp", "hpp", "java", "kt",
        "swift", "rb", "php", "cs", "sh", "lua", "sql", "toml", "yaml", "yml", "json", "proto",
    ];

    pub const DOC_EXTS: &'static [&'static str] = &["md", "markdown"];

    pub const DEFAULT_EXCLUDE: &'static [&'static str] = &[
        ".git",
        ".hg",
        ".svn",
        "target",
        "node_modules",
        "dist",
        "build",
        "__pycache__",
        ".venv",
        ".sutcac",
    ];
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            extra_exts: Vec::new(),
            exclude: Self::DEFAULT_EXCLUDE
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let cfg = RagConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.tools_enabled);
        assert!(!cfg.auto_inject);
        assert_eq!(cfg.inject_top_k, 3);
        assert_eq!(cfg.embedding.model, "EmbeddingGemma300MQ4");
        assert_eq!(cfg.fusion.rrf_k, 60);
        assert!(cfg.index_dir.is_empty());
    }

    #[test]
    fn parses_partial_toml() {
        let cfg: RagConfig = toml::from_str(
            r#"
            enabled = true
            auto_inject = true
        "#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert!(cfg.auto_inject);
        assert!(cfg.tools_enabled);
        assert_eq!(cfg.dense.m, 16);
    }

    #[test]
    fn roundtrips_toml() {
        let cfg = RagConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: RagConfig = toml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }
}
