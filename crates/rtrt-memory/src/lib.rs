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

/// A concept node in the Obsidian-style "digital brain" map. A concept is a
/// salient, word-like token (see [`token_set`]) whose document frequency sits
/// in the keep band — frequent enough to matter, not so frequent it is a
/// stop-word. Built **without any LLM**: concepts and their relationships come
/// entirely from token co-occurrence over the existing memory bodies.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptNode {
    /// The concept token, lowercase (the `name` the UI labels the node with).
    pub name: String,
    /// Number of memories containing this concept (document frequency).
    pub freq: usize,
    /// Number of distinct co-occurring concepts in the kept edge set.
    pub degree: usize,
    /// Distinct projects whose memories contain this concept. For a per-project
    /// brain this is a single project; for the GLOBAL brain a concept shared by
    /// several projects lists them all (the bridge that links the projects).
    pub projects: Vec<String>,
}

/// The full "digital brain" graph: concept nodes plus weighted concept↔concept
/// edges, derived purely from token co-occurrence ([`MemoryStore::concept_graph`]).
#[derive(Debug, Clone, Serialize)]
pub struct ConceptGraph {
    /// Concept nodes, sorted by degree desc, then freq desc, then name asc.
    pub nodes: Vec<ConceptNode>,
    /// Undirected co-occurrence edges `(concept_a, concept_b, weight)` with
    /// `concept_a < concept_b` lexicographically, strongest first. `weight` is
    /// the number of memories in which the two concepts co-occur.
    pub edges: Vec<(String, String, f32)>,
    /// Total memories scanned to build this brain.
    pub total_memories: usize,
}

/// A TOPIC COMMUNITY — the top level of the "digital brain" map. A community is
/// a cluster of co-occurring [`ConceptNode`]s, found **with no LLM** by running
/// the shared union-find ([`MemoryStore::cluster_from_peers`]) over the concept
/// co-occurrence graph itself. The first view shows a few dozen of these
/// super-nodes; drilling into one yields its concepts ([`ConceptGraph`]), and
/// drilling into a concept yields its memories.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptCommunity {
    /// Deterministic, stable id = the minimum member concept index (the same
    /// root `cluster_from_peers` assigns). Used as the drill key.
    pub id: i64,
    /// The highest-degree member concept's name (the community's headline).
    pub label: String,
    /// Memories touched = sum of member concept `freq` (document frequencies).
    pub size: usize,
    /// Number of member concepts in this community.
    pub concept_count: usize,
    /// A few highest-degree member concept names, for the hover tooltip.
    pub top_concepts: Vec<String>,
}

/// The top-level "digital brain" hierarchy: topic communities plus the
/// aggregated inter-community co-occurrence edges
/// ([`MemoryStore::concept_communities`]). Derived purely from token
/// co-occurrence — no LLM, no background job.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptHierarchy {
    /// Communities, sorted by `size` (memories touched) descending, ties by
    /// `id` ascending — deterministic.
    pub communities: Vec<ConceptCommunity>,
    /// Aggregated inter-community edges `(community_a_id, community_b_id, weight)`
    /// with `community_a_id < community_b_id`, strongest first. `weight` is the
    /// summed weight of the concept edges that cross the two communities.
    pub edges: Vec<(i64, i64, f32)>,
    /// Total concepts placed into communities.
    pub total_concepts: usize,
    /// Total memories scanned to build the underlying brain.
    pub total_memories: usize,
}

/// One row from [`MemoryStore::reattribution_candidates`]:
/// `(id, transcript_file, current_project, source_kind)`.
pub type ReattributionRow = (i64, String, String, Option<String>);

/// A clusterable row: a [`MemNode`] paired with its salient-[`token_set`].
/// The unit consumed by [`MemoryStore::cluster_rows`] /
/// [`MemoryStore::subcluster`] / [`MemoryStore::members_for_ids`].
pub type ClusterRow = (MemNode, std::collections::HashSet<String>);

/// Prefix applied to the `kind` column for Letta-style memory blocks.
const BLOCK_KIND_PREFIX: &str = "block:";

/// Default LOD-overview bubble target: the singleton-fold ceiling for a
/// whole-project [`MemoryStore::graph_clusters`] pass. Deeper drill-down levels
/// pass their own (smaller) target to [`MemoryStore::subcluster`].
const CLUSTER_TARGET: usize = 320;

/// Max member nodes a single drill-down returns to the client — keeps the
/// canvas renderable even for a huge catch-all cluster. Shared by
/// [`MemoryStore::cluster_members`] and [`MemoryStore::members_for_ids`].
const MEMBER_RENDER_CAP: usize = 800;

/// Max intra-cluster edges streamed per drill-down: the UI only draws the
/// strongest links, so an unbounded list would waste memory and sort time.
const INTRA_EDGE_CAP: usize = 4000;

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

/// Curated linguistic STOP-WORD set for the "digital brain" concept salience —
/// LINGUISTIC REFERENCE DATA, not a tunable magic number. These tokens are
/// excluded from concept candidates (and from co-occurrence) because they carry
/// no topical signal no matter how frequent they are: a brain whose top concepts
/// are "have"/"could"/"the" tells the user nothing, so distinctive
/// project/topic terms (tessellate / jacobian / podman / x86 / auth …) can only
/// surface once these are removed.
///
/// Two families, both function-word-like:
/// 1. English function words — auxiliaries / modals / pronouns / determiners /
///    prepositions / conjunctions / common adverbs. A concept is *about*
///    something; these are pure glue.
/// 2. Generic dev-log VERBS — "completed"/"added"/"fixed"/"working"… A memory
///    store of agent work logs is saturated with these; they describe the *act*
///    of working, never the *topic* worked on.
///
/// Kept lowercase (matches [`token_set`] output) and SORTED so membership is an
/// `O(log n)` `binary_search` with zero per-call allocation. Only tokens of ≥3
/// chars can ever be candidates (see [`token_set`]), so 1–2-char words are
/// already excluded and are intentionally absent here.
///
/// Family 1 (function words) and family 2 (dev-log verbs) are merged into one
/// sorted slice below; the section comments above describe intent, but the data
/// is kept flat-and-sorted for the `binary_search`.
static STOP_WORDS: &[&str] = &[
    "about",
    "add",
    "added",
    "adding",
    "adds",
    "after",
    "again",
    "against",
    "all",
    "also",
    "although",
    "always",
    "among",
    "and",
    "another",
    "any",
    "are",
    "around",
    "because",
    "been",
    "before",
    "being",
    "between",
    "both",
    "but",
    "can",
    "cannot",
    "change",
    "changed",
    "changes",
    "changing",
    "check",
    "checked",
    "checking",
    "checks",
    "complete",
    "completed",
    "completes",
    "completing",
    "continue",
    "continued",
    "continuing",
    "could",
    "create",
    "created",
    "creates",
    "creating",
    "cross",
    "did",
    "does",
    "doing",
    "done",
    "down",
    "during",
    "each",
    "either",
    "else",
    "ensure",
    "ensured",
    "ensures",
    "etc",
    "even",
    "ever",
    "every",
    "facing",
    "few",
    "find",
    "finding",
    "finds",
    "fix",
    "fixed",
    "fixes",
    "fixing",
    "for",
    "found",
    "from",
    "further",
    "get",
    "gets",
    "getting",
    "got",
    "had",
    "has",
    "have",
    "having",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "however",
    "into",
    "its",
    "itself",
    "just",
    "less",
    "like",
    "look",
    "looked",
    "looking",
    "looks",
    "made",
    "make",
    "makes",
    "making",
    "many",
    "may",
    "might",
    "more",
    "most",
    "much",
    "must",
    "need",
    "needed",
    "needs",
    "neither",
    "never",
    "new",
    "nor",
    "not",
    "now",
    "off",
    "once",
    "one",
    "only",
    "onto",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "per",
    "ran",
    "rather",
    "run",
    "running",
    "runs",
    "same",
    "see",
    "seen",
    "shall",
    "should",
    "since",
    "some",
    "start",
    "started",
    "starting",
    "starts",
    "such",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "thus",
    "too",
    "try",
    "trying",
    "two",
    "under",
    "understand",
    "understood",
    "until",
    "update",
    "updated",
    "updates",
    "updating",
    "upon",
    "use",
    "used",
    "uses",
    "using",
    "verified",
    "verifies",
    "verify",
    "verifying",
    "very",
    "via",
    "want",
    "wanted",
    "wants",
    "was",
    "were",
    "what",
    "when",
    "where",
    "whether",
    "which",
    "while",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "within",
    "without",
    "work",
    "worked",
    "working",
    "works",
    "would",
    "you",
    "your",
    "yours",
    "yourself",
];

/// `true` when `tok` is in the curated [`STOP_WORDS`] set. Binary search over the
/// sorted slice — deterministic, allocation-free. NOTE: relies on [`STOP_WORDS`]
/// being sorted; a `cfg(test)` test asserts that invariant.
fn is_stop_word(tok: &str) -> bool {
    STOP_WORDS.binary_search(&tok).is_ok()
}

/// Conservative single-step morphological base for merging surface variants
/// (`identifiers` → `identifier`, `occurrences` → `occurrence`, `facing` →
/// `face`). Returns `Some(base)` only for a SAFE strip; `None` when no rule
/// applies. This is *candidate* generation only — the caller keeps the stripped
/// form **just** when `base` is itself an attested corpus token (data-driven),
/// so it never mangles words like `class` → `clas` or `address` → `addres`.
///
/// Rules (first match wins), each requiring the base to stay ≥3 chars so we
/// never collapse a word into noise:
/// - `-ies` → `-y`   (`bodies` → `body`)
/// - `-es`  → `-e` or bare (`occurrences`→`occurrence` checked by caller as
///   `occurrenc(e)`; here we strip the trailing `s`, the `-e` is recovered by
///   the caller's attestation check against both `occurrenc` and `occurrence`)
/// - `-s`   → bare  (`identifiers` → `identifier`)
/// - `-ing` → bare or `+e` (`facing` → `fac`/`face`)
/// - `-ed`  → bare or `+e` (`faced` → `fac`/`face`)
///
/// Returns up to a few candidate bases (e.g. `fac` and `face` for `facing`); the
/// caller picks whichever is attested. Kept dependency-free and allocation-lean.
fn inflection_bases(tok: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(2);
    let n = tok.chars().count();
    let push = |s: String, out: &mut Vec<String>| {
        if s.chars().count() >= 3 && !out.iter().any(|e| e == &s) {
            out.push(s);
        }
    };
    // -ies → -y (bodies → body). Needs the strip to leave ≥3 chars after +y.
    if n >= 5 && tok.ends_with("ies") {
        let stem = &tok[..tok.len() - 3];
        push(format!("{stem}y"), &mut out);
    }
    // -ing → stem and stem+e (facing → fac, face). 'ing' only.
    if n >= 6 && tok.ends_with("ing") {
        let stem = &tok[..tok.len() - 3];
        push(stem.to_string(), &mut out);
        push(format!("{stem}e"), &mut out);
    }
    // -ed → stem and stem+e (faced → fac, face).
    if n >= 5 && tok.ends_with("ed") {
        let stem = &tok[..tok.len() - 2];
        push(stem.to_string(), &mut out);
        push(format!("{stem}e"), &mut out);
    }
    // -es → stem (occurrences → occurrence via stem 'occurrenc' + 'e', boxes →
    // box). Push both the bare stem and stem+e so the caller's attestation check
    // recovers either spelling.
    if n >= 5 && tok.ends_with("es") {
        let stem = &tok[..tok.len() - 2];
        push(stem.to_string(), &mut out);
        push(format!("{stem}e"), &mut out);
    }
    // -s plural (identifiers → identifier). Guard against -ss (class, address)
    // and the already-handled -ies/-es so we don't double-strip.
    if n >= 4 && tok.ends_with('s') && !tok.ends_with("ss") && !tok.ends_with("es") {
        let stem = &tok[..tok.len() - 1];
        push(stem.to_string(), &mut out);
    }
    out
}

/// Reduce a set of per-member `source_kind`s to a single dominant label:
/// `"main"` / `"subagent"` when one non-empty source covers every labelled
/// member, `"mixed"` otherwise (and when no member carries a source). Mirrors
/// the inline summary logic in [`MemoryStore::graph_clusters`]; reused by
/// [`MemoryStore::group_meta`].
fn dominant_source(sources: &[Option<String>]) -> String {
    let mut seen: Option<&str> = None;
    let mut mixed = false;
    for src in sources.iter().filter_map(|s| s.as_deref()) {
        match seen {
            None => seen = Some(src),
            Some(prev) if prev == src => {}
            Some(_) => {
                mixed = true;
                break;
            }
        }
    }
    if mixed {
        "mixed".to_string()
    } else {
        seen.map(str::to_string)
            .unwrap_or_else(|| "mixed".to_string())
    }
}

