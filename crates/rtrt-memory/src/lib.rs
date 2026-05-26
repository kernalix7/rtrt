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
pub mod payload;
pub mod summarise;

#[cfg(feature = "embeddings")]
pub use embed::FastEmbedder;
pub use embed::{Embedder, cosine, vector_from_blob, vector_to_blob};
#[cfg(feature = "hnsw")]
pub use hnsw_index::{EmbVec, HnswIndex};
pub use payload::{PayloadFilter, PayloadPredicate};
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

/// Outcome of [`MemoryStore::extract_and_save_unique`]: the new ids that
/// landed and the number of duplicate facts that were skipped.
#[derive(Debug, Clone, Default)]
pub struct UniqueIngest {
    pub added_ids: Vec<i64>,
    pub skipped: usize,
}

/// Prefix applied to the `kind` column for Letta-style memory blocks.
const BLOCK_KIND_PREFIX: &str = "block:";

/// Builds the `kind` string for a Letta memory block.
pub fn block_kind(name: &str) -> String {
    format!("{BLOCK_KIND_PREFIX}{name}")
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
        // v3: add `metadata` column holding a JSON-encoded BTreeMap<String,
        // String> for qdrant-style payload filtering.
        if v < 3 {
            self.conn
                .execute_batch(
                    r#"
                    ALTER TABLE memories ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}';
                    PRAGMA user_version = 3;
                    "#,
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        // v4: add `session_id` and `body_sha` columns for the auto-capture
        // pipeline. session_id groups every event fired during one agent
        // session; body_sha indexes the SHA-256 of the body so the dedup
        // path can answer "have I seen this in the last 5 minutes?" in O(1).
        if v < 4 {
            self.conn
                .execute_batch(
                    r#"
                    ALTER TABLE memories ADD COLUMN session_id TEXT;
                    ALTER TABLE memories ADD COLUMN body_sha   TEXT;
                    CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);
                    CREATE INDEX IF NOT EXISTS idx_memories_body_sha ON memories(body_sha, created_at);
                    PRAGMA user_version = 4;
                    "#,
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        // v5: add a covering index for the timeline pager. The earlier
        // `idx_memories_project` only covered the WHERE clause; the
        // `ORDER BY created_at DESC, id DESC LIMIT … OFFSET …` had to scan
        // the full project slice for deep pages. With this composite index
        // SQLite can serve recent_paged off a single seek + sequential walk
        // even at 100k rows per project.
        if v < 5 {
            self.conn
                .execute_batch(
                    r#"
                    CREATE INDEX IF NOT EXISTS idx_memories_timeline
                        ON memories(project, created_at DESC, id DESC);
                    PRAGMA user_version = 5;
                    "#,
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        // v6: `body_full` preserves the pre-compression original. NULL means
        // the row was never compressed (so `body` IS the original). The LLM
        // compress path writes the original here once, then overwrites
        // `body` with the compressed text — recall reads `body` (terse), the
        // original stays available for reference / audit.
        if v < 6 {
            self.conn
                .execute_batch(
                    r#"
                    ALTER TABLE memories ADD COLUMN body_full TEXT;
                    PRAGMA user_version = 6;
                    "#,
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        Ok(())
    }

    /// Compute a hex SHA-256 of `body`. Stable across machines, used by the
    /// dedup index.
    pub fn body_sha(body: &str) -> String {
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(body.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// Returns the most recent created_at for a body hash, or None when this
    /// hash hasn't been seen. Used by the auto-capture pipeline to decide
    /// whether to skip a save inside the dedup window.
    pub fn body_seen_at(&self, project: &str, sha: &str) -> Result<Option<i64>> {
        let row: rusqlite::Result<i64> = self.conn.query_row(
            "SELECT created_at FROM memories WHERE project = ?1 AND body_sha = ?2 \
             ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![project, sha],
            |row| row.get(0),
        );
        match row {
            Ok(ts) => Ok(Some(ts)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Memory(e.to_string())),
        }
    }

    /// Tag the most recently inserted row (or a specific id) with a session
    /// and / or body_sha. Best-effort: skipped on error.
    pub fn tag_row(&self, id: i64, session_id: Option<&str>, body_sha: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE memories SET session_id = COALESCE(?2, session_id), \
                                       body_sha   = COALESCE(?3, body_sha) \
                 WHERE id = ?1",
                rusqlite::params![id, session_id, body_sha],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// Returns one summary row per project — `(project, count, latest_ts)` —
    /// so the dashboard can present a project picker without scanning the
    /// whole table on the client.
    pub fn projects(&self) -> Result<Vec<(String, usize, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT project, COUNT(*) AS n, COALESCE(MAX(created_at), 0) AS latest \
                   FROM memories GROUP BY project ORDER BY latest DESC",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Memories in `project` ordered newest-first. Drives the dashboard
    /// timeline / history view.
    pub fn recent(&self, project: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        self.recent_paged(project, limit, 0)
    }

    /// Paginated newest-first view. `offset` skips that many rows before
    /// returning up to `limit` records.
    pub fn recent_paged(
        &self,
        project: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope FROM memories \
                  WHERE project = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![project, limit as i64, offset as i64],
                |row| {
                    let scope: String = row.get(5)?;
                    Ok(MemoryRecord {
                        id: row.get(0)?,
                        project: row.get(1)?,
                        kind: row.get(2)?,
                        body: row.get(3)?,
                        created_at: row.get(4)?,
                        scope: MemoryScope::parse(&scope),
                    })
                },
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// One summary row per `session_id` for a project. Returns
    /// `(session_id, count, first_ts, last_ts)` ordered by `last_ts DESC`.
    /// Rows with a NULL `session_id` (legacy v1–v3 captures) are folded into
    /// a single synthetic bucket keyed by the empty string so they remain
    /// listable in the UI.
    pub fn sessions(&self, project: &str) -> Result<Vec<(String, usize, i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT COALESCE(session_id, '') AS sid, \
                        COUNT(*) AS n, \
                        MIN(created_at) AS first_ts, \
                        MAX(created_at) AS last_ts \
                   FROM memories \
                  WHERE project = ?1 \
                  GROUP BY sid \
                  ORDER BY last_ts DESC",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// All memory rows tagged with `session_id` inside `project`, newest
    /// first. Useful for replaying or exporting one agent session.
    pub fn session_records(
        &self,
        project: &str,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope FROM memories \
                  WHERE project = ?1 AND COALESCE(session_id, '') = ?2 \
                  ORDER BY created_at DESC, id DESC LIMIT ?3",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![project, session_id, limit as i64],
                |row| {
                    let scope: String = row.get(5)?;
                    Ok(MemoryRecord {
                        id: row.get(0)?,
                        project: row.get(1)?,
                        kind: row.get(2)?,
                        body: row.get(3)?,
                        created_at: row.get(4)?,
                        scope: MemoryScope::parse(&scope),
                    })
                },
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Row count for one project. Used by paginated views to compute the
    /// total page count without scanning every row client-side.
    pub fn count_by_project(&self, project: &str) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE project = ?1",
                rusqlite::params![project],
                |row| row.get(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(n as usize)
    }

    /// Returns every `(src_id, dst_id, relation)` edge whose endpoints are
    /// inside `project`. Used by the dashboard graph view; intentionally
    /// scoped to one project so a global view doesn't degrade with growth.
    pub fn project_edges(&self, project: &str) -> Result<Vec<(i64, i64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT e.src_id, e.dst_id, e.relation \
                   FROM edges e \
                   JOIN memories ms ON ms.id = e.src_id \
                   JOIN memories md ON md.id = e.dst_id \
                  WHERE ms.project = ?1 AND md.project = ?1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Iterates every memory row in `project`, emitting one JSON Line per
    /// record (`{ id, project, kind, body, scope, created_at, metadata }`).
    /// Pair with [`import_jsonl`] for portable backups across machines.
    pub fn export_jsonl<W: std::io::Write>(&self, project: &str, mut w: W) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope, metadata FROM memories WHERE project = ?1 ORDER BY id",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
                let scope: String = row.get(5)?;
                let metadata: String = row.get(6)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    scope,
                    metadata,
                ))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        let mut count = 0usize;
        for row in rows {
            let (id, project, kind, body, created_at, scope, metadata) =
                row.map_err(|e| Error::Memory(e.to_string()))?;
            let metadata_v: serde_json::Value =
                serde_json::from_str(&metadata).unwrap_or(serde_json::json!({}));
            let line = serde_json::json!({
                "id": id,
                "project": project,
                "kind": kind,
                "body": body,
                "created_at": created_at,
                "scope": scope,
                "metadata": metadata_v,
            });
            writeln!(w, "{}", line).map_err(Error::Io)?;
            count += 1;
        }
        Ok(count)
    }

    /// Replays a JSON-Lines stream produced by [`export_jsonl`] into the
    /// store. Each record is inserted fresh; original ids are not preserved
    /// (SQLite assigns a new rowid). Returns the number of rows inserted.
    pub fn import_jsonl<R: std::io::BufRead>(&self, mut r: R) -> Result<usize> {
        let mut count = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            let n = r.read_line(&mut line).map_err(Error::Io)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| Error::Memory(format!("jsonl decode: {e}")))?;
            let project = v
                .get("project")
                .and_then(|x| x.as_str())
                .ok_or_else(|| Error::Memory("jsonl: missing `project`".into()))?;
            let kind = v
                .get("kind")
                .and_then(|x| x.as_str())
                .ok_or_else(|| Error::Memory("jsonl: missing `kind`".into()))?;
            let body = v
                .get("body")
                .and_then(|x| x.as_str())
                .ok_or_else(|| Error::Memory("jsonl: missing `body`".into()))?;
            let scope = v
                .get("scope")
                .and_then(|x| x.as_str())
                .map(MemoryScope::parse)
                .unwrap_or(MemoryScope::Project);
            let id = self.save_scoped(project, kind, body, scope)?;
            if let Some(meta) = v.get("metadata").and_then(|m| m.as_object()) {
                let map: std::collections::BTreeMap<String, String> = meta
                    .iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect();
                if !map.is_empty() {
                    self.set_metadata(id, &map)?;
                }
            }
            count += 1;
        }
        Ok(count)
    }

    /// Returns the JSON payload attached to a memory row. Empty when the row
    /// was stored without metadata.
    pub fn get_metadata(&self, id: i64) -> Result<std::collections::BTreeMap<String, String>> {
        let raw: String = self
            .conn
            .query_row(
                "SELECT metadata FROM memories WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        if raw.is_empty() {
            return Ok(Default::default());
        }
        serde_json::from_str(&raw).map_err(|e| Error::Memory(format!("metadata decode: {e}")))
    }

    /// Overwrites the body of an existing memory row. Keeps the FTS5 index
    /// in sync so later `recall_bm25` calls see the rewritten content. Used
    /// by the optional LLM auto-compress background worker.
    ///
    /// Note: any embedding row for `id` is left untouched. The auto-compress
    /// worker treats embeddings as a lossy summary that doesn't need to be
    /// regenerated for shorter rewrites; full re-embedding remains explicit.
    pub fn set_body(&self, id: i64, new_body: &str) -> Result<()> {
        // Fetch the old body so we can pass it to the FTS5 'delete' command —
        // external-content FTS doesn't track row contents itself.
        let old_body: String = self
            .conn
            .query_row(
                "SELECT body FROM memories WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        self.conn
            .execute(
                "UPDATE memories SET body = ?1 WHERE id = ?2",
                rusqlite::params![new_body, id],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO memories_fts(memories_fts, rowid, body) VALUES ('delete', ?1, ?2)",
                rusqlite::params![id, old_body],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO memories_fts(rowid, body) VALUES (?1, ?2)",
                rusqlite::params![id, new_body],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// Compress a row in place while preserving the original. On first
    /// compression the current `body` is copied into `body_full` (kept for
    /// reference); `body` then holds `compressed` and the FTS index is
    /// re-synced to it so recall returns the terse text. Re-compressing an
    /// already-compressed row leaves `body_full` as the true original.
    pub fn compress_in_place(&self, id: i64, compressed: &str) -> Result<()> {
        let (old_body, has_full): (String, bool) = self
            .conn
            .query_row(
                "SELECT body, body_full IS NOT NULL FROM memories WHERE id = ?1",
                rusqlite::params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        if !has_full {
            self.conn
                .execute(
                    "UPDATE memories SET body_full = body WHERE id = ?1",
                    rusqlite::params![id],
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }
        self.set_body(id, compressed)?;
        let _ = old_body;
        Ok(())
    }

    /// Returns the preserved pre-compression original for `id`, or `None`
    /// when the row was never compressed (its `body` is already the full
    /// text).
    pub fn full_body(&self, id: i64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT body_full FROM memories WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Returns up to `limit` rows in `project` that are eligible for LLM
    /// auto-compression:
    ///   - `created_at < older_than_ts` (cool-off window passed),
    ///   - `LENGTH(body) >= min_chars` (worth the LLM round-trip),
    ///   - metadata does not already contain `compressed_at` (idempotent).
    ///
    /// The metadata check is a `LIKE '%compressed_at%'` on the serialised
    /// JSON blob — false positives on a literal user-written key are
    /// possible but harmless because the worker re-writes idempotently.
    pub fn compress_candidates(
        &self,
        project: &str,
        older_than_ts: i64,
        min_chars: usize,
        limit: usize,
    ) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, body FROM memories \
                  WHERE project = ?1 \
                    AND created_at < ?2 \
                    AND LENGTH(body) >= ?3 \
                    AND (metadata IS NULL OR metadata NOT LIKE '%compressed_at%') \
                  ORDER BY created_at ASC LIMIT ?4",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![project, older_than_ts, min_chars as i64, limit as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Stores `metadata` against an existing memory row.
    pub fn set_metadata(
        &self,
        id: i64,
        metadata: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let raw = serde_json::to_string(metadata)
            .map_err(|e| Error::Memory(format!("metadata encode: {e}")))?;
        self.conn
            .execute(
                "UPDATE memories SET metadata = ?1 WHERE id = ?2",
                rusqlite::params![raw, id],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// Saves a project-scoped memory along with a metadata payload.
    pub fn save_with_metadata(
        &self,
        project: &str,
        kind: &str,
        body: &str,
        metadata: &std::collections::BTreeMap<String, String>,
    ) -> Result<i64> {
        let id = self.save(project, kind, body)?;
        self.set_metadata(id, metadata)?;
        Ok(id)
    }

    /// BM25 recall + qdrant-style payload filter. Over-fetches by 4× to keep
    /// the post-filter pass from starving the caller of hits; the limit is
    /// the maximum number of rows returned after filtering.
    pub fn recall_bm25_with_filter(
        &self,
        project: &str,
        query: &str,
        limit: usize,
        filter: &crate::payload::PayloadFilter,
    ) -> Result<Vec<MemoryRecord>> {
        if filter.is_empty() {
            return self.recall_bm25(project, query, limit);
        }
        let prelim = self.recall_bm25(project, query, limit.saturating_mul(4))?;
        let mut out = Vec::with_capacity(limit);
        for rec in prelim {
            let payload = self.get_metadata(rec.id)?;
            if filter.matches(&payload) {
                out.push(rec);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
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
        let placeholders = std::iter::repeat_n("?", order.len())
            .collect::<Vec<_>>()
            .join(",");
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
        let placeholders = std::iter::repeat_n("?", scopes.len())
            .collect::<Vec<_>>()
            .join(",");
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

    /// mem0-style single-pass ADD: extracts atomic facts, drops the ones whose
    /// body already exists for this `project` (exact string match), and saves
    /// only the survivors. Returns counts so the caller can show "+N facts /
    /// M duplicates skipped" feedback.
    pub async fn extract_and_save_unique(
        &self,
        project: &str,
        kind: &str,
        body: &str,
        summariser: &dyn Summariser,
    ) -> Result<UniqueIngest> {
        let facts = summariser.extract_atomic(body).await?;
        let existing = self.list_by_project(project, 10_000)?;
        let mut seen: std::collections::HashSet<String> =
            existing.into_iter().map(|m| m.body).collect();
        let mut added_ids = Vec::new();
        let mut skipped = 0usize;
        for fact in facts {
            if !seen.insert(fact.clone()) {
                skipped += 1;
                continue;
            }
            let id = self.save(project, kind, &fact)?;
            added_ids.push(id);
        }
        Ok(UniqueIngest { added_ids, skipped })
    }

    /// Letta-style memory block upsert. A "block" is a singleton memory keyed
    /// by `(project, "block:<name>")` — typical names are `persona`, `human`,
    /// `context`, but the caller picks. Setting a block overwrites any
    /// previous version (the old row + its FTS + embedding + edges all roll
    /// off via the cascading delete on `memories`). Returns the new id.
    pub fn set_block(&self, project: &str, name: &str, body: &str) -> Result<i64> {
        let kind = block_kind(name);
        self.conn
            .execute(
                "DELETE FROM memories WHERE project = ?1 AND kind = ?2",
                rusqlite::params![project, &kind],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        self.save(project, &kind, body)
    }

    /// Returns the singleton block for `(project, name)` if present.
    pub fn get_block(&self, project: &str, name: &str) -> Result<Option<MemoryRecord>> {
        let kind = block_kind(name);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope
                   FROM memories
                  WHERE project = ?1 AND kind = ?2
               ORDER BY created_at DESC
                  LIMIT 1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let mut rows = stmt
            .query_map(rusqlite::params![project, &kind], |row| {
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
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(Error::Memory(e.to_string())),
            None => Ok(None),
        }
    }

    /// Lists every block in `project` (entries whose `kind` starts with
    /// `"block:"`). Ordered by name ascending.
    pub fn list_blocks(&self, project: &str) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope
                   FROM memories
                  WHERE project = ?1 AND kind LIKE 'block:%'
               ORDER BY kind ASC, created_at DESC",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![project], |row| {
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

    /// LLM-driven entity linking. Extracts named entities from every memory
    /// in `project`, finds other memories that mention each entity via FTS5,
    /// and inserts `relation`-labelled edges between them. Returns the number
    /// of new edges actually written.
    ///
    /// Inspired by mem0's entity-linking step. The summariser supplies the
    /// entity list per memory; the search reuses the existing FTS5 index so
    /// no extra dependency or schema change is required.
    pub async fn link_entities(
        &self,
        project: &str,
        summariser: &dyn Summariser,
        relation: &str,
    ) -> Result<usize> {
        let mems = self.list_by_project(project, 10_000)?;
        let mut new_edges = 0usize;
        for source in &mems {
            let entities = summariser.extract_entities(&source.body).await?;
            for entity in entities {
                let cleaned = entity.replace('"', "");
                if cleaned.is_empty() {
                    continue;
                }
                let query = format!("\"{cleaned}\"");
                let hits = match self.recall_bm25(project, &query, 16) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                for hit in hits {
                    if hit.id == source.id {
                        continue;
                    }
                    let before: i64 = self
                        .conn
                        .query_row(
                            "SELECT COUNT(*) FROM edges WHERE src_id = ?1 AND dst_id = ?2 AND relation = ?3",
                            rusqlite::params![source.id, hit.id, relation],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    self.add_edge(source.id, hit.id, relation)?;
                    let after: i64 = self
                        .conn
                        .query_row(
                            "SELECT COUNT(*) FROM edges WHERE src_id = ?1 AND dst_id = ?2 AND relation = ?3",
                            rusqlite::params![source.id, hit.id, relation],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if after > before {
                        new_edges += 1;
                    }
                }
            }
        }
        Ok(new_edges)
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

    /// LLM-free consolidation: deletes the oldest rows beyond `keep_recent`,
    /// returns how many were removed. Used by the hourly background daemon
    /// when no summariser is wired and by the `memory_consolidate` MCP tool.
    pub fn archive_overflow_no_llm(&self, project: &str, keep_recent: usize) -> Result<usize> {
        let all = self.list_by_project(project, 100_000)?;
        if all.len() <= keep_recent {
            return Ok(0);
        }
        let to_remove = &all[..all.len() - keep_recent];
        for m in to_remove {
            self.delete(m.id)?;
        }
        Ok(to_remove.len())
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

    #[test]
    fn export_import_roundtrips() {
        use std::collections::BTreeMap;
        let src = MemoryStore::open_in_memory().unwrap();
        let mut m = BTreeMap::new();
        m.insert("source".into(), "claude".into());
        src.save_with_metadata("p1", "note", "alpha", &m).unwrap();
        src.save("p1", "note", "beta").unwrap();
        let mut buf = Vec::new();
        let count = src.export_jsonl("p1", &mut buf).unwrap();
        assert_eq!(count, 2);

        let dst = MemoryStore::open_in_memory().unwrap();
        let imported = dst.import_jsonl(std::io::Cursor::new(buf)).unwrap();
        assert_eq!(imported, 2);
        let hits = dst.recall_bm25("p1", "alpha", 5).unwrap();
        assert_eq!(hits.len(), 1);
        let meta = dst.get_metadata(hits[0].id).unwrap();
        assert_eq!(meta.get("source").map(|s| s.as_str()), Some("claude"));
    }

    #[test]
    fn payload_filter_narrows_bm25_recall() {
        use std::collections::BTreeMap;
        let store = MemoryStore::open_in_memory().unwrap();
        let mut m_a = BTreeMap::new();
        m_a.insert("source".into(), "claude".into());
        m_a.insert("topic".into(), "auth_flow".into());
        store
            .save_with_metadata("p1", "note", "rust auth token rotation", &m_a)
            .unwrap();
        let mut m_b = BTreeMap::new();
        m_b.insert("source".into(), "cursor".into());
        m_b.insert("topic".into(), "billing".into());
        store
            .save_with_metadata("p1", "note", "rust billing flow", &m_b)
            .unwrap();

        let filter = PayloadFilter::parse("source=claude").unwrap();
        let hits = store
            .recall_bm25_with_filter("p1", "rust", 5, &filter)
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].body.contains("auth"), "{hits:?}");

        let neg = PayloadFilter::parse("source!=cursor,topic~^auth").unwrap();
        let hits = store
            .recall_bm25_with_filter("p1", "rust", 5, &neg)
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].body.contains("auth"), "{hits:?}");
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

    /// Stub summariser that lets us drive `link_entities` deterministically.
    struct EntitySummariser;

    #[async_trait::async_trait]
    impl Summariser for EntitySummariser {
        fn model(&self) -> &str {
            "entity-stub"
        }
        async fn summarise(&self, text: &str) -> Result<String> {
            Ok(text.to_string())
        }
        async fn extract_atomic(&self, text: &str) -> Result<Vec<String>> {
            Ok(vec![text.to_string()])
        }
        async fn extract_entities(&self, text: &str) -> Result<Vec<String>> {
            // Pull bare words ≥ 3 chars, lowercased. Good enough for tests.
            Ok(text
                .split_whitespace()
                .map(|w| {
                    w.trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase()
                })
                .filter(|w| w.len() >= 3)
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect())
        }
    }

    #[tokio::test]
    async fn link_entities_creates_edges_between_co_mentioning_memories() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store
            .save("p", "fact", "rust cargo workspace ships well")
            .unwrap();
        let b = store
            .save("p", "fact", "cargo is rust's package manager")
            .unwrap();
        let _c = store.save("p", "fact", "go modules are similar").unwrap();
        let created = store
            .link_entities("p", &EntitySummariser, "mentions")
            .await
            .unwrap();
        assert!(created >= 2, "expected ≥2 edges, got {created}");
        // A and B both mention rust+cargo so they should be connected.
        let walk = store.recall_via_graph(&[a], 1).unwrap();
        assert!(walk.iter().any(|m| m.id == b), "{walk:?}");
    }

    #[test]
    fn memory_block_upsert_and_list() {
        let store = MemoryStore::open_in_memory().unwrap();
        let id1 = store
            .set_block("p1", "persona", "I am a careful Rust dev.")
            .unwrap();
        let id2 = store
            .set_block("p1", "persona", "I am a careful Rust dev v2.")
            .unwrap();
        assert_ne!(id1, id2);
        let got = store.get_block("p1", "persona").unwrap().unwrap();
        assert!(got.body.ends_with("v2."));
        store.set_block("p1", "human", "Kim DaeHyun").unwrap();
        let blocks = store.list_blocks("p1").unwrap();
        // 2 blocks (human + persona) — old persona row is deleted.
        assert_eq!(blocks.len(), 2);
        let names: Vec<_> = blocks.iter().map(|b| b.kind.clone()).collect();
        assert_eq!(
            names,
            vec!["block:human".to_string(), "block:persona".to_string()]
        );
    }

    #[tokio::test]
    async fn extract_and_save_unique_dedupes() {
        let store = MemoryStore::open_in_memory().unwrap();
        let s = crate::summarise::test_support::MockSummariser;
        // First ingest: 3 unique facts land.
        let first = store
            .extract_and_save_unique("p1", "note", "a is 1; b is 2; c is 3", &s)
            .await
            .unwrap();
        assert_eq!(first.added_ids.len(), 3);
        assert_eq!(first.skipped, 0);
        // Second ingest: 2 facts already present, 1 new.
        let second = store
            .extract_and_save_unique("p1", "note", "a is 1; b is 2; d is 4", &s)
            .await
            .unwrap();
        assert_eq!(second.added_ids.len(), 1);
        assert_eq!(second.skipped, 2);
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

    /// Regression test for the auto-capture pipeline primitives that the
    /// dashboard and rtrt-mcp both depend on.
    ///
    /// Verifies:
    /// 1. `body_sha` is deterministic and changes with content.
    /// 2. `save` returns a fresh id; `tag_row` writes session + sha.
    /// 3. `body_seen_at` returns the most recent timestamp for that sha
    ///    inside the same project, and `None` for unseen shas.
    /// 4. `sessions` groups rows by session id with correct counts.
    /// 5. `archive_overflow_no_llm` keeps the N newest rows.
    #[test]
    fn auto_capture_pipeline_primitives() {
        let store = MemoryStore::open_in_memory().unwrap();

        // 1. body_sha
        let sha_a = MemoryStore::body_sha("alpha");
        let sha_a2 = MemoryStore::body_sha("alpha");
        let sha_b = MemoryStore::body_sha("beta");
        assert_eq!(sha_a, sha_a2);
        assert_ne!(sha_a, sha_b);
        assert_eq!(sha_a.len(), 64, "sha-256 hex should be 64 chars");

        // 2. save + tag_row
        let id_a = store.save("p1", "note", "alpha").unwrap();
        store
            .tag_row(id_a, Some("session-x"), Some(&sha_a))
            .unwrap();
        let id_b = store.save("p1", "note", "beta").unwrap();
        store
            .tag_row(id_b, Some("session-y"), Some(&sha_b))
            .unwrap();
        assert_ne!(id_a, id_b);

        // 3. body_seen_at
        let seen_a = store.body_seen_at("p1", &sha_a).unwrap();
        assert!(seen_a.is_some(), "tagged sha should be discoverable");
        let unseen = store
            .body_seen_at("p1", &MemoryStore::body_sha("never-saved"))
            .unwrap();
        assert!(unseen.is_none(), "unseen sha must return None");
        let wrong_project = store.body_seen_at("p2", &sha_a).unwrap();
        assert!(wrong_project.is_none(), "dedup is scoped per project");

        // 4. sessions grouping
        let summary = store.sessions("p1").unwrap();
        let by_id: std::collections::BTreeMap<_, _> = summary
            .iter()
            .map(|(sid, n, _, _)| (sid.as_str(), *n))
            .collect();
        assert_eq!(by_id.get("session-x"), Some(&1));
        assert_eq!(by_id.get("session-y"), Some(&1));
        let session_x_rows = store.session_records("p1", "session-x", 10).unwrap();
        assert_eq!(session_x_rows.len(), 1);
        assert_eq!(session_x_rows[0].body, "alpha");

        // 5. archive_overflow_no_llm keeps the N newest
        for i in 0..5 {
            let body = format!("extra-{i}");
            let id = store.save("p1", "note", &body).unwrap();
            store
                .tag_row(id, Some("session-x"), Some(&MemoryStore::body_sha(&body)))
                .unwrap();
        }
        let total_before = store.count_by_project("p1").unwrap();
        assert_eq!(total_before, 7); // 2 originals + 5 extras
        let removed = store.archive_overflow_no_llm("p1", 3).unwrap();
        assert_eq!(removed, 4, "should drop the 4 oldest rows");
        let total_after = store.count_by_project("p1").unwrap();
        assert_eq!(total_after, 3);
    }

    /// Building blocks for the LLM auto-compress background worker.
    ///
    /// Verifies:
    /// 1. `set_body` overwrites the row and keeps `recall_bm25` in sync —
    ///    the old token disappears from FTS, the new one becomes findable.
    /// 2. `compress_candidates` honours the age, min-chars, and
    ///    "not-yet-compressed" filters, and excludes rows once
    ///    `metadata.compressed_at` is set.
    #[test]
    fn auto_compress_primitives() {
        use std::collections::BTreeMap;
        let store = MemoryStore::open_in_memory().unwrap();

        // long body, fresh — should not be a candidate (age filter)
        let long_fresh_body = "alpha ".repeat(200);
        store.save("p1", "note", &long_fresh_body).unwrap();

        // short body — should not be a candidate (min_chars filter)
        let short_id = store.save("p1", "note", "tiny").unwrap();

        // long + old — candidate; we forge `created_at` directly.
        let long_old_body = "beta ".repeat(200);
        let long_old_id = store.save("p1", "note", &long_old_body).unwrap();
        store
            .conn
            .execute(
                "UPDATE memories SET created_at = 1000 WHERE id = ?1",
                rusqlite::params![long_old_id],
            )
            .unwrap();

        // already-compressed row — must be excluded.
        let already = store.save("p1", "note", &"gamma ".repeat(200)).unwrap();
        store
            .conn
            .execute(
                "UPDATE memories SET created_at = 1000 WHERE id = ?1",
                rusqlite::params![already],
            )
            .unwrap();
        let mut meta = BTreeMap::new();
        meta.insert("compressed_at".into(), "1234".into());
        store.set_metadata(already, &meta).unwrap();

        let candidates = store.compress_candidates("p1", 5000, 100, 10).unwrap();
        assert!(
            candidates.iter().any(|(id, _)| *id == long_old_id),
            "old long row should be a candidate"
        );
        assert!(
            !candidates.iter().any(|(id, _)| *id == short_id),
            "short row should not be a candidate"
        );
        assert!(
            !candidates.iter().any(|(id, _)| *id == already),
            "already-compressed row should be excluded"
        );

        // 2. set_body keeps the FTS5 index in sync.
        let target_id = store.save("p2", "note", "rust cargo workspace").unwrap();
        let hits = store.recall_bm25("p2", "rust", 5).unwrap();
        assert_eq!(hits.len(), 1);
        store.set_body(target_id, "go module replace").unwrap();
        let rust_hits = store.recall_bm25("p2", "rust", 5).unwrap();
        assert!(rust_hits.is_empty(), "old token must drop out of FTS");
        let go_hits = store.recall_bm25("p2", "module", 5).unwrap();
        assert_eq!(go_hits.len(), 1);
        assert!(go_hits[0].body.contains("module"));
    }
}
