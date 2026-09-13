//! Agent RAG bridge: manages the workspace `catus-rag` hybrid index.
//!
//! Follows the `McpManager` pattern: one [`RagManager`] owned by `App`
//! behind `Arc`, shared with the `rag_search` / `rag_index` tools. The
//! embedding model and the HNSW graph load lazily on first use (tool call
//! or explicit `/rag index`); auto-injection only reads an already-loaded
//! index so the turn loop never blocks on model download.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use catus_rag::config::RagConfig;
use catus_rag::embed::{EmbedBackend, FakeEmbedder, FastembedEmbedder};
use catus_rag::filter;
use catus_rag::index::{HybridIndex, RawDoc, SearchOpts};

/// Where the embedding engine comes from; tests substitute the fake.
type EmbedderFactory = Box<dyn Fn() -> Box<dyn EmbedBackend> + Send + Sync>;

/// Workspace RAG index owner.
pub struct RagManager {
    index_dir: PathBuf,
    cfg: RagConfig,
    inner: Mutex<Option<HybridIndex>>,
    embedder_factory: EmbedderFactory,
}

impl RagManager {
    /// Build the manager for an index directory; the fastembed engine is
    /// created on first `ensure_loaded` (model downloads on first use).
    pub fn new(index_dir: PathBuf, cfg: RagConfig) -> Arc<Self> {
        let embedding_cfg = cfg.embedding.clone();
        Self::with_embedder_factory(
            index_dir,
            cfg,
            Box::new(move || {
                let embedding_cfg = embedding_cfg.clone();
                Box::new(FastembedEmbedder::new(embedding_cfg, default_cache_dir()))
                    as Box<dyn EmbedBackend>
            }),
        )
    }

    /// Manager variant with an injected embedder factory (tests).
    pub fn with_embedder_factory(
        index_dir: PathBuf,
        cfg: RagConfig,
        factory: EmbedderFactory,
    ) -> Arc<Self> {
        Arc::new(Self {
            index_dir,
            cfg,
            inner: Mutex::new(None),
            embedder_factory: factory,
        })
    }

    /// A deterministic fake backend for tests (hash-based vectors).
    pub fn fake_embedder_factory() -> EmbedderFactory {
        Box::new(|| Box::new(FakeEmbedder { dim: 64 }))
    }

    /// The [rag] configuration as loaded.
    pub fn config(&self) -> &RagConfig {
        &self.cfg
    }

    /// The index directory.
    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// True when the hybrid index is ready in this process.
    pub fn is_loaded(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Load (or repair) the index; downloads the embedding model the first
    /// time. Blocking.
    pub fn ensure_loaded(&self) -> Result<(), String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "rag manager mutex poisoned".to_string())?;
        if guard.is_none() {
            tracing::info!(target: "rag", "loading rag index at {}", self.index_dir.display());
            let embedder = (self.embedder_factory)();
            let index = HybridIndex::open(&self.index_dir, self.cfg.clone(), embedder)
                .map_err(|e| e.to_string())?;
            *guard = Some(index);
        }
        Ok(())
    }

    /// Drop the in-process index (used by `/rag rebuild` before clearing the
    /// directory).
    pub fn unload(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = None;
        }
    }

    /// Run the pending dense replay if one exists; blocking.
    pub fn replay_if_pending(&self) -> Result<bool, String> {
        {
            let guard = self
                .inner
                .lock()
                .map_err(|_| "rag manager mutex poisoned".to_string())?;
            if guard.is_none() {
                return Ok(false);
            }
        }
        // Replay needs &mut access.
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "rag manager mutex poisoned".to_string())?;
        if let Some(index) = guard.as_mut() {
            if index.pending_dense_replay() {
                index.replay_dense().map_err(|e| e.to_string())?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Run the pending dense replay if any (blocking; used from the tool path
    /// where model load and re-embedding are acceptable).
    pub fn ensure_dense_ready(&self) -> Result<(), String> {
        self.ensure_loaded()?;
        let replayed = self.replay_if_pending()?;
        if replayed {
            tracing::info!(target: "rag", "dense lane replayed on search path");
        }
        Ok(())
    }

    /// Search against a loaded index; `None` when the index is not loaded
    /// (keeps auto-injection non-blocking).
    pub fn search_loaded(
        &self,
        query: &str,
        k: usize,
        path_prefix: Option<String>,
    ) -> Option<Vec<catus_rag::Hit>> {
        let guard = self.inner.lock().ok()?;
        let index = guard.as_ref()?;
        let opts = SearchOpts {
            path_prefix,
            lang: None,
        };
        index.search(query, k, &opts).ok()
    }

    /// Blocking search against an ensured index.
    pub fn search(
        &self,
        query: &str,
        k: usize,
        path_prefix: Option<String>,
    ) -> Result<Vec<catus_rag::Hit>, String> {
        self.ensure_dense_ready()?;
        self.search_loaded(query, k, path_prefix)
            .ok_or_else(|| "rag index unavailable".to_string())
    }

    /// Add files / whole directories to the index. Whitelisted extensions
    /// only (code + Markdown by default). Blocking; heavy IO + model use.
    pub fn index_paths(&self, paths: &[PathBuf]) -> Result<String, String> {
        self.ensure_loaded()?;
        self.replay_if_pending()?;
        let files =
            filter::collect_indexable(paths, &self.cfg.filter).map_err(|e| e.to_string())?;
        let mut docs: Vec<RawDoc> = Vec::new();
        let mut skipped = 0usize;
        for path in &files {
            match std::fs::read_to_string(path) {
                Ok(text) => docs.push(RawDoc {
                    path: path.clone(),
                    text,
                }),
                Err(e) => {
                    skipped += 1;
                    tracing::warn!(target: "rag", "skip {}: {}", path.display(), e);
                }
            }
        }
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "rag manager mutex poisoned".to_string())?;
        let index = guard.as_mut().ok_or("rag index unavailable")?;
        let written = index.add_documents(&docs).map_err(|e| e.to_string())?;
        Ok(format!(
            "indexed {} file(s) from {} path(s): {} chunk(s) live{}",
            files.len(),
            paths.len(),
            written,
            if skipped > 0 {
                format!(", {} read-skip", skipped)
            } else {
                String::new()
            }
        ))
    }

    /// Remove chunks for given paths.
    pub fn delete_paths(&self, paths: &[PathBuf]) -> Result<usize, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "rag manager mutex poisoned".to_string())?;
        guard
            .as_mut()
            .ok_or("rag index is not loaded")?
            .delete_paths(paths)
            .map_err(|e| e.to_string())
    }

    /// One-line status for `/rag status`.
    pub fn status_line(&self) -> String {
        let loaded = self.is_loaded();
        let chunks = {
            let guard = self.inner.lock().ok();
            guard.and_then(|g| g.as_ref().map(|i| i.len()))
        };
        match (loaded, chunks) {
            (false, _) => format!(
                "rag: index at {} not loaded (run /rag index)",
                self.index_dir.display()
            ),
            (true, Some(n)) => format!(
                "rag: index at {}, {} chunk(s), model {}{}",
                self.index_dir.display(),
                n,
                self.cfg.embedding.model,
                if self.config().auto_inject {
                    ", inject on"
                } else {
                    ""
                }
            ),
            (true, None) => "rag: index loaded".to_string(),
        }
    }

    /// Delete the index directory entirely (`/rag rebuild`).
    pub fn wipe(&self) -> Result<(), std::io::Error> {
        self.unload();
        if self.index_dir.exists() {
            std::fs::remove_dir_all(&self.index_dir)?;
        }
        Ok(())
    }
}

