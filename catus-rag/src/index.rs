//! Hybrid index: dense (HNSW) + lexical (Tantivy) lanes sharing one
//! `doc_id` space, fused with RRF (design §4.5–4.6). Deletes are logical in
//! the mapping/HNSW lanes and physical in the lexical lane; query results
//! always filter the logical set first.

pub mod dense;
pub mod lexical;
pub mod vectors;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chunker::{self, Chunk};
use crate::config::RagConfig;
use crate::embed::EmbedBackend;
use crate::error::{RagError, RagResult};

/// Index metadata (`metadata.json`); model / MRL dimension changes invalidate
/// loading and require a rebuild (design §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexMeta {
    model: String,
    mrl_dim: usize,
}

/// Metadata kept for every live chunk.
pub use crate::chunker::ChunkMeta as HitMeta;

/// Fusion provenance of a hit, exposed for observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLane {
    DenseOnly,
    Bm25Only,
    Both,
}

/// Query-side search options.
#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    /// Restrict results to paths with this prefix.
    pub path_prefix: Option<String>,
    /// Restrict results to one language tag.
    pub lang: Option<String>,
}

/// One search result.
#[derive(Debug, Clone)]
pub struct Hit {
    pub doc_id: u64,
    /// RRF fused score (rank-based, design §4.5).
    pub score: f32,
    pub path: String,
    pub lang: String,
    pub symbol: String,
    /// Stored chunk text (回表).
    pub text: String,
    pub source: SourceLane,
}

/// A file to be chunked and indexed.
pub struct RawDoc {
    pub path: PathBuf,
    pub text: String,
}

const META_FILE: &str = "metadata.json";
const MAPPING_FILE: &str = "mapping.json";
const DELETED_FILE: &str = "deleted.json";

/// The hybrid retrieval index. Writes take `&mut self`; search is `&self`
/// (the embedder carries its own interior mutability).
pub struct HybridIndex {
    cfg: RagConfig,
    root: PathBuf,
    embedder: Box<dyn EmbedBackend>,
    dense: Option<dense::DenseIndex>,
    vectors: Option<vectors::VectorStore>,
    lexical: lexical::LexicalIndex,
    mapping: HashMap<u64, chunker::ChunkMeta>,
    deleted: HashSet<u64>,
    next_id: u64,
}

impl HybridIndex {
    /// Open (or create) the hybrid index at `dir`. A stored-metadata
    /// mismatch with the configured model / dimension refuses to load.
    pub fn open(dir: &Path, cfg: RagConfig, embedder: Box<dyn EmbedBackend>) -> RagResult<Self> {
        std::fs::create_dir_all(dir)?;
        let root = dir.to_path_buf();
        let meta = read_meta(&root)?;
        if let Some(meta) = &meta {
            if meta.model != cfg.embedding.model || meta.mrl_dim != cfg.embedding.mrl_dim {
                return Err(RagError::Config(format!(
                    "rag index at {} was built with model={} mrl_dim={}; the current config is \
                     model={} mrl_dim={} — rebuild the index (/rag rebuild)",
                    root.display(),
                    meta.model,
                    meta.mrl_dim,
                    cfg.embedding.model,
                    cfg.embedding.mrl_dim
                )));
            }
        }
        // Dense lane: rebuild the HNSW graph from persisted vectors when
        // available (fast, no model); otherwise defer to the re-embed
        // replay until the next write / explicit request (search still
        // works through the lexical lane meanwhile).
        let lexical = lexical::LexicalIndex::open(&root)?;
        let (mapping, deleted) = read_state(&root)?;
        let dim = cfg.embedding.mrl_dim;
        let (dense, vectors) = if mapping.is_empty() {
            (
                Some(dense::DenseIndex::new(
                    1024,
                    cfg.dense.m.max(2) as usize,
                    cfg.dense.ef_construction.max(4) as usize,
                )),
                Some(vectors::VectorStore::fresh(dim)),
            )
        } else {
            match Self::rebuild_dense_from_store(&root, &mapping, &deleted, &cfg, dim) {
                Ok((graph, store)) => (Some(graph), Some(store)),
                // Persisted vectors unreadable: fall back to the re-embed
                // replay path; it rewrites the store on completion.
                Err(e) => {
                    tracing::debug!(target: "rag", "vector store fallback: {}", e);
                    (None, None)
                }
            }
        };
        let next_id = mapping.keys().copied().max().map(|m| m + 1).unwrap_or(0);
        let index = Self {
            cfg,
            root,
            embedder,
            dense,
            vectors,
            lexical,
            mapping,
            deleted,
            next_id,
        };
        index.save_meta()?;
        Ok(index)
    }

