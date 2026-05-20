//! rtrt-memory — SQLite-backed persistent memory for AI agents.
//!
//! Recall combines BM25 ([`MemoryStore::recall_bm25`]), dense vectors
//! ([`MemoryStore::recall_vector`]), and Reciprocal Rank Fusion
//! ([`MemoryStore::recall_hybrid`]). Embeddings default to `all-MiniLM-L6-v2`
//! (local, offline after first download) and are only required when calling the
//! vector / hybrid paths.

use std::path::Path;

use rtrt_core::{Error, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub mod embed;

pub use embed::{Embedder, cosine, vector_from_blob, vector_to_blob};
#[cfg(feature = "embeddings")]
pub use embed::FastEmbedder;

pub struct MemoryStore {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: i64,
    pub project: String,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredRecord {
    pub record: MemoryRecord,
    pub score: f32,
}

impl MemoryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(Error::Io)?;
            }
        }
        let conn = Connection::open(path.as_ref()).map_err(|e| Error::Memory(e.to_string()))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Memory(e.to_string()))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS memories (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    project     TEXT NOT NULL,
                    kind        TEXT NOT NULL,
                    body        TEXT NOT NULL,
                    created_at  INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_memories_project ON memories(project);
                CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
                    USING fts5(body, content='memories', content_rowid='id');
                CREATE TABLE IF NOT EXISTS embeddings (
                    memory_id   INTEGER PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                    model       TEXT NOT NULL,
                    vector      BLOB NOT NULL
                );
                CREATE TABLE IF NOT EXISTS edges (
                    src_id      INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    dst_id      INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    relation    TEXT NOT NULL,
                    PRIMARY KEY (src_id, dst_id, relation)
                );
                "#,
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    pub fn save(&self, project: &str, kind: &str, body: &str) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::Memory(e.to_string()))?
            .as_secs() as i64;
        self.conn
            .execute(
                "INSERT INTO memories(project, kind, body, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![project, kind, body, now],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let id = self.conn.last_insert_rowid();
        self.conn
            .execute(
                "INSERT INTO memories_fts(rowid, body) VALUES (?1, ?2)",
                rusqlite::params![id, body],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(id)
    }

    /// Saves a memory record and stores its embedding in the `embeddings` table.
    pub fn save_embedded(
        &self,
        project: &str,
        kind: &str,
        body: &str,
        embedder: &dyn Embedder,
    ) -> Result<i64> {
        let id = self.save(project, kind, body)?;
        let vector = embedder.embed_one(body)?;
        self.conn
            .execute(
                "INSERT INTO embeddings(memory_id, model, vector) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, embedder.model_name(), vector_to_blob(&vector)],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(id)
    }

    pub fn recall_bm25(
        &self,
        project: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.id, m.project, m.kind, m.body, m.created_at
                   FROM memories_fts f
                   JOIN memories m ON m.id = f.rowid
                  WHERE memories_fts MATCH ?1 AND m.project = ?2
               ORDER BY rank LIMIT ?3",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![query, project, limit as i64], |row| {
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Pure vector recall — embeds the query and ranks every memory in the
    /// project by cosine similarity. Scales linearly with the number of stored
    /// embeddings; v0.3 will replace this with an HNSW index for large stores.
    pub fn recall_vector(
        &self,
        project: &str,
        query: &str,
        limit: usize,
        embedder: &dyn Embedder,
    ) -> Result<Vec<ScoredRecord>> {
        let q = embedder.embed_one(query)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.id, m.project, m.kind, m.body, m.created_at, e.vector
                   FROM embeddings e
                   JOIN memories m ON m.id = e.memory_id
                  WHERE m.project = ?1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
                let record = MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                };
                let blob: Vec<u8> = row.get(5)?;
                Ok((record, blob))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        let mut scored: Vec<ScoredRecord> = Vec::new();
        for row in rows {
            let (record, blob) = row.map_err(|e| Error::Memory(e.to_string()))?;
            let v = vector_from_blob(&blob)?;
            let score = cosine(&q, &v);
            scored.push(ScoredRecord { record, score });
        }
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Hybrid recall — Reciprocal Rank Fusion of BM25 and dense-vector
    /// rankings. Score per record is `Σ 1 / (rrf_k + rank_i)` over the two
    /// streams; default `rrf_k = 60`. Each stream is fetched at `limit * 2` so
    /// items only appearing in one stream still surface.
    pub fn recall_hybrid(
        &self,
        project: &str,
        query: &str,
        limit: usize,
        embedder: &dyn Embedder,
    ) -> Result<Vec<ScoredRecord>> {
        let fetch = limit.saturating_mul(2).max(limit);
        let bm25 = self.recall_bm25(project, query, fetch)?;
        let vector = self.recall_vector(project, query, fetch, embedder)?;

        let rrf_k = 60.0f32;
        let mut fused: std::collections::HashMap<i64, ScoredRecord> =
            std::collections::HashMap::new();

        for (rank, record) in bm25.into_iter().enumerate() {
            let score = 1.0 / (rrf_k + (rank + 1) as f32);
            fused.entry(record.id).and_modify(|sr| sr.score += score).or_insert(ScoredRecord {
                record,
                score,
            });
        }
        for (rank, scored) in vector.into_iter().enumerate() {
            let score = 1.0 / (rrf_k + (rank + 1) as f32);
            fused
                .entry(scored.record.id)
                .and_modify(|sr| sr.score += score)
                .or_insert(ScoredRecord { record: scored.record, score });
        }

        let mut out: Vec<ScoredRecord> = fused.into_values().collect();
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_recall_bm25() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("p1", "note", "rust is a systems language").unwrap();
        store.save("p1", "note", "node is a javascript runtime").unwrap();
        let hits = store.recall_bm25("p1", "rust", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("rust"));
    }

    /// Mock embedder that produces deterministic vectors so we can test
    /// recall_vector / recall_hybrid without pulling in fastembed.
    struct WordHashEmbedder;

    impl Embedder for WordHashEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_name(&self) -> &str {
            "test-word-hash"
        }
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| word_hash_vec(t)).collect())
        }
    }

    fn word_hash_vec(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v = vec![0.0f32; 4];
        // very rough one-hot-like vector keyed on a few topic words
        if lower.contains("rust") || lower.contains("cargo") {
            v[0] = 1.0;
        }
        if lower.contains("javascript") || lower.contains("node") || lower.contains("npm") {
            v[1] = 1.0;
        }
        if lower.contains("python") || lower.contains("pip") {
            v[2] = 1.0;
        }
        if lower.contains("go") || lower.contains("golang") {
            v[3] = 1.0;
        }
        v
    }

    #[test]
    fn save_embedded_and_recall_vector() {
        let store = MemoryStore::open_in_memory().unwrap();
        let embedder = WordHashEmbedder;
        store.save_embedded("p1", "note", "cargo builds rust", &embedder).unwrap();
        store.save_embedded("p1", "note", "npm installs javascript packages", &embedder).unwrap();
        let hits = store.recall_vector("p1", "rust toolchain", 5, &embedder).unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert!(hits[0].record.body.contains("cargo"), "{:?}", hits);
    }

    #[test]
    fn recall_hybrid_merges_two_streams() {
        let store = MemoryStore::open_in_memory().unwrap();
        let embedder = WordHashEmbedder;
        // BM25 will rank by FTS5 token match; vector ranks by hashed-word vector.
        // Both streams should agree that the rust note wins on a rust query.
        store.save_embedded("p1", "note", "rust cargo workspace", &embedder).unwrap();
        store.save_embedded("p1", "note", "python pip dependencies", &embedder).unwrap();
        store.save_embedded("p1", "note", "go module replace directive", &embedder).unwrap();
        let hits = store.recall_hybrid("p1", "rust", 3, &embedder).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].record.body.contains("rust"), "{:?}", hits);
    }
}
