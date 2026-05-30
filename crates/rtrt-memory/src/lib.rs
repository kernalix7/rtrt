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
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub mod embed;
#[cfg(feature = "hnsw")]
pub mod hnsw_index;
pub mod payload;
pub mod summarise;

#[cfg(feature = "embeddings")]
pub use embed::FastEmbedder;
#[cfg(feature = "ollama-embed")]
pub use embed::OllamaEmbedder;
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

/// Full detail for a single memory row, including the pre-compression original
/// body, the parsed metadata map, and a deterministic importance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailedRecord {
    pub id: i64,
    pub project: String,
    pub kind: String,
    /// Current body (terse when compressed).
    pub body: String,
    /// Pre-compression original; `None` when the row was never compressed.
    pub body_full: Option<String>,
    pub created_at: i64,
    pub scope: MemoryScope,
    /// JSON payload attached to this row.
    pub metadata: std::collections::BTreeMap<String, String>,
    /// True when `body_full IS NOT NULL` (an LLM rewrote the body).
    pub compressed: bool,
    /// Deterministic importance in `[0.0, 1.0]` — recency + length + bonuses.
    /// See [`MemoryStore::get_row`] for the exact formula.
    pub importance: f32,
}

/// Outcome of [`MemoryStore::extract_and_save_unique`]: the new ids that
/// landed and the number of duplicate facts that were skipped.
#[derive(Debug, Clone, Default)]
pub struct UniqueIngest {
    pub added_ids: Vec<i64>,
    pub skipped: usize,
}

/// A memory node in the bipartite entity graph.
#[derive(Debug, Clone, Serialize)]
pub struct MemNode {
    pub id: i64,
    pub kind: String,
    /// First 60 chars of the body.
    pub preview: String,
    /// `metadata.$.source_kind` (`main` / `subagent`), absent when unset.
    pub source_kind: Option<String>,
}

/// An entity node in the bipartite entity graph.
#[derive(Debug, Clone, Serialize)]
pub struct EntNode {
    pub id: i64,
    pub name: String,
    /// Number of memories linked to this entity.
    pub degree: usize,
}

/// Bipartite memory↔entity graph for one project: memory nodes, entity nodes,
/// and the `(memory_id, entity_id)` links between them.
#[derive(Debug, Clone, Serialize)]
pub struct BipartiteGraph {
    pub memories: Vec<MemNode>,
    pub entities: Vec<EntNode>,
    pub links: Vec<(i64, i64)>,
}

/// Similarity graph for one project: memory nodes plus weighted memory↔memory
/// edges built WITHOUT any generative LLM. Edge weight in `[0,1]`. When stored
/// embeddings exist the edges are dense-vector cosine similarity; otherwise they
/// fall back to BM25 lexical overlap — both are model-call-free (cosine reads
/// already-stored vectors; BM25 is the FTS5 index).
#[derive(Debug, Clone, Serialize)]
pub struct SimilarityGraph {
    pub memories: Vec<MemNode>,
    /// `(memory_a, memory_b, weight)`, undirected (a < b), strongest first.
    pub edges: Vec<(i64, i64, f32)>,
    /// `"vector"` (cosine over stored embeddings) or `"bm25"` (lexical fallback).
    pub basis: String,
}

/// Level-of-detail (LOD) overview: one bubble per cluster. Scales to hundreds
/// of thousands of nodes because the client only ever sees these summaries plus,
/// on demand, the members of a single cluster ([`ClusterMembers`]).
#[derive(Debug, Clone, Serialize)]
pub struct ClusterSummary {
    /// Deterministic cluster root = minimum member id.
    pub id: i64,
    /// Number of memories in the cluster.
    pub size: usize,
    /// Representative member preview (the root member's preview).
    pub label: String,
    /// `"main"` / `"subagent"` when a single source dominates, else `"mixed"`.
    pub dominant_source: String,
}

/// Whole-project clustering for the LOD overview. Built without any O(n²) pass:
/// an inverted token index yields candidate pairs, top-k peers per node are
/// union-found into clusters, and inter-cluster edges are aggregated + capped.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterIndex {
    /// Cluster summaries, sorted by size descending (ties by root id ascending).
    pub clusters: Vec<ClusterSummary>,
    /// Aggregated `(root_a, root_b, weight)` edges between clusters, capped at
    /// the strongest ~2000.
    pub cluster_edges: Vec<(i64, i64, f32)>,
    /// `memory_id -> cluster root`. Used by drill-down; not serialised.
    #[serde(skip)]
    pub node_cluster: std::collections::HashMap<i64, i64>,
}

/// Drill-down payload: the members of one cluster plus their intra-cluster
/// similarity edges.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterMembers {
    pub nodes: Vec<MemNode>,
    /// `(memory_a, memory_b, weight)`, undirected (a < b), strongest first.
    pub edges: Vec<(i64, i64, f32)>,
}

/// One row from [`MemoryStore::reattribution_candidates`]:
/// `(id, transcript_file, current_project, source_kind)`.
pub type ReattributionRow = (i64, String, String, Option<String>);

/// Prefix applied to the `kind` column for Letta-style memory blocks.
const BLOCK_KIND_PREFIX: &str = "block:";

/// Builds the `kind` string for a Letta memory block.
pub fn block_kind(name: &str) -> String {
    format!("{BLOCK_KIND_PREFIX}{name}")
}