/// Default fastembed model cache directory. Lives under the catus XDG base
/// dir when available so repeated downloads are avoided (treated best
/// effort; fastembed also handles its own fallback).
fn default_cache_dir() -> PathBuf {
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(home_config);
    xdg.map(|p| p.join("catus").join("rag-model-cache"))
        .unwrap_or_else(|| PathBuf::from(".cache/catus-rag"))
}

fn home_config() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("catus-rag-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg() -> RagConfig {
        let mut c = RagConfig::default();
        c.enabled = true;
        c.embedding.mrl_dim = 64; // FakeEmbedder's output dim
        c
    }

    #[test]
    fn lazy_load_and_search() {
        let manager = RagManager::with_embedder_factory(
            temp_dir("lazy"),
            cfg(),
            RagManager::fake_embedder_factory(),
        );
        assert!(!manager.is_loaded());
        // Search without loading stays None (non-blocking path).
        assert!(manager.search_loaded("query", 5, None).is_none());
        manager.ensure_loaded().unwrap();
        assert!(manager.is_loaded());
        let hits = manager
            .search_loaded("anything", 5, None)
            .unwrap_or_default();
        assert!(hits.is_empty());
        // Index a file and find it.
        let dir = temp_dir("lazy");
        let doc = dir.join("sample.rs");
        std::fs::write(&doc, "fn specially_searchable() {}\n".repeat(10)).unwrap();
        let summary = manager.index_paths(&[doc.clone()]).unwrap();
        let hits = manager
            .search_loaded("specially_searchable", 5, None)
            .unwrap_or_default();
        assert!(
            !hits.is_empty(),
            "hits: {:?}",
            manager
                .search_loaded("specially_searchable", 5, None)
                .is_none()
        );
        assert!(hits.iter().any(|h| h.path.ends_with("sample.rs")));
    }

    #[test]
    fn delete_removes_chunks() {
        let manager = RagManager::with_embedder_factory(
            temp_dir("delete"),
            cfg(),
            RagManager::fake_embedder_factory(),
        );
        manager.ensure_loaded().unwrap();
        let dir = temp_dir("delete");
        let doc = dir.join("a.rs");
        std::fs::write(&doc, "fn to_be_removed_symbol() {}\n".repeat(10)).unwrap();
        manager.index_paths(&[doc.clone()]).unwrap();
        assert!(
            manager
                .search_loaded("to_be_removed_symbol", 5, None)
                .unwrap_or_default()
                .len()
                > 0
        );
        manager.delete_paths(&[doc]).unwrap();
        assert!(
            manager
                .search_loaded("to_be_removed_symbol", 5, None)
                .unwrap()
                .is_empty()
        );
    }
}
