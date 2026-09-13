//! Dense lane: HNSW wrapper (`hnsw_rs` 0.3, design §4.3).
//!
//! Vectors are normalized at write time, so cosine is equivalent to inner
//! product. The HNSW graph is process-lifetime state: on open it is replayed
//! from the stored chunk texts (design §4.6 rebuild path), so the graph is
//! always freshly built and its lifetime is capped by no external borrow.

use hnsw_rs::prelude::*;

use crate::error::RagResult;

/// In-process HNSW ANN index over u64 doc ids.
pub struct DenseIndex {
    graph: Hnsw<'static, f32, DistCosine>,
}

impl DenseIndex {
    /// Build an empty graph sized for `initial` elements (design §4.3:
    /// M / ef_construction from config, 16 layers max).
    pub fn new(initial: usize, m: usize, ef_construction: usize) -> Self {
        Self {
            graph: Hnsw::<f32, DistCosine>::new(
                m.max(2),
                initial.max(1000),
                16,
                ef_construction.max(4),
                DistCosine {},
            ),
        }
    }

    /// Insert a normalized vector under `id`.
    pub fn insert(&self, vec: &[f32], id: u64) -> RagResult<()> {
        self.graph.insert((vec, id as usize));
        Ok(())
    }

    /// k-NN query; returns `(doc_id, distance)` pairs, best first.
    pub fn search(&self, query: &[f32], k: usize, ef_search: usize) -> Vec<(u64, f32)> {
        self.graph
            .search(query, k.max(1), ef_search.max(k))
            .into_iter()
            .map(|n| (n.d_id as u64, n.distance))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(mut v: Vec<f32>) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in v.iter_mut() {
            *x /= n;
        }
        v
    }

    #[test]
    fn knn_finds_inserted() {
        let idx = DenseIndex::new(1000, 16, 64);
        let a = norm(vec![1.0, 0.0]);
        let b = norm(vec![0.9, 0.1]);
        let c = norm(vec![0.0, 1.0]);
        idx.insert(&a, 1).unwrap();
        idx.insert(&b, 2).unwrap();
        idx.insert(&c, 3).unwrap();
        let hits = idx.search(&a, 2, 32);
        assert_eq!(hits[0].0, 1);
        assert!(hits[0].1 < 0.1);
    }
}
