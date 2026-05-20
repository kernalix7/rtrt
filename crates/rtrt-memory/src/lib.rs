//! rtrt-memory — SQLite-backed persistent memory for AI agents.
//!
//! Recall combines BM25, dense vectors, and a knowledge graph via Reciprocal Rank Fusion.
//! Embeddings default to `all-MiniLM-L6-v2` (local, offline).

use std::path::Path;

use rtrt_core::{Error, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

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

    pub fn recall_bm25(&self, project: &str, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
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
            .query_map(
                rusqlite::params![query, project, limit as i64],
                |row| {
                    Ok(MemoryRecord {
                        id: row.get(0)?,
                        project: row.get(1)?,
                        kind: row.get(2)?,
                        body: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
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
}