/// Bucket pre-labelled rows `(id, label, source_kind)` into a [`ClusterIndex`]:
/// one cluster per distinct label, root = min member id, dominant source from
/// members. When distinct labels exceed `target`, the largest `target-1` are
/// kept and the rest fold into one "(기타)" catch-all. Shared by
/// [`MemoryStore::group_meta`] (whole project) and
/// [`MemoryStore::group_meta_ids`] (an explicit id set).
fn bucket_meta_rows(rows: Vec<(i64, String, Option<String>)>, target: usize) -> ClusterIndex {
    struct Group {
        min_id: i64,
        label: String,
        member_ids: Vec<i64>,
        sources: Vec<Option<String>>,
    }
    // Preserve first-seen order so the catch-all fold is deterministic.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Group> = std::collections::HashMap::new();
    for (id, label, source_kind) in rows {
        let g = groups.entry(label.clone()).or_insert_with(|| {
            order.push(label.clone());
            Group {
                min_id: id,
                label: label.chars().take(60).collect(),
                member_ids: Vec::new(),
                sources: Vec::new(),
            }
        });
        g.min_id = g.min_id.min(id);
        g.member_ids.push(id);
        g.sources.push(source_kind);
    }

    let mut node_cluster: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut clusters: Vec<ClusterSummary> = Vec::new();
    let distinct = order.len();
    let mut owned: Vec<Group> = order
        .into_iter()
        .map(|lbl| groups.remove(&lbl).expect("label present"))
        .collect();

    if target >= 1 && distinct > target {
        owned.sort_by(|a, b| {
            b.member_ids
                .len()
                .cmp(&a.member_ids.len())
                .then(a.min_id.cmp(&b.min_id))
        });
        let keep = target.saturating_sub(1);
        let mut kept = owned;
        let folded: Vec<Group> = kept.split_off(keep);
        for g in &kept {
            for &mid in &g.member_ids {
                node_cluster.insert(mid, g.min_id);
            }
            clusters.push(ClusterSummary {
                id: g.min_id,
                size: g.member_ids.len(),
                label: g.label.clone(),
                dominant_source: dominant_source(&g.sources),
            });
        }
        if !folded.is_empty() {
            let catch_root = folded.iter().map(|g| g.min_id).min().expect("non-empty");
            let mut size = 0usize;
            let mut sources: Vec<Option<String>> = Vec::new();
            for g in &folded {
                for &mid in &g.member_ids {
                    node_cluster.insert(mid, catch_root);
                }
                size += g.member_ids.len();
                sources.extend(g.sources.iter().cloned());
            }
            clusters.push(ClusterSummary {
                id: catch_root,
                size,
                label: "(기타)".to_string(),
                dominant_source: dominant_source(&sources),
            });
        }
    } else {
        for g in &owned {
            for &mid in &g.member_ids {
                node_cluster.insert(mid, g.min_id);
            }
            clusters.push(ClusterSummary {
                id: g.min_id,
                size: g.member_ids.len(),
                label: g.label.clone(),
                dominant_source: dominant_source(&g.sources),
            });
        }
    }

    clusters.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
    ClusterIndex {
        clusters,
        cluster_edges: Vec::new(),
        node_cluster,
    }
}