/// Salient-token set of free text for lexical similarity: distinct lowercase
/// alphanumeric/Hangul tokens of ≥3 chars. Used by the similarity graph's
/// model-free fallback (Jaccard overlap between memories).
fn token_set(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Fast, deterministic hasher for integer keys (FxHash-style: one multiply +
/// rotate per word). The LOD clusterer hashes millions of packed `u64`
/// candidate-pair keys; the default SipHash makes that the dominant cost, so we
/// use this for those integer-keyed maps only. Not for untrusted/DoS-sensitive
/// keys — it is used purely on internal node-position integers.
#[derive(Default)]
struct FxHasher {
    state: u64,
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Process whole u64 words where possible; the clusterer only ever feeds
        // u64 / u32 / usize keys, so this hot path dominates.
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let word = u64::from_le_bytes(c.try_into().unwrap());
            self.state = (self.state.rotate_left(5) ^ word).wrapping_mul(SEED);
        }
        for &b in chunks.remainder() {
            self.state = (self.state.rotate_left(5) ^ b as u64).wrapping_mul(SEED);
        }
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        self.state = (self.state.rotate_left(5) ^ i).wrapping_mul(SEED);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.write_u64(i as u64);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
}

type FxBuildHasher = std::hash::BuildHasherDefault<FxHasher>;
type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

/// Disjoint-set (union-find) with path compression + union by rank. Used by the
/// LOD clusterer to merge candidate edges into connected components in near
/// constant amortised time per operation.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]]; // path halving
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

impl MemoryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(Error::Io)?;
            }
        }
        let conn = Connection::open(path.as_ref()).map_err(|e| Error::Memory(e.to_string()))?;
        // WAL lets readers (dashboard API) proceed while a writer (the transcript
        // watcher / capture path) holds the write lock — without it a sweep can
        // stall every read for seconds. busy_timeout waits out brief contention
        // instead of failing with SQLITE_BUSY.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL;",
        )
        .map_err(|e| Error::Memory(e.to_string()))?;
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
        // Enable FK enforcement for this connection. Must run before any DML.
        // SQLite only cascades when this pragma is ON; without it, ON DELETE
        // CASCADE on embeddings/edges is silently ignored.
        self.conn
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| Error::Memory(e.to_string()))?;

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
        // v7: bipartite entity graph. `entities` holds the deduped entity names
        // per project; `memory_entities` is the join table linking a memory to
        // every entity it mentions. This is additive — the BM25-driven `edges`
        // path (memory↔memory) stays intact for other features; the bipartite
        // tables let the dashboard render a memory/entity graph directly.
        if v < 7 {
            self.conn
                .execute_batch(
                    r#"
                    CREATE TABLE IF NOT EXISTS entities (
                        id      INTEGER PRIMARY KEY,
                        project TEXT NOT NULL,
                        name    TEXT NOT NULL,
                        UNIQUE(project, name)
                    );
                    CREATE TABLE IF NOT EXISTS memory_entities (
                        memory_id INTEGER NOT NULL,
                        entity_id INTEGER NOT NULL,
                        UNIQUE(memory_id, entity_id)
                    );
                    CREATE INDEX IF NOT EXISTS idx_memory_entities_entity
                        ON memory_entities(entity_id);
                    CREATE INDEX IF NOT EXISTS idx_memory_entities_memory
                        ON memory_entities(memory_id);
                    PRAGMA user_version = 7;
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

    /// Stamp a transcript-captured row's `source_kind` (`main` | `subagent`)
    /// into its metadata, and optionally move it to a different project. The FTS
    /// index mirrors only the body, so changing `project` needs no FTS sync.
    /// One UPDATE via `json_set` so a row is never left half-migrated.
    pub fn reattribute(&self, id: i64, source_kind: &str, project: Option<&str>) -> Result<()> {
        match project {
            Some(p) => self.conn.execute(
                "UPDATE memories \
                    SET project = ?3, \
                        metadata = json_set(COALESCE(metadata, '{}'), '$.source_kind', ?2) \
                  WHERE id = ?1",
                rusqlite::params![id, source_kind, p],
            ),
            None => self.conn.execute(
                "UPDATE memories \
                    SET metadata = json_set(COALESCE(metadata, '{}'), '$.source_kind', ?2) \
                  WHERE id = ?1",
                rusqlite::params![id, source_kind],
            ),
        }
        .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
    }

    /// Transcript-captured rows that may need (re)attribution — purely by
    /// PROVENANCE, no project-name pattern matching. Returns every
    /// `source = "transcript"` row as `(id, transcript_file, current_project,
    /// source_kind)`; the caller re-resolves the project from the file's encoded
    /// dir and only writes when the project differs or the row is unclassified,
    /// so it's idempotent and cheap once everything has settled.
    pub fn reattribution_candidates(&self) -> Result<Vec<ReattributionRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.id, json_extract(m.metadata, '$.transcript_file') AS tf, m.project, \
                        json_extract(m.metadata, '$.source_kind') AS sk \
                   FROM memories m \
                  WHERE json_extract(m.metadata, '$.source') = 'transcript' \
                    AND tf IS NOT NULL",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
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
        self.recent_paged_filtered(project, limit, offset, None)
    }

    /// Like [`recent_paged`] but optionally restricted to rows whose
    /// `metadata.source_kind` equals `source_kind` (e.g. `"main"` / `"subagent"`).
    /// `None` returns every row — the server-side half of the memory page's
    /// 전체 / 메인 / 서브 filter, so the filter spans the whole project rather
    /// than just the current page.
    pub fn recent_paged_filtered(
        &self,
        project: &str,
        limit: usize,
        offset: usize,
        source_kind: Option<&str>,
    ) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, created_at, scope FROM memories \
                  WHERE project = ?1 \
                    AND (?4 IS NULL OR json_extract(metadata, '$.source_kind') = ?4) \
                  ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![project, limit as i64, offset as i64, source_kind],
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
        self.count_by_project_filtered(project, None)
    }

    /// [`count_by_project`] optionally restricted by `metadata.source_kind`, so
    /// the paged total matches a source-filtered timeline.
    pub fn count_by_project_filtered(
        &self,
        project: &str,
        source_kind: Option<&str>,
    ) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memories \
                  WHERE project = ?1 \
                    AND (?2 IS NULL OR json_extract(metadata, '$.source_kind') = ?2)",
                rusqlite::params![project, source_kind],
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
    /// Inserts a directed edge, ignoring duplicates. Returns `true` when a new
    /// row was created (the pair did not already exist), `false` when the edge
    /// was already present. Callers that don't care can discard the bool.
    pub fn add_edge(&self, src_id: i64, dst_id: i64, relation: &str) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO edges(src_id, dst_id, relation) VALUES (?1, ?2, ?3)",
                rusqlite::params![src_id, dst_id, relation],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(affected > 0)
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

    /// Governance delete: remove a single memory row, its FTS5 entry, and any
    /// edges or embeddings that reference it. With `PRAGMA foreign_keys = ON`
    /// (set in [`Self::migrate`]) the embeddings and edges cascade; the FTS5
    /// external-content table must be updated manually before the DELETE.
    ///
    /// Returns `true` when the row existed and was removed, `false` when the
    /// id was not found.
    pub fn delete_row(&self, id: i64) -> Result<bool> {
        // Fetch body before the DELETE so the FTS 'delete' command can pass the
        // old text (FTS5 external-content doesn't store it internally).
        let body: Option<String> = self
            .conn
            .query_row(
                "SELECT body FROM memories WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| Error::Memory(e.to_string()))?;

        let Some(old_body) = body else {
            return Ok(false);
        };

        // Remove the FTS5 entry first so the index stays consistent even if
        // the subsequent DELETE fails partway through.
        self.conn
            .execute(
                "INSERT INTO memories_fts(memories_fts, rowid, body) VALUES ('delete', ?1, ?2)",
                rusqlite::params![id, old_body],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;

        // The DELETE cascades to embeddings and edges via FK.
        self.conn
            .execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| Error::Memory(e.to_string()))?;

        Ok(true)
    }

    /// Governance batch delete: remove several rows in a single transaction.
    /// Returns the number of rows actually removed (ids not found are silently
    /// skipped). Cheaper than calling [`delete_row`] in a loop because the
    /// FTS5 maintenance and the FK cascades share one transaction.
    pub fn delete_rows(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }

        // Fetch (id, body) for every id that still exists, so we can feed the
        // FTS5 'delete' command the exact old body text.
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id, body FROM memories WHERE id IN ({placeholders})");
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| Error::Memory(e.to_string()))?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;

        let mut found: Vec<(i64, String)> = Vec::new();
        for r in rows {
            found.push(r.map_err(|e| Error::Memory(e.to_string()))?);
        }

        if found.is_empty() {
            return Ok(0);
        }

        // Remove FTS5 entries for every row we're about to delete.
        for (id, old_body) in &found {
            self.conn
                .execute(
                    "INSERT INTO memories_fts(memories_fts, rowid, body) VALUES ('delete', ?1, ?2)",
                    rusqlite::params![id, old_body],
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
        }

        // Delete the rows. FK cascades handle embeddings + edges.
        let del_sql = format!("DELETE FROM memories WHERE id IN ({placeholders})");
        let del_params: Vec<&dyn rusqlite::ToSql> = found
            .iter()
            .map(|(id, _)| id as &dyn rusqlite::ToSql)
            .collect();
        self.conn
            .execute(&del_sql, del_params.as_slice())
            .map_err(|e| Error::Memory(e.to_string()))?;

        Ok(found.len())
    }

    /// Returns the full detail row for a single memory id, including
    /// `body_full` (pre-compression original), `metadata`, and a deterministic
    /// importance score.
    ///
    /// Importance is computed without an LLM:
    ///   - recency component: `1 / (1 + age_days)` — newer rows score higher.
    ///   - length bonus: `min(1.0, body_len / 2000.0)` — longer bodies carry more signal.
    ///   - compression bonus: `+0.1` when `body_full IS NOT NULL` (row was compressed,
    ///     implying it was judged worth keeping).
    ///   - metadata bonus: `+0.05` when metadata is non-empty.
    ///
    /// Final score is clamped to `[0.0, 1.0]`.
    pub fn get_row(&self, id: i64) -> Result<Option<DetailedRecord>> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let row: rusqlite::Result<DetailedRecord> = self.conn.query_row(
            "SELECT id, project, kind, body, body_full, created_at, scope, metadata \
               FROM memories WHERE id = ?1",
            rusqlite::params![id],
            |row| {
                let scope_str: String = row.get(6)?;
                let meta_str: String = row.get(7)?;
                let body: String = row.get(3)?;
                let body_full: Option<String> = row.get(4)?;
                let created_at: i64 = row.get(5)?;
                let compressed = body_full.is_some();
                let age_days = ((now_secs - created_at).max(0) as f32) / 86400.0;
                let recency = 1.0 / (1.0 + age_days);
                let length_bonus = (body.len() as f32 / 2000.0).min(1.0);
                let compress_bonus = if compressed { 0.1 } else { 0.0 };
                let meta_bonus = if meta_str.len() > 2 { 0.05 } else { 0.0 };
                let importance =
                    (recency * 0.6 + length_bonus * 0.35 + compress_bonus + meta_bonus)
                        .clamp(0.0, 1.0);
                Ok(DetailedRecord {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    kind: row.get(2)?,
                    body,
                    body_full,
                    created_at,
                    scope: MemoryScope::parse(&scope_str),
                    metadata: serde_json::from_str(&meta_str).unwrap_or_default(),
                    compressed,
                    importance,
                })
            },
        );

        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Memory(e.to_string())),
        }
    }

    /// Paginated timeline ordered by importance score (deterministic, no LLM).
    /// The importance formula matches [`DetailedRecord`] — see [`get_row`].
    pub fn recent_paged_by_importance(
        &self,
        project: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<DetailedRecord>> {
        self.recent_paged_by_importance_filtered(project, limit, offset, None)
    }

    /// [`recent_paged_by_importance`] optionally restricted by
    /// `metadata.source_kind`.
    pub fn recent_paged_by_importance_filtered(
        &self,
        project: &str,
        limit: usize,
        offset: usize,
        source_kind: Option<&str>,
    ) -> Result<Vec<DetailedRecord>> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Fetch a larger window and sort in Rust so we can compute the
        // composite score without storing it in the schema.
        let fetch_limit = (offset + limit).saturating_mul(2).max(200);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, kind, body, body_full, created_at, scope, metadata \
                   FROM memories WHERE project = ?1 \
                    AND (?3 IS NULL OR json_extract(metadata, '$.source_kind') = ?3) \
                  ORDER BY created_at DESC, id DESC LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![project, fetch_limit as i64, source_kind],
                |row| {
                    let scope_str: String = row.get(6)?;
                    let meta_str: String = row.get(7)?;
                    let body: String = row.get(3)?;
                    let body_full: Option<String> = row.get(4)?;
                    let created_at: i64 = row.get(5)?;
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        body,
                        body_full,
                        created_at,
                        scope_str,
                        meta_str,
                    ))
                },
            )
            .map_err(|e| Error::Memory(e.to_string()))?;

        let mut records: Vec<DetailedRecord> = Vec::new();
        for r in rows {
            let (id, project, kind, body, body_full, created_at, scope_str, meta_str) =
                r.map_err(|e| Error::Memory(e.to_string()))?;
            let compressed = body_full.is_some();
            let age_days = ((now_secs - created_at).max(0) as f32) / 86400.0;
            let recency = 1.0 / (1.0 + age_days);
            let length_bonus = (body.len() as f32 / 2000.0).min(1.0);
            let compress_bonus = if compressed { 0.1 } else { 0.0 };
            let meta_bonus = if meta_str.len() > 2 { 0.05 } else { 0.0 };
            let importance =
                (recency * 0.6 + length_bonus * 0.35 + compress_bonus + meta_bonus).clamp(0.0, 1.0);
            records.push(DetailedRecord {
                id,
                project,
                kind,
                body,
                body_full,
                created_at,
                scope: MemoryScope::parse(&scope_str),
                metadata: serde_json::from_str(&meta_str).unwrap_or_default(),
                compressed,
                importance,
            });
        }

        records.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(records.into_iter().skip(offset).take(limit).collect())
    }

    /// Hybrid BM25 + graph-neighbour recall. Runs a BM25 search to find the
    /// initial seed hits, then expands each seed by one graph hop to pull in
    /// structurally related memories. The BM25 score and a graph-proximity
    /// bonus (0.1 per hop-1 neighbour) are combined for the final ranking.
    ///
    /// This gives a "graph-enhanced BM25" recall path without requiring true
    /// dense embeddings. Pass `mode = "bm25"` to skip the graph expansion and
    /// get pure BM25.
    pub fn recall_bm25_graph_blend(
        &self,
        project: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ScoredRecord>> {
        // BM25 seeds — over-fetch to leave room for graph expansion.
        let fetch = limit.saturating_mul(3).max(limit);
        let seeds = self.recall_bm25(project, query, fetch)?;
        if seeds.is_empty() {
            return Ok(vec![]);
        }

        // Assign BM25 rank-based scores (Reciprocal Rank).
        let rrf_k = 60.0f32;
        let mut scored: std::collections::HashMap<i64, (MemoryRecord, f32)> =
            std::collections::HashMap::new();
        for (rank, rec) in seeds.iter().enumerate() {
            let s = 1.0 / (rrf_k + (rank + 1) as f32);
            scored.insert(rec.id, (rec.clone(), s));
        }

        // Expand one hop out from the top seeds.
        let seed_ids: Vec<i64> = seeds.iter().map(|r| r.id).collect();
        let neighbours = self.recall_via_graph(&seed_ids, 1)?;
        for (i, nb) in neighbours.iter().enumerate() {
            if scored.contains_key(&nb.id) {
                continue; // already from BM25 — don't double-count
            }
            if nb.project != project {
                continue;
            }
            // Small bonus for being 1 hop away from a BM25 hit.
            let graph_score = 0.1 / (1.0 + i as f32);
            scored.insert(nb.id, (nb.clone(), graph_score));
        }

        let mut out: Vec<ScoredRecord> = scored
            .into_values()
            .map(|(record, score)| ScoredRecord { record, score })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(limit);
        Ok(out)
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
        let mut extracted: Vec<(i64, Vec<String>)> = Vec::with_capacity(mems.len());
        for source in &mems {
            let entities = summariser.extract_entities(&source.body).await?;
            extracted.push((source.id, entities));
        }
        self.link_extracted(project, &extracted, relation)
    }

    /// Synchronous half of [`link_entities`]: given entities already extracted
    /// per source memory, fans each entity out through BM25 recall and links
    /// the source to every co-mentioning memory. Returns the count of *new*
    /// edges created.
    ///
    /// Split out so callers in `Send` contexts (e.g. an axum handler) can run
    /// the async extraction step without holding a `&MemoryStore` borrow — the
    /// store is `!Sync`, so no reference to it may live across an `.await`.
    pub fn link_extracted(
        &self,
        project: &str,
        extracted: &[(i64, Vec<String>)],
        relation: &str,
    ) -> Result<usize> {
        let mut new_edges = 0usize;
        for (source_id, entities) in extracted {
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
                    if hit.id == *source_id {
                        continue;
                    }
                    if self.add_edge(*source_id, hit.id, relation)? {
                        new_edges += 1;
                    }
                }
            }
        }
        Ok(new_edges)
    }

    /// Insert an entity for `project` if absent, returning its id. Dedups on the
    /// `(project, name)` unique constraint.
    pub fn upsert_entity(&self, project: &str, name: &str) -> Result<i64> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO entities (project, name) VALUES (?1, ?2)",
                rusqlite::params![project, name],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        self.conn
            .query_row(
                "SELECT id FROM entities WHERE project = ?1 AND name = ?2",
                rusqlite::params![project, name],
                |r| r.get(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Link a memory to an entity. Idempotent via the `(memory_id, entity_id)`
    /// unique constraint; returns `true` only when a new link was created.
    pub fn link_memory_entity(&self, memory_id: i64, entity_id: i64) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id) VALUES (?1, ?2)",
                rusqlite::params![memory_id, entity_id],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(changed > 0)
    }

    /// Bipartite half of entity linking: given entities already extracted per
    /// source memory, upsert each entity and link the memory to it. Returns the
    /// count of *new* links created. Synchronous and `Send`-safe so axum
    /// handlers can run the async extraction without holding a `&self` borrow
    /// across an `.await`.
    pub fn link_extracted_bipartite(
        &self,
        project: &str,
        extracted: &[(i64, Vec<String>)],
    ) -> Result<usize> {
        let mut new_links = 0usize;
        for (memory_id, names) in extracted {
            for name in names {
                let cleaned = name.trim().replace('"', "");
                let cleaned = cleaned.trim();
                if cleaned.is_empty() {
                    continue;
                }
                let entity_id = self.upsert_entity(project, cleaned)?;
                if self.link_memory_entity(*memory_id, entity_id)? {
                    new_links += 1;
                }
            }
        }
        Ok(new_links)
    }

    /// Build the bipartite memory↔entity graph for `project`: up to `limit`
    /// memories, the project's entities, and the links between them. Each
    /// entity's `degree` is its linked-memory count; a memory's `source_kind`
    /// is read from `metadata.$.source_kind`.
    pub fn graph_bipartite(&self, project: &str, limit: usize) -> Result<BipartiteGraph> {
        let mut mem_stmt = self
            .conn
            .prepare(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind') \
                   FROM memories \
                  WHERE project = ?1 \
                  ORDER BY created_at DESC, id DESC \
                  LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let memories: Vec<MemNode> = mem_stmt
            .query_map(rusqlite::params![project, limit as i64], |r| {
                let body: String = r.get(2)?;
                Ok(MemNode {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    preview: body.chars().take(60).collect(),
                    source_kind: r.get(3)?,
                })
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?;

        let mem_ids: std::collections::HashSet<i64> = memories.iter().map(|m| m.id).collect();

        let mut ent_stmt = self
            .conn
            .prepare(
                "SELECT e.id, e.name, COUNT(me.memory_id) \
                   FROM entities e \
                   LEFT JOIN memory_entities me ON me.entity_id = e.id \
                  WHERE e.project = ?1 \
                  GROUP BY e.id, e.name",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let entities: Vec<EntNode> = ent_stmt
            .query_map(rusqlite::params![project], |r| {
                let degree: i64 = r.get(2)?;
                Ok(EntNode {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    degree: degree as usize,
                })
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?;

        let ent_ids: std::collections::HashSet<i64> = entities.iter().map(|e| e.id).collect();

        let mut link_stmt = self
            .conn
            .prepare(
                "SELECT me.memory_id, me.entity_id \
                   FROM memory_entities me \
                   JOIN entities e ON e.id = me.entity_id \
                  WHERE e.project = ?1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let links: Vec<(i64, i64)> = link_stmt
            .query_map(rusqlite::params![project], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| Error::Memory(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?
            .into_iter()
            // Keep only links whose memory survived the `limit` cap and whose
            // entity belongs to this project.
            .filter(|(m, e)| mem_ids.contains(m) && ent_ids.contains(e))
            .collect();

        Ok(BipartiteGraph {
            memories,
            entities,
            links,
        })
    }

    /// Build a memory↔memory similarity graph with NO generative LLM. Each
    /// memory is linked to its `top_k` most-similar peers. When stored
    /// embeddings cover the project, edges are cosine similarity over those
    /// vectors (no inference — the vectors already exist); otherwise it falls
    /// back to BM25 lexical overlap via the FTS5 index. Edges are undirected
    /// (`a < b`), deduped, and kept only at/above `min_weight`.
    pub fn graph_similarity(
        &self,
        project: &str,
        limit: usize,
        top_k: usize,
        min_weight: f32,
    ) -> Result<SimilarityGraph> {
        // Memory nodes (newest first, capped at `limit`).
        let mut mem_stmt = self
            .conn
            .prepare(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind') \
                   FROM memories \
                  WHERE project = ?1 \
                  ORDER BY created_at DESC, id DESC \
                  LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows: Vec<(MemNode, String)> = mem_stmt
            .query_map(rusqlite::params![project, limit as i64], |r| {
                let body: String = r.get(2)?;
                let node = MemNode {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    preview: body.chars().take(60).collect(),
                    source_kind: r.get(3)?,
                };
                Ok((node, body))
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?;

        let memories: Vec<MemNode> = rows.iter().map(|(n, _)| n.clone()).collect();
        let id_in_scope: std::collections::HashSet<i64> = memories.iter().map(|m| m.id).collect();

        // Try the dense-vector path: load stored embeddings for in-scope rows.
        let mut vectors: Vec<(i64, Vec<f32>)> = Vec::new();
        {
            let mut vec_stmt = self
                .conn
                .prepare(
                    "SELECT e.memory_id, e.vector FROM embeddings e \
                       JOIN memories m ON m.id = e.memory_id \
                      WHERE m.project = ?1",
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
            let it = vec_stmt
                .query_map(rusqlite::params![project], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|e| Error::Memory(e.to_string()))?;
            for row in it {
                let (id, blob) = row.map_err(|e| Error::Memory(e.to_string()))?;
                if id_in_scope.contains(&id) {
                    if let Ok(v) = vector_from_blob(&blob) {
                        vectors.push((id, v));
                    }
                }
            }
        }

        let mut edge_set: std::collections::BTreeMap<(i64, i64), f32> =
            std::collections::BTreeMap::new();
        let basis;

        if vectors.len() >= 2 {
            basis = "vector".to_string();
            // Pairwise cosine; keep each node's top_k strongest peers.
            for i in 0..vectors.len() {
                let mut peers: Vec<(i64, f32)> = Vec::with_capacity(vectors.len() - 1);
                for j in 0..vectors.len() {
                    if i == j {
                        continue;
                    }
                    let w = cosine(&vectors[i].1, &vectors[j].1);
                    if w >= min_weight {
                        peers.push((vectors[j].0, w));
                    }
                }
                peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                peers.truncate(top_k);
                let a = vectors[i].0;
                for (b, w) in peers {
                    let key = if a < b { (a, b) } else { (b, a) };
                    let e = edge_set.entry(key).or_insert(w);
                    if w > *e {
                        *e = w;
                    }
                }
            }
        } else {
            basis = "lexical".to_string();
            // Lexical fallback, fully in-memory (no FTS query per node — that
            // was O(n × FTS) and too slow). Build a salient-token set per memory
            // and link by Jaccard overlap; each node keeps its top_k peers.
            let toksets: Vec<(i64, std::collections::HashSet<String>)> = rows
                .iter()
                .map(|(n, body)| (n.id, token_set(body)))
                .collect();
            for i in 0..toksets.len() {
                if toksets[i].1.is_empty() {
                    continue;
                }
                let mut peers: Vec<(i64, f32)> = Vec::new();
                for j in 0..toksets.len() {
                    if i == j || toksets[j].1.is_empty() {
                        continue;
                    }
                    let inter = toksets[i].1.intersection(&toksets[j].1).count();
                    if inter == 0 {
                        continue;
                    }
                    let union = toksets[i].1.union(&toksets[j].1).count().max(1);
                    let w = inter as f32 / union as f32; // Jaccard
                    if w >= min_weight {
                        peers.push((toksets[j].0, w));
                    }
                }
                peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                peers.truncate(top_k);
                let a = toksets[i].0;
                for (b, w) in peers {
                    let key = if a < b { (a, b) } else { (b, a) };
                    let e = edge_set.entry(key).or_insert(w);
                    if w > *e {
                        *e = w;
                    }
                }
            }
        }

        let mut edges: Vec<(i64, i64, f32)> =
            edge_set.into_iter().map(|((a, b), w)| (a, b, w)).collect();
        edges.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));

        Ok(SimilarityGraph {
            memories,
            edges,
            basis,
        })
    }

    /// Build a level-of-detail [`ClusterIndex`] for one project with **no O(n²)
    /// pass**, so it stays fast at hundreds of thousands of rows.
    ///
    /// Pipeline:
    /// 1. Load up to `max_nodes` newest rows; tokenise each body with
    ///    [`token_set`].
    /// 2. Build an inverted index `token -> [memory_id]`. Postings larger than
    ///    a cap (`sqrt(n) * STOP_TOKEN_K`, floored at `STOP_TOKEN_MIN`) are
    ///    dropped as stop-tokens so common words never generate quadratic
    ///    candidate pairs.
    /// 3. From the surviving postings, generate candidate pairs and count
    ///    shared tokens — only nodes that share a posting are ever compared.
    /// 4. Score each candidate by Jaccard, keep each node's `top_k` strongest
    ///    peers at/above `min_weight`.
    /// 5. Union-find those edges into clusters; the cluster root is the
    ///    **minimum member id** (deterministic).
    /// 6. Build summaries (label = root member preview, dominant source from
    ///    member source kinds) sorted by size desc, and aggregate inter-cluster
    ///    edge weights capped at the strongest ~2000.
    pub fn graph_clusters(
        &self,
        project: &str,
        max_nodes: usize,
        top_k: usize,
        min_weight: f32,
    ) -> Result<ClusterIndex> {
        // 1. Load newest rows (capped). Keep token sets alongside node metadata.
        let mut mem_stmt = self
            .conn
            .prepare(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind') \
                   FROM memories \
                  WHERE project = ?1 \
                  ORDER BY created_at DESC, id DESC \
                  LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows: Vec<(MemNode, std::collections::HashSet<String>)> = mem_stmt
            .query_map(rusqlite::params![project, max_nodes as i64], |r| {
                let body: String = r.get(2)?;
                let tokens = token_set(&body);
                let node = MemNode {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    preview: body.chars().take(60).collect(),
                    source_kind: r.get(3)?,
                };
                Ok((node, tokens))
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?;

        let n = rows.len();

        // Index nodes 0..n for compact union-find / posting lists. `id_of[i]`
        // is the memory id of position `i`; `pos_of` maps id -> position.
        let id_of: Vec<i64> = rows.iter().map(|(node, _)| node.id).collect();
        let mut pos_of: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::with_capacity(n);
        for (i, id) in id_of.iter().enumerate() {
            pos_of.insert(*id, i);
        }

        // 2. Inverted index token -> positions. Drop oversized postings.
        // The posting cap is the single most important scaling knob: a posting
        // of length p contributes O(p²) candidate pairs, so common ("stop")
        // tokens must be excluded. We cap at `sqrt(n) * STOP_TOKEN_K` (floored at
        // STOP_TOKEN_MIN, hard-ceilinged at STOP_TOKEN_MAX) which keeps the total
        // candidate-pair count ~O(n · sqrt(n)) instead of O(n²) — and on real
        // data the surviving postings are far smaller than the cap.
        const STOP_TOKEN_K: f64 = 0.35;
        const STOP_TOKEN_MIN: usize = 24;
        const STOP_TOKEN_MAX: usize = 48;
        let posting_cap = ((n as f64).sqrt() * STOP_TOKEN_K) as usize;
        let posting_cap = posting_cap.clamp(STOP_TOKEN_MIN, STOP_TOKEN_MAX);

        let mut inverted: std::collections::HashMap<&str, Vec<u32>> =
            std::collections::HashMap::new();
        for (i, (_, tokens)) in rows.iter().enumerate() {
            for tok in tokens {
                inverted.entry(tok.as_str()).or_default().push(i as u32);
            }
        }

        // 3. Candidate generation: only pairs that share a surviving posting.
        // Shared-token counts are keyed on a single packed u64 (`hi<<32 | lo`)
        // so the map probes one integer instead of a tuple — markedly cheaper at
        // millions of candidate pairs. The FxHashMap avoids SipHash, which would
        // otherwise dominate this multi-million-insert hot loop.
        let pack = |x: u32, y: u32| -> u64 {
            let (lo, hi) = if x < y { (x, y) } else { (y, x) };
            ((hi as u64) << 32) | lo as u64
        };
        let mut shared: FxHashMap<u64, u32> = FxHashMap::default();
        for postings in inverted.values() {
            if postings.len() < 2 || postings.len() > posting_cap {
                // Singletons add no pairs; oversized postings are stop-tokens.
                continue;
            }
            for a in 0..postings.len() {
                for b in (a + 1)..postings.len() {
                    *shared.entry(pack(postings[a], postings[b])).or_insert(0) += 1;
                }
            }
        }

        // Precompute token-set sizes once: the scoring + aggregation loops each
        // touch millions of pairs, and recomputing `HashSet::len()` per touch is
        // a measurable cost.
        let tok_len: Vec<usize> = rows.iter().map(|(_, t)| t.len()).collect();

        // 4. Score candidates by Jaccard, keep each node's top_k peers.
        // peers[i] collects (other_pos, weight); we truncate per node afterwards.
        // We ALSO retain, per node, its single strongest candidate neighbour
        // regardless of weight (`best_peer`). The strong union-find only links
        // high-similarity pairs and leaves most rows as singletons, so the merge
        // passes below need these weak "best" edges as the rails along which
        // singletons get absorbed into a neighbour. Keeping only the best edge
        // per node bounds the merge edge set at O(n) — not O(n·sqrt(n)) — so the
        // fold stays cheap even with millions of candidate pairs.
        let unpack = |key: u64| -> (u32, u32) { (key as u32, (key >> 32) as u32) };
        let mut peers: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
        // best_peer[i] = (other_pos, weight) of i's strongest candidate edge.
        let mut best_peer: Vec<Option<(u32, f32)>> = vec![None; n];
        #[inline]
        fn consider_best(slot: &mut Option<(u32, f32)>, other: u32, w: f32) {
            match *slot {
                // Strongest wins; deterministic tiebreak on the smaller position.
                Some((bo, bw)) if bw > w || (bw == w && bo <= other) => {}
                _ => *slot = Some((other, w)),
            }
        }
        for (&key, inter) in &shared {
            let (x, y) = unpack(key);
            let (xi, yi) = (x as usize, y as usize);
            let union = tok_len[xi] + tok_len[yi] - *inter as usize;
            if union == 0 {
                continue;
            }
            let w = *inter as f32 / union as f32;
            consider_best(&mut best_peer[xi], y, w);
            consider_best(&mut best_peer[yi], x, w);
            if w < min_weight {
                continue;
            }
            peers[xi].push((y, w));
            peers[yi].push((x, w));
        }
        // Flatten best-peer edges into a compact, deduped merge-rail list (one
        // per node that has any candidate neighbour, direction a < b). O(n).
        let mut cand_edges: Vec<(u32, u32, f32)> = Vec::with_capacity(n);
        for (i, bp) in best_peer.iter().enumerate() {
            if let Some((other, w)) = *bp {
                let (a, b) = if (i as u32) < other {
                    (i as u32, other)
                } else {
                    (other, i as u32)
                };
                cand_edges.push((a, b, w));
            }
        }
        cand_edges.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        cand_edges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        // 5. Union-find over each node's top_k strongest peer edges.
        let mut uf = UnionFind::new(n);
        for (i, plist) in peers.iter_mut().enumerate() {
            plist.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            });
            plist.truncate(top_k);
            for (j, _w) in plist.iter() {
                uf.union(i, *j as usize);
            }
        }

        // 5b. Fold the singleton explosion down to a manageable bubble count.
        //
        // The strong union-find above merges only high-similarity pairs, so on
        // real projects most rows survive as size-1 components (00G_winpodx:
        // ~17k singletons of ~18k rows). The LOD overview must stay in the low
        // hundreds, so we absorb singleton/tiny clusters into the neighbour they
        // share their strongest *candidate* edge with — using ANY edge, even one
        // below the union threshold — over several passes, mirroring the
        // client-side `buildClusters` merge. `comp_min_id` is recomputed AFTER
        // all merges, so every cluster root stays the deterministic min member id
        // regardless of union-find's rank-based representative choice.
        const CLUSTER_TARGET: usize = 320;
        if n > 0 {
            // Candidate edges, strongest first (deterministic tiebreak on the
            // packed endpoints) so fragments pull toward their best neighbour.
            cand_edges.sort_by(|a, b| {
                b.2.partial_cmp(&a.2)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
                    .then(a.1.cmp(&b.1))
            });

            // Live component size, indexed by current union-find root position.
            let mut comp_size: Vec<usize> = vec![0; n];
            for i in 0..n {
                comp_size[uf.find(i)] += 1;
            }
            let mut cluster_count = comp_size.iter().filter(|&&s| s > 0).count();

            // Pass 1..k: absorb fragments whose size is at or below `small_bar`
            // into their best-candidate-edge neighbour. Relax `small_bar` each
            // pass so progressively larger linked fragments consolidate, while
            // genuinely disjoint clusters (no shared candidate edge) stay apart.
            let mut small_bar = 3usize;
            for _ in 0..6 {
                if cluster_count <= CLUSTER_TARGET {
                    break;
                }
                let mut merged_any = false;
                for &(x, y, _w) in &cand_edges {
                    let ra = uf.find(x as usize);
                    let rb = uf.find(y as usize);
                    if ra == rb {
                        continue;
                    }
                    let (sa, sb) = (comp_size[ra], comp_size[rb]);
                    // Merge only when at least one side is still a small fragment.
                    if sa > small_bar && sb > small_bar {
                        continue;
                    }
                    uf.union(ra, rb);
                    let merged_root = uf.find(ra);
                    let other = if merged_root == ra { rb } else { ra };
                    comp_size[merged_root] = sa + sb;
                    comp_size[other] = 0;
                    cluster_count -= 1;
                    merged_any = true;
                    if cluster_count <= CLUSTER_TARGET {
                        break;
                    }
                }
                if !merged_any {
                    // Nothing left that is linked at this bar; relaxing further
                    // would not help — the remainder is genuinely disjoint.
                    break;
                }
                small_bar = (small_bar * 3).min(400);
            }

            // Pass 2 (catch-all fallback): if disjoint singletons/tiny fragments
            // still blow past the target, fold every remaining cluster of size
            // <= `fold_bar` into one "misc/unclustered" catch-all so the overview
            // never explodes. The catch-all's members are real memory ids, so
            // `cluster_members` can still drill into it. We seed the catch-all on
            // the smallest leftover root (its min member id becomes the bubble's
            // deterministic id once `comp_min_id` is recomputed below).
            if cluster_count > CLUSTER_TARGET {
                // Order leftover roots by size desc; keep the largest, fold the
                // rest. Deterministic: tiebreak roots by their min member id.
                let mut min_id_of_root: Vec<i64> = vec![i64::MAX; n];
                for (i, &id) in id_of.iter().enumerate() {
                    let r = uf.find(i);
                    if id < min_id_of_root[r] {
                        min_id_of_root[r] = id;
                    }
                }
                let mut roots: Vec<usize> = (0..n).filter(|&r| comp_size[r] > 0).collect();
                roots.sort_by(|&a, &b| {
                    comp_size[b]
                        .cmp(&comp_size[a])
                        .then(min_id_of_root[a].cmp(&min_id_of_root[b]))
                });
                // Keep the largest (CLUSTER_TARGET - 1) clusters intact, leaving
                // one slot for the catch-all bubble.
                let keep = CLUSTER_TARGET.saturating_sub(1);
                let mut catch_all: Option<usize> = None;
                for &r in roots.iter().skip(keep) {
                    match catch_all {
                        None => catch_all = Some(r),
                        Some(c) => {
                            let cr = uf.find(c);
                            let rr = uf.find(r);
                            if cr != rr {
                                let sz = comp_size[cr] + comp_size[rr];
                                uf.union(cr, rr);
                                let merged_root = uf.find(cr);
                                let other = if merged_root == cr { rr } else { cr };
                                comp_size[merged_root] = sz;
                                comp_size[other] = 0;
                                catch_all = Some(merged_root);
                            }
                        }
                    }
                }
            }
        }

        // Resolve every node to its cluster root expressed as a *memory id* (the
        // minimum member id of the union-find component — deterministic), once,
        // into a flat array. Downstream loops then index this array instead of
        // calling `uf.find` + a HashMap lookup per touch (millions of touches in
        // the inter-cluster aggregation).
        let mut comp_of_pos: Vec<usize> = vec![0; n];
        let mut comp_min_id: FxHashMap<usize, i64> = FxHashMap::default();
        for (i, slot) in comp_of_pos.iter_mut().enumerate() {
            let comp = uf.find(i);
            *slot = comp;
            let id = id_of[i];
            comp_min_id
                .entry(comp)
                .and_modify(|m| {
                    if id < *m {
                        *m = id;
                    }
                })
                .or_insert(id);
        }
        // pos -> cluster root id (memory id), flat.
        let root_of_pos: Vec<i64> = comp_of_pos.iter().map(|c| comp_min_id[c]).collect();

        let mut node_cluster: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::with_capacity(n);
        for i in 0..n {
            node_cluster.insert(id_of[i], root_of_pos[i]);
        }

        // 6a. Cluster summaries: size, label (root member preview), dominant
        // source. Group members by cluster root id.
        let mut members_by_root: std::collections::HashMap<i64, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, &root_id) in root_of_pos.iter().enumerate() {
            members_by_root.entry(root_id).or_default().push(i);
        }

        let mut clusters: Vec<ClusterSummary> = members_by_root
            .iter()
            .map(|(root_id, positions)| {
                // Label = preview of the root member (min id).
                let root_pos = *pos_of.get(root_id).unwrap_or(&positions[0]);
                let label = rows[root_pos].0.preview.clone();
                // Dominant source: "main"/"subagent" if a single non-empty kind
                // covers every labelled member; "mixed" otherwise.
                let mut seen_source: Option<String> = None;
                let mut mixed = false;
                for &p in positions {
                    if let Some(src) = rows[p].0.source_kind.as_deref() {
                        match &seen_source {
                            None => seen_source = Some(src.to_string()),
                            Some(prev) if prev == src => {}
                            Some(_) => {
                                mixed = true;
                                break;
                            }
                        }
                    }
                }
                let dominant_source = if mixed {
                    "mixed".to_string()
                } else {
                    seen_source.unwrap_or_else(|| "mixed".to_string())
                };
                ClusterSummary {
                    id: *root_id,
                    size: positions.len(),
                    label,
                    dominant_source,
                }
            })
            .collect();
        clusters.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));

        // 6b. Inter-cluster aggregated edges. Reuse the candidate `shared` map:
        // every cross-cluster shared-token pair contributes its Jaccard weight
        // (summed). Cluster roots come from the flat `root_of_pos` array (no
        // `uf.find`/HashMap per touch), and the aggregation map is keyed on a
        // packed pair of *cluster indices* via the same FxHashMap as `shared`.
        // Cap at the strongest CLUSTER_EDGE_CAP.
        const CLUSTER_EDGE_CAP: usize = 2000;
        let mut agg: FxHashMap<u64, f32> = FxHashMap::default();
        for (&key, inter) in &shared {
            let (x, y) = unpack(key);
            let (xi, yi) = (x as usize, y as usize);
            // Pack on union-find component index (compact, fits u32) rather than
            // the i64 root id, so the key is a single u64.
            let (ca, cb) = (comp_of_pos[xi], comp_of_pos[yi]);
            if ca == cb {
                continue;
            }
            let union = tok_len[xi] + tok_len[yi] - *inter as usize;
            if union == 0 {
                continue;
            }
            let w = *inter as f32 / union as f32;
            *agg.entry(pack(ca as u32, cb as u32)).or_insert(0.0) += w;
        }
        let mut cluster_edges: Vec<(i64, i64, f32)> = agg
            .into_iter()
            .map(|(key, w)| {
                let (ca, cb) = unpack(key);
                // Map component indices back to deterministic root ids, ordered.
                let ra = comp_min_id[&(ca as usize)];
                let rb = comp_min_id[&(cb as usize)];
                let (a, b) = if ra < rb { (ra, rb) } else { (rb, ra) };
                (a, b, w)
            })
            .collect();
        cluster_edges.sort_by(|x, y| {
            y.2.partial_cmp(&x.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(x.0.cmp(&y.0))
                .then(x.1.cmp(&y.1))
        });
        cluster_edges.truncate(CLUSTER_EDGE_CAP);

        Ok(ClusterIndex {
            clusters,
            cluster_edges,
            node_cluster,
        })
    }

    /// Drill-down: the members of one cluster (by root id) plus their
    /// intra-cluster similarity edges. Reuses a prebuilt [`ClusterIndex`]
    /// (`index.node_cluster`) to decide membership, then recomputes the
    /// lexical similarity *within* the cluster only — cheap because a single
    /// cluster is a small slice of the project.
    pub fn cluster_members(
        &self,
        project: &str,
        root_id: i64,
        index: &ClusterIndex,
    ) -> Result<ClusterMembers> {
        // Max member nodes returned to the client per drill-down — keeps the
        // canvas renderable even for a huge catch-all cluster.
        const MEMBER_RENDER_CAP: usize = 800;
        // Member ids = nodes mapped to this root in the cached index.
        let member_ids: std::collections::HashSet<i64> = index
            .node_cluster
            .iter()
            .filter_map(|(&mid, &root)| (root == root_id).then_some(mid))
            .collect();
        if member_ids.is_empty() {
            return Ok(ClusterMembers {
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        }

        // Load member rows (newest first) + their token sets for intra edges.
        let mut mem_stmt = self
            .conn
            .prepare(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind') \
                   FROM memories \
                  WHERE project = ?1 \
                  ORDER BY created_at DESC, id DESC",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows: Vec<(MemNode, std::collections::HashSet<String>)> = mem_stmt
            .query_map(rusqlite::params![project], |r| {
                let id: i64 = r.get(0)?;
                let body: String = r.get(2)?;
                let node = MemNode {
                    id,
                    kind: r.get(1)?,
                    preview: body.chars().take(60).collect(),
                    source_kind: r.get(3)?,
                };
                Ok((node, body))
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .filter_map(|res| match res {
                Ok((node, body)) if member_ids.contains(&node.id) => {
                    Some(Ok((node, token_set(&body))))
                }
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            })
            // Cap the members streamed to the client: a catch-all / large cluster
            // can hold tens of thousands of rows, which would freeze the canvas.
            // Rows are newest-first, so this drills into the most recent members;
            // the summary's `size` still reports the true total.
            .take(MEMBER_RENDER_CAP)
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?;

        let nodes: Vec<MemNode> = rows.iter().map(|(node, _)| node.clone()).collect();

        // Intra-cluster edges via the same inverted-index candidate generation as
        // `graph_clusters` — NOT a naive O(m²) double loop. Most clusters are
        // small, but the "misc/unclustered" catch-all can hold thousands of rows;
        // a quadratic pass over it would take tens of seconds. The shared-posting
        // approach scores only pairs that actually share a (non-stop) token, so
        // drill-down stays fast for clusters of any size.
        let m = rows.len();
        let tok_len: Vec<usize> = rows.iter().map(|(_, t)| t.len()).collect();

        // Posting cap: drop common ("stop") tokens whose postings would make the
        // candidate-pair count quadratic (same knob/derivation as graph_clusters).
        const STOP_TOKEN_K: f64 = 0.35;
        const STOP_TOKEN_MIN: usize = 24;
        const STOP_TOKEN_MAX: usize = 48;
        let posting_cap = ((m as f64).sqrt() * STOP_TOKEN_K) as usize;
        let posting_cap = posting_cap.clamp(STOP_TOKEN_MIN, STOP_TOKEN_MAX);

        let mut inverted: std::collections::HashMap<&str, Vec<u32>> =
            std::collections::HashMap::new();
        for (i, (_, tokens)) in rows.iter().enumerate() {
            for tok in tokens {
                inverted.entry(tok.as_str()).or_default().push(i as u32);
            }
        }

        let pack = |x: u32, y: u32| -> u64 {
            let (lo, hi) = if x < y { (x, y) } else { (y, x) };
            ((hi as u64) << 32) | lo as u64
        };
        let unpack = |key: u64| -> (u32, u32) { (key as u32, (key >> 32) as u32) };
        let mut shared: FxHashMap<u64, u32> = FxHashMap::default();
        for postings in inverted.values() {
            if postings.len() < 2 || postings.len() > posting_cap {
                continue;
            }
            for a in 0..postings.len() {
                for b in (a + 1)..postings.len() {
                    *shared.entry(pack(postings[a], postings[b])).or_insert(0) += 1;
                }
            }
        }

        // Edge cap mirrors the inter-cluster cap: the UI only ever draws the
        // strongest intra-cluster links, so an unbounded edge list on the
        // catch-all would waste memory and sort time for no visual gain.
        const INTRA_EDGE_CAP: usize = 4000;
        let mut edges: Vec<(i64, i64, f32)> = Vec::with_capacity(shared.len());
        for (&key, inter) in &shared {
            let (x, y) = unpack(key);
            let (xi, yi) = (x as usize, y as usize);
            let union = tok_len[xi] + tok_len[yi] - *inter as usize;
            if union == 0 {
                continue;
            }
            let w = *inter as f32 / union as f32;
            let (a, b) = (rows[xi].0.id, rows[yi].0.id);
            let (a, b) = if a < b { (a, b) } else { (b, a) };
            edges.push((a, b, w));
        }
        edges.sort_by(|x, y| {
            y.2.partial_cmp(&x.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(x.0.cmp(&y.0))
                .then(x.1.cmp(&y.1))
        });
        edges.truncate(INTRA_EDGE_CAP);

        Ok(ClusterMembers { nodes, edges })
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

    /// Backfill embeddings for every row in `project` that does not yet have
    /// an entry in the `embeddings` table. Returns the number of newly embedded
    /// rows. Rows where the embedder returns an error are silently skipped so a
    /// transient Ollama hiccup doesn't abort the entire backfill.
    pub fn backfill_embeddings(&self, project: &str, embedder: &dyn Embedder) -> Result<usize> {
        let all = self.list_by_project(project, 100_000)?;
        let mut embedded = 0usize;
        for rec in all {
            // Skip rows that already have an embedding.
            let already: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM embeddings WHERE memory_id = ?1",
                    rusqlite::params![rec.id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            if already > 0 {
                continue;
            }
            let vec = match embedder.embed_one(&rec.body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let blob = vector_to_blob(&vec);
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO embeddings(memory_id, model, vector) VALUES (?1, ?2, ?3)",
                    rusqlite::params![rec.id, embedder.model_name(), blob],
                )
                .map_err(|e| Error::Memory(e.to_string()))?;
            embedded += 1;
        }
        Ok(embedded)
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

    #[test]
    fn upsert_entity_dedups_by_project_name() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store.upsert_entity("p1", "rust").unwrap();
        let b = store.upsert_entity("p1", "rust").unwrap();
        assert_eq!(a, b, "same (project,name) must return the same id");
        // Same name, different project is a distinct entity.
        let c = store.upsert_entity("p2", "rust").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn link_memory_entity_is_idempotent() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mem = store.save("p1", "note", "rust is fast").unwrap();
        let ent = store.upsert_entity("p1", "rust").unwrap();
        assert!(
            store.link_memory_entity(mem, ent).unwrap(),
            "first link new"
        );
        assert!(
            !store.link_memory_entity(mem, ent).unwrap(),
            "duplicate link must not count"
        );
    }

    #[test]
    fn link_extracted_bipartite_builds_entities_and_links() {
        let store = MemoryStore::open_in_memory().unwrap();
        let m1 = store.save("p1", "note", "rust and axum").unwrap();
        let m2 = store.save("p1", "note", "axum handler").unwrap();
        // m1 -> [rust, axum], m2 -> [axum]; entity names are cleaned/deduped.
        let extracted = vec![
            (m1, vec!["rust".to_string(), "\"axum\"".to_string()]),
            (m2, vec![" axum ".to_string(), "".to_string()]),
        ];
        let new_links = store.link_extracted_bipartite("p1", &extracted).unwrap();
        assert_eq!(new_links, 3, "rust+axum for m1, axum for m2");
        // Re-running creates no new links (idempotent entities + links).
        let again = store.link_extracted_bipartite("p1", &extracted).unwrap();
        assert_eq!(again, 0);

        let graph = store.graph_bipartite("p1", 100).unwrap();
        assert_eq!(graph.entities.len(), 2, "rust + axum, deduped");
        assert_eq!(graph.links.len(), 3);
    }

    #[test]
    fn graph_bipartite_returns_degree_and_source_kind() {
        let store = MemoryStore::open_in_memory().unwrap();
        let m1 = store.save("p1", "note", "rust and axum together").unwrap();
        let m2 = store.save("p1", "note", "axum only here").unwrap();
        store.reattribute(m1, "main", None).unwrap();
        store.reattribute(m2, "subagent", None).unwrap();
        store
            .link_extracted_bipartite(
                "p1",
                &[
                    (m1, vec!["rust".into(), "axum".into()]),
                    (m2, vec!["axum".into()]),
                ],
            )
            .unwrap();

        let graph = store.graph_bipartite("p1", 100).unwrap();

        let axum = graph
            .entities
            .iter()
            .find(|e| e.name == "axum")
            .expect("axum entity present");
        assert_eq!(axum.degree, 2, "axum linked by m1 and m2");
        let rust = graph
            .entities
            .iter()
            .find(|e| e.name == "rust")
            .expect("rust entity present");
        assert_eq!(rust.degree, 1);

        let mem1 = graph.memories.iter().find(|m| m.id == m1).unwrap();
        assert_eq!(mem1.source_kind.as_deref(), Some("main"));
        let mem2 = graph.memories.iter().find(|m| m.id == m2).unwrap();
        assert_eq!(mem2.source_kind.as_deref(), Some("subagent"));
    }

    #[test]
    fn graph_clusters_groups_shared_tokens_and_separates_unrelated() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Two clearly-related families plus one lone row.
        // Family A: authentication / token rotation.
        let a1 = store
            .save("p1", "note", "authentication token rotation policy")
            .unwrap();
        let a2 = store
            .save("p1", "note", "rotation authentication token expiry policy")
            .unwrap();
        let a3 = store
            .save("p1", "note", "token authentication rotation refresh")
            .unwrap();
        // Family B: database migration / schema.
        let b1 = store
            .save("p1", "note", "database migration schema versioning")
            .unwrap();
        let b2 = store
            .save(
                "p1",
                "note",
                "schema migration database rollback versioning",
            )
            .unwrap();
        // Lone row: shares no salient tokens with either family.
        let lone = store
            .save("p1", "note", "weather forecast tomorrow sunny")
            .unwrap();

        let idx = store.graph_clusters("p1", 100_000, 6, 0.1).unwrap();

        // node_cluster maps every loaded node.
        assert_eq!(idx.node_cluster.len(), 6);

        // Family A rows share a root; family B rows share a (different) root.
        let root_a = idx.node_cluster[&a1];
        assert_eq!(idx.node_cluster[&a2], root_a, "a2 with a1");
        assert_eq!(idx.node_cluster[&a3], root_a, "a3 with a1");
        let root_b = idx.node_cluster[&b1];
        assert_eq!(idx.node_cluster[&b2], root_b, "b2 with b1");
        assert_ne!(root_a, root_b, "the two families are distinct clusters");

        // The lone row is its own cluster, separate from both families.
        let root_lone = idx.node_cluster[&lone];
        assert_ne!(root_lone, root_a);
        assert_ne!(root_lone, root_b);

        // Deterministic root = min member id within each family.
        assert_eq!(root_a, a1.min(a2).min(a3), "family A root is its min id");
        assert_eq!(root_b, b1.min(b2), "family B root is its min id");

        // Summaries: family A (size 3) sorts before family B (size 2), and the
        // largest cluster is first.
        assert_eq!(idx.clusters[0].size, 3, "{:?}", idx.clusters);
        assert_eq!(idx.clusters[0].id, root_a);
        let summary_b = idx
            .clusters
            .iter()
            .find(|c| c.id == root_b)
            .expect("family B summary present");
        assert_eq!(summary_b.size, 2);
        // The lone row is a size-1 cluster.
        let summary_lone = idx
            .clusters
            .iter()
            .find(|c| c.id == root_lone)
            .expect("lone summary present");
        assert_eq!(summary_lone.size, 1);
    }

    #[test]
    fn graph_clusters_dominant_source_main_subagent_mixed() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Family where every member is "main".
        let m1 = store
            .save("p1", "note", "deploy pipeline release stage")
            .unwrap();
        let m2 = store
            .save("p1", "note", "release deploy pipeline rollback stage")
            .unwrap();
        store.reattribute(m1, "main", None).unwrap();
        store.reattribute(m2, "main", None).unwrap();
        // Family with a mix of main + subagent.
        let x1 = store
            .save("p1", "note", "kubernetes cluster scaling autoscaler")
            .unwrap();
        let x2 = store
            .save("p1", "note", "autoscaler kubernetes cluster scaling pods")
            .unwrap();
        store.reattribute(x1, "main", None).unwrap();
        store.reattribute(x2, "subagent", None).unwrap();

        let idx = store.graph_clusters("p1", 100_000, 6, 0.1).unwrap();

        let root_main = idx.node_cluster[&m1];
        let summary_main = idx.clusters.iter().find(|c| c.id == root_main).unwrap();
        assert_eq!(summary_main.dominant_source, "main");

        let root_mixed = idx.node_cluster[&x1];
        let summary_mixed = idx.clusters.iter().find(|c| c.id == root_mixed).unwrap();
        assert_eq!(summary_mixed.dominant_source, "mixed");
    }

    #[test]
    fn cluster_members_returns_members_and_intra_edges() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a1 = store
            .save("p1", "note", "indexing inverted posting list")
            .unwrap();
        let a2 = store
            .save("p1", "note", "posting list inverted indexing merge")
            .unwrap();
        let a3 = store
            .save("p1", "note", "inverted indexing posting compression")
            .unwrap();
        // Unrelated row that must NOT appear in the cluster drill-down.
        let _other = store
            .save("p1", "note", "guacamole avocado lime recipe")
            .unwrap();

        let idx = store.graph_clusters("p1", 100_000, 6, 0.1).unwrap();
        let root = idx.node_cluster[&a1];

        let members = store.cluster_members("p1", root, &idx).unwrap();
        let ids: std::collections::HashSet<i64> = members.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), 3, "exactly the three related rows");
        assert!(ids.contains(&a1) && ids.contains(&a2) && ids.contains(&a3));

        // Intra-cluster edges exist (the three share tokens) and are undirected
        // a < b, strongest first.
        assert!(!members.edges.is_empty(), "members are similar -> edges");
        for (a, b, w) in &members.edges {
            assert!(a < b, "undirected a<b");
            assert!(*w > 0.0);
            assert!(ids.contains(a) && ids.contains(b), "edge within cluster");
        }
        let weights: Vec<f32> = members.edges.iter().map(|e| e.2).collect();
        assert!(
            weights.windows(2).all(|w| w[0] >= w[1]),
            "edges sorted strongest first"
        );
    }

    /// Many mutually-unrelated rows must NOT explode into thousands of singleton
    /// bubbles. The merge passes fold the singleton explosion down to at most the
    /// internal CLUSTER_TARGET, and the leftover disjoint rows collapse into one
    /// "misc/unclustered" catch-all that `cluster_members` can still drill into.
    /// A handful of genuinely related rows still cluster together on the side.
    #[test]
    fn graph_clusters_folds_singletons_under_target() {
        let store = MemoryStore::open_in_memory().unwrap();
        // 900 rows that share no salient tokens with each other: each is a
        // distinct three-word "topic". With the old strong-only union-find these
        // would be ~900 singleton clusters; the fold passes must crush that down.
        for i in 0..900 {
            let body = format!("alphaword{i}xx betaword{i}yy gammaword{i}zz");
            store.save("big", "note", &body).unwrap();
        }
        // One clearly-related family that must stay clustered together.
        let r1 = store
            .save("big", "note", "shared anchor topic recurring phrase one")
            .unwrap();
        let r2 = store
            .save("big", "note", "shared anchor topic recurring phrase two")
            .unwrap();
        let r3 = store
            .save("big", "note", "shared anchor topic recurring phrase three")
            .unwrap();

        let idx = store.graph_clusters("big", 100_000, 6, 0.1).unwrap();

        // Every loaded node is still mapped.
        assert_eq!(idx.node_cluster.len(), 903);

        // The whole point: a manageable bubble count, never thousands.
        const TARGET: usize = 320;
        assert!(
            idx.clusters.len() <= TARGET,
            "cluster count must stay <= target, got {}",
            idx.clusters.len()
        );

        // The related family is still grouped into a single cluster.
        let root_fam = idx.node_cluster[&r1];
        assert_eq!(idx.node_cluster[&r2], root_fam, "r2 with r1");
        assert_eq!(idx.node_cluster[&r3], root_fam, "r3 with r1");

        // Every cluster root drills down successfully (incl. the catch-all),
        // and its members are real memory ids present in node_cluster.
        for c in &idx.clusters {
            let members = store.cluster_members("big", c.id, &idx).unwrap();
            assert_eq!(
                members.nodes.len(),
                c.size,
                "cluster {} drill-down returns all members",
                c.id
            );
            for node in &members.nodes {
                assert!(
                    idx.node_cluster.contains_key(&node.id),
                    "member {} is a real memory id",
                    node.id
                );
            }
        }

        // A catch-all bubble exists (the disjoint singletons folded together)
        // and is itself a real cluster that drills into many members.
        let biggest = idx.clusters.first().expect("at least one cluster");
        let catch = store.cluster_members("big", biggest.id, &idx).unwrap();
        assert_eq!(catch.nodes.len(), biggest.size);
        assert!(
            biggest.size > 1,
            "the catch-all absorbed many singletons, got size {}",
            biggest.size
        );
    }

    /// Performance probe against the real on-disk store. Ignored by default;
    /// run with `--ignored` after setting `RTRT_MEMORY_PATH` (defaults to
    /// `~/.rtrt/memory.sqlite`). The 18k-row `00G_winpodx` project timed the old
    /// O(n²) path out at >30s; the LOD path must clear it in well under a second.
    ///
    /// We run once to warm the OS page cache (the 95 MB sqlite file's first read
    /// is one-time disk I/O, not algorithmic), then time the steady-state build
    /// — which is what the dashboard's per-project cache actually serves.
    #[test]
    #[ignore = "needs the real on-disk memory store"]
    fn graph_clusters_winpodx_perf() {
        let path = std::env::var("RTRT_MEMORY_PATH").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap();
            format!("{home}/.rtrt/memory.sqlite")
        });
        let store = MemoryStore::open(&path).unwrap();
        // Warm-up (page cache + query plan); not measured.
        let _ = store
            .graph_clusters("00G_winpodx", 100_000, 6, 0.1)
            .unwrap();

        let t0 = std::time::Instant::now();
        let idx = store
            .graph_clusters("00G_winpodx", 100_000, 6, 0.1)
            .unwrap();
        let elapsed = t0.elapsed();
        let singletons = idx.clusters.iter().filter(|c| c.size == 1).count();
        eprintln!(
            "graph_clusters(00G_winpodx): {} nodes -> {} clusters ({} singletons), {} cluster_edges in {:?}",
            idx.node_cluster.len(),
            idx.clusters.len(),
            singletons,
            idx.cluster_edges.len(),
            elapsed
        );
        // The fold passes must keep the bubble count manageable for the overview.
        assert!(
            idx.clusters.len() <= 320,
            "cluster count must stay <= target, got {}",
            idx.clusters.len()
        );
        // Drill-down on the biggest cluster must also be fast.
        if let Some(top) = idx.clusters.first() {
            let dt0 = std::time::Instant::now();
            let members = store.cluster_members("00G_winpodx", top.id, &idx).unwrap();
            eprintln!(
                "cluster_members(root={}): {} nodes, {} edges in {:?}",
                top.id,
                members.nodes.len(),
                members.edges.len(),
                dt0.elapsed()
            );
        }
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "graph_clusters too slow: {elapsed:?}"
        );
    }
}
