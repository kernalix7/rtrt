//! HNSW vector index for sub-linear approximate-nearest-neighbour recall.
//!
//! Wraps [`instant_distance`] with the same cosine-distance semantics the
//! rest of [`crate::embed`] uses. The index is built on demand from whatever
//! rows are stored in the `embeddings` table; rebuild after any batch insert
//! by calling [`HnswIndex::rebuild`].
//!
//! Gated behind the `hnsw` feature so the base `rtrt-memory` build doesn't
//! pull `rayon` + `parking_lot` for callers that don't need ANN search.

use instant_distance::{Builder, HnswMap, Point, Search};
use rtrt_core::Result;

use crate::{Embedder, MemoryRecord, MemoryStore, ScoredRecord};

#[derive(Debug, Clone, PartialEq)]
pub struct EmbVec(pub Vec<f32>);

impl Point for EmbVec {
    fn distance(&self, other: &Self) -> f32 {
        // Cosine distance in [0, 2] so smaller is closer, matching the
        // instant_distance contract. We clamp the underlying cosine to
        // [-1, 1] just in case rounding pushes us out of bounds.
        let sim = crate::embed::cosine(&self.0, &other.0).clamp(-1.0, 1.0);
        1.0 - sim
    }
}

pub struct HnswIndex {
    inner: HnswMap<EmbVec, i64>,
}

impl HnswIndex {
    /// Builds an index over every embedding in `project`. Returns `None` when
    /// the project has no embedded memories.
    pub fn rebuild(store: &MemoryStore, project: &str) -> Result<Option<Self>> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT m.id, e.vector FROM embeddings e \
                 JOIN memories m ON m.id = e.memory_id \
                 WHERE m.project = ?1",
            )
            .map_err(|e| rtrt_core::Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
                let id: i64 = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, blob))
            })
            .map_err(|e| rtrt_core::Error::Memory(e.to_string()))?;
        let mut points: Vec<EmbVec> = Vec::new();
        let mut values: Vec<i64> = Vec::new();
        for row in rows {
            let (id, blob) = row.map_err(|e| rtrt_core::Error::Memory(e.to_string()))?;
            let v = crate::embed::vector_from_blob(&blob)?;
            points.push(EmbVec(v));
            values.push(id);
        }
        if points.is_empty() {
            return Ok(None);
        }
        let inner = Builder::default().build(points, values);
        Ok(Some(Self { inner }))
    }

    /// Builds an index directly from `(vector, id)` pairs — no database read.
    /// Returns `None` when `items` is empty. Used by vector clustering, which
    /// already has the project's vectors in hand and needs to query each
    /// point's neighbours against the same set.
    ///
    /// Two deliberate departures from [`Builder::default`]:
    /// * **Fixed seed** — the default seeds the layer RNG from entropy, making
    ///   the graph (and therefore clustering) non-deterministic across runs.
    ///   Clustering must be reproducible, so we pin the seed.
    /// * **Lower `ef_construction`** — the default (100) makes building a few
    ///   hundred 768-dim points take seconds. Clustering only needs each point's
    ///   immediate neighbourhood, not high-recall ANN, so a small beam keeps the
    ///   build well under a second without changing which tight clusters form.
    pub fn from_vectors(items: Vec<(Vec<f32>, i64)>) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        let mut points: Vec<EmbVec> = Vec::with_capacity(items.len());
        let mut values: Vec<i64> = Vec::with_capacity(items.len());
        for (v, id) in items {
            points.push(EmbVec(v));
            values.push(id);
        }
        let inner = Builder::default()
            .seed(0x5251_5254_5645_4332) // "RQRTVEC2" — fixed for determinism.
            .ef_construction(24)
            .ef_search(24)
            .build(points, values);
        Some(Self { inner })
    }

    /// Approximate top-`limit` nearest neighbours of a raw vector, as
    /// `(cosine_distance, memory_id)` where distance = `1 - cosine` in `[0, 2]`
    /// (smaller is closer). The query point itself is included if present in the
    /// index, so callers that query a stored point should request `limit + 1`
    /// and drop the self-hit.
    pub fn neighbors(&self, v: &[f32], limit: usize) -> Vec<(f32, i64)> {
        let mut search = Search::default();
        let qp = EmbVec(v.to_vec());
        self.inner
            .search(&qp, &mut search)
            .take(limit)
            .map(|item| (item.distance, *item.value))
            .collect()
    }

    /// Approximate top-`limit` nearest neighbours of the query string.
    pub fn search(
        &self,
        store: &MemoryStore,
        query: &str,
        limit: usize,
        embedder: &dyn Embedder,
    ) -> Result<Vec<ScoredRecord>> {
        let q = embedder.embed_one(query)?;
        let mut search = Search::default();
        let qp = EmbVec(q);
        let raw: Vec<(f32, i64)> = self
            .inner
            .search(&qp, &mut search)
            .take(limit)
            .map(|item| (item.distance, *item.value))
            .collect();
        let mut hits = Vec::with_capacity(raw.len());
        for (dist, id) in raw {
            if let Some(record) = fetch_record(store, id)? {
                let score = (1.0 - dist).clamp(-1.0, 1.0);
                hits.push(ScoredRecord { record, score });
            }
        }
        Ok(hits)
    }
}

fn fetch_record(store: &MemoryStore, id: i64) -> Result<Option<MemoryRecord>> {
    let row = store.conn.query_row(
        "SELECT id, project, kind, body, created_at, scope FROM memories WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            let scope: String = r.get(5)?;
            Ok(MemoryRecord {
                id: r.get(0)?,
                project: r.get(1)?,
                kind: r.get(2)?,
                body: r.get(3)?,
                created_at: r.get(4)?,
                scope: crate::MemoryScope::parse(&scope),
            })
        },
    );
    match row {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(rtrt_core::Error::Memory(e.to_string())),
    }
}