    /// Rebuild the HNSW graph from the persisted vector store. The graph
    /// is always freshly built, so no external borrow caps its lifetime.
    fn rebuild_dense_from_store(
        root: &Path,
        mapping: &HashMap<u64, chunker::ChunkMeta>,
        deleted: &HashSet<u64>,
        cfg: &RagConfig,
        dim: usize,
    ) -> RagResult<(dense::DenseIndex, vectors::VectorStore)> {
        let Some(store) = vectors::VectorStore::open(root, dim)? else {
            return Err(RagError::Index(format!("{} missing", vectors::DATA_FILE)));
        };
        let live: Vec<u64> = mapping
            .keys()
            .copied()
            .filter(|id| !deleted.contains(id))
            .collect();
        // The stored set must exactly cover the live mapping ids.
        if store.ids() != live.iter().copied().collect() {
            return Err(RagError::Index(format!(
                "vector store covers {} ids, live mapping has {} — falling back to replay",
                store.ids().len(),
                live.len()
            )));
        }
        let graph = dense::DenseIndex::new(
            live.len().max(1000),
            cfg.dense.m.max(2) as usize,
            cfg.dense.ef_construction.max(4) as usize,
        );
        for id in live {
            let vec = store
                .read(root, id)?
                .ok_or_else(|| RagError::Index(format!("vector store missing doc {}", id)))?;
            graph.insert(&vec, id)?;
        }
        Ok((graph, store))
    }

    /// True when the dense lane still needs its replay rebuild.
    pub fn pending_dense_replay(&self) -> bool {
        self.dense.is_none()
    }

    /// Replay the dense lane from the stored chunk texts (design §4.6
    /// rebuild path): fetch stored rows, re-embed, refit the HNSW graph.
    pub fn replay_dense(&mut self) -> RagResult<()> {
        if self.dense.is_some() {
            return Ok(());
        }
        let rec: Vec<(u64, String)> = self
            .mapping
            .keys()
            .copied()
            .filter(|id| !self.deleted.contains(id))
            .map(|id| Ok((id, self.lexical.row_for(id)?.unwrap_or_default())))
            .collect::<RagResult<_>>()?;
        let texts: Vec<String> = rec.iter().map(|(_, t)| t.clone()).collect();
        let vectors = self.embedder.embed_texts(&texts, false)?;
        let graph = dense::DenseIndex::new(
            texts.len().max(1000),
            self.cfg.dense.m.max(2) as usize,
            self.cfg.dense.ef_construction.max(4) as usize,
        );
        let pairs: Vec<(u64, Vec<f32>)> = rec
            .into_iter()
            .zip(vectors)
            .map(|((id, _text), vec)| (id, vec))
            .collect();
        for (id, vec) in &pairs {
            graph.insert(vec, *id)?;
        }
        self.dense = Some(graph);
        // Charge the once-per-run re-embed cost into a persistent store so
        // the next process start rebuilds the graph without the model.
        let dim = self
            .vectors
            .as_ref()
            .map(|v| v.dim())
            .unwrap_or(self.cfg.embedding.mrl_dim);
        let mut store = self
            .vectors
            .replace(vectors::VectorStore::fresh(dim))
            .unwrap_or_else(|| vectors::VectorStore::fresh(dim));
        let items: Vec<(u64, &[f32])> = pairs
            .iter()
            .map(|(id, vec)| (*id, vec.as_slice()))
            .collect();
        store.rewrite(&self.root, &items)?;
        self.vectors = Some(store);
        Ok(())
    }