/// First file-path-like token in free text, for [`MemoryStore::group_meta`]'s
/// `"file"` facet. Matches a run of `[\w./-]` of length ≥3 that either contains
/// a `/` (a path) or ends in a short `.ext` extension. The regex is compiled
/// once (process-wide) — never per row.
fn first_file_token(text: &str) -> Option<String> {
    use std::sync::LazyLock;
    // Two branches, leftmost-match wins:
    //  * a path: a `[\w.\-]` run, a `/`, then more `[\w./\-]` (contains a slash).
    //  * a filename: 2+ word/dash chars ending in a short `.ext` (1–5 alnum).
    // Both yield a token of length >= 3.
    static FILE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"[\w.\-]*/[\w./\-]+|[\w\-]{2,}\.[A-Za-z0-9]{1,5}")
            .expect("static file-token regex compiles")
    });
    FILE_RE.find(text).map(|m| m.as_str().to_string())
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
        self.graph_clusters_opt(project, max_nodes, top_k, min_weight, true, CLUSTER_TARGET)
    }

    /// Like [`graph_clusters`](Self::graph_clusters) but with explicit control
    /// over whether the semantic (vector) path may be used (`allow_vector =
    /// false` forces lexical) and the bubble `target` (the "세밀도" / granularity
    /// knob — a larger target means more, finer bubbles and a smaller catch-all).
    pub fn graph_clusters_opt(
        &self,
        project: &str,
        max_nodes: usize,
        top_k: usize,
        min_weight: f32,
        allow_vector: bool,
        target: usize,
    ) -> Result<ClusterIndex> {
        // When the project is (mostly) embedded, cluster on the dense vectors —
        // the real semantic signal — instead of lexical token overlap. Lexical
        // Jaccard collapses ~2/3 of a large project into one "unclustered" blob
        // because most rows share no salient tokens; cosine over embeddings does
        // not. We only switch when the build can actually use the HNSW index AND
        // at least half the project is embedded (so the vector map is
        // representative); otherwise we fall back to the lexical path below.
        #[cfg(feature = "hnsw")]
        if allow_vector {
            let (embedded, total) = self.embedding_coverage(project)?;
            if embedded > 0 && embedded >= total / 2 {
                return self.graph_clusters_vec(project, max_nodes, top_k, target);
            }
        }
        #[cfg(not(feature = "hnsw"))]
        let _ = allow_vector;

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

        // Whole-project clustering folds the singleton explosion down to the
        // requested bubble target.
        Ok(self.cluster_rows(rows, top_k, min_weight, target))
    }

    /// Cluster an already-loaded set of `(node, token_set)` rows into a
    /// [`ClusterIndex`]. This is the pure clustering core extracted from
    /// [`graph_clusters`]: it makes no database calls, so it can re-cluster an
    /// arbitrary subset (deeper LOD levels via [`subcluster`](Self::subcluster))
    /// just as well as a whole project. `target` is the singleton-fold ceiling
    /// (the overview uses [`CLUSTER_TARGET`]); deeper levels pass a smaller one.
    fn cluster_rows(
        &self,
        rows: Vec<(MemNode, std::collections::HashSet<String>)>,
        top_k: usize,
        min_weight: f32,
        target: usize,
    ) -> ClusterIndex {
        let n = rows.len();

        // Index nodes 0..n for compact union-find / posting lists. `id_of[i]`
        // is the memory id of position `i`.
        let id_of: Vec<i64> = rows.iter().map(|(node, _)| node.id).collect();

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
        // Hand off to the shared core: union-find over top_k peers, the
        // multi-pass singleton fold, the catch-all, and summary/edge building.
        // The lexical previews + sources come straight from the loaded rows.
        let previews: Vec<String> = rows.iter().map(|(node, _)| node.preview.clone()).collect();
        let sources: Vec<Option<String>> = rows
            .iter()
            .map(|(node, _)| node.source_kind.clone())
            .collect();
        // pos -> id, kept for the edge remap below (id_of is moved into the core).
        let id_pos = id_of.clone();
        let mut idx =
            Self::cluster_from_peers(id_of, previews, sources, peers, best_peer, top_k, target);

        // ── Split the dominant lexical "미분류" catch-all into topic bubbles ──
        // cluster_from_peers dumps every row it could not link into ONE bubble;
        // on a large project that is ~half the rows and reads as a single
        // undifferentiated blob ("정량으로 잘린 느낌"). Re-group those rows by each
        // one's SIGNATURE token — its rarest SHARED token, usually a topic word
        // the posting cap had dropped — so the blob becomes many small topic
        // groups instead. Only fires when a bubble really dominates (>= 25%).
        {
            let n_total = idx.node_cluster.len();
            let dom = idx
                .clusters
                .iter()
                .max_by_key(|c| c.size)
                .map(|c| (c.id, c.size));
            if let Some((dom_root, dom_size)) = dom
                && n_total > 0
                && dom_size >= 8
                && dom_size * 4 >= n_total
            {
                // Each dominant member's rarest token that at least two rows share.
                let mut by_sig: std::collections::HashMap<&str, Vec<usize>> =
                    std::collections::HashMap::new();
                let mut misc: Vec<usize> = Vec::new();
                for i in 0..n {
                    if idx.node_cluster.get(&id_pos[i]) != Some(&dom_root) {
                        continue;
                    }
                    // Signature = rarest SHARED, word-like token. Skip pure-numeric
                    // tokens (PR/issue numbers, counts) — they make noise topics.
                    let sig = rows[i]
                        .1
                        .iter()
                        .filter(|t| {
                            inverted.get(t.as_str()).map(|v| v.len()).unwrap_or(0) >= 2
                                && t.chars().any(|c| !c.is_numeric())
                        })
                        .min_by_key(|t| inverted[t.as_str()].len())
                        .map(|t| t.as_str());
                    match sig {
                        Some(s) => by_sig.entry(s).or_default().push(i),
                        None => misc.push(i),
                    }
                }
                // Groups of >=2 become topic bubbles; lone signatures fall to misc.
                let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
                for (s, v) in by_sig {
                    if v.len() >= 2 {
                        groups.push((s.to_string(), v));
                    } else {
                        misc.extend(v);
                    }
                }
                // Only worth doing if it actually breaks the blob into several.
                if groups.len() >= 2 {
                    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
                    // Topic-bubble budget = the 세밀도 target: more bubbles means a
                    // smaller leftover "(기타)". The tail folds into 기타 (drillable).
                    let keep = target.max(8);
                    if groups.len() > keep {
                        for (_, v) in groups.split_off(keep) {
                            misc.extend(v);
                        }
                    }
                    let src_of = |positions: &[usize]| -> String {
                        let s: Vec<Option<String>> = positions
                            .iter()
                            .map(|&i| rows[i].0.source_kind.clone())
                            .collect();
                        dominant_source(&s)
                    };
                    idx.clusters.retain(|c| c.id != dom_root);
                    for (label, positions) in &groups {
                        let root = positions
                            .iter()
                            .map(|&i| id_pos[i])
                            .min()
                            .unwrap_or(dom_root);
                        for &i in positions {
                            idx.node_cluster.insert(id_pos[i], root);
                        }
                        idx.clusters.push(ClusterSummary {
                            id: root,
                            size: positions.len(),
                            label: label.clone(),
                            dominant_source: src_of(positions),
                        });
                    }
                    if !misc.is_empty() {
                        let root = misc.iter().map(|&i| id_pos[i]).min().unwrap_or(dom_root);
                        for &i in &misc {
                            idx.node_cluster.insert(id_pos[i], root);
                        }
                        idx.clusters.push(ClusterSummary {
                            id: root,
                            size: misc.len(),
                            label: "(기타)".to_string(),
                            dominant_source: src_of(&misc),
                        });
                    }
                    idx.clusters
                        .sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
                }
            }
        }

        // Rebuild inter-cluster edges from the FULL candidate map (`shared`) —
        // EVERY token-sharing pair, including sub-`min_weight` ones the union-find
        // ignored. The fold leaves a node's above-threshold peers almost all
        // intra-cluster, so aggregating only those left the overview edgeless (a
        // disconnected grid); the weak cross pairs are exactly what tied the small
        // clusters to the rest in the original, richly-connected lexical map.
        const CLUSTER_EDGE_CAP: usize = 2000;
        let mut agg: std::collections::HashMap<(i64, i64), f32> = std::collections::HashMap::new();
        for (&key, &inter) in &shared {
            let (x, y) = unpack(key);
            let (xi, yi) = (x as usize, y as usize);
            let union = tok_len[xi] + tok_len[yi] - inter as usize;
            if union == 0 {
                continue;
            }
            let w = inter as f32 / union as f32;
            let (Some(&ra), Some(&rb)) = (
                idx.node_cluster.get(&id_pos[xi]),
                idx.node_cluster.get(&id_pos[yi]),
            ) else {
                continue;
            };
            if ra == rb {
                continue;
            }
            let pair = if ra < rb { (ra, rb) } else { (rb, ra) };
            *agg.entry(pair).or_insert(0.0) += w;
        }
        let mut edges: Vec<(i64, i64, f32)> =
            agg.into_iter().map(|((a, b), w)| (a, b, w)).collect();
        edges.sort_by(|x, y| {
            y.2.partial_cmp(&x.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(x.0.cmp(&y.0))
                .then(x.1.cmp(&y.1))
        });
        edges.truncate(CLUSTER_EDGE_CAP);
        idx.cluster_edges = edges;
        idx
    }

    /// Pure clustering core shared by the lexical [`cluster_rows`](Self::cluster_rows)
    /// and the vector [`graph_clusters_vec`](Self::graph_clusters_vec) paths.
    ///
    /// Given, per position `i`:
    /// * `ids[i]`     — its memory id,
    /// * `previews[i]`— its label text,
    /// * `sources[i]` — its `source_kind`,
    /// * `peers[i]`   — `(other_pos, weight)` neighbours above the caller's
    ///   similarity threshold (the union-find rails), and
    /// * `best_peer[i]` — its single strongest neighbour regardless of threshold
    ///   (the singleton-fold rails),
    ///
    /// this runs the union-find over each node's `top_k` strongest peers, the
    /// multi-pass singleton fold + catch-all down to `target` bubbles, then
    /// builds the deterministic summaries (root = min member id, size desc / id
    /// asc) and aggregated inter-cluster edges. No database calls and no
    /// dependence on *how* the peers were scored (lexical Jaccard or vector
    /// cosine), so both paths share it verbatim.
    fn cluster_from_peers(
        ids: Vec<i64>,
        previews: Vec<String>,
        sources: Vec<Option<String>>,
        mut peers: Vec<Vec<(u32, f32)>>,
        best_peer: Vec<Option<(u32, f32)>>,
        top_k: usize,
        target: usize,
    ) -> ClusterIndex {
        let n = ids.len();
        let id_of = &ids;

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

        // Keep the FULL candidate peer set (all pairs the caller passed, before
        // the top_k truncation below) for inter-cluster edge aggregation. The
        // union-find only needs each node's top_k, but aggregating edges from the
        // truncated set leaves the overview almost edgeless (a grid) — the lexical
        // map used to be richly connected and the truncation regressed it.
        let full_peers = peers.clone();

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
        // real projects most rows survive as size-1 components. The LOD overview
        // must stay in the low hundreds, so we absorb singleton/tiny clusters
        // into the neighbour they share their strongest *candidate* edge with —
        // using ANY edge, even one below the union threshold — over several
        // passes, mirroring the client-side `buildClusters` merge. `comp_min_id`
        // is recomputed AFTER all merges, so every cluster root stays the
        // deterministic min member id regardless of union-find's rank-based
        // representative choice. `target` is the fold ceiling (the whole-project
        // overview passes [`CLUSTER_TARGET`]; deeper LOD levels pass a smaller
        // value).
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
                if cluster_count <= target {
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
                    if cluster_count <= target {
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
            if cluster_count > target {
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
                // Keep the largest (target - 1) clusters intact, leaving
                // one slot for the catch-all bubble.
                let keep = target.saturating_sub(1);
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
        // calling `uf.find` + a HashMap lookup per touch.
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
        // source. Group member positions by cluster root id.
        let mut pos_by_root: std::collections::HashMap<i64, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, &root_id) in root_of_pos.iter().enumerate() {
            pos_by_root.entry(root_id).or_default().push(i);
        }
        // root id -> its member position (min id), for the label preview.
        let mut root_pos: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::with_capacity(pos_by_root.len());
        for (i, &root_id) in root_of_pos.iter().enumerate() {
            if id_of[i] == root_id {
                root_pos.insert(root_id, i);
            }
        }

        let mut clusters: Vec<ClusterSummary> = pos_by_root
            .iter()
            .map(|(root_id, positions)| {
                // Label = preview of the root member (min id).
                let rp = *root_pos.get(root_id).unwrap_or(&positions[0]);
                let label = previews[rp].clone();
                // Dominant source: "main"/"subagent" if a single non-empty kind
                // covers every labelled member; "mixed" otherwise.
                let mut seen_source: Option<&str> = None;
                let mut mixed = false;
                for &p in positions {
                    if let Some(src) = sources[p].as_deref() {
                        match seen_source {
                            None => seen_source = Some(src),
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
                    seen_source
                        .map(str::to_string)
                        .unwrap_or_else(|| "mixed".to_string())
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

        // 6b. Inter-cluster aggregated edges from the (post-truncation) peer
        // lists: every cross-cluster peer edge contributes its weight (summed).
        // Direction-agnostic (each undirected pair is seen twice — once from
        // each endpoint — so we halve), keyed on a packed pair of *cluster
        // indices*. Cap at the strongest CLUSTER_EDGE_CAP.
        const CLUSTER_EDGE_CAP: usize = 2000;
        let pack = |x: u32, y: u32| -> u64 {
            let (lo, hi) = if x < y { (x, y) } else { (y, x) };
            ((hi as u64) << 32) | lo as u64
        };
        let unpack = |key: u64| -> (u32, u32) { (key as u32, (key >> 32) as u32) };
        let mut agg: FxHashMap<u64, f32> = FxHashMap::default();
        for (xi, plist) in full_peers.iter().enumerate() {
            let ca = comp_of_pos[xi];
            for &(y, w) in plist {
                let yi = y as usize;
                let cb = comp_of_pos[yi];
                if ca == cb {
                    continue;
                }
                *agg.entry(pack(ca as u32, cb as u32)).or_insert(0.0) += w * 0.5;
            }
        }
        // Also fold in each node's single strongest candidate edge (`best_peer`,
        // ANY weight). After the aggressive singleton-fold the top-k `peers` are
        // almost all INTRA-cluster, leaving the overview with no connecting lines;
        // the best-peer rails are the cross-cluster links that keep the map
        // connected (every node contributes the one edge that pulled it toward
        // its nearest neighbour, even across cluster boundaries).
        for &(x, y, w) in &cand_edges {
            let ca = comp_of_pos[x as usize];
            let cb = comp_of_pos[y as usize];
            if ca == cb {
                continue;
            }
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

        ClusterIndex {
            clusters,
            cluster_edges,
            node_cluster,
        }
    }

    /// `(embedded, total)` row counts for `project`: how many rows have an entry
    /// in the `embeddings` table, and how many rows the project has overall.
    /// Drives the lexical-vs-vector clustering choice in
    /// [`graph_clusters`](Self::graph_clusters).
    pub fn embedding_coverage(&self, project: &str) -> Result<(usize, usize)> {
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE project = ?1",
                rusqlite::params![project],
                |r| r.get(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let embedded: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings e \
                   JOIN memories m ON m.id = e.memory_id \
                  WHERE m.project = ?1",
                rusqlite::params![project],
                |r| r.get(0),
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok((embedded as usize, total as usize))
    }

    /// Whole-project clustering over **dense embedding vectors** instead of
    /// lexical token overlap. This is the semantic clustering: it loads every
    /// row that has a stored embedding, builds an HNSW index over those vectors,
    /// and for each point takes its nearest neighbours as the union-find /
    /// singleton-fold rails — then hands off to the same
    /// [`cluster_from_peers`](Self::cluster_from_peers) core as the lexical path,
    /// so determinism (root = min id, size desc / id asc) and the fold-to-`target`
    /// behaviour are identical.
    ///
    /// The union threshold is the internal `VEC_MIN_SIM` cosine constant, NOT the
    /// lexical `min_weight`: unrelated nomic-embed text routinely sits at cosine
    /// 0.3–0.5, so a 0.15-style lexical threshold would merge everything into one
    /// blob. `VEC_MIN_SIM` is tuned so meaningful clusters form, the biggest stays
    /// well under ~30% of the project, and we get neither all-singletons nor one
    /// bubble. `best_peer` keeps each point's single strongest neighbour
    /// regardless of threshold so the fold always has rails to pull singletons in.
    #[cfg(feature = "hnsw")]
    pub fn graph_clusters_vec(
        &self,
        project: &str,
        max_nodes: usize,
        top_k: usize,
        target: usize,
    ) -> Result<ClusterIndex> {
        // Cosine-similarity union threshold for vectors. Tuned empirically on a
        // real embedded project (nomic-embed-text, 768-dim). See the tuning
        // notes / `graph_clusters_vec_speechpad_tune`.
        #[cfg(test)]
        let vec_min_sim: f32 = std::env::var("RTRT_VEC_MIN_SIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(VEC_MIN_SIM);
        #[cfg(not(test))]
        let vec_min_sim: f32 = VEC_MIN_SIM;
        const VEC_MIN_SIM: f32 = 0.82;

        // 1. Load every embedded row's (id, preview, source_kind) + its vector,
        //    newest first and capped at `max_nodes`. JOIN drops rows without an
        //    embedding so the HNSW map only ever sees points we can cluster.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.id, substr(m.body, 1, 60), \
                        json_extract(m.metadata, '$.source_kind'), e.vector \
                   FROM memories m \
                   JOIN embeddings e ON e.memory_id = m.id \
                  WHERE m.project = ?1 \
                  ORDER BY m.created_at DESC, m.id DESC \
                  LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        struct VecRow {
            id: i64,
            preview: String,
            source: Option<String>,
            vector: Vec<f32>,
        }
        let loaded: Vec<VecRow> = stmt
            .query_map(rusqlite::params![project, max_nodes as i64], |r| {
                let id: i64 = r.get(0)?;
                let preview: String = r.get(1)?;
                let source: Option<String> = r.get(2)?;
                let blob: Vec<u8> = r.get(3)?;
                Ok((id, preview, source, blob))
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .map(|row| {
                let (id, preview, source, blob) = row.map_err(|e| Error::Memory(e.to_string()))?;
                let vector = vector_from_blob(&blob)?;
                Ok(VecRow {
                    id,
                    preview,
                    source,
                    vector,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let n = loaded.len();
        if n == 0 {
            return Ok(ClusterIndex {
                clusters: Vec::new(),
                cluster_edges: Vec::new(),
                node_cluster: std::collections::HashMap::new(),
            });
        }

        // 2. Positional arrays for cluster_from_peers, and a pos lookup so we can
        //    convert neighbour ids back to compact positions.
        let ids: Vec<i64> = loaded.iter().map(|r| r.id).collect();
        let previews: Vec<String> = loaded.iter().map(|r| r.preview.clone()).collect();
        let sources: Vec<Option<String>> = loaded.iter().map(|r| r.source.clone()).collect();
        let mut pos_of: std::collections::HashMap<i64, u32> =
            std::collections::HashMap::with_capacity(n);
        for (i, &id) in ids.iter().enumerate() {
            pos_of.insert(id, i as u32);
        }

        // 3. Build the HNSW index over the loaded vectors.
        let index = match crate::hnsw_index::HnswIndex::from_vectors(
            loaded
                .iter()
                .map(|r| (r.vector.clone(), r.id))
                .collect::<Vec<_>>(),
        ) {
            Some(idx) => idx,
            None => {
                return Ok(ClusterIndex {
                    clusters: Vec::new(),
                    cluster_edges: Vec::new(),
                    node_cluster: std::collections::HashMap::new(),
                });
            }
        };

        // 4. For each point, query its top (top_k + 1) neighbours, drop self,
        //    convert cosine distance -> similarity (sim = 1 - distance), and
        //    build peers (sim >= VEC_MIN_SIM) + best_peer (strongest neighbour
        //    regardless of threshold, for the fold rails).
        let mut peers: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
        let mut best_peer: Vec<Option<(u32, f32)>> = vec![None; n];
        for (i, row) in loaded.iter().enumerate() {
            let hits = index.neighbors(&row.vector, top_k + 1);
            for (dist, nid) in hits {
                if nid == row.id {
                    continue; // self
                }
                let Some(&j) = pos_of.get(&nid) else {
                    continue;
                };
                if j as usize == i {
                    continue;
                }
                let sim = 1.0 - dist;
                // Strongest-neighbour rail (deterministic tiebreak on smaller pos).
                match best_peer[i] {
                    Some((bo, bw)) if bw > sim || (bw == sim && bo <= j) => {}
                    _ => best_peer[i] = Some((j, sim)),
                }
                if sim >= vec_min_sim {
                    peers[i].push((j, sim));
                }
            }
        }

        let mut idx =
            Self::cluster_from_peers(ids, previews, sources, peers, best_peer, top_k, target);

        // Cluster-level backbone: connect each cluster to its 2 nearest OTHER
        // clusters by CENTROID cosine. The per-node peer aggregation alone leaves
        // the overview almost edgeless (after the fold, a node's strongest links
        // are intra-cluster), so the map renders as a disconnected grid. Centroids
        // give a reliable, data-independent backbone — there are <= `target`
        // clusters, so the O(k²) pass is trivial. Replaces the sparse peer edges.
        let dim = loaded.first().map(|r| r.vector.len()).unwrap_or(0);
        if dim > 0 && idx.clusters.len() > 1 {
            let mut sum: std::collections::HashMap<i64, (Vec<f32>, usize)> =
                std::collections::HashMap::new();
            for row in &loaded {
                if let Some(&root) = idx.node_cluster.get(&row.id) {
                    let e = sum.entry(root).or_insert_with(|| (vec![0.0f32; dim], 0));
                    for (a, b) in e.0.iter_mut().zip(&row.vector) {
                        *a += *b;
                    }
                    e.1 += 1;
                }
            }
            let roots: Vec<i64> = {
                let mut r: Vec<i64> = sum.keys().copied().collect();
                r.sort_unstable();
                r
            };
            let centroids: Vec<Vec<f32>> = roots
                .iter()
                .map(|r| {
                    let (s, c) = &sum[r];
                    let inv = 1.0 / (*c as f32);
                    s.iter().map(|x| x * inv).collect()
                })
                .collect();
            let mut seen: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
            let mut edges: Vec<(i64, i64, f32)> = Vec::new();
            for i in 0..roots.len() {
                let mut best: Vec<(f32, usize)> = Vec::with_capacity(roots.len());
                for j in 0..roots.len() {
                    if i != j {
                        best.push((crate::embed::cosine(&centroids[i], &centroids[j]), j));
                    }
                }
                best.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                for &(s, j) in best.iter().take(2) {
                    let (a, b) = if roots[i] < roots[j] {
                        (roots[i], roots[j])
                    } else {
                        (roots[j], roots[i])
                    };
                    if seen.insert((a, b)) {
                        edges.push((a, b, s));
                    }
                }
            }
            edges.sort_by(|x, y| {
                y.2.partial_cmp(&x.2)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(x.0.cmp(&y.0))
                    .then(x.1.cmp(&y.1))
            });
            idx.cluster_edges = edges;
        }

        Ok(idx)
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
        let edges = Self::intra_edges(&rows);
        Ok(ClusterMembers { nodes, edges })
    }

    /// Intra-set lexical similarity edges over an already-loaded row slice.
    /// Shared by [`cluster_members`](Self::cluster_members) and
    /// [`members_for_ids`](Self::members_for_ids).
    ///
    /// Uses the same inverted-index candidate generation as
    /// [`graph_clusters`](Self::graph_clusters) — NOT a naive O(m²) double loop.
    /// Most clusters are small, but the "misc/unclustered" catch-all can hold
    /// thousands of rows; a quadratic pass over it would take tens of seconds.
    /// The shared-posting approach scores only pairs that actually share a
    /// (non-stop) token, so drill-down stays fast for sets of any size. Edges
    /// are undirected (`a < b`), strongest first, capped at [`INTRA_EDGE_CAP`].
    fn intra_edges(rows: &[(MemNode, std::collections::HashSet<String>)]) -> Vec<(i64, i64, f32)> {
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
        edges
    }

    /// Load exactly the given memory ids as clusterable rows
    /// (`id, kind, preview, source_kind` + [`token_set`] of the body),
    /// regardless of project. Used to re-cluster / render an arbitrary subset
    /// for deeper LOD levels. The IN-list is chunked to stay under SQLite's
    /// bound-variable limit (999); input order is not preserved.
    pub fn rows_for_ids(&self, ids: &[i64]) -> Result<Vec<ClusterRow>> {
        // SQLite's default SQLITE_MAX_VARIABLE_NUMBER is 999; chunk well under it.
        const CHUNK: usize = 900;
        let mut out: Vec<ClusterRow> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            // Build a `?,?,...` placeholder list for this chunk.
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind') \
                   FROM memories WHERE id IN ({placeholders})"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .map_err(|e| Error::Memory(e.to_string()))?;
            let params = rusqlite::params_from_iter(chunk.iter());
            let rows = stmt
                .query_map(params, |r| {
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
            out.extend(rows);
        }
        Ok(out)
    }

    /// Semantic re-clustering of an arbitrary memory-id subset: load those rows
    /// ([`rows_for_ids`](Self::rows_for_ids)) then run the same lexical
    /// clustering core as [`graph_clusters`](Self::graph_clusters)
    /// ([`cluster_rows`](Self::cluster_rows)) with the caller's fold `target`.
    /// This is the building block for deeper LOD drill-down levels.
    pub fn subcluster(
        &self,
        ids: &[i64],
        top_k: usize,
        min_weight: f32,
        target: usize,
    ) -> Result<ClusterIndex> {
        let rows = self.rows_for_ids(ids)?;
        Ok(self.cluster_rows(rows, top_k, min_weight, target))
    }

    /// Leaf rendering for an arbitrary memory-id set: load those rows, build
    /// their [`MemNode`]s and intra-set Jaccard similarity edges (same scoring
    /// as [`cluster_members`](Self::cluster_members)), capped at
    /// [`MEMBER_RENDER_CAP`] nodes / [`INTRA_EDGE_CAP`] edges.
    pub fn members_for_ids(&self, ids: &[i64]) -> Result<ClusterMembers> {
        let mut rows = self.rows_for_ids(ids)?;
        // Cap the members streamed to the client: a huge set would freeze the
        // canvas. `rows_for_ids` does not order, so cap on the newest ids
        // (largest id first) to drill into the most recent members.
        if rows.len() > MEMBER_RENDER_CAP {
            rows.sort_by(|a, b| b.0.id.cmp(&a.0.id));
            rows.truncate(MEMBER_RENDER_CAP);
        }
        let nodes: Vec<MemNode> = rows.iter().map(|(node, _)| node.clone()).collect();
        let edges = Self::intra_edges(&rows);
        Ok(ClusterMembers { nodes, edges })
    }

    /// Top-level metadata grouping (a non-semantic alternative to
    /// [`graph_clusters`](Self::graph_clusters)): bucket the newest `max_nodes`
    /// rows of `project` by a single metadata facet rather than lexical
    /// similarity. `key` selects the facet:
    ///
    /// * `"kind"`    — the `kind` column value.
    /// * `"source"`  — `metadata.$.source_kind` (`main` / `subagent`), else `unknown`.
    /// * `"session"` — `session_id`, else `(세션 없음)`.
    /// * `"file"`    — the first file-path-like token in the body, else `(파일 없음)`.
    ///
    /// Each group's `id` is its **minimum member id** (deterministic root),
    /// `size` its member count, `label` the (truncated) group label, and
    /// `dominant_source` is derived from members. Groups beyond `target` fold
    /// into one catch-all labelled `(기타)`. `cluster_edges` is always empty
    /// (metadata groups need no inter-edges). An unknown `key` is an error.
    pub fn group_meta(
        &self,
        project: &str,
        max_nodes: usize,
        key: &str,
        target: usize,
    ) -> Result<ClusterIndex> {
        // Reject unknown facets up front (spec: Err, not silent "kind" fallback).
        if !matches!(key, "kind" | "source" | "session" | "file" | "time") {
            return Err(Error::Memory(format!(
                "group_meta: unknown key `{key}` (expected kind|source|session|file|time)"
            )));
        }

        // Load newest rows of the project with every column a facet may need.
        // `time` buckets by hour (epoch -> local-agnostic UTC label).
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind'), session_id, \
                        strftime('%Y-%m-%d %Hh', created_at, 'unixepoch') \
                   FROM memories \
                  WHERE project = ?1 \
                  ORDER BY created_at DESC, id DESC \
                  LIMIT ?2",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        // Per row: (id, label, source_kind).
        let rows: Vec<(i64, String, Option<String>)> = stmt
            .query_map(rusqlite::params![project, max_nodes as i64], |r| {
                let id: i64 = r.get(0)?;
                let kind: String = r.get(1)?;
                let body: String = r.get(2)?;
                let source_kind: Option<String> = r.get(3)?;
                let session_id: Option<String> = r.get(4)?;
                let time_bucket: Option<String> = r.get(5)?;
                let label = match key {
                    "kind" => kind,
                    "source" => source_kind.clone().unwrap_or_else(|| "unknown".to_string()),
                    "session" => session_id.unwrap_or_else(|| "(세션 없음)".to_string()),
                    "time" => time_bucket.unwrap_or_else(|| "(시간 없음)".to_string()),
                    // "file": first file-path-like token in the body.
                    _ => first_file_token(&body).unwrap_or_else(|| "(파일 없음)".to_string()),
                };
                Ok((id, label, source_kind))
            })
            .map_err(|e| Error::Memory(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Memory(e.to_string()))?;

        Ok(bucket_meta_rows(rows, target))
    }

    /// Like [`group_meta`](Self::group_meta) but over an explicit id set rather
    /// than a whole project. Used as the drill-down fallback when a semantic
    /// re-cluster cannot meaningfully split a bucket (a lexically-disjoint
    /// "unclustered" mass): instead of recursing on a near-identical set forever,
    /// the caller re-partitions it by a metadata facet so it stays navigable.
    pub fn group_meta_ids(&self, ids: &[i64], key: &str, target: usize) -> Result<ClusterIndex> {
        if !matches!(key, "kind" | "source" | "session" | "file" | "time") {
            return Err(Error::Memory(format!(
                "group_meta_ids: unknown key `{key}` (expected kind|source|session|file|time)"
            )));
        }
        let mut rows: Vec<(i64, String, Option<String>)> = Vec::with_capacity(ids.len());
        // Chunk the IN-list to stay under SQLite's bound-variable limit.
        for chunk in ids.chunks(900) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, kind, body, json_extract(metadata, '$.source_kind'), session_id, \
                        strftime('%Y-%m-%d %Hh', created_at, 'unixepoch') \
                   FROM memories WHERE id IN ({placeholders})"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .map_err(|e| Error::Memory(e.to_string()))?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            let mapped = stmt
                .query_map(params.as_slice(), |r| {
                    let id: i64 = r.get(0)?;
                    let kind: String = r.get(1)?;
                    let body: String = r.get(2)?;
                    let source_kind: Option<String> = r.get(3)?;
                    let session_id: Option<String> = r.get(4)?;
                    let time_bucket: Option<String> = r.get(5)?;
                    let label = match key {
                        "kind" => kind,
                        "source" => source_kind.clone().unwrap_or_else(|| "unknown".to_string()),
                        "session" => session_id.unwrap_or_else(|| "(세션 없음)".to_string()),
                        "time" => time_bucket.unwrap_or_else(|| "(시간 없음)".to_string()),
                        _ => first_file_token(&body).unwrap_or_else(|| "(파일 없음)".to_string()),
                    };
                    Ok((id, label, source_kind))
                })
                .map_err(|e| Error::Memory(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::Memory(e.to_string()))?;
            rows.extend(mapped);
        }
        Ok(bucket_meta_rows(rows, target))
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

    /// Up to `limit` rows (newest first) that have **no** entry in the
    /// `embeddings` table, as `(id, project, body)` tuples. Lets the background
    /// auto-embed daemon embed incrementally — one capped batch per sweep —
    /// instead of scanning the whole project like [`backfill_embeddings`]. The
    /// `LEFT JOIN … WHERE e.memory_id IS NULL` is the unembedded set; `id DESC`
    /// makes each sweep prefer the freshest captures.
    pub fn unembedded_batch(&self, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.id, m.project, m.body
                   FROM memories m
                   LEFT JOIN embeddings e ON e.memory_id = m.id
                  WHERE e.memory_id IS NULL
               ORDER BY m.id DESC
                  LIMIT ?1",
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Memory(e.to_string()))
    }

    /// Store one embedding for `memory_id`. `INSERT OR IGNORE` so a row that was
    /// concurrently embedded (or re-fed by the daemon) is a no-op rather than an
    /// error. Companion to [`unembedded_batch`](Self::unembedded_batch): the
    /// daemon pulls a batch, embeds each body, then calls this per row.
    pub fn store_embedding(&self, memory_id: i64, model: &str, vector: &[f32]) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO embeddings(memory_id, model, vector) VALUES (?1, ?2, ?3)",
                rusqlite::params![memory_id, model, vector_to_blob(vector)],
            )
            .map_err(|e| Error::Memory(e.to_string()))?;
        Ok(())
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

    /// Build an Obsidian-style "digital brain" [`ConceptGraph`] — nodes are
    /// CONCEPTS (salient tokens), edges are their CO-OCCURRENCE — **with no LLM
    /// and no background job**. The signal is read straight from the memory
    /// bodies via [`token_set`], so the whole graph is computed on demand and
    /// stays well under a second at 20k+ memories.
    ///
    /// `scope`:
    /// - `Some(project)` → one project's brain.
    /// - `None` → the GLOBAL brain: every project merged into one map, where a
    ///   concept appearing in several projects bridges them (its `projects`
    ///   lists each one).
    ///
    /// Pipeline (entirely LLM-free; salience is data-driven, no flat magic-number
    /// cutoffs — every threshold is a formula over the corpus, plus a curated
    /// linguistic [`STOP_WORDS`] reference set):
    /// 1. Load the bodies (project filter applied in SQL when scoped), tokenise
    ///    each with [`token_set`]; keep only **word-like, non-stop-word** tokens
    ///    (≥1 non-digit + ≥1 alphabetic char, not in [`STOP_WORDS`]) so PR / issue
    ///    numbers and function words / dev-log verbs never become concepts.
    /// 2. Document frequency (df) per token + the set of projects it appears in.
    /// 3. NORMALISE then SUPPRESS, both data-driven. First merge morphological
    ///    variants (identifiers→identifier, occurrences→occurrence, facing→face)
    ///    into one concept, but ONLY when the stripped base is an attested corpus
    ///    token (so class↛clas); the concept NAME is the group's most-common
    ///    surface form, df/freq aggregate over the group (distinct docs, no
    ///    double-count). Then apply a DYNAMIC corpus-generic ceiling: drop groups
    ///    above the ~93rd percentile of candidate group df (derived from the df
    ///    distribution, not a constant) so the handful of corpus-ubiquitous tokens
    ///    are cut while mid-frequency distinctive terms survive. Keep the MIN_DF
    ///    floor, rank survivors by df desc, keep the top `max_concepts`.
    /// 4. EDGES: for each memory take its concept tokens, cap to the rarest
    ///    `PER_MEM_CAP` (rarest = lowest df, the most informative; bounds cost),
    ///    and increment co-occurrence for every unordered pair. Keep pairs with
    ///    weight ≥ `min_cooccur`, then keep the strongest `max_edges`.
    /// 5. `degree` = distinct neighbours in the kept edge set.
    ///
    /// Determinism: nodes sorted by degree desc, then freq desc, then name asc;
    /// edges (`a < b`) sorted by weight desc, then `a` asc, then `b` asc.
    pub fn concept_graph(
        &self,
        scope: Option<&str>,
        max_concepts: usize,
        max_edges: usize,
        min_cooccur: usize,
    ) -> Result<ConceptGraph> {
        /// df floor: drop hapax / near-unique noise tokens.
        const MIN_DF: usize = 3;
        /// Absolute df ceiling for stop-words even on a huge store.
        const DF_ABS_CAP: usize = 2_000;
        /// Max concept tokens contributed per memory to the pair count, picked
        /// by rarity (lowest df) — keeps edge generation O(memories · cap²).
        const PER_MEM_CAP: usize = 12;

        // 1. Load bodies (+ project, for the GLOBAL merge) and INTERN tokens in
        // the same streaming pass. One SQL shape with an optional project filter,
        // no per-token query, no LLM. Interning to u32 ids here means the df and
        // co-occurrence loops never re-hash a token string — the single biggest
        // win at 60k+ bodies, where building a `HashSet<String>` per row dominated.
        let mut tok_ids: Vec<String> = Vec::new(); // interner: id -> token text
        let mut tok_intern: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut proj_ids: Vec<String> = Vec::new(); // interner: id -> project name
        let mut proj_intern: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        // Per doc: (project id, deduped token ids). The token ids are already the
        // distinct salient tokens of the body (token_set dedups), word-like only.
        let mut docs: Vec<(u32, Vec<u32>)> = Vec::new();
        {
            let tok_ids = &mut tok_ids;
            let tok_intern = &mut tok_intern;
            let proj_ids = &mut proj_ids;
            let proj_intern = &mut proj_intern;
            let docs = &mut docs;
            let mut load = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Result<()> {
                let mut stmt = self
                    .conn
                    .prepare(sql)
                    .map_err(|e| Error::Memory(e.to_string()))?;
                let mapped = stmt
                    .query_map(params, |r| {
                        let project: String = r.get(0)?;
                        let body: String = r.get(1)?;
                        Ok((project, body))
                    })
                    .map_err(|e| Error::Memory(e.to_string()))?;
                for row in mapped {
                    let (project, body) = row.map_err(|e| Error::Memory(e.to_string()))?;
                    let pid = match proj_intern.get(project.as_str()) {
                        Some(&id) => id,
                        None => {
                            let id = proj_ids.len() as u32;
                            proj_ids.push(project.clone());
                            proj_intern.insert(project, id);
                            id
                        }
                    };
                    // Candidate tokens only. Three filters, cheapest first:
                    //   - word-like: at least one non-digit char, so "1234" /
                    //     "#42" never becomes a node (all-digit tokens dropped);
                    //   - not a curated STOP_WORD (function word / dev-log verb)
                    //     — excluded from BOTH concepts and co-occurrence, so a
                    //     stop-word can never even contribute an edge;
                    //   - ≥1 alphabetic char so pure-symbol noise can't slip in.
                    // token_set already enforces ≥3 chars + alphanumeric/Hangul.
                    let mut ids: Vec<u32> = token_set(&body)
                        .into_iter()
                        .filter(|t| {
                            t.chars().any(|c| !c.is_ascii_digit())
                                && t.chars().any(char::is_alphabetic)
                                && !is_stop_word(t)
                        })
                        .map(|t| match tok_intern.get(t.as_str()) {
                            Some(&id) => id,
                            None => {
                                let id = tok_ids.len() as u32;
                                tok_ids.push(t.clone());
                                tok_intern.insert(t, id);
                                id
                            }
                        })
                        .collect();
                    ids.sort_unstable();
                    docs.push((pid, ids));
                }
                Ok(())
            };
            match scope {
                Some(project) => load(
                    "SELECT project, body FROM memories WHERE project = ?1",
                    &[&project],
                )?,
                None => load("SELECT project, body FROM memories", &[])?,
            }
        }
        let total_memories = docs.len();
        let n_tokens = tok_ids.len();

        // Auto budget (0 = derive from the corpus): the concept/edge count scales
        // with the number of memories so a small project shows fewer nodes and a
        // large one more — never a flat hard-coded cap. ~2·√memories, clamped to a
        // renderable range; edges follow at 3× the concept count.
        let max_concepts = if max_concepts == 0 {
            ((total_memories as f64).sqrt() * 2.0)
                .round()
                .clamp(80.0, 600.0) as usize
        } else {
            max_concepts
        };
        let max_edges = if max_edges == 0 {
            max_concepts.saturating_mul(3)
        } else {
            max_edges
        };

        // 2. Document frequency + the projects each token appears in, over u32
        // ids (vector-indexed, no string hashing).
        let mut df: Vec<usize> = vec![0; n_tokens];
        let mut tok_projects: Vec<std::collections::BTreeSet<u32>> =
            vec![std::collections::BTreeSet::new(); n_tokens];
        for (pid, ids) in &docs {
            for &id in ids {
                df[id as usize] += 1;
                tok_projects[id as usize].insert(*pid);
            }
        }

        // 3a. NORMALISE — merge simple morphological variants into one concept.
        // For every token, ask `inflection_bases` for conservative stripped forms
        // (identifiers→identifier, occurrences→occurrence, facing→fac/face) and
        // redirect the token onto a base ONLY when that base is itself an attested
        // corpus token (`tok_intern` hit). Data-driven: an unattested strip
        // (class→clas, address→addres) finds no target and the surface form is
        // kept verbatim, so we never mangle a word. `merge_root[old]` follows the
        // redirect chain (with path compression) to the group representative.
        let mut merge_root: Vec<u32> = (0..n_tokens as u32).collect();
        for old in 0..n_tokens as u32 {
            // Skip near-unique noise early: a hapax variant can't pull a real
            // concept around, and resolving it wastes work.
            if df[old as usize] == 0 {
                continue;
            }
            let surface = tok_ids[old as usize].as_str();
            // Prefer the attested base with the HIGHEST df (the dominant spelling
            // of the lemma) so merges flow toward the most-used form.
            let mut best: Option<(u32, usize)> = None;
            for base in inflection_bases(surface) {
                if base == surface {
                    continue;
                }
                if let Some(&bid) = tok_intern.get(base.as_str()) {
                    let bdf = df[bid as usize];
                    if bdf > 0 && best.is_none_or(|(_, d)| bdf > d) {
                        best = Some((bid, bdf));
                    }
                }
            }
            if let Some((bid, _)) = best {
                merge_root[old as usize] = bid;
            }
        }
        // Resolve every token to its group root (path-compressed). A two-hop
        // chain (e.g. plural→singular→base) collapses to the final attested root.
        fn find_root(merge_root: &mut [u32], mut x: u32) -> u32 {
            while merge_root[x as usize] != x {
                let parent = merge_root[x as usize];
                merge_root[x as usize] = merge_root[parent as usize]; // halve path
                x = merge_root[x as usize];
            }
            x
        }
        let mut group_root: Vec<u32> = vec![0; n_tokens];
        for old in 0..n_tokens as u32 {
            group_root[old as usize] = find_root(&mut merge_root, old);
        }

        // 3b. Per-GROUP document frequency, projects, and the dominant surface
        // form. Group df is counted as DISTINCT docs containing ANY member (one
        // pass over docs with a per-group "seen this doc" stamp) so two variants
        // in the same body never double-count. The group's NAME is its
        // highest-df member surface form (ties broken lexicographically), which
        // — being a real token — still appears verbatim in bodies, so
        // `concept_memories` keeps matching it.
        let mut gdf: Vec<usize> = vec![0; n_tokens]; // indexed by group-root id
        let mut gprojects: Vec<std::collections::BTreeSet<u32>> =
            vec![std::collections::BTreeSet::new(); n_tokens];
        let mut seen_stamp: Vec<u32> = vec![u32::MAX; n_tokens];
        for (doc_no, (pid, ids)) in docs.iter().enumerate() {
            let stamp = doc_no as u32;
            for &id in ids {
                let g = group_root[id as usize] as usize;
                gprojects[g].insert(*pid);
                if seen_stamp[g] != stamp {
                    seen_stamp[g] = stamp;
                    gdf[g] += 1;
                }
            }
        }
        // Dominant surface form per group: highest member df, then lexicographic.
        // Members contribute their *own* token df to the choice of spelling.
        let mut group_name: Vec<u32> = (0..n_tokens as u32).collect(); // root -> name token id
        let mut name_df: Vec<usize> = vec![0; n_tokens]; // df of the current name choice
        for old in 0..n_tokens as u32 {
            let g = group_root[old as usize] as usize;
            let d = df[old as usize];
            if d == 0 {
                continue;
            }
            let better = d > name_df[g]
                || (d == name_df[g]
                    && tok_ids[old as usize].as_str() < tok_ids[group_name[g] as usize].as_str());
            if name_df[g] == 0 || better {
                name_df[g] = d;
                group_name[g] = old;
            }
        }

        // 3c. DYNAMIC corpus-generic ceiling — derived from the df DISTRIBUTION of
        // the candidate GROUPS, never a flat constant. A token in a very high
        // fraction of the scope (e.g. "identifiers" in a docstring-heavy store) is
        // generic FOR THIS CORPUS even though it is no stop-word. We compute the
        // ~93rd percentile of candidate group df and use it as the ceiling: the
        // handful of corpus-ubiquitous groups above it are cut, while mid-
        // frequency distinctive terms below it survive. The MIN_DF floor still
        // drops hapax/noise. On a tiny corpus the percentile can collapse onto the
        // max df, so we never let the ceiling fall below MIN_DF + 1.
        const GENERIC_PCTL: f64 = 0.93;
        let candidate_roots: Vec<u32> = (0..n_tokens as u32)
            .filter(|&r| group_root[r as usize] == r && gdf[r as usize] >= MIN_DF)
            .collect();
        // Percentile over the candidate group df values (ascending). Empty/tiny
        // sets fall back to the legacy total/3 cap so behaviour stays sane.
        let df_ceiling = if candidate_roots.is_empty() {
            (total_memories / 3).clamp(MIN_DF + 1, DF_ABS_CAP)
        } else {
            let mut dfs: Vec<usize> = candidate_roots.iter().map(|&r| gdf[r as usize]).collect();
            dfs.sort_unstable();
            let idx = ((dfs.len() as f64 - 1.0) * GENERIC_PCTL).round() as usize;
            let pctl = dfs[idx.min(dfs.len() - 1)];
            // Keep groups AT the percentile value; clamp into a sane band so a
            // tiny corpus still admits concepts and a giant one still sheds the
            // ubiquitous tail.
            pctl.max(MIN_DF + 1).min(DF_ABS_CAP.max(MIN_DF + 1))
        };

        // Rank surviving groups by df desc (then name asc, deterministic); keep
        // the top `max_concepts`. A group passes when its df is in
        // `[MIN_DF, df_ceiling]`.
        let mut kept: Vec<u32> = candidate_roots
            .iter()
            .copied()
            .filter(|&r| gdf[r as usize] <= df_ceiling)
            .collect();
        kept.sort_by(|&a, &b| {
            gdf[b as usize].cmp(&gdf[a as usize]).then_with(|| {
                tok_ids[group_name[a as usize] as usize]
                    .as_str()
                    .cmp(tok_ids[group_name[b as usize] as usize].as_str())
            })
        });
        kept.truncate(max_concepts);

        // Token old-id -> compact concept index, plus per-concept name / df /
        // freq / projects. EVERY token whose group root is a kept concept maps to
        // the SAME compact index, so a variant ("identifiers") and its canonical
        // ("identifier") drive one shared node in the co-occurrence loop below.
        // `concept_of[old_id]` is the compact index or u32::MAX otherwise.
        let mut root_to_concept: Vec<u32> = vec![u32::MAX; n_tokens];
        let mut concept_of: Vec<u32> = vec![u32::MAX; n_tokens];
        let mut names: Vec<&str> = Vec::with_capacity(kept.len());
        let mut freqs: Vec<usize> = Vec::with_capacity(kept.len());
        // Per concept: its distinct projects (resolved from interned ids), sorted
        // for determinism. For the GLOBAL brain this is the project bridge list.
        let mut concept_projects: Vec<Vec<String>> = Vec::with_capacity(kept.len());
        for &root in &kept {
            let ci = names.len() as u32;
            root_to_concept[root as usize] = ci;
            names.push(tok_ids[group_name[root as usize] as usize].as_str());
            freqs.push(gdf[root as usize]);
            let mut projects: Vec<String> = gprojects[root as usize]
                .iter()
                .map(|&pid| proj_ids[pid as usize].clone())
                .collect();
            projects.sort();
            concept_projects.push(projects);
        }
        // Fan the compact index out to every member token of each kept group.
        for old in 0..n_tokens as u32 {
            let ci = root_to_concept[group_root[old as usize] as usize];
            if ci != u32::MAX {
                concept_of[old as usize] = ci;
            }
        }

        // 4. Co-occurrence over concept tokens only. Per memory: select its
        // concept tokens, keep the rarest PER_MEM_CAP (lowest df = most
        // informative), and bump every unordered pair. Packed-u32 key + FxHashMap
        // keeps the multi-million-pair loop off SipHash, same as the clusterer.
        let pack = |x: u32, y: u32| -> u64 {
            let (lo, hi) = if x < y { (x, y) } else { (y, x) };
            ((hi as u64) << 32) | lo as u64
        };
        let mut pair_w: FxHashMap<u64, u32> = FxHashMap::default();
        let mut sel: Vec<(u32, usize)> = Vec::with_capacity(PER_MEM_CAP * 2);
        for (_, ids) in &docs {
            sel.clear();
            for &old in ids {
                let i = concept_of[old as usize];
                if i != u32::MAX {
                    sel.push((i, freqs[i as usize]));
                }
            }
            if sel.len() < 2 {
                continue;
            }
            // Dedup by concept index: after NORMALISE two variants in one body
            // (e.g. "identifier" + "identifiers") map to the SAME concept, so
            // without this they'd produce a spurious self-pair and over-weight the
            // node. Sort-by-index then dedup is deterministic and cheap (≤ a few
            // dozen tokens per body).
            sel.sort_by(|a, b| a.0.cmp(&b.0));
            sel.dedup_by_key(|p| p.0);
            if sel.len() < 2 {
                continue;
            }
            if sel.len() > PER_MEM_CAP {
                // Keep the rarest tokens; tie-break on index so the cut is stable.
                sel.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                sel.truncate(PER_MEM_CAP);
            }
            for a in 0..sel.len() {
                for b in (a + 1)..sel.len() {
                    *pair_w.entry(pack(sel[a].0, sel[b].0)).or_insert(0) += 1;
                }
            }
        }

        // Keep pairs at/above min_cooccur; sort strongest first (deterministic
        // tie-break on the two endpoint names) and cap to max_edges.
        let min_w = min_cooccur.max(1) as u32;
        let unpack = |key: u64| -> (u32, u32) { (key as u32, (key >> 32) as u32) };
        let mut edge_idx: Vec<(u32, u32, u32)> = pair_w
            .iter()
            .filter(|&(_, &w)| w >= min_w)
            .map(|(&key, &w)| {
                let (lo, hi) = unpack(key);
                (lo, hi, w)
            })
            .collect();
        edge_idx.sort_by(|x, y| {
            y.2.cmp(&x.2)
                .then_with(|| names[x.0 as usize].cmp(names[y.0 as usize]))
                .then_with(|| names[x.1 as usize].cmp(names[y.1 as usize]))
        });
        edge_idx.truncate(max_edges);

        // 5. degree = distinct neighbours in the kept edge set.
        let mut degree: Vec<usize> = vec![0; names.len()];
        {
            let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
            for &(lo, hi, _) in &edge_idx {
                if seen.insert(pack(lo, hi)) {
                    degree[lo as usize] += 1;
                    degree[hi as usize] += 1;
                }
            }
        }

        // Materialise edges with `a < b` lexicographically (names are already the
        // canonical lowercase token; the packed key gives lo<hi by *index*, so
        // re-order by name here).
        let mut edges: Vec<(String, String, f32)> = edge_idx
            .iter()
            .map(|&(lo, hi, w)| {
                let (na, nb) = (names[lo as usize], names[hi as usize]);
                let (a, b) = if na <= nb { (na, nb) } else { (nb, na) };
                (a.to_string(), b.to_string(), w as f32)
            })
            .collect();
        // Re-sort: the index sort approximated the name order; the swap above can
        // perturb the name tie-breaks, so finalise on the real (weight, a, b).
        edges.sort_by(|x, y| {
            y.2.partial_cmp(&x.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| x.0.cmp(&y.0))
                .then_with(|| x.1.cmp(&y.1))
        });

        // Build concept nodes; sort by degree desc, freq desc, name asc.
        let mut nodes: Vec<ConceptNode> = names
            .iter()
            .enumerate()
            .map(|(i, &name)| ConceptNode {
                name: name.to_string(),
                freq: freqs[i],
                degree: degree[i],
                projects: concept_projects[i].clone(),
            })
            .collect();
        nodes.sort_by(|a, b| {
            b.degree
                .cmp(&a.degree)
                .then_with(|| b.freq.cmp(&a.freq))
                .then_with(|| a.name.cmp(&b.name))
        });

        Ok(ConceptGraph {
            nodes,
            edges,
            total_memories,
        })
    }

    /// The memories containing a given concept token, newest first, capped at
    /// `limit`. Returned as `(MemNode, project)` so the GLOBAL brain can show
    /// which project each memory belongs to. LLM-free: a plain body scan via the
    /// same [`token_set`] used to build the brain (the token must survive the
    /// word-like filter to match a concept node).
    pub fn concept_memories(
        &self,
        scope: Option<&str>,
        concept: &str,
        limit: usize,
    ) -> Result<Vec<(MemNode, String)>> {
        let needle = concept.to_lowercase();
        let sql = "SELECT id, kind, body, json_extract(metadata, '$.source_kind'), project \
                     FROM memories \
                    WHERE (?1 IS NULL OR project = ?1) \
                    ORDER BY created_at DESC, id DESC";
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| Error::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![scope], |r| {
                let body: String = r.get(2)?;
                let node = MemNode {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    preview: body.chars().take(60).collect(),
                    source_kind: r.get(3)?,
                };
                let project: String = r.get(4)?;
                Ok((node, project, body))
            })
            .map_err(|e| Error::Memory(e.to_string()))?;

        let mut out: Vec<(MemNode, String)> = Vec::new();
        for row in rows {
            let (node, project, body) = row.map_err(|e| Error::Memory(e.to_string()))?;
            if token_set(&body).contains(&needle) {
                out.push((node, project));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Build the TOP LEVEL of the "digital brain" — a few dozen TOPIC
    /// COMMUNITIES — **with no LLM**. The brain overview otherwise shows hundreds
    /// of concepts at once (global ~500); this folds them into super-nodes the
    /// first view can actually read.
    ///
    /// `scope`: `Some(project)` for one project, `None` for the GLOBAL brain
    /// (concepts merged by name across projects).
    ///
    /// Pipeline (entirely co-occurrence driven):
    /// 1. Build [`concept_graph`](Self::concept_graph)`(scope, 0, 0, min_cooccur)`
    ///    — AUTO sizing, the same brain the overview renders.
    /// 2. Treat each concept as a node and its co-occurrence edges as peers
    ///    (`peers[i]` = concept `i`'s neighbours with the edge weight; `best_peer`
    ///    = the strongest neighbour), then cluster with the SHARED
    ///    [`cluster_from_peers`](Self::cluster_from_peers) — the exact union-find +
    ///    singleton-fold core the memory map uses. The concept *index* is passed
    ///    as the `id`, so every community's deterministic root is the minimum
    ///    member concept index (its stable id).
    /// 3. The fold target is DYNAMIC: `√concepts` clamped to `[8, 60]` — never a
    ///    flat hard-coded count.
    /// 4. For each community: `label` = highest-degree member concept, `size` =
    ///    Σ member `freq` (memories touched), `top_concepts` = top ~5 members by
    ///    degree. Inter-community edges aggregate the concept edges that cross
    ///    two communities.
    ///
    /// Determinism: communities sorted by `size` desc then `id` asc; ids stable
    /// (min member concept index); edges `a < b`, strongest first.
    pub fn concept_communities(
        &self,
        scope: Option<&str>,
        min_cooccur: usize,
    ) -> Result<ConceptHierarchy> {
        let graph = self.concept_graph(scope, 0, 0, min_cooccur)?;
        Ok(communities_from_graph(&graph))
    }

    /// The concept sub-graph for ONE topic community: its member concepts plus
    /// the co-occurrence edges *among them* (intra-community edges only). LLM-free.
    ///
    /// The concept graph is small, so this just recomputes
    /// [`concept_communities`](Self::concept_communities) and slices out the
    /// requested community, identified by its stable `community_id` (the min
    /// member concept index that [`concept_communities`](Self::concept_communities)
    /// reports as the community `id`). An unknown id yields an empty
    /// [`ConceptGraph`] (no error) so the caller can render "nothing here".
    pub fn community_concepts(
        &self,
        scope: Option<&str>,
        community_id: i64,
        min_cooccur: usize,
    ) -> Result<ConceptGraph> {
        let graph = self.concept_graph(scope, 0, 0, min_cooccur)?;
        Ok(community_subgraph(&graph, community_id))
    }
}

/// Compute the community assignment of every concept in `graph`, returning, for
/// each concept index, the deterministic community root id (min member concept
/// index) it belongs to — plus the `ConceptHierarchy` summary. Shared by
/// [`MemoryStore::concept_communities`] and [`community_subgraph`] so the two
/// public methods agree on ids without re-clustering twice. Pure / LLM-free.
fn cluster_concepts(graph: &ConceptGraph) -> (Vec<i64>, ConceptHierarchy) {
    let n = graph.nodes.len();

    // Concept name -> its index in `graph.nodes`. Names are unique per graph.
    let mut index_of: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(n);
    for (i, node) in graph.nodes.iter().enumerate() {
        index_of.insert(node.name.as_str(), i);
    }

    // Build per-concept peer lists from the co-occurrence edges. `peers[i]` =
    // concept i's neighbours, weighted by ASSOCIATION STRENGTH rather than raw
    // co-occurrence. These are exactly what `cluster_from_peers` consumes —
    // concept indices stand in for memory ids.
    //
    // Why not the raw weight: `cluster_from_peers` single-link-unions each
    // node's top_k strongest peers, so a hub concept (one that co-occurs with
    // everything at a high RAW count) would chain the whole brain into one giant
    // community. Normalising the edge by the geometric mean of the two concepts'
    // frequencies (a cosine-style association, `w / √(freq_a·freq_b)`) damps the
    // hub: a pair that almost always appears together scores ~1, while a hub that
    // co-occurs with a rare token only because the hub is everywhere scores low.
    // This keeps topic communities balanced while still being a pure, LLM-free
    // function of co-occurrence. The PUBLIC `ConceptGraph` edges and the
    // inter-community aggregation below keep the RAW weight.
    let assoc = |ia: usize, ib: usize, w: f32| -> f32 {
        let denom = ((graph.nodes[ia].freq as f64) * (graph.nodes[ib].freq as f64)).sqrt();
        if denom > 0.0 {
            (w as f64 / denom) as f32
        } else {
            0.0
        }
    };
    let mut peers: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
    for (a, b, w) in &graph.edges {
        let (Some(&ia), Some(&ib)) = (index_of.get(a.as_str()), index_of.get(b.as_str())) else {
            continue;
        };
        let s = assoc(ia, ib, *w);
        peers[ia].push((ib as u32, s));
        peers[ib].push((ia as u32, s));
    }
    let best_peer: Vec<Option<(u32, f32)>> = peers
        .iter()
        .map(|plist| {
            plist.iter().copied().max_by(|x, y| {
                x.1.partial_cmp(&y.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        // Prefer the lower neighbour index on a tie — deterministic.
                        .then(y.0.cmp(&x.0))
            })
        })
        .collect();

    // Positional arrays for cluster_from_peers: the concept INDEX is the id, so
    // each community's root (min member id) is the min member concept index — a
    // stable, deterministic community id. Previews carry the concept name only
    // for completeness; we derive the community label from degree below.
    let ids: Vec<i64> = (0..n as i64).collect();
    let previews: Vec<String> = graph.nodes.iter().map(|node| node.name.clone()).collect();
    let sources: Vec<Option<String>> = vec![None; n];

    // DYNAMIC community target: ~√concepts, clamped to a readable band. Never a
    // flat hard-coded count — a small brain shows a handful of communities, a
    // large one a few dozen.
    let target = (n as f64).sqrt().round().clamp(8.0, 60.0) as usize;
    // top_k peers per concept feeding the union-find. `cluster_from_peers`
    // single-link-unions each concept's top_k strongest peers; on a CONNECTED
    // co-occurrence graph a large top_k chains the whole brain into one bubble
    // (transitive closure of single linkage). Keeping top_k SMALL means each
    // concept only fuses with its single strongest topical neighbour, so the
    // graph fragments into many natural topic communities before the fold ever
    // runs. 1 = "join only your strongest link" — the maximum-spanning-forest of
    // mutual best edges, which is exactly the topic-community signal we want.
    const CONCEPT_TOP_K: usize = 1;

    let index = MemoryStore::cluster_from_peers(
        ids,
        previews,
        sources,
        peers,
        best_peer,
        CONCEPT_TOP_K,
        target,
    );

    // `node_cluster[concept_index] -> community root id (min member index)`.
    let mut community_of: Vec<i64> = vec![-1; n];
    for (concept_index, root) in &index.node_cluster {
        community_of[*concept_index as usize] = *root;
    }

    // Build the community summaries. Group member concept indices by root.
    let mut members_by_root: std::collections::HashMap<i64, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, &root) in community_of.iter().enumerate() {
        members_by_root.entry(root).or_default().push(i);
    }

    let mut communities: Vec<ConceptCommunity> = members_by_root
        .into_iter()
        .map(|(root, mut members)| {
            // Order members by degree desc, then freq desc, then name asc — the
            // label and top_concepts both read off the front of this order.
            members.sort_by(|&a, &b| {
                graph.nodes[b]
                    .degree
                    .cmp(&graph.nodes[a].degree)
                    .then_with(|| graph.nodes[b].freq.cmp(&graph.nodes[a].freq))
                    .then_with(|| graph.nodes[a].name.cmp(&graph.nodes[b].name))
            });
            let label = graph.nodes[members[0]].name.clone();
            let size: usize = members.iter().map(|&m| graph.nodes[m].freq).sum();
            let top_concepts: Vec<String> = members
                .iter()
                .take(5)
                .map(|&m| graph.nodes[m].name.clone())
                .collect();
            ConceptCommunity {
                id: root,
                label,
                size,
                concept_count: members.len(),
                top_concepts,
            }
        })
        .collect();
    communities.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));

    // Aggregate inter-community edges: every concept edge whose endpoints fall
    // in different communities contributes its weight to that community pair.
    let pack = |x: i64, y: i64| -> (i64, i64) { if x < y { (x, y) } else { (y, x) } };
    let mut agg: std::collections::HashMap<(i64, i64), f32> = std::collections::HashMap::new();
    for (a, b, w) in &graph.edges {
        let (Some(&ia), Some(&ib)) = (index_of.get(a.as_str()), index_of.get(b.as_str())) else {
            continue;
        };
        let (ca, cb) = (community_of[ia], community_of[ib]);
        if ca == cb {
            continue;
        }
        *agg.entry(pack(ca, cb)).or_insert(0.0) += *w;
    }
    let mut edges: Vec<(i64, i64, f32)> = agg.into_iter().map(|((a, b), w)| (a, b, w)).collect();
    edges.sort_by(|x, y| {
        y.2.partial_cmp(&x.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(x.0.cmp(&y.0))
            .then(x.1.cmp(&y.1))
    });

    let hierarchy = ConceptHierarchy {
        communities,
        edges,
        total_concepts: n,
        total_memories: graph.total_memories,
    };
    (community_of, hierarchy)
}

/// Topic communities of a prebuilt brain — the pure core behind
/// [`MemoryStore::concept_communities`].
fn communities_from_graph(graph: &ConceptGraph) -> ConceptHierarchy {
    cluster_concepts(graph).1
}

/// The concept sub-graph for ONE community of a prebuilt brain — the pure core
/// behind [`MemoryStore::community_concepts`]. Returns the member concepts and
/// the co-occurrence edges among them (intra-community only). An unknown
/// `community_id` yields an empty graph (preserving `total_memories`).
fn community_subgraph(graph: &ConceptGraph, community_id: i64) -> ConceptGraph {
    let (community_of, _) = cluster_concepts(graph);

    // The member concept indices of the requested community.
    let mut keep: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (i, &root) in community_of.iter().enumerate() {
        if root == community_id {
            keep.insert(i);
        }
    }

    // Names of the kept concepts, for the edge filter (intra-community only).
    let kept_names: std::collections::HashSet<&str> =
        keep.iter().map(|&i| graph.nodes[i].name.as_str()).collect();

    let mut nodes: Vec<ConceptNode> = keep.iter().map(|&i| graph.nodes[i].clone()).collect();
    nodes.sort_by(|a, b| {
        b.degree
            .cmp(&a.degree)
            .then_with(|| b.freq.cmp(&a.freq))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut edges: Vec<(String, String, f32)> = graph
        .edges
        .iter()
        .filter(|(a, b, _)| kept_names.contains(a.as_str()) && kept_names.contains(b.as_str()))
        .cloned()
        .collect();
    edges.sort_by(|x, y| {
        y.2.partial_cmp(&x.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.0.cmp(&y.0))
            .then_with(|| x.1.cmp(&y.1))
    });

    ConceptGraph {
        nodes,
        edges,
        total_memories: graph.total_memories,
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
    fn concept_graph_links_cooccurring_tokens() {
        let store = MemoryStore::open_in_memory().unwrap();
        // "alpha" + "bravo" co-occur in 4 memories → must form an edge.
        for _ in 0..4 {
            store.save("p1", "note", "alpha bravo charlie").unwrap();
        }
        let g = store.concept_graph(Some("p1"), 250, 750, 2).unwrap();
        assert_eq!(g.total_memories, 4);
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"bravo"));
        // The alpha—bravo edge exists, a < b, weight = co-occurrence count.
        let edge = g
            .edges
            .iter()
            .find(|(a, b, _)| a == "alpha" && b == "bravo")
            .expect("alpha—bravo edge");
        assert_eq!(edge.2, 4.0);
        // freq = document frequency; degree counts distinct neighbours.
        let alpha = g.nodes.iter().find(|n| n.name == "alpha").unwrap();
        assert_eq!(alpha.freq, 4);
        assert!(alpha.degree >= 1);
    }

    #[test]
    fn concept_graph_excludes_hapax_and_function_words() {
        let store = MemoryStore::open_in_memory().unwrap();
        // "core"   → df 6  → real concept, kept.
        // "lonely" → df 1  → hapax (< MIN_DF = 3), dropped.
        // "have" / "with" → curated function words, dropped no matter how often.
        for _ in 0..6 {
            store.save("p1", "note", "have core with widget").unwrap();
        }
        for _ in 0..14 {
            store.save("p1", "note", "have gadget with thing").unwrap();
        }
        for _ in 0..9 {
            store.save("p1", "note", "filler topic distinct").unwrap();
        }
        store.save("p1", "note", "lonely outlier zzznote").unwrap();
        let g = store.concept_graph(Some("p1"), 250, 750, 2).unwrap();
        assert_eq!(g.total_memories, 30);
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(!names.contains(&"lonely"), "hapax must be dropped");
        assert!(
            !names.contains(&"have"),
            "curated function word must be dropped"
        );
        assert!(
            !names.contains(&"with"),
            "curated function word must be dropped"
        );
        assert!(names.contains(&"core"), "real concept must survive");
    }

    /// STOP_WORDS must stay sorted (the `binary_search` membership test relies on
    /// it) and carry no duplicates (a duplicate is a sign two families collided).
    #[test]
    fn stop_words_are_sorted_and_unique() {
        for w in STOP_WORDS.windows(2) {
            assert!(
                w[0] < w[1],
                "STOP_WORDS must be strictly sorted/unique: {:?} !< {:?}",
                w[0],
                w[1]
            );
        }
        // Spot-check membership both ways.
        assert!(is_stop_word("have"));
        assert!(is_stop_word("completed"));
        assert!(!is_stop_word("tessellate"));
        assert!(!is_stop_word("jacobian"));
    }

    /// A curated stop word never becomes a concept even when it is the single
    /// most frequent token in the scope.
    #[test]
    fn stop_word_is_excluded_even_when_ubiquitous() {
        let store = MemoryStore::open_in_memory().unwrap();
        // "could" is in every body (df = 20) but is a function word; the
        // distinctive "podman"/"jacobian"/"tessellate" terms are the real
        // concepts. Vary the distinctive token so each clears MIN_DF.
        for _ in 0..7 {
            store.save("p1", "note", "could podman registry").unwrap();
        }
        for _ in 0..7 {
            store.save("p1", "note", "could jacobian matrix").unwrap();
        }
        for _ in 0..6 {
            store.save("p1", "note", "could tessellate mesh").unwrap();
        }
        let g = store.concept_graph(Some("p1"), 250, 750, 1).unwrap();
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            !names.contains(&"could"),
            "stop word must never be a concept"
        );
        assert!(names.contains(&"podman"), "distinctive term must survive");
        assert!(names.contains(&"jacobian"), "distinctive term must survive");
        assert!(
            names.contains(&"tessellate"),
            "distinctive term must survive"
        );
    }

    /// Singular/plural (and -ing/-ed) variants whose base is attested in the
    /// corpus merge into ONE concept: a single node, df aggregated over the
    /// group, named by the dominant surface form.
    #[test]
    fn morphological_variants_merge_into_one_concept() {
        let store = MemoryStore::open_in_memory().unwrap();
        // "identifier" (singular) is the dominant spelling: 8 bodies.
        for _ in 0..8 {
            store.save("p1", "note", "identifier parser lexer").unwrap();
        }
        // "identifiers" (plural) in 4 more bodies; base "identifier" is attested
        // so the plural must redirect onto it -> ONE merged concept.
        for _ in 0..4 {
            store
                .save("p1", "note", "identifiers parser tokens")
                .unwrap();
        }
        // Filler so MIN_DF and the percentile ceiling stay sane.
        for i in 0..6 {
            store
                .save("p1", "note", &format!("filler topic numq{i}"))
                .unwrap();
        }
        let g = store.concept_graph(Some("p1"), 250, 750, 2).unwrap();
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        // Exactly one of the two surface forms appears, and it's the dominant
        // (singular) spelling.
        assert!(
            names.contains(&"identifier"),
            "merged concept named by dominant surface form"
        );
        assert!(
            !names.contains(&"identifiers"),
            "plural variant must have merged away, not appear as its own node"
        );
        // df aggregates the whole group: 8 + 4 = 12 distinct bodies.
        let node = g.nodes.iter().find(|n| n.name == "identifier").unwrap();
        assert_eq!(node.freq, 12, "df aggregates over the merged group");
        // The merge does not strip a word whose base is unattested: "parser" has
        // no attested "pars" base, so it survives whole.
        assert!(
            names.contains(&"parser"),
            "unattested strip must NOT happen"
        );
    }

    /// A token in a very high FRACTION of the corpus is generic FOR THIS CORPUS
    /// and is cut by the dynamic percentile ceiling, while a genuinely
    /// distinctive mid-frequency token survives — no flat constant involved.
    #[test]
    fn dynamic_ceiling_drops_corpus_ubiquitous_token_keeps_distinctive() {
        let store = MemoryStore::open_in_memory().unwrap();
        // "context" is sprinkled into nearly EVERY body (corpus-ubiquitous, like a
        // docstring boilerplate word that is not a stop word). A broad spread of
        // distinctive mid-frequency terms fills the bulk of the df distribution so
        // the 93rd percentile sits at the mid-frequency df and the ubiquitous tail
        // is the part that gets cut. The third token "thingy{i}" is unique per
        // body (hapax, dropped) so only "context" + the distinctive term matter.
        let distinctive = [
            "tessellate",
            "jacobian",
            "podman",
            "auth",
            "kernel",
            "vector",
            "lexer",
            "registry",
            "matrix",
            "scheduler",
            "allocator",
            "raster",
            "shader",
            "mutex",
            "rgba",
            "quaternion",
            "bezier",
            "frustum",
            "viewport",
            "sampler",
            "atlas",
            "glyph",
            "kerning",
            "codepoint",
            "opcode",
            "syscall",
            "epoll",
            "futex",
            "cgroup",
            "namespace",
        ];
        let mut tag = 0u32;
        for term in distinctive {
            // Each distinctive term: df 4 (mid-frequency), always with "context".
            for _ in 0..4 {
                tag += 1;
                store
                    .save("p1", "note", &format!("context {term} thingy{tag}"))
                    .unwrap();
            }
        }
        let g = store.concept_graph(Some("p1"), 250, 750, 1).unwrap();
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        // "context" sits in ~48 bodies — far above the percentile of the df
        // distribution (most candidate groups have df 4) — so it is suppressed.
        assert!(
            !names.contains(&"context"),
            "corpus-ubiquitous token must be cut by the dynamic ceiling"
        );
        // The distinctive mid-frequency terms survive.
        assert!(
            names.contains(&"tessellate"),
            "distinctive term must survive"
        );
        assert!(names.contains(&"jacobian"), "distinctive term must survive");
        assert!(names.contains(&"podman"), "distinctive term must survive");
    }

    #[test]
    fn concept_graph_global_merges_token_across_projects() {
        let store = MemoryStore::open_in_memory().unwrap();
        // "shared" appears in both projects; global scope must merge it and list
        // both projects on the one node (the bridge between projects). Filler
        // memories keep df_cap = total/3 = 6 above "shared"'s df of 6.
        for _ in 0..3 {
            store.save("proj_a", "note", "shared widget alpha").unwrap();
        }
        for _ in 0..3 {
            store.save("proj_b", "note", "shared gadget bravo").unwrap();
        }
        for i in 0..6 {
            store
                .save("proj_a", "note", &format!("filler topic numa{i}"))
                .unwrap();
        }
        for i in 0..6 {
            store
                .save("proj_b", "note", &format!("filler topic numb{i}"))
                .unwrap();
        }
        let g = store.concept_graph(None, 250, 750, 2).unwrap();
        assert_eq!(g.total_memories, 18);
        let shared = g
            .nodes
            .iter()
            .find(|n| n.name == "shared")
            .expect("shared concept");
        assert_eq!(shared.freq, 6);
        assert!(shared.projects.contains(&"proj_a".to_string()));
        assert!(shared.projects.contains(&"proj_b".to_string()));
        // A per-project brain sees only its own project on the node.
        let ga = store.concept_graph(Some("proj_a"), 250, 750, 2).unwrap();
        let shared_a = ga.nodes.iter().find(|n| n.name == "shared").unwrap();
        assert_eq!(shared_a.projects, vec!["proj_a".to_string()]);
    }

    #[test]
    fn concept_memories_returns_matching_rows() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("p1", "note", "alpha bravo charlie").unwrap();
        store.save("p1", "note", "alpha delta echo").unwrap();
        store.save("p1", "note", "foxtrot golf hotel").unwrap();
        let hits = store.concept_memories(Some("p1"), "alpha", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|(n, _)| n.preview.contains("alpha")));
        assert!(hits.iter().all(|(_, project)| project == "p1"));
        // Newest first.
        assert!(hits[0].0.id > hits[1].0.id);
        // Cap is honoured.
        let one = store.concept_memories(Some("p1"), "alpha", 1).unwrap();
        assert_eq!(one.len(), 1);
        // Global scope finds it too.
        let global = store.concept_memories(None, "foxtrot", 10).unwrap();
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].1, "p1");
    }

    /// Build a corpus with two TIGHT token cliques that never co-occur with each
    /// other (so they must land in different communities), plus an isolated
    /// clique that shares no token with either. df is engineered so every concept
    /// clears MIN_DF=3 and stays under df_cap = total/3.
    fn seed_community_corpus(store: &MemoryStore, project: &str) {
        // Clique A: alpha+bravo+charlie co-occur 6×.
        for _ in 0..6 {
            store.save(project, "note", "alpha bravo charlie").unwrap();
        }
        // Clique B: delta+echo+foxtrot co-occur 6× (no overlap with A).
        for _ in 0..6 {
            store.save(project, "note", "delta echo foxtrot").unwrap();
        }
        // Isolated clique C: hotel+india co-occur 4× (shares nothing with A/B).
        for _ in 0..4 {
            store.save(project, "note", "hotel india").unwrap();
        }
        // Filler so df_cap = total/3 = (16+filler)/3 stays above every concept's
        // df (max 6) — keeps them all out of the stop-word band.
        for i in 0..8 {
            store
                .save(project, "note", &format!("filler topic numz{i}"))
                .unwrap();
        }
    }

    #[test]
    fn cooccurring_concepts_share_a_community() {
        let store = MemoryStore::open_in_memory().unwrap();
        seed_community_corpus(&store, "p1");
        let h = store.concept_communities(Some("p1"), 2).unwrap();
        // total_concepts matches the brain's node count.
        let g = store.concept_graph(Some("p1"), 0, 0, 2).unwrap();
        assert_eq!(h.total_concepts, g.nodes.len());
        assert_eq!(h.total_memories, g.total_memories);
        assert!(!h.communities.is_empty());

        // Locate the community each concept belongs to via its sub-graph.
        let find = |concept: &str| -> i64 {
            for c in &h.communities {
                let sub = store.community_concepts(Some("p1"), c.id, 2).unwrap();
                if sub.nodes.iter().any(|n| n.name == concept) {
                    return c.id;
                }
            }
            panic!("concept {concept} not placed in any community");
        };

        // Clique A members co-occur heavily -> same community.
        assert_eq!(find("alpha"), find("bravo"), "alpha+bravo co-occur");
        assert_eq!(find("alpha"), find("charlie"), "alpha+charlie co-occur");
        // Clique B is a DIFFERENT community from clique A (they never co-occur).
        assert_ne!(find("alpha"), find("delta"), "A and B must split");
        assert_eq!(find("delta"), find("echo"), "delta+echo co-occur");

        // Communities are sorted by size (memories touched) desc, ties by id asc.
        for w in h.communities.windows(2) {
            assert!(
                w[0].size > w[1].size || (w[0].size == w[1].size && w[0].id < w[1].id),
                "communities must be size-desc / id-asc: {:?}",
                h.communities
            );
        }
        // Every community has a non-empty label and at least one concept.
        for c in &h.communities {
            assert!(!c.label.is_empty());
            assert!(c.concept_count >= 1);
            assert!(c.top_concepts.len() <= 5);
            assert!(c.top_concepts.contains(&c.label), "label is a member");
        }
    }

    #[test]
    fn isolated_concept_is_its_own_or_misc_community() {
        let store = MemoryStore::open_in_memory().unwrap();
        seed_community_corpus(&store, "p1");
        let h = store.concept_communities(Some("p1"), 2).unwrap();

        // The isolated clique C (hotel/india) shares no token with A or B, so it
        // cannot be unioned into either — it forms its own community (or, if the
        // singleton-fold ran, a misc bubble), distinct from A and B.
        let find = |concept: &str| -> i64 {
            for c in &h.communities {
                let sub = store.community_concepts(Some("p1"), c.id, 2).unwrap();
                if sub.nodes.iter().any(|n| n.name == concept) {
                    return c.id;
                }
            }
            panic!("concept {concept} not placed");
        };
        assert_ne!(find("hotel"), find("alpha"), "C must not join A");
        assert_ne!(find("hotel"), find("delta"), "C must not join B");
        // hotel and india co-occur -> they stay together.
        assert_eq!(find("hotel"), find("india"));
    }

    #[test]
    fn community_concepts_returns_only_its_members_and_intra_edges() {
        let store = MemoryStore::open_in_memory().unwrap();
        seed_community_corpus(&store, "p1");
        let h = store.concept_communities(Some("p1"), 2).unwrap();

        // The community holding "alpha".
        let mut alpha_comm: Option<i64> = None;
        for c in &h.communities {
            let sub = store.community_concepts(Some("p1"), c.id, 2).unwrap();
            if sub.nodes.iter().any(|n| n.name == "alpha") {
                alpha_comm = Some(c.id);
                break;
            }
        }
        let cid = alpha_comm.expect("alpha community");
        let sub = store.community_concepts(Some("p1"), cid, 2).unwrap();

        let names: std::collections::HashSet<&str> =
            sub.nodes.iter().map(|n| n.name.as_str()).collect();
        // Clique A members are present; clique B / C members are NOT.
        assert!(names.contains("alpha"));
        assert!(names.contains("bravo"));
        assert!(!names.contains("delta"), "cross-community concept leaked");
        assert!(!names.contains("hotel"), "cross-community concept leaked");
        // Every returned edge is INTRA-community (both endpoints are members).
        for (a, b, _) in &sub.edges {
            assert!(
                names.contains(a.as_str()),
                "edge endpoint outside community"
            );
            assert!(
                names.contains(b.as_str()),
                "edge endpoint outside community"
            );
        }
        // total_memories is carried through unchanged.
        assert_eq!(sub.total_memories, h.total_memories);

        // Unknown community id -> empty graph (no error), memories preserved.
        let empty = store.community_concepts(Some("p1"), i64::MAX, 2).unwrap();
        assert!(empty.nodes.is_empty());
        assert!(empty.edges.is_empty());
        assert_eq!(empty.total_memories, h.total_memories);
    }

    #[test]
    fn concept_communities_global_scope() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Same two cliques but split across two projects; global scope (None)
        // must merge by token and still separate A from B.
        for _ in 0..6 {
            store.save("proj_a", "note", "alpha bravo charlie").unwrap();
        }
        for _ in 0..6 {
            store.save("proj_b", "note", "delta echo foxtrot").unwrap();
        }
        for i in 0..8 {
            store
                .save("proj_a", "note", &format!("filler topic numa{i}"))
                .unwrap();
        }
        for i in 0..8 {
            store
                .save("proj_b", "note", &format!("filler topic numb{i}"))
                .unwrap();
        }
        let h = store.concept_communities(None, 2).unwrap();
        assert!(h.total_memories >= 28);
        assert!(!h.communities.is_empty());

        let find = |concept: &str| -> i64 {
            for c in &h.communities {
                let sub = store.community_concepts(None, c.id, 2).unwrap();
                if sub.nodes.iter().any(|n| n.name == concept) {
                    return c.id;
                }
            }
            panic!("concept {concept} not placed");
        };
        assert_eq!(find("alpha"), find("bravo"));
        assert_ne!(find("alpha"), find("delta"), "A and B split globally too");
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

    /// Performance probe for the topic-community top level against the real
    /// on-disk store. Ignored by default; run with `--ignored` after setting
    /// `RTRT_MEMORY_PATH`. The concept graph is small, so `concept_communities`
    /// must clear well under a second for both a project and the global scope.
    #[test]
    #[ignore = "needs the real on-disk memory store"]
    fn concept_communities_perf() {
        let path = std::env::var("RTRT_MEMORY_PATH").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap();
            format!("{home}/.rtrt/memory.sqlite")
        });
        let store = MemoryStore::open(&path).unwrap();

        for scope in [Some("00G_winpodx"), None] {
            // Warm-up (page cache + concept_graph build); not measured.
            let _ = store.concept_communities(scope, 2).unwrap();
            let t0 = std::time::Instant::now();
            let h = store.concept_communities(scope, 2).unwrap();
            let elapsed = t0.elapsed();
            let label = scope.unwrap_or("<global>");
            eprintln!(
                "concept_communities({label}): {} concepts -> {} communities, {} inter-edges, {} memories in {:?}",
                h.total_concepts,
                h.communities.len(),
                h.edges.len(),
                h.total_memories,
                elapsed
            );
            for c in h.communities.iter().take(8) {
                eprintln!(
                    "  community id={} label={:?} size={} concepts={} top={:?}",
                    c.id, c.label, c.size, c.concept_count, c.top_concepts
                );
            }
            // Drill into the biggest community.
            if let Some(top) = h.communities.first() {
                let dt0 = std::time::Instant::now();
                let sub = store.community_concepts(scope, top.id, 2).unwrap();
                eprintln!(
                    "  community_concepts(id={}): {} concepts, {} intra-edges in {:?}",
                    top.id,
                    sub.nodes.len(),
                    sub.edges.len(),
                    dt0.elapsed()
                );
            }
            assert!(
                elapsed.as_secs_f64() < 1.0,
                "concept_communities({label}) too slow: {elapsed:?}"
            );
            // Dynamic target band: ~√concepts clamped [8, 60].
            assert!(
                h.communities.len() <= 60,
                "community count must stay within the dynamic band, got {}",
                h.communities.len()
            );
        }
    }

    /// Empirical tuning probe for `VEC_MIN_SIM` against the real on-disk store.
    /// Ignored by default; run with `--ignored` after setting `RTRT_MEMORY_PATH`
    /// (defaults to `~/.rtrt/memory.sqlite`). Reports cluster count, biggest
    /// cluster size + %, and elapsed for `00G_SpeechPad`.
    #[cfg(feature = "hnsw")]
    #[test]
    #[ignore = "needs the real on-disk memory store"]
    fn graph_clusters_vec_speechpad_tune() {
        let path = std::env::var("RTRT_MEMORY_PATH").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap();
            format!("{home}/.rtrt/memory.sqlite")
        });
        let store = MemoryStore::open(&path).unwrap();
        let (embedded, total) = store.embedding_coverage("00G_SpeechPad").unwrap();
        // Warm-up (page cache); not measured.
        let _ = store
            .graph_clusters_vec("00G_SpeechPad", 200_000, 4, 320)
            .unwrap();
        let t0 = std::time::Instant::now();
        let idx = store
            .graph_clusters_vec("00G_SpeechPad", 200_000, 4, 320)
            .unwrap();
        let elapsed = t0.elapsed();
        let nodes = idx.node_cluster.len();
        let biggest = idx.clusters.first().map(|c| c.size).unwrap_or(0);
        let singletons = idx.clusters.iter().filter(|c| c.size == 1).count();
        let pct = if nodes > 0 {
            biggest as f64 * 100.0 / nodes as f64
        } else {
            0.0
        };
        eprintln!(
            "SpeechPad: coverage {embedded}/{total}, {} nodes -> {} clusters ({} singletons), biggest {} ({:.1}%), {:?}",
            nodes,
            idx.clusters.len(),
            singletons,
            biggest,
            pct,
            elapsed
        );
        // The vector path must break the blob: biggest bubble well under 30%.
        assert!(pct < 30.0, "biggest bubble {pct:.1}% too large");
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "vec clustering too slow: {elapsed:?}"
        );
    }

    #[test]
    fn unembedded_batch_returns_only_unembedded_and_honours_limit() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store.save("p", "note", "alpha").unwrap();
        let b = store.save("p", "note", "beta").unwrap();
        let c = store.save("p", "note", "gamma").unwrap();

        // All three start unembedded.
        let batch = store.unembedded_batch(100).unwrap();
        assert_eq!(batch.len(), 3);
        // Newest first: gamma, beta, alpha.
        assert_eq!(
            batch.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            vec![c, b, a]
        );
        // (id, project, body) shape is preserved.
        assert_eq!(batch[0], (c, "p".to_string(), "gamma".to_string()));

        // Limit caps the batch size, still newest first.
        let two = store.unembedded_batch(2).unwrap();
        assert_eq!(
            two.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            vec![c, b]
        );

        // Embed `b`; it must drop out of the unembedded set.
        store
            .store_embedding(b, "test-model", &[0.1, 0.2, 0.3])
            .unwrap();
        let after = store.unembedded_batch(100).unwrap();
        assert_eq!(
            after.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            vec![c, a],
            "embedded row must be excluded"
        );
    }

    #[test]
    fn store_embedding_round_trips_with_coverage() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store.save("p", "note", "alpha").unwrap();
        let _b = store.save("p", "note", "beta").unwrap();

        // Start: nothing embedded.
        assert_eq!(store.embedding_coverage("p").unwrap(), (0, 2));

        // Embed one row -> coverage reflects it, and unembedded_batch drops it.
        store.store_embedding(a, "test-model", &[1.0, 0.0]).unwrap();
        assert_eq!(store.embedding_coverage("p").unwrap(), (1, 2));
        let remaining = store.unembedded_batch(100).unwrap();
        assert!(remaining.iter().all(|(id, _, _)| *id != a));

        // INSERT OR IGNORE: re-storing the same id is a harmless no-op.
        store.store_embedding(a, "test-model", &[9.0, 9.0]).unwrap();
        assert_eq!(store.embedding_coverage("p").unwrap(), (1, 2));
    }

    /// Hand-insert an embedding row for `id` so vector clustering has dense
    /// signal without running a real embedder.
    #[cfg(feature = "hnsw")]
    fn put_embedding(store: &MemoryStore, id: i64, vec: &[f32]) {
        store
            .conn
            .execute(
                "INSERT OR REPLACE INTO embeddings(memory_id, model, vector) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, "test-vec", vector_to_blob(vec)],
            )
            .unwrap();
    }

    #[cfg(feature = "hnsw")]
    #[test]
    fn embedding_coverage_counts() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store.save("p", "note", "alpha").unwrap();
        let b = store.save("p", "note", "beta").unwrap();
        let _c = store.save("p", "note", "gamma").unwrap();
        // 2 of 3 rows embedded.
        put_embedding(&store, a, &[1.0, 0.0, 0.0]);
        put_embedding(&store, b, &[0.0, 1.0, 0.0]);
        let (embedded, total) = store.embedding_coverage("p").unwrap();
        assert_eq!((embedded, total), (2, 3));
        // Unknown project: zero of zero.
        assert_eq!(store.embedding_coverage("none").unwrap(), (0, 0));
    }

    #[cfg(feature = "hnsw")]
    #[test]
    fn graph_clusters_vec_groups_two_clusters_and_outlier() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Two semantic groups + one outlier. Bodies are deliberately distinct
        // lexically; the grouping comes purely from the hand-inserted vectors.
        let a1 = store.save("p", "note", "first about deployments").unwrap();
        let a2 = store.save("p", "note", "second about deployments").unwrap();
        let a3 = store.save("p", "note", "third about deployments").unwrap();
        let b1 = store.save("p", "note", "first about parsing").unwrap();
        let b2 = store.save("p", "note", "second about parsing").unwrap();
        let outlier = store.save("p", "note", "a lone unrelated thought").unwrap();

        // Cluster A: tight around (1, 0, 0). Cluster B: tight around (0, 1, 0).
        // Outlier: (0, 0, 1) — orthogonal to both (cosine 0 << VEC_MIN_SIM).
        put_embedding(&store, a1, &[1.00, 0.02, 0.0]);
        put_embedding(&store, a2, &[0.98, 0.05, 0.0]);
        put_embedding(&store, a3, &[0.99, 0.00, 0.0]);
        put_embedding(&store, b1, &[0.02, 1.00, 0.0]);
        put_embedding(&store, b2, &[0.05, 0.98, 0.0]);
        put_embedding(&store, outlier, &[0.0, 0.0, 1.0]);

        let idx = store.graph_clusters_vec("p", 100_000, 4, 320).unwrap();

        // Membership comes from node_cluster (cluster root per memory id).
        let root = |id: i64| -> i64 { *idx.node_cluster.get(&id).unwrap() };
        // A's three rows share one root; B's two share another; they differ.
        assert_eq!(root(a1), root(a2));
        assert_eq!(root(a1), root(a3));
        assert_eq!(root(b1), root(b2));
        assert_ne!(root(a1), root(b1), "two semantic groups must separate");
        // The outlier is in neither group.
        assert_ne!(root(outlier), root(a1));
        assert_ne!(root(outlier), root(b1));

        // Deterministic roots = min member id of each group.
        assert_eq!(root(a1), a1.min(a2).min(a3));
        assert_eq!(root(b1), b1.min(b2));
        assert_eq!(root(outlier), outlier);

        // Summaries: cluster A (size 3) is largest and sorts first.
        assert_eq!(idx.clusters[0].size, 3, "{:?}", idx.clusters);
        assert_eq!(idx.clusters[0].id, root(a1));
        // Exactly three bubbles: A, B, outlier.
        assert_eq!(idx.clusters.len(), 3, "{:?}", idx.clusters);
    }

    #[cfg(feature = "hnsw")]
    #[test]
    fn graph_clusters_auto_dispatches_to_vector_when_embedded() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Lexically near-identical bodies that vectors place in two groups.
        let a1 = store.save("p", "note", "topic alpha alpha alpha").unwrap();
        let a2 = store.save("p", "note", "topic alpha alpha alpha").unwrap();
        let b1 = store.save("p", "note", "topic beta beta beta").unwrap();
        let b2 = store.save("p", "note", "topic beta beta beta").unwrap();
        put_embedding(&store, a1, &[1.0, 0.0]);
        put_embedding(&store, a2, &[0.99, 0.02]);
        put_embedding(&store, b1, &[0.0, 1.0]);
        put_embedding(&store, b2, &[0.02, 0.99]);
        // All four embedded -> coverage 100% -> graph_clusters delegates to the
        // vector path, which separates the two vector groups.
        let idx = store.graph_clusters("p", 100_000, 4, 0.15).unwrap();
        let root = |id: i64| -> i64 { *idx.node_cluster.get(&id).unwrap() };
        assert_eq!(root(a1), root(a2));
        assert_eq!(root(b1), root(b2));
        assert_ne!(root(a1), root(b1));
    }
}
