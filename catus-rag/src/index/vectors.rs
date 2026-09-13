//! Persistent dense vector store (`vectors.data` + `vectors.idx`).
//!
//! The HNSW graph is rebuilt from stored vectors instead of re-embedding
//! on every process start (design §4.6): embeddings are the expensive part
//! (ONNX inference), rebuilding the graph from normalized f32 rows is fast
//! pure CPU work.
//!
//! Layout:
//! - `vectors.data`: append-only fixed-size records, header
//!   `[magic u32][version u32][dim u32]` then records
//!   `[doc_id u64 LE][dim × f32 LE]` (offset = 12 + seq × record len).
//! - `vectors.idx`: JSON mapping `doc_id -> seq` plus `dim` / `next_seq`
//!   / `dead` counters, rewritten after every committed write.
//!
//! Ids are never rewritten in place: replaced / deleted chunks retire
//! their ids (counted as `dead`) and compaction rewrites both files when
//! the dead set dominates. Any load-time inconsistency (bad magic, wrong
//! dim, missing entries, unreadable record) is reported by the open path
//! and degrades to the re-embed replay, which rewrites both files.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{RagError, RagResult};

pub const DATA_FILE: &str = "vectors.data";
pub const IDX_FILE: &str = "vectors.idx";

/// `u32` LE bytes of "CRVD".
const MAGIC: u32 = 0x4452_5643;
const VERSION: u32 = 1;
const HEADER_LEN: u64 = 12;

/// Serialized `vectors.idx` contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdxFile {
    dim: usize,
    next_seq: u32,
    dead: u32,
    entries: BTreeMap<u64, u32>,
}

/// Append-only dense vector store rooted at `root`.
pub struct VectorStore {
    dim: usize,
    next_seq: u32,
    dead: u32,
    entries: BTreeMap<u64, u32>,
}

impl VectorStore {
    /// Load the store. Returns `Ok(None)` when the files are absent (fresh
    /// index — the caller treats that as "no stored vectors yet"), and
    /// `Err` on any inconsistency the caller should recover from by
    /// replaying embeddings and rewriting the files.
    pub fn open(root: &Path, expect_dim: usize) -> RagResult<Option<Self>> {
        let data = root.join(DATA_FILE);
        let idx = root.join(IDX_FILE);
        if !data.exists() || !idx.exists() {
            return Ok(None);
        }
        let parsed: IdxFile = serde_json::from_slice(&std::fs::read(&idx)?)
            .map_err(|e| RagError::Index(format!("{} parse: {}", IDX_FILE, e)))?;
        if parsed.dim != expect_dim {
            return Err(RagError::Index(format!(
                "{} dim {} does not match expected {}",
                IDX_FILE, parsed.dim, expect_dim
            )));
        }
        let header = read_header(&data)?;
        if header.0 != MAGIC || header.1 != VERSION || header.2 as usize != expect_dim {
            return Err(RagError::Index(format!(
                "{} header magic/version/dim mismatch",
                DATA_FILE
            )));
        }
        // Entries must reference records that actually exist in the data
        // file (more records than referenced is tolerated — orphaned tail
        // entries from a crash between the append and the idx rewrite).
        if let Some(max) = parsed.entries.values().copied().max() {
            let file_bytes = std::fs::metadata(&data)?.len();
            let record_len = 8 + expect_dim as u64 * 4;
            let capacity = file_bytes.saturating_sub(HEADER_LEN) / record_len;
            if max as u64 >= capacity {
                return Err(RagError::Index(format!(
                    "{} references record {} but holds {} complete records",
                    DATA_FILE, max, capacity
                )));
            }
        }
        Ok(Some(Self {
            dim: expect_dim,
            next_seq: parsed.next_seq,
            dead: parsed.dead,
            entries: parsed.entries,
        }))
    }