    /// Number of live (non-deleted) chunks.
    pub fn len(&self) -> usize {
        self.mapping.len() - self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Chunk / embed / index a batch of files (design §A.3 flow).
    ///
    /// Returns the number of chunks written in this call.
    pub fn add_documents(&mut self, docs: &[RawDoc]) -> RagResult<usize> {
        // Upsert semantics: replace prior chunks of the same paths before
        // writing; doc ids for replaced paths are retired (mapping kept as
        // tombstones) and lexical rows deleted physically.
        let stale: Vec<u64> = {
            let targets: std::collections::BTreeSet<String> =
                docs.iter().map(|d| d.path.display().to_string()).collect();
            self.mapping
                .iter()
                .filter_map(|(id, meta)| targets.contains(&meta.path).then_some(*id))
                .collect()
        };
        let stale_persist = stale.clone();
        for id in &stale {
            self.mapping.remove(id);
        }
        self.lexical.delete_rows(&stale)?;
        let mut chunks: Vec<Chunk> = Vec::new();
        for doc in docs {
            let lang = chunker::lang_of(&doc.path).to_string();
            for chunk in chunker::split(&doc.path, &lang, &doc.text) {
                chunks.push(chunk);
            }
        }
        if chunks.is_empty() {
            return Ok(0);
        }
        // Writes refit a pending dense replay first.
        self.replay_dense()?;
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        // Doc lane: embedded WITHOUT prefix (§4.2 hard requirement).
        let vectors = self.embedder.embed_texts(&texts, false)?;
        if vectors.len() != chunks.len() {
            return Err(RagError::Embed(format!(
                "embedder returned {} vectors for {} chunks",
                vectors.len(),
                chunks.len()
            )));
        }
        for (chunk, vec) in chunks.into_iter().zip(vectors) {
            let id = self.next_id;
            self.next_id += 1;
            if let Some(graph) = &self.dense {
                graph.insert(&vec, id)?;
            }
            if let Some(store) = &mut self.vectors {
                store.append(&self.root, id, &vec)?;
            }
            self.lexical
                .add_row(id, &chunk.text, &chunk.meta.path, &chunk.meta.lang)?;
            self.mapping.insert(id, chunk.meta);
        }
        // Retire the vectors of replaced chunks so the store keeps
        // mirroring the live mapping.
        for id in &stale_persist {
            if let Some(store) = &mut self.vectors {
                store.remove(*id);
            }
        }
        self.lexical.commit()?;
        self.save_state()?;
        if let Some(store) = &mut self.vectors {
            Self::maybe_compact(&self.root, &self.mapping, &self.deleted, store);
            store.save(&self.root)?;
        }
        Ok(self.mapping.len() - self.deleted.len())
    }

    /// Delete all chunks whose `path` matches (or lives under with a `/`)
    /// one of the given paths. Logical deletion filters both lanes at query
    /// time; the lexical lane also drops matching rows physically.
    pub fn delete_paths(&mut self, paths: &[PathBuf]) -> RagResult<usize> {
        let mut removed: Vec<u64> = Vec::new();
        for (id, meta) in self.mapping.iter() {
            for path in paths {
                let target = path.display().to_string();
                if meta.path == target || meta.path.starts_with(&format!("{}/", target)) {
                    self.deleted.insert(*id);
                    removed.push(*id);
                }
            }
        }
        removed.sort();
        removed.dedup();
        if !removed.is_empty() {
            self.deleted.retain(|id| self.mapping.contains_key(id));
            self.lexical.delete_rows(&removed)?;
            for id in &removed {
                if let Some(store) = &mut self.vectors {
                    store.remove(*id);
                }
            }
            self.save_state()?;
            if let Some(store) = &mut self.vectors {
                Self::maybe_compact(&self.root, &self.mapping, &self.deleted, store);
                store.save(&self.root)?;
            }
        }
        Ok(removed.len())
    }

    /// Compaction trigger: rewrite `vectors.data` once the retired-record
    /// garbage outweighs the live set (with a small floor so sparse
    /// deletions do not churn the files). Any read error during compaction
    /// drops the in-memory store (`self.vectors = None` ahead of this call
    /// via the same fallback contract — here we just skip compaction and
    /// let the next replay rebuild it).
    fn maybe_compact(
        root: &Path,
        mapping: &HashMap<u64, chunker::ChunkMeta>,
        deleted: &HashSet<u64>,
        store: &mut vectors::VectorStore,
    ) {
        let live = mapping.len().saturating_sub(deleted.len());
        if !(store.dead() > 64 && store.dead() as usize >= live / 2) {
            return;
        }
        let mut ids: Vec<u64> = mapping
            .keys()
            .copied()
            .filter(|id| !deleted.contains(id))
            .collect();
        ids.sort();
        let mut items: Vec<(u64, Vec<f32>)> = Vec::new();
        for id in ids {
            match store.read(root, id) {
                Ok(Some(vec)) => items.push((id, vec)),
                _ => return, // broken store; leave it, replay will rewrite.
            }
        }
        let refs: Vec<(u64, &[f32])> = items
            .iter()
            .map(|(id, vec)| (*id, vec.as_slice()))
            .collect();
        let _ = store.rewrite(root, &refs);
    }

    /// Mixed hybrid search: dense top-N ∥ BM25 top-N, RRF fused
    /// (k from config), logical-delete filtered, and truncated to `k` hits.
    pub fn search(&self, query: &str, k: usize, opts: &SearchOpts) -> RagResult<Vec<Hit>> {
        if query.trim().is_empty() || self.is_empty() {
            return Ok(Vec::new());
        }

        // 1. Dense lane (query embedded WITH prefix — the hard requirement).
        // Skipped while the replay rebuild is pending; the lexical lane
        // alone still answers correctly.
        let dense_hits: Vec<(u64, f32)> = if let Some(graph) = &self.dense {
            let qvec = self
                .embedder
                .embed_texts(&[query.to_string()], true)?
                .pop()
                .ok_or_else(|| RagError::Embed("empty embedding".to_string()))?;
            graph.search(
                &qvec,
                self.cfg.dense.recall_top,
                self.cfg.dense.ef_search as usize,
            )
        } else {
            Vec::new()
        };

        // 2. Lexical lane.
        let bm25_rows = self.lexical.search(query, self.cfg.bm25.recall_top)?;
        let bm25_hits: HashMap<u64, String> = bm25_rows
            .into_iter()
            .map(|(row, _)| (row.doc_id, row.text))
            .collect();

        // 3. RRF fusion.
        let rrf_k = self.cfg.fusion.rrf_k as f32;
        let mut fused: HashMap<u64, (f32, bool, bool, Option<String>)> = HashMap::new();
        for (rank, (id, _dist)) in dense_hits.iter().enumerate() {
            let e = fused.entry(*id).or_insert((0.0, false, false, None));
            e.0 += 1.0 / (rrf_k + rank as f32 + 1.0);
            e.1 = true;
        }
        for (rank, (id, text)) in bm25_hits.iter().enumerate() {
            let e = fused.entry(*id).or_insert((0.0, false, false, None));
            e.0 += 1.0 / (rrf_k + rank as f32 + 1.0);
            e.2 = true;
            e.3 = Some(text.clone());
        }

        // 4. Sort + logical-delete filter.
        let mut results: Vec<(u64, f32, bool, bool, Option<String>)> = fused
            .into_iter()
            .filter(|(id, _)| !self.deleted.contains(id))
            .map(|(id, (score, dense, bm25, text))| (id, score, dense, bm25, text))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 5. 回表 + filters.
        let mut out = Vec::new();
        for (id, score, dense_lane, bm25_lane, bm25_text) in results.into_iter().take(k) {
            let Some(meta) = self.mapping.get(&id) else {
                continue;
            };
            if let Some(prefix) = &opts.path_prefix {
                if !meta.path.starts_with(prefix.as_str()) {
                    continue;
                }
            }
            if let Some(lang) = &opts.lang {
                if meta.lang != *lang {
                    continue;
                }
            }
            let text = match bm25_text {
                Some(t) => t,
                None => self.lexical.row_for(id)?.unwrap_or_default(),
            };
            let source = match (dense_lane, bm25_lane) {
                (true, true) => SourceLane::Both,
                (true, false) => SourceLane::DenseOnly,
                (false, true) => SourceLane::Bm25Only,
                (false, false) => continue,
            };
            out.push(Hit {
                doc_id: id,
                score,
                path: meta.path.clone(),
                lang: meta.lang.clone(),
                symbol: meta.symbol.clone(),
                text,
                source,
            });
        }
        Ok(out)
    }

    fn save_meta(&self) -> RagResult<()> {
        let meta = IndexMeta {
            model: self.cfg.embedding.model.clone(),
            mrl_dim: self.cfg.embedding.mrl_dim,
        };
        std::fs::write(
            self.root.join(META_FILE),
            serde_json::to_vec(&meta).map_err(|e| RagError::Index(e.to_string()))?,
        )?;
        Ok(())
    }

    fn save_state(&self) -> RagResult<()> {
        std::fs::write(
            self.root.join(MAPPING_FILE),
            serde_json::to_vec(&self.mapping).map_err(|e| RagError::Index(e.to_string()))?,
        )?;
        std::fs::write(
            self.root.join(DELETED_FILE),
            serde_json::to_vec(&self.deleted).map_err(|e| RagError::Index(e.to_string()))?,
        )?;
        Ok(())
    }
}

fn read_meta(root: &Path) -> RagResult<Option<IndexMeta>> {
    let path = root.join(META_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| RagError::Index(format!("metadata parse: {}", e)))
}

fn read_state(root: &Path) -> RagResult<(HashMap<u64, chunker::ChunkMeta>, HashSet<u64>)> {
    let mapping = {
        let path = root.join(MAPPING_FILE);
        if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?).unwrap_or_default()
        } else {
            HashMap::new()
        }
    };
    let deleted = {
        let path = root.join(DELETED_FILE);
        if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?).unwrap_or_default()
        } else {
            HashSet::new()
        }
    };
    Ok((mapping, deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::FakeEmbedder;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rag-hybrid-{}-{}", tag, std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_search_fusion_and_metadata() {
        let dir = temp_dir("fusion");
        let mut cfg = RagConfig::default();
        cfg.embedding.mrl_dim = 64;
        let mut idx = HybridIndex::open(&dir, cfg, Box::new(FakeEmbedder { dim: 64 })).unwrap();
        idx.add_documents(&[RawDoc {
            path: PathBuf::from("src/llm.rs"),
            text: "fn stream_chat(model) { model.request(); }\n".repeat(8),
        }])
        .unwrap();
        idx.add_documents(&[RawDoc {
            path: PathBuf::from("src/history.rs"),
            text: "docs about the sqlite history database\n".repeat(40),
        }])
        .unwrap();
        assert!(!idx.is_empty());

        let hits = idx
            .search("stream_chat model request", 5, &SearchOpts::default())
            .unwrap();
        assert!(!hits.is_empty(), "expected fusion hits");
        assert!(hits[0].path.contains("llm.rs"));

        // Metadata survives reopen and model mismatch is refused.
        drop(idx);
        let mut cfg = RagConfig::default();
        cfg.embedding.mrl_dim = 64;
        let idx = HybridIndex::open(&dir, cfg, Box::new(FakeEmbedder { dim: 64 })).unwrap();
        assert!(!idx.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_is_visible_in_search() {
        let dir = temp_dir("delete");
        let mut cfg = RagConfig::default();
        cfg.embedding.mrl_dim = 64;
        let mut idx = HybridIndex::open(&dir, cfg, Box::new(FakeEmbedder { dim: 64 })).unwrap();
        idx.add_documents(&[RawDoc {
            path: PathBuf::from("gone.rs"),
            text: "fn searchable_symbol() {}\n".repeat(4),
        }])
        .unwrap();
        assert!(!idx.is_empty());
        idx.delete_paths(&[PathBuf::from("gone.rs")]).unwrap();
        assert!(idx.is_empty());
        let hits = idx
            .search("searchable_symbol", 10, &SearchOpts::default())
            .unwrap();
        assert!(hits.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_rebuilds_dense_from_persisted_vectors() {
        let dir = temp_dir("rebuild-store");
        let mut cfg = RagConfig::default();
        cfg.embedding.mrl_dim = 64;
        {
            let mut idx =
                HybridIndex::open(&dir, cfg.clone(), Box::new(FakeEmbedder { dim: 64 })).unwrap();
            idx.add_documents(&[RawDoc {
                path: PathBuf::from("src/x.rs"),
                text: "fn vector_rebuilded_symbol() {}\n".repeat(6),
            }])
            .unwrap();
            assert!(dir.join(vectors::DATA_FILE).exists());
            assert!(dir.join(vectors::IDX_FILE).exists());
        }
        // Re-open with an embedder that refuses to embed DOC texts
        // (the graph must come from the store, not a re-embed replay).
        struct RefusingDocs(Box<dyn EmbedBackend>, std::sync::Mutex<bool>);
        impl EmbedBackend for RefusingDocs {
            fn dim(&self) -> usize {
                self.0.dim()
            }
            fn embed_texts(
                &self,
                texts: &[String],
                is_query: bool,
            ) -> crate::RagResult<Vec<Vec<f32>>> {
                if !is_query {
                    return Err(crate::error::RagError::Embed("must not embed docs".into()));
                }
                self.0.embed_texts(texts, true)
            }
        }
        let refused = std::sync::Mutex::new(false);
        let fake = RefusingDocs(Box::new(FakeEmbedder { dim: 64 }), refused);
        let idx = HybridIndex::open(&dir, cfg, Box::new(fake)).unwrap();
        assert!(!idx.pending_dense_replay());
        let hits = idx
            .search("vector_rebuilded_symbol", 10, &SearchOpts::default())
            .unwrap();
        assert!(
            hits.iter()
                .any(|h| matches!(h.source, SourceLane::DenseOnly | SourceLane::Both)),
            "expected dense lane hits, got {:?}",
            hits.iter().map(|h| h.source).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupted_vector_store_falls_back_to_replay() {
        let dir = temp_dir("store-corrupt");
        let mut cfg = RagConfig::default();
        cfg.embedding.mrl_dim = 64;
        {
            let mut idx =
                HybridIndex::open(&dir, cfg.clone(), Box::new(FakeEmbedder { dim: 64 })).unwrap();
            idx.add_documents(&[RawDoc {
                path: PathBuf::from("src/y.rs"),
                text: "fn replay_recovered_symbol() {}\n".repeat(6),
            }])
            .unwrap();
        }
        // Corrupt the data file: header stays readable, records are gone.
        std::fs::write(dir.join(vectors::DATA_FILE), &vec![0u8; 12]).unwrap();
        let mut idx = HybridIndex::open(&dir, cfg, Box::new(FakeEmbedder { dim: 64 })).unwrap();
        assert!(idx.pending_dense_replay());
        // Next write replays and self-heals the store.
        idx.add_documents(&[RawDoc {
            path: PathBuf::from("src/z.rs"),
            text: "fn replay_recovered_symbol_two() {}\n".repeat(6),
        }])
        .unwrap();
        assert!(!idx.pending_dense_replay());
        // New chunks are in the store.
        let store = vectors::VectorStore::open(&dir, 64).unwrap().unwrap();
        assert!(!store.ids().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metadata_mismatch_is_rejected() {
        let dir = temp_dir("mismatch");
        let mut cfg = RagConfig::default();
        cfg.embedding.mrl_dim = 64;
        let mut idx = HybridIndex::open(&dir, cfg, Box::new(FakeEmbedder { dim: 64 })).unwrap();
        idx.add_documents(&[RawDoc {
            path: PathBuf::from("x.rs"),
            text: "fn anything() {}\n".repeat(10),
        }])
        .unwrap();
        drop(idx);
        let mut cfg2 = RagConfig::default();
        cfg2.embedding.mrl_dim = 512;
        let err = HybridIndex::open(&dir, cfg2, Box::new(FakeEmbedder { dim: 64 }))
            .err()
            .expect("mismatched metadata must refuse load");
        assert!(err.to_string().contains("rebuild"), "got: {}", err);
        std::fs::remove_dir_all(&dir).ok();
    }
}
