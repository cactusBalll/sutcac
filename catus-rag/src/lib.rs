//! catus-rag: local hybrid retrieval for catus.
//!
//! Dense (EmbeddingGemma + HNSW) + lexical (Tantivy BM25) retrieval fused
//! with Reciprocal Rank Fusion, per `hybrid-retrieval-design.md`. Pure Rust,
//! in-process, no runtime service dependencies; the embedding model is
//! downloaded lazily on first use.
//!
//! Layout note: modules follow the workspace convention `foo.rs` + `foo/`
//! (never `mod.rs`).

pub mod chunker;
pub mod config;
pub mod embed;
pub mod error;
pub mod filter;
pub mod index;
pub mod tokenizer;

pub use chunker::ChunkMeta;
pub use config::RagConfig;
pub use error::{RagError, RagResult};
pub use index::{Hit, HybridIndex, SearchOpts, SourceLane};