    /// Build a fresh empty store coherent with the embedding dimension.
    pub fn fresh(dim: usize) -> Self {
        Self {
            dim,
            next_seq: 0,
            dead: 0,
            entries: BTreeMap::new(),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn dead(&self) -> u32 {
        self.dead
    }

    /// Read one stored vector by doc id (`None` when the id has no record).
    pub fn read(&self, root: &Path, id: u64) -> RagResult<Option<Vec<f32>>> {
        let Some(&seq) = self.entries.get(&id) else {
            return Ok(None);
        };
        let mut file = std::fs::File::open(root.join(DATA_FILE))?;
        let record_len = 8 + self.dim as u64 * 4;
        file.seek(SeekFrom::Start(HEADER_LEN + seq as u64 * record_len + 8))?;
        let mut buf = vec![0u8; self.dim * 4];
        std::io::Read::read_exact(&mut file, &mut buf)
            .map_err(|e| RagError::Index(format!("{} read record {}: {}", DATA_FILE, seq, e)))?;
        let vec: Vec<f32> = buf
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(Some(vec))
    }

    /// Append one record for `id`; success also flips the in-memory index —
    /// call [`save`](Self::save) after the surrounding write transaction
    /// commits.
    pub fn append(&mut self, root: &Path, id: u64, vec: &[f32]) -> RagResult<()> {
        if vec.len() != self.dim {
            return Err(RagError::Index(format!(
                "vector dim {} does not match store dim {}",
                vec.len(),
                self.dim
            )));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(DATA_FILE))?;
        let mut record = Vec::with_capacity(8 + vec.len() * 4);
        if self.entries.is_empty() && seq == 0 {
            // First record in a fresh file: write the header first.
            header_bytes(&mut record, self.dim);
        }
        record.extend_from_slice(&id.to_le_bytes());
        for v in vec {
            record.extend_from_slice(&v.to_le_bytes());
        }
        std::io::Write::write_all(&mut out, &record)
            .map_err(|e| RagError::Index(format!("{} append: {}", DATA_FILE, e)))?;
        self.entries.insert(id, seq);
        Ok(())
    }

    /// Drop the vector of a retired/deleted doc id.
    pub fn remove(&mut self, id: u64) {
        if self.entries.remove(&id).is_some() {
            self.dead += 1;
        }
    }

    /// Compact rewrite: `items` defines the complete live set `(id, vec)`;
    /// both files are replaced atomically-ish (tmp + rename).
    pub fn rewrite(&mut self, root: &Path, items: &[(u64, &[f32])]) -> RagResult<()> {
        let record_len = 8 + self.dim * 4;
        let mut data = Vec::with_capacity(HEADER_LEN as usize + items.len() * record_len);
        header_bytes(&mut data, self.dim);
        let mut entries = BTreeMap::new();
        for (seq, (id, vec)) in items.iter().enumerate() {
            if vec.len() != self.dim {
                return Err(RagError::Index(format!(
                    "rewrite vector dim {} does not match store dim {}",
                    vec.len(),
                    self.dim
                )));
            }
            data.extend_from_slice(&(*id as u64).to_le_bytes());
            for v in vec.iter() {
                data.extend_from_slice(&v.to_le_bytes());
            }
            entries.insert(*id, seq as u32);
        }
        let data_path = root.join(DATA_FILE);
        let tmp = root.join(format!("{}.tmp", DATA_FILE));
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &data_path)?;
        self.entries = entries;
        self.next_seq = items.len() as u32;
        self.dead = 0;
        self.save(root)
    }

    /// Persist the side-index.
    pub fn save(&self, root: &Path) -> RagResult<()> {
        let idx = IdxFile {
            dim: self.dim,
            next_seq: self.next_seq,
            dead: self.dead,
            entries: self.entries.clone(),
        };
        std::fs::write(
            root.join(IDX_FILE),
            serde_json::to_vec(&idx).map_err(|e| RagError::Index(e.to_string()))?,
        )?;
        Ok(())
    }

    /// Doc-id set of stored records (for coherence checks against the
    /// live mapping).
    pub fn ids(&self) -> std::collections::BTreeSet<u64> {
        self.entries.keys().copied().collect()
    }
}

fn header_bytes(buf: &mut Vec<u8>, dim: usize) {
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&(dim as u32).to_le_bytes());
}

