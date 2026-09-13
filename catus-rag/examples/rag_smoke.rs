//! End-to-end smoke for the real embedding engine (fastembed/Gemma).
//!
//! Indexes whitelisted files under the given paths, then answers queries
//! through the full hybrid pipeline. First run downloads the
//! EmbeddingGemma-300M-Q4 model (~190MB) into the fastembed cache and needs
//! a shared ONNX Runtime (`ORT_DYLIB_PATH=<dir>/libonnxruntime.so...`).
//!
//! Usage: cargo run -p catus-rag --example rag_smoke -- <workspace> [query...]

use std::path::PathBuf;

use catus_rag::config::RagConfig;
use catus_rag::embed::FastembedEmbedder;
use catus_rag::filter;
use catus_rag::index::{HybridIndex, RawDoc, SearchOpts};

fn main() {
    let mut args = std::env::args().skip(1);
    let workspace = args.next().unwrap_or_else(|| ".".to_string());
    let queries: Vec<String> = match args.next() {
        Some(first) => std::iter::once(first).chain(args).collect(),
        None => vec!["prompt cache investigation".to_string()],
    };
    let workspace = PathBuf::from(workspace);
    let index_dir = workspace.join(".sutcac").join("rag");
    let cfg = RagConfig::default();
    let cache_dir = index_dir.join("model-cache");
    let embedder = FastembedEmbedder::new(cfg.embedding.clone(), cache_dir);
    let cfg2 = cfg.clone();
    let cfg3 = cfg.clone();
    let mut index =
        HybridIndex::open(&index_dir, cfg2, Box::new(embedder)).expect("open hybrid index");
    println!("index dir: {}", index_dir.display());

    let roots: Vec<PathBuf> = vec![
        workspace.join("hybrid-retrieval-design.md"),
        workspace.join("AGENTS.md"),
        workspace.join("catus-rag").join("src"),
        workspace.join("sutcac-sh").join("src").join("exec.rs"),
        workspace.join("catus-core").join("src").join("mcp.rs"),
    ];
    let files = filter::collect_indexable(&roots, &cfg3.filter).expect("collect files");
    println!("collected {} indexable file(s)", files.len());
    let docs: Vec<RawDoc> = files
        .iter()
        .filter_map(|p| {
            std::fs::read_to_string(p).ok().map(|t| RawDoc {
                path: p.clone(),
                text: t,
            })
        })
        .collect();
    let live = index.add_documents(&docs).expect("add documents");
    println!(
        "live chunks after add: {} (pending dense replay: {})",
        live,
        index.pending_dense_replay()
    );
    if index.pending_dense_replay() {
        index.replay_dense().expect("replay dense lane");
        println!("dense lane replayed");
    }

    for query in &queries {
        let hits = index
            .search(query, 5, &SearchOpts::default())
            .expect("search");
        println!("\n=== query: {}", query);
        for hit in &hits {
            println!(
                "  {:.4} [{:?}] {}:{} @{}",
                hit.score,
                hit.source,
                hit.path,
                hit.symbol,
                hit.text.lines().next().unwrap_or("")
            );
        }
        if hits.is_empty() {
            println!("  (no hits)");
        }
    }
    let _ = workspace; // keep lints calm
}
