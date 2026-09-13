//! Error type for catus-rag.
//!
//! Hand-written (mirrors `McpError` in catus-core); the workspace does not
//! use `eyre`/`thiserror`.

use std::fmt;

/// Errors produced by the hybrid index pipeline.
#[derive(Debug)]
pub enum RagError {
    /// Embedding model load / inference failure.
    Embed(String),
    /// Index (HNSW / Tantivy / mapping) failure.
    Index(String),
    /// Filesystem failure.
    Io(std::io::Error),
    /// Configuration failure (unknown model, bad dimension, ...).
    Config(String),
}

impl fmt::Display for RagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Embed(msg) => write!(f, "rag embed error: {}", msg),
            Self::Index(msg) => write!(f, "rag index error: {}", msg),
            Self::Io(e) => write!(f, "rag io error: {}", e),
            Self::Config(msg) => write!(f, "rag config error: {}", msg),
        }
    }
}

impl std::error::Error for RagError {}

/// Convenience result alias used across the crate.
pub type RagResult<T> = Result<T, RagError>;

impl From<std::io::Error> for RagError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for RagError {
    fn from(e: serde_json::Error) -> Self {
        Self::Index(format!("mapping serialization: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_variant() {
        assert_eq!(
            RagError::Embed("boom".into()).to_string(),
            "rag embed error: boom"
        );
        assert_eq!(
            RagError::Config("bad".into()).to_string(),
            "rag config error: bad"
        );
    }
}