fn read_header(path: &Path) -> RagResult<(u32, u32, u32)> {
    let mut buf = [0u8; 12];
    use std::io::Read as _;
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| RagError::Index(format!("{} header: {}", path.display(), e)))?;
    let u = |b: [u8; 4]| u32::from_le_bytes(b);
    Ok((
        u([buf[0], buf[1], buf[2], buf[3]]),
        u([buf[4], buf[5], buf[6], buf[7]]),
        u([buf[8], buf[9], buf[10], buf[11]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir();
        let path = base.join(format!(
            "catus-rag-vec-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn vec3(a: f32, b: f32, c: f32) -> Vec<f32> {
        vec![a, b, c]
    }

    #[test]
    fn missing_files_are_fresh() {
        let dir = temp("missing");
        assert!(VectorStore::open(&dir, 3).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_read_roundtrip() {
        let dir = temp("roundtrip");
        let mut store = VectorStore::fresh(3);
        store.append(&dir, 7, &vec3(1.0, 0.0, 0.0)).unwrap();
        store.append(&dir, 9, &vec3(0.0, 1.0, 0.0)).unwrap();
        store.save(&dir).unwrap();
        let loaded = VectorStore::open(&dir, 3).unwrap().unwrap();
        assert_eq!(loaded.read(&dir, 7).unwrap(), Some(vec3(1.0, 0.0, 0.0)));
        assert_eq!(loaded.read(&dir, 9).unwrap(), Some(vec3(0.0, 1.0, 0.0)));
        assert_eq!(loaded.read(&dir, 10).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_retires_and_read_yields_none() {
        let dir = temp("retire");
        let mut store = VectorStore::fresh(3);
        store.append(&dir, 1, &vec3(1.0, 0.0, 0.0)).unwrap();
        store.append(&dir, 2, &vec3(0.0, 1.0, 0.0)).unwrap();
        store.remove(1);
        store.save(&dir).unwrap();
        let loaded = VectorStore::open(&dir, 3).unwrap().unwrap();
        assert_eq!(loaded.read(&dir, 1).unwrap(), None);
        assert_eq!(loaded.read(&dir, 2).unwrap().unwrap()[1], 1.0);
        assert_eq!(loaded.dead(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dim_mismatch_rejects_load() {
        let dir = temp("dim");
        let mut store = VectorStore::fresh(3);
        store.append(&dir, 1, &vec3(1.0, 0.0, 0.0)).unwrap();
        store.save(&dir).unwrap();
        assert!(VectorStore::open(&dir, 4).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncation_beyond_referenced_record_rejects_load() {
        let dir = temp("trunc");
        let mut store = VectorStore::fresh(3);
        store.append(&dir, 1, &vec3(1.0, 0.0, 0.0)).unwrap();
        store.append(&dir, 2, &vec3(0.0, 1.0, 0.0)).unwrap();
        store.save(&dir).unwrap();
        // Cut the file back to header + 1 record.
        let mut truncated = std::fs::read(dir.join(DATA_FILE)).unwrap();
        truncated.truncate(HEADER_LEN as usize + (8 + 12));
        std::fs::write(dir.join(DATA_FILE), &truncated).unwrap();
        // Record 1 (doc 2) is referenced but no longer present.
        assert!(VectorStore::open(&dir, 3).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rewrite_resets_dead_and_shrinks() {
        let dir = temp("rewrite");
        let mut store = VectorStore::fresh(3);
        store.append(&dir, 1, &vec3(1.0, 0.0, 0.0)).unwrap();
        store.append(&dir, 2, &vec3(0.0, 1.0, 0.0)).unwrap();
        store.remove(1);
        store
            .rewrite(&dir, &[(2, vec3(0.0, 1.0, 0.0).as_slice())])
            .unwrap();
        assert_eq!(store.dead(), 0);
        assert_eq!(store.read(&dir, 1).unwrap(), None);
        assert_eq!(store.read(&dir, 2).unwrap().unwrap()[1], 1.0);
        let loaded = VectorStore::open(&dir, 3).unwrap().unwrap();
        assert_eq!(loaded.read(&dir, 2).unwrap().unwrap(), vec3(0.0, 1.0, 0.0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn orphan_tail_records_are_tolerated() {
        let dir = temp("orphan");
        let mut store = VectorStore::fresh(3);
        store.append(&dir, 1, &vec3(1.0, 0.0, 0.0)).unwrap();
        store.save(&dir).unwrap();
        // Crash between append and idx rewrite: data has an extra record
        // the idx does not reference.
        store.append(&dir, 2, &vec3(0.0, 1.0, 0.0)).unwrap();
        let loaded = VectorStore::open(&dir, 3).unwrap().unwrap();
        assert_eq!(loaded.read(&dir, 1).unwrap().unwrap(), vec3(1.0, 0.0, 0.0));
        assert_eq!(loaded.read(&dir, 2).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
