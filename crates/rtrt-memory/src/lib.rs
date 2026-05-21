//! rtrt-memory — SQLite-backed persistent memory for AI agents.
//!
//! Recall combines BM25 ([`MemoryStore::recall_bm25`]), dense vectors
//! ([`MemoryStore::recall_vector`]), and Reciprocal Rank Fusion
//! ([`MemoryStore::recall_hybrid`]). Embeddings default to `all-MiniLM-L6-v2`
//! (local, offline after first download) and are only required when calling the
//! vector / hybrid paths.

use std::path::Path;
use std::sync::Arc;

use rtrt_core::{Error, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub mod embed;
#[cfg(feature = "hnsw")]
pub mod hnsw_index;
pub mod summarise;

#[cfg(feature = "embeddings")]
pub use embed::FastEmbedder;
pub use embed::{Embedder, cosine, vector_from_blob, vector_to_blob};
#[cfg(feature = "hnsw")]
pub use hnsw_index::{EmbVec, HnswIndex};
#[cfg(feature = "llm")]
pub use summarise::LlmSummariser;
pub use summarise::Summariser;

pub struct MemoryStore {
    pub(crate) conn: Connection,
    embedder: Option<Arc<dyn Embedder>>,
}

/// Memory tier. Higher tiers persist longer; lower tiers belong to a
/// specific session or agent and may be evicted independently.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    /// Truly global memory across every project + agent + session.
    User,
    /// Bound to a single agent identity (`claude`, `cursor`, `codex`, …).
    Agent,
    /// Bound to a single chat / pod session.
    Session,
    /// Project-scoped (default). Persists with the repo.
    #[default]
    Project,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Session => "session",
            Self::Project => "project",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "user" => Self::User,
            "agent" => Self::Agent,
            "session" => Self::Session,
            _ => Self::Project,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: i64,
    pub project: String,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
    #[serde(default)]
    pub scope: MemoryScope,
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
        let store = Self {
            conn,
            embedder: None,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Memory(e.to_string()))?;
        let store = Self {
            conn,
            embedder: None,
        };
        store.migrate()?;
        Ok(store)
    }

    /// Attaches an embedder so plain `save()` calls auto-embed into the
    /// `embeddings` table (chroma-style ergonomics). The embedder must be
    /// `'static + Send + Sync` because it is shared across calls.
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    fn migrate(&self) -> Result<()> {
        // Bootstrap v1 schema.
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

        // v2: add `scope` column. SQLite doesn't support ADD COLUMN IF NOT
        // EXISTS so we gate on PRAGMA user_version.
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if v < 2 {
            self.conn
                .execute_batch(
                    r#"
                    ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'project';
                    CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope, project);
                    PRAGMA user_version = 2;
                    "#,
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        Ok(())
    }

    pub fn save(&self, project: &str, kind: &str, body: &str) -> Result<i64> {
        self.save_scoped(project, kind, body, MemoryScope::Project)
    }

    /// Saves a memory record at the requested scope.
    pub fn save_scoped(
        &self,
        project: &str,
        kind: &str,
        body: &str,
        scope: MemoryScope,
    ) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::Memory(e.to_string()))?
            .as_secs() as i64;
        self.conn
            .execute(
                "INSERT INTO memories(project, kind, body, created_at, scope) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![project, kind, body, now, scope.as_str()],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let id = self.conn.last_insert_rowid();
        self.conn
            .execute(
                "INSERT INTO memories_fts(rowid, body) VALUES (?1, ?2)",
                rusqlite::params![id, body],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        // Auto-embed when an embedder is attached.
        if let Some(e) = self.embedder.clone() {
            let vector = e.embed_one(body)?;
            self.conn
                .execute(
                    "INSERT INTO embeddings(memory_id, model, vector) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, e.model_name(), vector_to_blob(&vector)],
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        Ok(id)
    }

    /// Inserts a directed labelled edge between two memory records.
    pub fn add_edge(&self, src_id: i64, dst_id: i64, relation: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO edges(src_id, dst_id, relation) VALUES (?1, ?2, ?3)",
                rusqlite::params![src_id, dst_id, relation],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// Drops a specific edge. No-op when missing.
    pub fn delete_edge(&self, src_id: i64, dst_id: i64, relation: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM edges WHERE src_id = ?1 AND dst_id = ?2 AND relation = ?3",
                rusqlite::params![src_id, dst_id, relation],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// Breadth-first neighbour walk starting from `seed_ids`, traversing up to
    /// `depth` edges. Returns every reachable [`MemoryRecord`] (including the
    /// seeds themselves) in BFS order.
    pub fn recall_via_graph(&self, seed_ids: &[i64], depth: u32) -> Result<Vec<MemoryRecord>> {
        use std::collections::{HashSet, VecDeque};
        let mut visited: HashSet<i64> = HashSet::new();
        let mut queue: VecDeque<(i64, u32)> = VecDeque::new();
        let mut order: Vec<i64> = Vec::new();
        for &id in seed_ids {
            if visited.insert(id) {
                queue.push_back((id, 0));
                order.push(id);
            }
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT dst_id FROM edges WHERE src_id = ?1 \
                 UNION SELECT src_id FROM edges WHERE dst_id = ?1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        while let Some((id, hop)) = queue.pop_front() {
            if hop >= depth {
                continue;
            }
            let rows = stmt
                .query_map(rusqlite::params![id], |row| row.get::<_, i64>(0))
                .map_err(|e| Error::Memory(e.to_string()))?;
            for next in rows {
                let next = next.map_err(|e| Error::Memory(e.to_string()))?;
                if visited.insert(next) {
                    queue.push_back((next, hop + 1));
                    order.push(next);
                }
            }
        }
        if order.is_empty() {
            return Ok(vec![]);
        }
        // Fetch the records preserving BFS order.
        let placeholders =
            std::iter::repeat_n("?", order.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, project, kind, body, created_at, scope FROM memories WHERE id IN ({placeholders})"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| Error::Memory(e.to_string()))?;
        let params: Vec<&dyn rusqlite::ToSql> =
            order.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let scope: String = row.get(5)?;
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    scope: MemoryScope::parse(&scope),
                })
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        let mut by_id: std::collections::HashMap<i64, MemoryRecord> =
            std::collections::HashMap::new();
        for row in rows {
            let rec = row.map_err(|e| Error::Memory(e.to_string()))?;
            by_id.insert(rec.id, rec);
        }
        Ok(order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect())
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
                "SELECT m.id, m.project, m.kind, m.body, m.created_at, m.scope
                   FROM memories_fts f
                   JOIN memories m ON m.id = f.rowid
                  WHERE memories_fts MATCH ?1 AND m.project = ?2
               ORDER BY rank LIMIT ?3",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![query, project, limit as i64], |row| {
                let scope: String = row.get(5)?;
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    scope: MemoryScope::parse(&scope),
                })
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// BM25 recall filtered by scope. When `include_user` is true, results
    /// from the global `user` scope are mixed in alongside the project-scoped
    /// hits (mem0-style tiered recall).
    pub fn recall_bm25_scoped(
        &self,
        project: &str,
        query: &str,
        limit: usize,
        scopes: &[MemoryScope],
    ) -> Result<Vec<MemoryRecord>> {
        if scopes.is_empty() {
            return self.recall_bm25(project, query, limit);
        }
        let placeholders =
            std::iter::repeat_n("?", scopes.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT m.id, m.project, m.kind, m.body, m.created_at, m.scope
               FROM memories_fts f
               JOIN memories m ON m.id = f.rowid
              WHERE memories_fts MATCH ?1
                AND (m.project = ?2 OR m.scope IN ({placeholders}))
           ORDER BY rank LIMIT ?{}",
            scopes.len() + 3
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| Error::Memory(e.to_string()))?;
        let scope_strs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(scopes.len() + 3);
        params.push(&query);
        params.push(&project);
        for s in &scope_strs {
            params.push(s);
        }
        let limit_i = limit as i64;
        params.push(&limit_i);
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let scope: String = row.get(5)?;
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    scope: MemoryScope::parse(&scope),
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
                "SELECT m.id, m.project, m.kind, m.body, m.created_at, m.scope, e.vector
                   FROM embeddings e
                   JOIN memories m ON m.id = e.memory_id
                  WHERE m.project = ?1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
                let scope: String = row.get(5)?;
                let record = MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    scope: MemoryScope::parse(&scope),
                };
                let blob: Vec<u8> = row.get(6)?;
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
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        Ok(scored)
    }

    /// Lists every memory in `project`, oldest first. Used by compression.
    pub fn list_by_project(&self, project: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope
                   FROM memories
                  WHERE project = ?1
               ORDER BY created_at ASC, id ASC
                  LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project, limit as i64], |row| {
                let scope: String = row.get(5)?;
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    scope: MemoryScope::parse(&scope),
                })
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Deletes a memory and any associated FTS / embedding rows.
    pub fn delete(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO memories_fts(memories_fts, rowid, body) \
                 SELECT 'delete', id, body FROM memories WHERE id = ?1",
                rusqlite::params![id],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        self.conn
            .execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// LLM-driven ingestion. The summariser extracts atomic facts from `body`
    /// and each becomes its own `kind` memory in `project`. Returns the new ids.
    pub async fn extract_and_save(
        &self,
        project: &str,
        kind: &str,
        body: &str,
        summariser: &dyn Summariser,
    ) -> Result<Vec<i64>> {
        let facts = summariser.extract_atomic(body).await?;
        let mut ids = Vec::with_capacity(facts.len());
        for fact in facts {
            let id = self.save(project, kind, &fact)?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Letta / MemGPT-style alias for [`compress_project`]: archives the
    /// oldest entries beyond `hot_limit` into a single summary while keeping
    /// the hot context intact. Returns the new archival entry id, if any.
    pub async fn archive_overflow(
        &self,
        project: &str,
        hot_limit: usize,
        summariser: &dyn Summariser,
    ) -> Result<Option<i64>> {
        self.compress_project(project, summariser, hot_limit).await
    }

    /// Compresses the oldest memories in `project`: keeps the most recent
    /// `keep_recent` rows, summarises the rest into one archival entry, and
    /// deletes the originals. Returns the new archival record id, or `None`
    /// if nothing was eligible.
    pub async fn compress_project(
        &self,
        project: &str,
        summariser: &dyn Summariser,
        keep_recent: usize,
    ) -> Result<Option<i64>> {
        let all = self.list_by_project(project, 10_000)?;
        if all.len() <= keep_recent {
            return Ok(None);
        }
        let to_summarise = &all[..all.len() - keep_recent];
        let joined = to_summarise
            .iter()
            .map(|m| format!("- [{}] {}", m.kind, m.body))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = summariser.summarise(&joined).await?;
        let archival_id = self.save(
            project,
            "archival",
            &format!("[compressed × {}]\n{summary}", to_summarise.len()),
        )?;
        for m in to_summarise {
            self.delete(m.id)?;
        }
        Ok(Some(archival_id))
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
            fused
                .entry(record.id)
                .and_modify(|sr| sr.score += score)
                .or_insert(ScoredRecord { record, score });
        }
        for (rank, scored) in vector.into_iter().enumerate() {
            let score = 1.0 / (rrf_k + (rank + 1) as f32);
            fused
                .entry(scored.record.id)
                .and_modify(|sr| sr.score += score)
                .or_insert(ScoredRecord {
                    record: scored.record,
                    score,
                });
        }

        let mut out: Vec<ScoredRecord> = fused.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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
        store
            .save("p1", "note", "rust is a systems language")
            .unwrap();
        store
            .save("p1", "note", "node is a javascript runtime")
            .unwrap();
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
        store
            .save_embedded("p1", "note", "cargo builds rust", &embedder)
            .unwrap();
        store
            .save_embedded("p1", "note", "npm installs javascript packages", &embedder)
            .unwrap();
        let hits = store
            .recall_vector("p1", "rust toolchain", 5, &embedder)
            .unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert!(hits[0].record.body.contains("cargo"), "{:?}", hits);
    }

    #[test]
    fn scope_filtering() {
        let store = MemoryStore::open_in_memory().unwrap();
        let p_id = store
            .save_scoped("p1", "note", "project secret", MemoryScope::Project)
            .unwrap();
        let u_id = store
            .save_scoped("any", "note", "user-wide preference", MemoryScope::User)
            .unwrap();
        assert_ne!(p_id, u_id);
        // Plain BM25 only sees the project entry.
        let hits = store.recall_bm25("p1", "secret OR preference", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope, MemoryScope::Project);
        // Scoped BM25 mixes in user-tier hits.
        let hits = store
            .recall_bm25_scoped("p1", "secret OR preference", 5, &[MemoryScope::User])
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn edges_graph_walk() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store.save("p", "fact", "alpha").unwrap();
        let b = store.save("p", "fact", "beta").unwrap();
        let c = store.save("p", "fact", "gamma").unwrap();
        store.add_edge(a, b, "linked").unwrap();
        store.add_edge(b, c, "linked").unwrap();
        let hits = store.recall_via_graph(&[a], 1).unwrap();
        // BFS: a, b at depth ≤ 1
        assert_eq!(hits.len(), 2, "{hits:?}");
        let ids: Vec<i64> = hits.iter().map(|r| r.id).collect();
        assert_eq!(ids[0], a);
        assert_eq!(ids[1], b);
        // Depth 2 also reaches c
        let hits = store.recall_via_graph(&[a], 2).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[cfg(feature = "hnsw")]
    #[test]
    fn hnsw_returns_nearest() {
        let store = MemoryStore::open_in_memory()
            .unwrap()
            .with_embedder(std::sync::Arc::new(WordHashEmbedder));
        store.save("p", "note", "rust cargo workspace").unwrap();
        store.save("p", "note", "python pip dependencies").unwrap();
        store.save("p", "note", "node npm packages").unwrap();
        let idx = HnswIndex::rebuild(&store, "p").unwrap().unwrap();
        let hits = idx
            .search(&store, "rust build", 2, &WordHashEmbedder)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].record.body.contains("rust"), "{:?}", hits);
    }

    #[test]
    fn auto_embed_on_save() {
        let store = MemoryStore::open_in_memory()
            .unwrap()
            .with_embedder(std::sync::Arc::new(WordHashEmbedder));
        let id = store.save("p", "note", "rust cargo build").unwrap();
        // Embedding row should exist for the new memory.
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE memory_id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn extract_and_save_splits_facts() {
        let store = MemoryStore::open_in_memory().unwrap();
        let s = crate::summarise::test_support::MockSummariser;
        let ids = store
            .extract_and_save(
                "p1",
                "note",
                "rust is fast; node is async; python has gil",
                &s,
            )
            .await
            .unwrap();
        assert_eq!(ids.len(), 3);
        let all = store.list_by_project("p1", 10).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn compress_project_collapses_old_memories() {
        let store = MemoryStore::open_in_memory().unwrap();
        let s = crate::summarise::test_support::MockSummariser;
        for i in 0..6 {
            store.save("p1", "note", &format!("note {i}")).unwrap();
        }
        let archival_id = store
            .compress_project("p1", &s, 2)
            .await
            .unwrap()
            .expect("compressed");
        let all = store.list_by_project("p1", 10).unwrap();
        assert_eq!(all.len(), 3, "{all:?}");
        let archival = all.iter().find(|m| m.id == archival_id).unwrap();
        assert!(archival.body.contains("compressed × 4"));
    }

    #[test]
    fn recall_hybrid_merges_two_streams() {
        let store = MemoryStore::open_in_memory().unwrap();
        let embedder = WordHashEmbedder;
        // BM25 will rank by FTS5 token match; vector ranks by hashed-word vector.
        // Both streams should agree that the rust note wins on a rust query.
        store
            .save_embedded("p1", "note", "rust cargo workspace", &embedder)
            .unwrap();
        store
            .save_embedded("p1", "note", "python pip dependencies", &embedder)
            .unwrap();
        store
            .save_embedded("p1", "note", "go module replace directive", &embedder)
            .unwrap();
        let hits = store.recall_hybrid("p1", "rust", 3, &embedder).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].record.body.contains("rust"), "{:?}", hits);
    }
}
