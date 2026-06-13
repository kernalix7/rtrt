//! rtrt-dashboard — axum web UI + REST API.
//!
//! Surfaces:
//! - `/`               — bundled HTML index (mini-app: stats / templates / metrics).
//! - `/healthz`        — liveness probe.
//! - `/api/stats`      — compression / proxy savings JSON.
//! - `/api/overview`   — aggregate persisted optimizer savings.
//! - `/api/gain`       — persisted `rtrt proxy-run` savings analytics.
//! - `/api/templates`  — list built-in + custom templates.
//! - `/api/templates/{name}` — full manifest for one template.
//! - `/api/templates/scaffold` — `POST` scaffold a project.
//! - `/api/chat`       — `POST` chat through the bundled provider gateway.
//! - `/api/metrics`    — gateway summary + recent per-request metrics.
//! - `/api/prompts`    — list versioned prompts from the langfuse-style registry.
//! - `/api/prompts/{name}` — list versions for a single prompt.
//! - `/api/prompts/{name}/{version}` — full prompt body.
//! - `/api/budget`     — gateway budget cap + cumulative spend.
//! - `/api/memory/graph`  — `GET` memory graph: `mode=similarity`/`entity` for
//!   small graphs, `mode=overview` for LOD cluster bubbles, and `cluster=<root>`
//!   to drill into one cluster's members (cached `ClusterIndex`, 60s TTL).
//! - `/api/memory/recall` — `POST` BM25 recall with optional qdrant-style payload filter.
//! - `/api/memory/stats`  — `GET` aggregate stats for a project (total, by_kind, compressed).
//! - `/api/memory/save`   — `POST` insert a memory row with optional metadata.
//! - `/api/memory/blocks` — `GET` list / `POST` set Letta-style memory blocks.
//! - `/api/memory/blocks/{name}` — `GET` a single block (project as query param).
//! - `/api/compress`      — `POST` run the rule, ML, or LLM compressor against arbitrary text.
//! - `/api/proxy`         — `POST` rtrt-proxy filters (command / errors_only / ultra_compact).
//! - `/api/diagnose`      — `POST` aider-style failure triage (errors_only + gateway chat).
//! - `/api/route`         — `GET` dry-run orchestration route selection.
//! - `/api/statusline/config` — `GET` / `POST` statusline rich-format config.
//! - `/api/statusline/preview` — `GET` rendered `rtrt statusline --rich` preview.
//!
//! All `/api/*` routes are gated by a bearer-token middleware when the
//! `RTRT_DASHBOARD_TOKEN` env var is set; the bundled HTML index and the
//! `/healthz` probe remain open so the UI can bootstrap.

mod transcripts;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{delete, get, post},
};
use rtrt_core::{
    OutputStyleLevel, read_output_style_level, read_output_style_level_for,
    write_output_style_level_for,
};
use rtrt_memory::{
    ClusterIndex, ConceptHierarchy, DetailedRecord, Embedder, MemoryStore, PayloadFilter,
    Summariser,
};
use rtrt_providers::{
    ChatMessage, ChatRequest, ChatResponse, Gateway, MetricsView, Prefer, Provider, RequestMetric,
    Role, RouteAlternative, RouteRequest, UsageSnapshot, select_route,
};
use rtrt_security::{Profile, ScanReport};
use rtrt_templates::{Prompt, PromptRegistry};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::broadcast;

#[derive(Clone)]
struct AppState {
    gateway: Arc<Gateway>,
    prompts: Option<Arc<PromptRegistry>>,
    memory: Option<Arc<Mutex<MemoryStore>>>,
    auto_capture: bool,
    auto_redact: bool,
    default_project: String,
    session_id: String,
    dedup_window_sec: i64,
    events: broadcast::Sender<String>,
    /// Shared Ollama embedder, present when `[embeddings] enabled = true` in
    /// the config (or `RTRT_EMBED_ENABLED=1`). `None` keeps all non-vector
    /// paths working without an Ollama instance.
    embedder: Option<Arc<dyn Embedder>>,
    /// Per-`(project, group)` LOD cluster/group index cache
    /// (`"project\x1fgroup" -> (built_at, index)`), TTL [`CLUSTER_INDEX_TTL`].
    /// The `overview` mode builds + caches; the legacy `cluster` drill-down
    /// reads the `context` entry (rebuilding if missing/expired).
    cluster_cache:
        Arc<Mutex<std::collections::HashMap<String, (std::time::Instant, ClusterIndex)>>>,
    /// Per-scope TOP-LEVEL "digital brain" community hierarchy cache (the
    /// `mode=brain` community level). Same style + TTL ([`CLUSTER_INDEX_TTL`]) as
    /// [`AppState::cluster_cache`], keyed `"brainh\x1f<scope>"` and holding a
    /// [`ConceptHierarchy`] (communities + inter-community edges). The per-
    /// community concept sub-graph (`community=ID`) and the per-concept memories
    /// drill (`concept=TOKEN`) are served fresh — only this top level is cached.
    brainh_cache:
        Arc<Mutex<std::collections::HashMap<String, (std::time::Instant, ConceptHierarchy)>>>,
    /// Opaque drill tokens minted per overview/group build
    /// (`token -> (built_at, entry)`), TTL [`LEVEL_TOKEN_TTL`]. Each token maps
    /// to the member-id set of one bubble so the client can drill many levels
    /// deep without ever sending large id lists over the wire.
    level_tokens: Arc<Mutex<std::collections::HashMap<String, (std::time::Instant, TokenEntry)>>>,
    /// On-disk SQLite path, so a background embedding backfill can open its OWN
    /// connection (WAL = concurrent with the main one) instead of holding the
    /// shared store mutex for the whole multi-minute job.
    memory_path: std::path::PathBuf,
    /// Projects with an embedding backfill currently running (dedup + progress).
    embedding_jobs: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// One drill token's payload: which project it belongs to, the member ids it
/// stands for, and the bubble label (kept for debugging / future reuse).
#[derive(Clone)]
struct TokenEntry {
    project: String,
    member_ids: Vec<i64>,
    /// Project total at mint time — drives the dynamic leaf cutoff so the
    /// "show individual nodes" threshold is consistent across drill levels.
    total_n: usize,
    /// Bubble label captured at mint time. Not read on the drill path (the
    /// child rebuild derives its own labels) but retained for debug logging
    /// and future "breadcrumb" responses.
    #[allow(dead_code)]
    label: String,
}

/// Monotonic source for unique drill-token suffixes (paired with a coarse
/// timestamp so tokens are short, opaque, and collision-free across rebuilds).
static LEVEL_TOKEN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint a fresh opaque drill token (e.g. `t-<seq>-<nanos>`).
fn mint_level_token() -> String {
    let seq = LEVEL_TOKEN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("t-{seq}-{nanos:09}")
}

/// Time-to-live for a cached [`ClusterIndex`] before it is rebuilt.
const CLUSTER_INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Time-to-live for a minted drill [`TokenEntry`]. Long enough for a user to
/// drill several levels; short enough that the token cache stays bounded.
const LEVEL_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(180);

/// Leaf cutoff: a bubble at/under this many members renders its individual
/// memory nodes instead of splitting further. Dynamic in the project total
/// (~√total, clamped) so a small project bottoms out in one or two levels while
/// a huge one does not dump thousands of points into a single leaf.
fn dynamic_leaf(total: usize) -> usize {
    ((total as f64).sqrt().round() as usize).clamp(40, 160)
}

/// Sub-bubble target when re-clustering a bubble of `size` members at a deeper
/// level. Dynamic in the bucket size (~1.4·√size, clamped): bigger buckets fan
/// out wider, so drill DEPTH grows with quantity instead of being a fixed step.
fn dynamic_branch(size: usize) -> usize {
    (((size as f64).sqrt() * 1.4).round() as usize).clamp(12, 64)
}

/// A re-cluster "did not split" — its largest child still holds this fraction of
/// the parent — so semantic clustering cannot break it up (a lexically-disjoint
/// unclustered mass). The drill path then falls back to a metadata facet.
const STALL_DOMINANCE: f64 = 0.6;

fn broadcast_event(tx: &broadcast::Sender<String>, payload: serde_json::Value) {
    let _ = tx.send(payload.to_string());
}

/// Extract the % savings from a memory row's metadata map (set by the LLM
/// compress sweep). Returns `None` when the row is not compressed or the
/// metadata fields are absent/unparseable.
fn compress_saved_pct_from_meta(
    meta: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<f64> {
    let m = meta?;
    let from: i64 = m.get("compressed_from_chars")?.parse().ok()?;
    let to: i64 = m.get("compressed_to_chars")?.parse().ok()?;
    if from <= 0 {
        return None;
    }
    let saved = (from - to).max(0);
    Some((saved as f64 / from as f64 * 100.0 * 10.0).round() / 10.0)
}

/// Thin wrapper that makes a [`Gateway`] look like a [`Provider`] so we can
/// pass it to [`rtrt_memory::LlmSummariser`]. The adapter forwards chat calls
/// directly; streaming is not needed for summarisation.
struct GatewayAdapter(Arc<Gateway>);

#[async_trait::async_trait]
impl Provider for GatewayAdapter {
    fn name(&self) -> &str {
        "gateway"
    }
    fn supported_models(&self) -> &[&'static str] {
        &[]
    }
    async fn chat(&self, req: ChatRequest) -> rtrt_core::Result<ChatResponse> {
        self.0.chat(req).await
    }
}

impl AppState {
    /// Best-effort auto-save into the memory store. Pipeline:
    /// 1. Privacy filter (`redact_secrets`) when `auto_redact` is true.
    /// 2. SHA-256 dedup against the last `dedup_window_sec` seconds.
    /// 3. Insert raw row, then tag it with session_id + body_sha.
    async fn capture(
        &self,
        kind: &str,
        body: &str,
        metadata: &std::collections::BTreeMap<String, String>,
    ) {
        if !self.auto_capture {
            return;
        }
        let Some(store) = &self.memory else { return };
        let filtered = if self.auto_redact {
            rtrt_compress::redact_secrets(body)
        } else {
            body.to_string()
        };
        let project = self.default_project.clone();
        let kind = kind.to_string();
        let metadata = metadata.clone();
        let session = self.session_id.clone();
        let window = self.dedup_window_sec;
        let guard = store.lock().await;
        let sha = rtrt_memory::MemoryStore::body_sha(&filtered);
        if window > 0
            && let Ok(Some(last_ts)) = guard.body_seen_at(&project, &sha)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if now.saturating_sub(last_ts) < window {
                tracing::debug!(kind = %kind, "auto-capture deduped within {window}s window");
                return;
            }
        }
        let result = if metadata.is_empty() {
            guard.save(&project, &kind, &filtered)
        } else {
            guard.save_with_metadata(&project, &kind, &filtered, &metadata)
        };
        match result {
            Ok(id) => {
                if let Err(e) = guard.tag_row(id, Some(&session), Some(&sha)) {
                    tracing::warn!("auto-capture tag {kind}: {e}");
                }
                broadcast_event(
                    &self.events,
                    serde_json::json!({
                        "type": "memory.save",
                        "id": id,
                        "kind": kind,
                        "project": project,
                        "session": session,
                    }),
                );
            }
            Err(e) => tracing::warn!("auto-capture {kind}: {e}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info,tower_http=info")
        .init();

    let bind =
        std::env::var("RTRT_DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:7311".to_string());
    let token = std::env::var("RTRT_DASHBOARD_TOKEN").ok();
    if token.is_none()
        && !bind.starts_with("127.")
        && !bind.starts_with("[::1]")
        && !bind.starts_with("localhost")
    {
        tracing::warn!(
            "binding {bind} without RTRT_DASHBOARD_TOKEN is risky; non-loopback callers can hit the API without authentication."
        );
    }
    // Feed the config's auto_compress.base_url into the env before building
    // the gateway, so `Gateway::from_env` registers the local/OpenAI-compat
    // provider even when only ~/.rtrt/config.toml (not an env var) sets it.
    // Without this, the LLM compress engine + auto-compress daemon can't
    // route a local model like gemma3:4b.
    if std::env::var_os("RTRT_PROVIDER_BASE_URL").is_none()
        && std::env::var_os("RTRT_OPENAI_COMPAT_URL").is_none()
        && let Ok(cfg) = rtrt_core::Config::load()
        && let Some(url) = cfg.auto_compress.base_url.as_deref()
    {
        // SAFETY: set during single-threaded startup before any task spawns.
        unsafe { std::env::set_var("RTRT_PROVIDER_BASE_URL", url) };
    }
    let gateway = Arc::new(Gateway::from_env());
    let prompts = open_prompt_registry();
    let memory = open_memory_store();
    let auto_capture = std::env::var("RTRT_AUTO_CAPTURE")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);
    let auto_redact = std::env::var("RTRT_AUTO_REDACT")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);
    let dedup_window_sec: i64 = std::env::var("RTRT_AUTO_DEDUP_WINDOW_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let default_project =
        std::env::var("RTRT_DEFAULT_PROJECT").unwrap_or_else(|_| "default".into());
    let session_id = uuid::Uuid::new_v4().to_string();
    if auto_capture {
        tracing::info!(
            "auto-capture on (project={default_project}, redact={auto_redact}, dedup_window={dedup_window_sec}s, session={session_id})"
        );
    } else {
        tracing::info!("auto-capture off (RTRT_AUTO_CAPTURE=0)");
    }
    let memory_for_daemon = memory.clone();
    let memory_for_compress_daemon = memory.clone();
    let memory_for_transcripts = memory.clone();
    let gateway_for_compress_daemon = gateway.clone();
    let (events_tx, _) = broadcast::channel::<String>(256);
    // Build the Ollama embedder when enabled in config / env.
    let embedder: Option<Arc<dyn Embedder>> = {
        let ecfg = rtrt_core::Config::load().unwrap_or_default().embeddings;
        if ecfg.is_enabled() {
            let base_url = ecfg.resolved_base_url(
                rtrt_core::Config::load()
                    .ok()
                    .and_then(|c| c.auto_compress.base_url)
                    .as_deref(),
            );
            let model = ecfg.effective_model();
            tracing::info!("embeddings enabled: model={model} base_url={base_url}");
            Some(Arc::new(rtrt_memory::OllamaEmbedder::new(base_url, model)))
        } else {
            tracing::info!(
                "embeddings disabled (set RTRT_EMBED_ENABLED=1 or [embeddings] enabled=true)"
            );
            None
        }
    };
    let state = AppState {
        gateway,
        prompts,
        memory,
        auto_capture,
        auto_redact,
        default_project,
        session_id,
        dedup_window_sec,
        events: events_tx,
        embedder,
        cluster_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        brainh_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        level_tokens: Arc::new(Mutex::new(std::collections::HashMap::new())),
        memory_path: memory_store_path(),
        embedding_jobs: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
    };
    spawn_consolidation_daemon(memory_for_daemon);
    spawn_auto_compress_daemon(memory_for_compress_daemon, gateway_for_compress_daemon);
    spawn_auto_embed_daemon(memory_store_path());
    transcripts::spawn_reattribution(memory_for_transcripts.clone());
    transcripts::spawn_transcript_watcher(memory_for_transcripts);

    let token_arc = token.clone().map(Arc::new);
    let app = Router::new()
        .route("/", get(index))
        .route("/vendor/{file}", get(vendor_asset))
        .route("/healthz", get(healthz))
        .route("/api/stats", get(stats))
        .route("/api/overview", get(optimizer_overview))
        .route("/api/optimizer/overview", get(optimizer_overview))
        .route("/api/gain", get(gain))
        .route("/api/detect", get(detect_tools_api))
        .route("/api/route", get(route_api))
        .route("/api/detect/toggle", post(toggle_detected_tool))
        .route(
            "/api/optimizer/level",
            get(get_optimizer_level).post(post_optimizer_level),
        )
        .route("/api/templates", get(list_templates).post(create_template))
        .route(
            "/api/templates/{name}",
            get(get_template)
                .put(update_template)
                .delete(delete_template),
        )
        .route("/api/templates/scaffold", post(scaffold))
        .route("/api/templates/scaffold/preview", post(scaffold_preview))
        .route("/api/chat", post(chat))
        .route("/api/metrics", get(metrics))
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts/{name}", get(list_prompt_versions))
        .route("/api/prompts/{name}/{version}", get(get_prompt))
        .route("/api/budget", get(budget))
        .route("/api/memory/projects", get(memory_projects))
        .route("/api/memory/timeline", get(memory_timeline))
        .route("/api/memory/recall", post(memory_recall))
        .route("/api/memory/graph", get(memory_graph))
        .route("/api/memory/export", get(memory_export))
        .route("/api/memory/save", post(memory_save))
        .route("/api/memory/blocks", get(list_blocks).post(set_block))
        .route("/api/memory/blocks/{name}", get(get_block))
        .route("/api/compress", post(compress))
        .route("/api/proxy", post(proxy_filter))
        .route("/api/diagnose", post(diagnose))
        .route("/api/repo-map", post(repo_map))
        .route("/api/setup", post(setup_snippet))
        .route("/api/stream", get(sse_stream))
        .route("/api/tokens/summary", get(tokens_summary))
        .route("/api/config", get(get_config).post(post_config))
        .route(
            "/api/statusline/config",
            get(get_statusline_config).post(post_statusline_config),
        )
        .route("/api/statusline/preview", get(statusline_preview))
        .route("/api/models", get(get_models))
        .route("/api/memory/compress", post(memory_compress))
        .route("/api/memory/stats", get(memory_stats))
        .route("/api/memory/queue", get(memory_queue))
        .route("/api/memory/delete", post(memory_delete_batch))
        .route("/api/memory/embed", post(memory_embed))
        .route("/api/memory/coverage", get(memory_coverage))
        .route("/api/memory/entities", post(memory_entities))
        .route("/api/ollama/models", get(ollama_models))
        .route("/api/ollama/{name}", delete(ollama_delete))
        .route("/api/ollama/ps", get(ollama_ps))
        .route("/api/ollama/pull", post(ollama_pull))
        .route("/api/security/profiles", get(security_profiles))
        .route("/api/security/profile/{name}", get(security_profile))
        .route("/api/security/scan", post(security_scan))
        .route("/api/security/profile", post(security_profile_save))
        .route("/api/projects", get(list_projects).put(upsert_project))
        .route(
            "/api/memory/{id}",
            get(memory_detail).delete(memory_delete_one),
        )
        .layer(axum::middleware::from_fn(move |req, next| {
            let token = token_arc.clone();
            async move { bearer_guard(token, req, next).await }
        }))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "address {bind} is already in use. Free the port (lsof -i :{port}) or set RTRT_DASHBOARD_BIND to another address (e.g. RTRT_DASHBOARD_BIND=127.0.0.1:3211 rtrt-dashboard).",
                port = bind.rsplit(':').next().unwrap_or("7311"),
            );
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!("rtrt-dashboard listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Hourly background sweep — keeps each project's row count under
/// `RTRT_CONSOLIDATE_KEEP` (default 1000) using the LLM-free archive path.
/// Disabled when `RTRT_CONSOLIDATE_INTERVAL_SEC=0`.
fn spawn_consolidation_daemon(memory: Option<Arc<Mutex<MemoryStore>>>) {
    // OFF by default: this daemon DELETES the oldest rows beyond `keep` (no LLM
    // summary), which conflicts with rtrt's permanent-memory promise. Opt in
    // explicitly with RTRT_CONSOLIDATE_INTERVAL_SEC > 0 if you want a hard cap.
    // Token growth is handled by the auto-compress daemon, which shrinks bodies
    // while keeping every row (and the original in body_full).
    let interval_sec: u64 = std::env::var("RTRT_CONSOLIDATE_INTERVAL_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if interval_sec == 0 {
        tracing::info!(
            "consolidation daemon off (permanent memory; set RTRT_CONSOLIDATE_INTERVAL_SEC>0 to cap)"
        );
        return;
    }
    let keep: usize = std::env::var("RTRT_CONSOLIDATE_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let Some(store) = memory else {
        tracing::info!("consolidation daemon off (no memory store)");
        return;
    };
    tracing::info!(
        "consolidation daemon every {interval_sec}s, keep {keep} most-recent rows per project"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_sec));
        // The first tick fires immediately — skip it so the daemon doesn't
        // sweep on startup before any rows have accumulated.
        tick.tick().await;
        loop {
            tick.tick().await;
            let guard = store.lock().await;
            let projects = match guard.projects() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("consolidate: list projects: {e}");
                    continue;
                }
            };
            for (project, count, _) in projects {
                if count <= keep {
                    continue;
                }
                match guard.archive_overflow_no_llm(&project, keep) {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(project = %project, removed, kept = keep, "consolidated");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("consolidate {project}: {e}"),
                }
            }
        }
    });
}

/// Opt-in background worker — sweeps memory rows older than
/// `RTRT_AUTO_COMPRESS_AGE_SEC` whose body is longer than
/// `RTRT_AUTO_COMPRESS_MIN_CHARS`, asks the configured LLM model to
/// compress each one, and writes the result back via `set_body`.
/// Idempotent: every rewritten row is tagged `metadata.compressed_at`,
/// and `compress_candidates` filters those out next sweep.
///
/// Disabled unless `RTRT_AUTO_COMPRESS_LLM=1`. Honours the same
/// `RTRT_AUTO_COMPRESS_*` knobs documented in `docs/USAGE.md`.
fn spawn_auto_compress_daemon(memory: Option<Arc<Mutex<MemoryStore>>>, gateway: Arc<Gateway>) {
    // Resolution order: env var > ~/.rtrt/config.toml > built-in default.
    // So the daemon turns on when EITHER RTRT_AUTO_COMPRESS_LLM=1 OR the
    // config's [auto_compress] enabled=true.
    let cfg = rtrt_core::Config::load().unwrap_or_default().auto_compress;
    let enabled = match std::env::var("RTRT_AUTO_COMPRESS_LLM") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
        Err(_) => cfg.enabled,
    };
    if !enabled {
        tracing::info!(
            "auto-compress daemon off (set RTRT_AUTO_COMPRESS_LLM=1 or [auto_compress] enabled=true)"
        );
        return;
    }
    let Some(store) = memory else {
        tracing::info!("auto-compress daemon off (no memory store)");
        return;
    };
    let interval_sec: u64 = std::env::var("RTRT_AUTO_COMPRESS_INTERVAL_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.interval_sec);
    let age_sec: i64 = std::env::var("RTRT_AUTO_COMPRESS_AGE_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.age_sec);
    let min_chars: usize = std::env::var("RTRT_AUTO_COMPRESS_MIN_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.min_chars);
    let batch: usize = std::env::var("RTRT_AUTO_COMPRESS_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.batch);
    let model = std::env::var("RTRT_AUTO_COMPRESS_MODEL").unwrap_or_else(|_| cfg.model.clone());
    let max_tokens: u32 = std::env::var("RTRT_AUTO_COMPRESS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.max_tokens);
    tracing::info!(
        "auto-compress daemon on: model={model}, every {interval_sec}s, age>{age_sec}s, min_chars={min_chars}, batch={batch}"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_sec));
        tick.tick().await;
        loop {
            tick.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let cutoff = now - age_sec;
            let projects = {
                let guard = store.lock().await;
                match guard.projects() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("auto-compress: list projects: {e}");
                        continue;
                    }
                }
            };
            for (project, _, _) in projects {
                let candidates = {
                    let guard = store.lock().await;
                    match guard.compress_candidates(&project, cutoff, min_chars, batch) {
                        Ok(rows) => rows,
                        Err(e) => {
                            tracing::warn!("auto-compress {project}: candidates: {e}");
                            continue;
                        }
                    }
                };
                for (id, body) in candidates {
                    let req = ChatRequest {
                        model: model.clone(),
                        messages: vec![
                            ChatMessage {
                                role: Role::System,
                                content: "You are a lossless-meaning compressor. Rewrite the user message in the shortest form that preserves every fact, decision, file path, identifier, command, and number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary, no preamble, no quotes — emit only the compressed text.".to_string(),
                            },
                            ChatMessage {
                                role: Role::User,
                                content: body.clone(),
                            },
                        ],
                        max_tokens: Some(max_tokens),
                        temperature: Some(0.0),
                    };
                    let resp = match gateway.chat(req).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("auto-compress {project}#{id}: {e}");
                            continue;
                        }
                    };
                    let new_body = resp.content.trim().to_string();
                    if new_body.is_empty() || new_body.len() >= body.len() {
                        // No win — skip but still mark so we don't retry.
                        let guard = store.lock().await;
                        let mut meta = guard.get_metadata(id).unwrap_or_default();
                        meta.insert("compressed_at".into(), now.to_string());
                        meta.insert("compressed_skip".into(), "no-shrink".into());
                        let _ = guard.set_metadata(id, &meta);
                        continue;
                    }
                    let guard = store.lock().await;
                    if let Err(e) = guard.compress_in_place(id, &new_body) {
                        tracing::warn!("auto-compress {project}#{id}: set_body: {e}");
                        continue;
                    }
                    let mut meta = guard.get_metadata(id).unwrap_or_default();
                    meta.insert("compressed_at".into(), now.to_string());
                    meta.insert("compressed_model".into(), model.clone());
                    meta.insert("compressed_from_chars".into(), body.len().to_string());
                    meta.insert("compressed_to_chars".into(), new_body.len().to_string());
                    let _ = guard.set_metadata(id, &meta);
                    tracing::info!(
                        project = %project,
                        id,
                        from = body.len(),
                        to = new_body.len(),
                        "auto-compressed"
                    );
                }
            }
        }
    });
}

/// Background auto-embed worker — puts embeddings into the AUTOMATIC loop so new
/// captures (saved text-only + fast) get a vector without ever blocking capture
/// or a prompt. Every `embeddings.auto_interval_sec` it opens its OWN
/// `MemoryStore` on `memory_store_path()` (WAL = concurrent with the main store),
/// pulls `unembedded_batch(auto_batch)` newest-first, embeds each body with an
/// `OllamaEmbedder` (`embed_one` already truncates to ~2000 chars), and writes
/// the vector via `store_embedding`. Capped at `auto_batch` rows per cycle so we
/// never hammer Ollama.
///
/// OPTIONAL / LLM-FREE BY DEFAULT: spawns ONLY when `[embeddings] enabled=true`
/// AND `auto=true`. When disabled, this returns immediately — the memory loop
/// stays pure BM25, zero Ollama calls. Probes the embedder reachable ONCE before
/// engaging; if unreachable it logs once and does not spawn (no per-cycle spam).
/// An embed error on a single row is logged and skipped — the row is retried next
/// cycle, the daemon keeps going.
fn spawn_auto_embed_daemon(path: PathBuf) {
    let ecfg = rtrt_core::Config::load().unwrap_or_default().embeddings;
    if !ecfg.is_enabled() {
        tracing::info!(
            "auto-embed daemon off (set RTRT_EMBED_ENABLED=1 or [embeddings] enabled=true)"
        );
        return;
    }
    if !ecfg.auto {
        tracing::info!("auto-embed daemon off ([embeddings] auto=false)");
        return;
    }
    let base_url = ecfg.resolved_base_url(
        rtrt_core::Config::load()
            .ok()
            .and_then(|c| c.auto_compress.base_url)
            .as_deref(),
    );
    let model = ecfg.effective_model();
    let interval_sec = ecfg.auto_interval_sec.max(1);
    let batch = ecfg.auto_batch.max(1);

    tokio::spawn(async move {
        // Probe the embedder reachable ONCE before engaging. If Ollama is
        // unreachable, log once and stay BM25 (do NOT spam every cycle).
        // `OllamaEmbedder` isn't `Clone` (it caches a detected dimension), so we
        // carry the `base_url`/`model` strings and build a fresh one each time —
        // the same pattern the `/api/memory/embed` handler uses.
        let probe = tokio::task::spawn_blocking({
            let base_url = base_url.clone();
            let model = model.clone();
            move || {
                rtrt_memory::OllamaEmbedder::new(base_url, model).embed_one("rtrt auto-embed probe")
            }
        })
        .await;
        match probe {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::info!(
                    "auto-embed daemon off (embedder {base_url} model={model} unreachable: {e}); staying BM25"
                );
                return;
            }
            Err(e) => {
                tracing::info!("auto-embed daemon off (probe task failed: {e}); staying BM25");
                return;
            }
        }
        tracing::info!(
            "auto-embed daemon on: model={model}, base_url={base_url}, every {interval_sec}s, batch={batch}"
        );

        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_sec));
        // First tick fires immediately — skip it so we don't race startup.
        tick.tick().await;
        loop {
            tick.tick().await;
            let path = path.clone();
            let base_url = base_url.clone();
            let model = model.clone();
            // Do the SQLite + embedding work off the async runtime: this opens its
            // OWN WAL connection (concurrent with the main store) and blocks on
            // Ollama HTTP, so it must not run on a tokio worker thread.
            let embedded = tokio::task::spawn_blocking(move || {
                let store = match MemoryStore::open(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("auto-embed: open store failed: {e}");
                        return 0usize;
                    }
                };
                let rows = match store.unembedded_batch(batch) {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::warn!("auto-embed: unembedded_batch: {e}");
                        return 0;
                    }
                };
                if rows.is_empty() {
                    return 0;
                }
                let embedder = rtrt_memory::OllamaEmbedder::new(base_url, model);
                let mut done = 0;
                for (id, _project, body) in rows {
                    let vector = match embedder.embed_one(&body) {
                        Ok(v) => v,
                        Err(e) => {
                            // Skip this row and keep going; retried next cycle.
                            tracing::warn!("auto-embed #{id}: embed: {e}");
                            continue;
                        }
                    };
                    if let Err(e) = store.store_embedding(id, embedder.model_name(), &vector) {
                        tracing::warn!("auto-embed #{id}: store_embedding: {e}");
                        continue;
                    }
                    done += 1;
                }
                done
            })
            .await
            .unwrap_or(0);
            if embedded > 0 {
                tracing::info!(embedded, "auto-embed cycle");
            }
        }
    });
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Serve the vendored graph libraries (Cytoscape + cola/fcose layout deps) from
/// the binary itself, so the memory map renders WITHOUT any CDN / internet — a
/// blocked CDN was making the map fall back to a plain list.
async fn vendor_asset(
    axum::extract::Path(file): axum::extract::Path<String>,
) -> std::result::Result<([(axum::http::HeaderName, &'static str); 2], &'static str), StatusCode> {
    let body = match file.as_str() {
        "cytoscape.min.js" => VENDOR_CYTOSCAPE,
        "layout-base.js" => VENDOR_LAYOUT_BASE,
        "cose-base.js" => VENDOR_COSE_BASE,
        "cytoscape-fcose.js" => VENDOR_FCOSE,
        "cola.min.js" => VENDOR_COLA,
        "cytoscape-cola.js" => VENDOR_CYTO_COLA,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    ))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn stats() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "input_saved": 0,
        "output_saved": 0,
        "provider": null,
    }))
}

#[derive(Debug, Deserialize)]
struct SetLevelRequest {
    level: String,
}

/// Optional `?project=<name>` selector shared by the project-aware endpoints.
/// An absent value, the empty string, or the literal `"global"` all resolve to
/// the global scope (back-compat with existing UI calls).
#[derive(Debug, Deserialize, Default)]
struct ProjectQuery {
    #[serde(default)]
    project: Option<String>,
    /// Optional scope hint for project-aware writes. The statusline POST honours
    /// `scope=global` (a.k.a. "Follow global") to CLEAR a project's own override
    /// so it inherits the global config again. Absent/`custom` writes the
    /// per-project override as before. Other endpoints ignore this field.
    #[serde(default)]
    scope: Option<String>,
}

/// Resolve a `?project=` selector to an on-disk repo path.
///
/// Returns `Some(path)` only when the named project exists in the global config
/// and carries an absolute `.path` (a real, on-disk repo). `None` (the global
/// scope) is returned for the `"global"`/empty sentinel, an unknown project, or
/// a memory-only project with no path.
fn resolve_project_repo(project: Option<&str>) -> Option<PathBuf> {
    let name = project.map(str::trim).filter(|n| !n.is_empty())?;
    if name.eq_ignore_ascii_case("global") {
        return None;
    }
    let cfg = rtrt_core::Config::load().ok()?;
    let path = cfg.project(name)?.path.as_deref()?;
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

async fn get_optimizer_level(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
) -> impl IntoResponse {
    let repo = resolve_project_repo(q.project.as_deref());
    let level = read_output_style_level_for(repo.as_deref());
    // `custom` is true when THIS project carries its own `output_level` override
    // in `<repo>/.rtrt/config.toml`; otherwise it inherits the global level.
    // `scope`/`custom`/`inherited` mirror the statusline endpoint so the UI's
    // "Follow global / Custom (this project)" helper can be shared between them.
    let custom = match &repo {
        Some(path) => rtrt_core::Config::load_project(path)
            .map(|p| p.output_level.is_some())
            .unwrap_or(false),
        None => false,
    };
    let scope = if custom { "custom" } else { "global" };
    Json(serde_json::json!({
        "level": level.as_str(),
        "active": level.is_active(),
        "scope": scope,
        "custom": custom,
        // `inherited` is the inverse of `custom` for a project scope; always
        // false for the global scope (there is nothing to inherit from).
        "inherited": repo.is_some() && !custom,
    }))
}

async fn post_optimizer_level(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
    // Optional: the "Follow global" clear path (`?scope=global`) carries no body,
    // so a missing/empty JSON payload must be tolerated. A level write supplies it.
    body: Option<Json<SetLevelRequest>>,
) -> impl IntoResponse {
    let repo = resolve_project_repo(q.project.as_deref());

    // "Follow global" action: `?scope=global` CLEARS this project's
    // `output_level` override so it inherits the global level again. Only
    // meaningful for a resolved project; ignored for the global scope.
    let follow_global = q
        .scope
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("global"));
    if follow_global {
        if let Some(path) = repo.as_deref() {
            // Clear only the output_level field; load → set None → save_project.
            // `save_project` re-serializes the whole ProjectConfig, so a
            // coexisting `[statusline]` override is preserved (and the file is
            // removed only when the ProjectConfig becomes entirely empty).
            let mut project = match rtrt_core::Config::load_project(path) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            };
            project.output_level = None;
            if let Err(e) = rtrt_core::Config::save_project(path, &project) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
            let level = read_output_style_level_for(repo.as_deref());
            return Json(serde_json::json!({
                "level": level.as_str(),
                "active": level.is_active(),
                "scope": "global",
                "custom": false,
                "inherited": true,
            }))
            .into_response();
        }
        // No project to clear in the global scope — fall through to a normal
        // write below, which keeps the existing global-level behaviour.
    }

    let Some(Json(body)) = body else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "missing level" })),
        )
            .into_response();
    };
    let Some(level) = OutputStyleLevel::parse(&body.level) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "unknown level" })),
        )
            .into_response();
    };
    if let Err(e) = write_output_style_level_for(repo.as_deref(), level) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    // A project write persists the project's own override (Custom scope);
    // the global write edits the global level (no inheritance).
    let custom = repo.is_some();
    Json(serde_json::json!({
        "level": level.as_str(),
        "active": level.is_active(),
        "scope": if custom { "custom" } else { "global" },
        "custom": custom,
        "inherited": false,
    }))
    .into_response()
}

fn metadata_savings(raw: &str, expected_source: &str) -> Option<(i64, i64, i64)> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object()?;
    if obj.get("source")?.as_str()? != expected_source {
        return None;
    }
    let original_chars = json_i64(obj.get("compressed_from_chars")?)?.max(0);
    let compressed_to_chars = obj.get("compressed_to_chars").and_then(json_i64);
    // The stored `saved_chars` field is unreliable (observed 0 for a real
    // 309->117 compression, and 81 for a 68-char input). Always derive the
    // delta from compressed_from/to when `to` is present; only fall back to the
    // stored field when `to` is missing.
    let (saved_chars, with_rtrt_chars) = match compressed_to_chars {
        Some(to_chars) => ((original_chars - to_chars).max(0), to_chars.max(0)),
        None => {
            let stored = obj.get("saved_chars").and_then(json_i64)?.max(0);
            (stored, original_chars.saturating_sub(stored).max(0))
        }
    };
    Some((saved_chars, original_chars, with_rtrt_chars))
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    value.as_str()?.parse::<i64>().ok()
}

fn estimate_saved_tokens(saved_chars: i64) -> i64 {
    saved_chars / 4
}

fn saved_pct(saved_chars: i64, original_chars: i64) -> Option<f64> {
    if original_chars <= 0 {
        return None;
    }
    Some(saved_chars.max(0) as f64 / original_chars as f64 * 100.0)
}

fn saved_pct_or_zero(saved_chars: i64, original_chars: i64) -> f64 {
    saved_pct(saved_chars, original_chars).unwrap_or(0.0)
}

fn proxy_stats_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rtrt")
        .join("proxy-stats.sqlite")
}

fn normalize_project(project: Option<String>) -> String {
    let project = project.unwrap_or_default();
    let project = project.trim();
    if project.is_empty() {
        "(unknown)".to_string()
    } else {
        project.to_string()
    }
}

const SECS_PER_HOUR: i64 = 60 * 60;
const SECS_PER_DAY: i64 = SECS_PER_HOUR * 24;

#[derive(Debug, Clone, Copy, Serialize)]
struct DashboardWindow {
    label: &'static str,
    since_unix: Option<i64>,
    proxy_modifier: Option<&'static str>,
}

impl DashboardWindow {
    fn parse(raw: Option<&str>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match raw.unwrap_or("all").trim().to_ascii_lowercase().as_str() {
            "1h" => Self {
                label: "1h",
                since_unix: Some(now.saturating_sub(SECS_PER_HOUR)),
                proxy_modifier: Some("-1 hours"),
            },
            "6h" => Self {
                label: "6h",
                since_unix: Some(now.saturating_sub(SECS_PER_HOUR * 6)),
                proxy_modifier: Some("-6 hours"),
            },
            "24h" => Self {
                label: "24h",
                since_unix: Some(now.saturating_sub(SECS_PER_DAY)),
                proxy_modifier: Some("-24 hours"),
            },
            "7d" => Self {
                label: "7d",
                since_unix: Some(now.saturating_sub(SECS_PER_DAY * 7)),
                proxy_modifier: Some("-7 days"),
            },
            "30d" => Self {
                label: "30d",
                since_unix: Some(now.saturating_sub(SECS_PER_DAY * 30)),
                proxy_modifier: Some("-30 days"),
            },
            _ => Self {
                label: "all",
                since_unix: None,
                proxy_modifier: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SavingsCoverage {
    reduced: i64,
    total: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SavingsProject {
    project: String,
    count: i64,
    reduced_count: i64,
    saved_chars: i64,
    saved_tokens: i64,
    original_chars: i64,
    with_rtrt_chars: i64,
    saved_pct: Option<f64>,
    effective_pct: Option<f64>,
    coverage: SavingsCoverage,
    #[serde(skip)]
    effective_pct_sum: f64,
}

#[derive(Debug, Clone, Serialize)]
struct SavingsSource {
    name: String,
    label: String,
    available: bool,
    count: i64,
    saved_chars: i64,
    saved_tokens: i64,
    original_chars: i64,
    with_rtrt_chars: i64,
    saved_pct: Option<f64>,
    effective_pct: Option<f64>,
    coverage: SavingsCoverage,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    measurement_note: Option<String>,
    by_project: Vec<SavingsProject>,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectSourceRollup {
    count: i64,
    reduced_count: i64,
    saved_chars: i64,
    saved_tokens: i64,
    original_chars: i64,
    with_rtrt_chars: i64,
    saved_pct: Option<f64>,
    effective_pct: Option<f64>,
    coverage: SavingsCoverage,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectRollup {
    project: String,
    saved_chars: i64,
    saved_tokens: i64,
    original_chars: i64,
    with_rtrt_chars: i64,
    saved_pct: Option<f64>,
    by_source: BTreeMap<String, ProjectSourceRollup>,
}

fn savings_source(
    name: &str,
    label: &str,
    source: &str,
    available: bool,
    mut by_project: Vec<SavingsProject>,
) -> SavingsSource {
    by_project.sort_by(|a, b| {
        b.saved_tokens
            .cmp(&a.saved_tokens)
            .then_with(|| b.saved_chars.cmp(&a.saved_chars))
            .then_with(|| a.project.cmp(&b.project))
    });
    let count = by_project.iter().map(|p| p.count).sum();
    let reduced_count = by_project.iter().map(|p| p.reduced_count).sum();
    let saved_chars = by_project.iter().map(|p| p.saved_chars).sum();
    let original_chars = by_project.iter().map(|p| p.original_chars).sum();
    let with_rtrt_chars = by_project.iter().map(|p| p.with_rtrt_chars).sum();
    let effective_pct_sum: f64 = by_project.iter().map(|p| p.effective_pct_sum).sum();
    SavingsSource {
        name: name.to_string(),
        label: label.to_string(),
        available: available && count > 0,
        count,
        saved_chars,
        saved_tokens: estimate_saved_tokens(saved_chars),
        original_chars,
        with_rtrt_chars,
        saved_pct: saved_pct(saved_chars, original_chars),
        effective_pct: (reduced_count > 0).then_some(effective_pct_sum / reduced_count as f64),
        coverage: SavingsCoverage {
            reduced: reduced_count,
            total: count,
        },
        source: source.to_string(),
        level: None,
        active: None,
        measurement_note: None,
        by_project,
    }
}

const OUTPUT_OPTIMIZER_MEASUREMENT_NOTE: &str =
    "Deterministic compress only — terse-mode injection savings are not measurable";

fn output_optimizer_active(level: &str) -> bool {
    !matches!(
        level.trim().to_ascii_lowercase().as_str(),
        "off" | "none" | "disabled"
    )
}

fn projects_rollup(sources: &[SavingsSource]) -> Vec<ProjectRollup> {
    let mut projects: BTreeMap<String, ProjectRollup> = BTreeMap::new();
    for source in sources {
        for item in &source.by_project {
            let entry = projects
                .entry(item.project.clone())
                .or_insert_with(|| ProjectRollup {
                    project: item.project.clone(),
                    saved_chars: 0,
                    saved_tokens: 0,
                    original_chars: 0,
                    with_rtrt_chars: 0,
                    saved_pct: None,
                    by_source: BTreeMap::new(),
                });
            entry.saved_chars += item.saved_chars;
            entry.original_chars += item.original_chars;
            entry.with_rtrt_chars += item.with_rtrt_chars;
            entry.by_source.insert(
                source.name.clone(),
                ProjectSourceRollup {
                    count: item.count,
                    reduced_count: item.reduced_count,
                    saved_chars: item.saved_chars,
                    saved_tokens: item.saved_tokens,
                    original_chars: item.original_chars,
                    with_rtrt_chars: item.with_rtrt_chars,
                    saved_pct: item.saved_pct,
                    effective_pct: item.effective_pct,
                    coverage: item.coverage.clone(),
                },
            );
        }
    }
    for project in projects.values_mut() {
        project.saved_tokens = estimate_saved_tokens(project.saved_chars);
        project.saved_pct = saved_pct(project.saved_chars, project.original_chars);
    }
    let mut projects: Vec<ProjectRollup> = projects.into_values().collect();
    projects.sort_by(|a, b| {
        b.saved_tokens
            .cmp(&a.saved_tokens)
            .then_with(|| b.saved_chars.cmp(&a.saved_chars))
            .then_with(|| a.project.cmp(&b.project))
    });
    projects
}

fn memory_savings_by_project(
    conn: &rusqlite::Connection,
    project: Option<&str>,
    window: DashboardWindow,
    notes: &mut Vec<String>,
) -> (bool, Vec<SavingsProject>) {
    let mut stmt = match conn.prepare(
        "SELECT project, COUNT(*) AS row_count, \
                COALESCE(SUM(CASE WHEN body_full IS NOT NULL AND LENGTH(body_full) > LENGTH(body) THEN 1 ELSE 0 END), 0) AS reduced_count, \
                COALESCE(SUM(CASE WHEN body_full IS NOT NULL AND LENGTH(body_full) > LENGTH(body) THEN LENGTH(body_full) - LENGTH(body) ELSE 0 END), 0) AS saved_chars, \
                COALESCE(SUM(CASE WHEN body_full IS NOT NULL THEN LENGTH(body_full) ELSE LENGTH(body) END), 0) AS original_chars, \
                COALESCE(SUM(LENGTH(body)), 0) AS with_rtrt_chars, \
                COALESCE(SUM(CASE WHEN body_full IS NOT NULL AND LENGTH(body_full) > LENGTH(body) AND LENGTH(body_full) > 0 THEN ((LENGTH(body_full) - LENGTH(body)) * 100.0 / LENGTH(body_full)) ELSE 0 END), 0.0) AS effective_pct_sum \
         FROM memories \
         WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR created_at >= ?2) \
         GROUP BY project",
    ) {
        Ok(stmt) => stmt,
        Err(e) => {
            notes.push(format!("Memory savings query unavailable: {e}."));
            return (false, Vec::new());
        }
    };
    let rows = match stmt.query_map(rusqlite::params![project, window.since_unix], |row| {
        let project = normalize_project(row.get::<_, Option<String>>(0)?);
        let count = row.get::<_, i64>(1)?;
        let reduced_count = nonnegative_i64(row.get::<_, i64>(2)?);
        let saved_chars = nonnegative_i64(row.get::<_, i64>(3)?);
        let original_chars = nonnegative_i64(row.get::<_, i64>(4)?);
        let with_rtrt_chars = nonnegative_i64(row.get::<_, i64>(5)?);
        let effective_pct_sum = row.get::<_, f64>(6)?.max(0.0);
        Ok(SavingsProject {
            project,
            count,
            reduced_count,
            saved_chars,
            saved_tokens: estimate_saved_tokens(saved_chars),
            original_chars,
            with_rtrt_chars,
            saved_pct: saved_pct(saved_chars, original_chars),
            effective_pct: (reduced_count > 0).then_some(effective_pct_sum / reduced_count as f64),
            coverage: SavingsCoverage {
                reduced: reduced_count,
                total: count,
            },
            effective_pct_sum,
        })
    }) {
        Ok(rows) => rows,
        Err(e) => {
            notes.push(format!("Memory savings query failed: {e}."));
            return (false, Vec::new());
        }
    };
    let mut projects = Vec::new();
    for row in rows {
        match row {
            Ok(project) => projects.push(project),
            Err(e) => {
                notes.push(format!("Memory savings row skipped: {e}."));
                return (false, projects);
            }
        }
    }
    (true, projects)
}

fn output_savings_by_project(
    conn: &rusqlite::Connection,
    project: Option<&str>,
    window: DashboardWindow,
    notes: &mut Vec<String>,
) -> (bool, Vec<SavingsProject>) {
    let mut stmt = match conn.prepare(
        "SELECT project, metadata FROM memories \
         WHERE metadata IS NOT NULL AND (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR created_at >= ?2)",
    ) {
        Ok(stmt) => stmt,
        Err(e) => {
            notes.push(format!("Output Optimizer query unavailable: {e}."));
            return (false, Vec::new());
        }
    };
    let rows = match stmt.query_map(rusqlite::params![project, window.since_unix], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            notes.push(format!("Output Optimizer query failed: {e}."));
            return (false, Vec::new());
        }
    };
    let mut grouped: BTreeMap<String, (i64, i64, i64, i64, i64, f64)> = BTreeMap::new();
    for row in rows {
        match row {
            Ok((project, raw)) => {
                if let Some((saved_chars, original_chars, with_rtrt_chars)) =
                    metadata_savings(&raw, "compress")
                {
                    let entry = grouped
                        .entry(normalize_project(project))
                        .or_insert((0, 0, 0, 0, 0, 0.0));
                    entry.0 += 1;
                    entry.1 += saved_chars;
                    entry.2 += original_chars;
                    entry.3 += with_rtrt_chars;
                    if saved_chars > 0 && original_chars > 0 {
                        entry.4 += 1;
                        entry.5 += saved_chars as f64 / original_chars as f64 * 100.0;
                    }
                }
            }
            Err(e) => {
                notes.push(format!("Output Optimizer row skipped: {e}."));
                return (false, Vec::new());
            }
        }
    }
    let projects = grouped
        .into_iter()
        .map(
            |(
                project,
                (
                    count,
                    saved_chars,
                    original_chars,
                    with_rtrt_chars,
                    reduced_count,
                    effective_pct_sum,
                ),
            )| {
                let saved_chars = nonnegative_i64(saved_chars);
                let original_chars = nonnegative_i64(original_chars);
                let with_rtrt_chars = nonnegative_i64(with_rtrt_chars);
                let reduced_count = nonnegative_i64(reduced_count);
                SavingsProject {
                    project,
                    count,
                    reduced_count,
                    saved_chars,
                    saved_tokens: estimate_saved_tokens(saved_chars),
                    original_chars,
                    with_rtrt_chars,
                    saved_pct: saved_pct(saved_chars, original_chars),
                    effective_pct: (reduced_count > 0)
                        .then_some(effective_pct_sum / reduced_count as f64),
                    coverage: SavingsCoverage {
                        reduced: reduced_count,
                        total: count,
                    },
                    effective_pct_sum,
                }
            },
        )
        .collect();
    (true, projects)
}

fn command_savings_by_project(
    db_path: &std::path::Path,
    project: Option<&str>,
    window: DashboardWindow,
    notes: &mut Vec<String>,
) -> (bool, Vec<SavingsProject>) {
    if !db_path.exists() {
        notes.push(format!(
            "Command Optimizer database not found at {}.",
            db_path.display()
        ));
        return (false, Vec::new());
    }
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(e) => {
            notes.push(format!("Command Optimizer database unavailable: {e}."));
            return (false, Vec::new());
        }
    };
    let mut stmt = match conn.prepare(
        "SELECT project, COUNT(*) AS run_count, COALESCE(SUM(saved_chars), 0) AS saved_chars, \
                COALESCE(SUM(input_chars), 0) AS original_chars, \
                COALESCE(SUM(output_chars), 0) AS with_rtrt_chars, \
                COALESCE(SUM(CASE WHEN saved_chars > 0 THEN 1 ELSE 0 END), 0) AS reduced_count, \
                COALESCE(SUM(CASE WHEN saved_chars > 0 AND input_chars > 0 THEN saved_chars * 100.0 / input_chars ELSE 0 END), 0.0) AS effective_pct_sum \
         FROM proxy_runs \
         WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR datetime(ts) >= datetime('now', ?2)) \
         GROUP BY project",
    ) {
        Ok(stmt) => stmt,
        Err(e) => {
            notes.push(format!("Command Optimizer stats unavailable: {e}."));
            return (false, Vec::new());
        }
    };
    let rows = match stmt.query_map(rusqlite::params![project, window.proxy_modifier], |row| {
        let project = normalize_project(row.get::<_, Option<String>>(0)?);
        let count = row.get::<_, i64>(1)?;
        let saved_chars = nonnegative_i64(row.get::<_, i64>(2)?);
        let original_chars = nonnegative_i64(row.get::<_, i64>(3)?);
        let with_rtrt_chars = nonnegative_i64(row.get::<_, i64>(4)?);
        let reduced_count = nonnegative_i64(row.get::<_, i64>(5)?);
        let effective_pct_sum = row.get::<_, f64>(6)?.max(0.0);
        Ok(SavingsProject {
            project,
            count,
            reduced_count,
            saved_chars,
            saved_tokens: estimate_saved_tokens(saved_chars),
            original_chars,
            with_rtrt_chars,
            saved_pct: saved_pct(saved_chars, original_chars),
            effective_pct: (reduced_count > 0).then_some(effective_pct_sum / reduced_count as f64),
            coverage: SavingsCoverage {
                reduced: reduced_count,
                total: count,
            },
            effective_pct_sum,
        })
    }) {
        Ok(rows) => rows,
        Err(e) => {
            notes.push(format!("Command Optimizer stats query failed: {e}."));
            return (false, Vec::new());
        }
    };
    let mut projects = Vec::new();
    for row in rows {
        match row {
            Ok(project) => projects.push(project),
            Err(e) => {
                notes.push(format!("Command Optimizer stats row skipped: {e}."));
                return (false, projects);
            }
        }
    }
    (true, projects)
}

const GAIN_MIN_ROWS: usize = 5;
const GAIN_MAX_ROWS: usize = 50;

fn integer_sqrt_ceil(n: u64) -> u64 {
    let mut root = 0u64;
    while root.saturating_mul(root) < n {
        root = root.saturating_add(1);
    }
    root
}

fn dynamic_gain_count(total_runs: i64) -> usize {
    let total = u64::try_from(total_runs.max(1)).unwrap_or(1);
    usize::try_from(integer_sqrt_ceil(total))
        .unwrap_or(GAIN_MAX_ROWS)
        .clamp(GAIN_MIN_ROWS, GAIN_MAX_ROWS)
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn nonnegative_i64(value: i64) -> i64 {
    value.max(0)
}

#[derive(Debug, Clone, Serialize)]
struct GainCommand {
    command: String,
    runs: i64,
    reduced_runs: i64,
    input_chars: i64,
    output_chars: i64,
    saved_chars: i64,
    saved_tokens: i64,
    saved_pct: f64,
    effective_pct: Option<f64>,
    coverage: SavingsCoverage,
}

#[derive(Debug, Clone, Serialize)]
struct GainProject {
    project: String,
    runs: i64,
    reduced_runs: i64,
    input_chars: i64,
    output_chars: i64,
    saved_chars: i64,
    saved_tokens: i64,
    saved_pct: f64,
    effective_pct: Option<f64>,
    coverage: SavingsCoverage,
}

#[derive(Debug, Clone, Serialize)]
struct GainRun {
    ts: String,
    project: String,
    original_cmd: String,
    mode: String,
    input_chars: i64,
    output_chars: i64,
    saved_chars: i64,
    saved_tokens: i64,
    saved_pct: f64,
    exec_ms: i64,
}

#[derive(Debug, Deserialize)]
struct GainQuery {
    project: Option<String>,
    window: Option<String>,
}

async fn gain(axum::extract::Query(q): axum::extract::Query<GainQuery>) -> Json<serde_json::Value> {
    let project = q.project.as_deref().filter(|p| !p.trim().is_empty());
    let window = DashboardWindow::parse(q.window.as_deref());
    let db_path = proxy_stats_path();
    if !db_path.exists() {
        return Json(serde_json::json!({
            "available": false,
            "reason": "database_missing",
            "path": db_path.display().to_string(),
        }));
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(e) => {
            return Json(serde_json::json!({
                "available": false,
                "reason": "database_unavailable",
                "error": e.to_string(),
                "path": db_path.display().to_string(),
            }));
        }
    };

    let (total_runs, total_input_chars, total_output_chars, total_saved_chars, reduced_runs, effective_pct_sum) = match conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(input_chars), 0), COALESCE(SUM(output_chars), 0), \
                COALESCE(SUM(saved_chars), 0), \
                COALESCE(SUM(CASE WHEN saved_chars > 0 THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN saved_chars > 0 AND input_chars > 0 THEN saved_chars * 100.0 / input_chars ELSE 0 END), 0.0) \
         FROM proxy_runs WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR datetime(ts) >= datetime('now', ?2))",
        rusqlite::params![project, window.proxy_modifier],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, f64>(5)?,
            ))
        },
    ) {
        Ok(row) => row,
        Err(e) => {
            return Json(serde_json::json!({
                "available": false,
                "reason": "table_unavailable",
                "error": e.to_string(),
                "path": db_path.display().to_string(),
            }));
        }
    };

    let total_runs = nonnegative_i64(total_runs);
    let total_input_chars = nonnegative_i64(total_input_chars);
    let total_output_chars = nonnegative_i64(total_output_chars);
    let total_saved_chars = nonnegative_i64(total_saved_chars);
    let reduced_runs = nonnegative_i64(reduced_runs);
    if total_runs == 0 {
        return Json(serde_json::json!({
            "available": false,
            "reason": "no_data_in_window",
            "scope": project.unwrap_or("all"),
            "window": window.label,
            "path": db_path.display().to_string(),
            "top_commands": [],
            "per_project": [],
            "recent_history": [],
        }));
    }
    let display_count = dynamic_gain_count(total_runs);
    let display_count_i64 = usize_to_i64(display_count);

    let top_commands = match load_gain_top_commands(&conn, project, window, display_count_i64) {
        Ok(rows) => rows,
        Err(e) => {
            return Json(serde_json::json!({
                "available": false,
                "reason": "top_commands_unavailable",
                "error": e.to_string(),
                "path": db_path.display().to_string(),
            }));
        }
    };
    let per_project = match load_gain_projects(&conn, project, window) {
        Ok(rows) => rows,
        Err(e) => {
            return Json(serde_json::json!({
                "available": false,
                "reason": "projects_unavailable",
                "error": e.to_string(),
                "path": db_path.display().to_string(),
            }));
        }
    };
    let recent_history = match load_gain_recent(&conn, project, window, display_count_i64) {
        Ok(rows) => rows,
        Err(e) => {
            return Json(serde_json::json!({
                "available": false,
                "reason": "history_unavailable",
                "error": e.to_string(),
                "path": db_path.display().to_string(),
            }));
        }
    };

    Json(serde_json::json!({
        "available": true,
        "scope": project.unwrap_or("all"),
        "window": window.label,
        "path": db_path.display().to_string(),
        "total_runs": total_runs,
        "reduced_runs": reduced_runs,
        "input_chars": total_input_chars,
        "total_input_chars": total_input_chars,
        "output_chars": total_output_chars,
        "total_output_chars": total_output_chars,
        "total_saved_chars": total_saved_chars,
        "total_saved_tokens": estimate_saved_tokens(total_saved_chars),
        "saved_pct": saved_pct_or_zero(total_saved_chars, total_input_chars),
        "effective_pct": (reduced_runs > 0).then_some(effective_pct_sum / reduced_runs as f64),
        "coverage": SavingsCoverage { reduced: reduced_runs, total: total_runs },
        "token_estimate": "chars/4",
        "display_count": display_count,
        "top_commands": top_commands,
        "per_project": per_project,
        "recent_history": recent_history,
    }))
}

fn load_gain_top_commands(
    conn: &rusqlite::Connection,
    project: Option<&str>,
    window: DashboardWindow,
    limit: i64,
) -> rusqlite::Result<Vec<GainCommand>> {
    let mut stmt = conn.prepare(
        "SELECT original_cmd, COUNT(*), COALESCE(SUM(input_chars), 0), COALESCE(SUM(output_chars), 0), \
                COALESCE(SUM(saved_chars), 0), \
                COALESCE(SUM(CASE WHEN saved_chars > 0 THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN saved_chars > 0 AND input_chars > 0 THEN saved_chars * 100.0 / input_chars ELSE 0 END), 0.0) \
         FROM proxy_runs \
         WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR datetime(ts) >= datetime('now', ?2)) \
         GROUP BY original_cmd \
         ORDER BY COALESCE(SUM(saved_chars), 0) DESC, COUNT(*) DESC, original_cmd ASC \
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![project, window.proxy_modifier, limit],
        |row| {
            let input_chars = nonnegative_i64(row.get::<_, i64>(2)?);
            let output_chars = nonnegative_i64(row.get::<_, i64>(3)?);
            let saved_chars = nonnegative_i64(row.get::<_, i64>(4)?);
            let reduced_runs = nonnegative_i64(row.get::<_, i64>(5)?);
            let effective_pct_sum = row.get::<_, f64>(6)?.max(0.0);
            let pct = saved_pct_or_zero(saved_chars, input_chars);
            let runs = nonnegative_i64(row.get::<_, i64>(1)?);
            Ok(GainCommand {
                command: row.get(0)?,
                runs,
                reduced_runs,
                input_chars,
                output_chars,
                saved_chars,
                saved_tokens: estimate_saved_tokens(saved_chars),
                saved_pct: pct,
                effective_pct: (reduced_runs > 0)
                    .then_some(effective_pct_sum / reduced_runs as f64),
                coverage: SavingsCoverage {
                    reduced: reduced_runs,
                    total: runs,
                },
            })
        },
    )?;
    rows.collect()
}

fn load_gain_projects(
    conn: &rusqlite::Connection,
    project: Option<&str>,
    window: DashboardWindow,
) -> rusqlite::Result<Vec<GainProject>> {
    let mut stmt = conn.prepare(
        "SELECT project, COUNT(*), COALESCE(SUM(input_chars), 0), COALESCE(SUM(output_chars), 0), \
                COALESCE(SUM(saved_chars), 0), \
                COALESCE(SUM(CASE WHEN saved_chars > 0 THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN saved_chars > 0 AND input_chars > 0 THEN saved_chars * 100.0 / input_chars ELSE 0 END), 0.0) \
         FROM proxy_runs \
         WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR datetime(ts) >= datetime('now', ?2)) \
         GROUP BY project \
         ORDER BY COALESCE(SUM(saved_chars), 0) DESC, COUNT(*) DESC, project ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![project, window.proxy_modifier], |row| {
        let input_chars = nonnegative_i64(row.get::<_, i64>(2)?);
        let output_chars = nonnegative_i64(row.get::<_, i64>(3)?);
        let saved_chars = nonnegative_i64(row.get::<_, i64>(4)?);
        let reduced_runs = nonnegative_i64(row.get::<_, i64>(5)?);
        let effective_pct_sum = row.get::<_, f64>(6)?.max(0.0);
        let runs = nonnegative_i64(row.get::<_, i64>(1)?);
        Ok(GainProject {
            project: normalize_project(row.get::<_, Option<String>>(0)?),
            runs,
            reduced_runs,
            input_chars,
            output_chars,
            saved_chars,
            saved_tokens: estimate_saved_tokens(saved_chars),
            saved_pct: saved_pct_or_zero(saved_chars, input_chars),
            effective_pct: (reduced_runs > 0).then_some(effective_pct_sum / reduced_runs as f64),
            coverage: SavingsCoverage {
                reduced: reduced_runs,
                total: runs,
            },
        })
    })?;
    rows.collect()
}

fn load_gain_recent(
    conn: &rusqlite::Connection,
    project: Option<&str>,
    window: DashboardWindow,
    limit: i64,
) -> rusqlite::Result<Vec<GainRun>> {
    let mut stmt = conn.prepare(
        "SELECT ts, project, original_cmd, mode, input_chars, output_chars, saved_chars, saved_pct, exec_ms \
         FROM proxy_runs \
         WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR datetime(ts) >= datetime('now', ?2)) \
         ORDER BY id DESC \
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![project, window.proxy_modifier, limit],
        |row| {
            let input_chars = nonnegative_i64(row.get::<_, i64>(4)?);
            let saved_chars = nonnegative_i64(row.get::<_, i64>(6)?);
            Ok(GainRun {
                ts: row.get(0)?,
                project: normalize_project(row.get::<_, Option<String>>(1)?),
                original_cmd: row.get(2)?,
                mode: row.get(3)?,
                input_chars,
                output_chars: nonnegative_i64(row.get::<_, i64>(5)?),
                saved_chars,
                saved_tokens: estimate_saved_tokens(saved_chars),
                saved_pct: saved_pct_or_zero(saved_chars, input_chars),
                exec_ms: nonnegative_i64(row.get::<_, i64>(8)?),
            })
        },
    )?;
    rows.collect()
}

#[derive(Debug, Deserialize)]
struct OverviewQuery {
    project: Option<String>,
    window: Option<String>,
}

async fn optimizer_overview(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<OverviewQuery>,
) -> Json<serde_json::Value> {
    let project = q.project.as_deref().filter(|p| !p.is_empty());
    let window = DashboardWindow::parse(q.window.as_deref());
    let mut notes = Vec::new();

    let (memory_available, memory_by_project, output_available, output_by_project) =
        if !state.memory_path.exists() {
            notes.push(format!(
                "Memory database not found at {}.",
                state.memory_path.display()
            ));
            (false, Vec::new(), false, Vec::new())
        } else {
            match rusqlite::Connection::open(&state.memory_path) {
                Ok(conn) => {
                    let (memory_available, memory_by_project) =
                        memory_savings_by_project(&conn, project, window, &mut notes);
                    let (output_available, output_by_project) =
                        output_savings_by_project(&conn, project, window, &mut notes);
                    (
                        memory_available,
                        memory_by_project,
                        output_available,
                        output_by_project,
                    )
                }
                Err(e) => {
                    notes.push(format!("Memory database unavailable: {e}."));
                    (false, Vec::new(), false, Vec::new())
                }
            }
        };

    let proxy_path = proxy_stats_path();
    let (command_available, command_by_project) =
        command_savings_by_project(&proxy_path, project, window, &mut notes);

    let output_level = read_output_style_level().as_str().to_string();
    let mut output_source = savings_source(
        "output_optimizer",
        "Output Optimizer",
        "memories metadata source=compress saved_chars",
        output_available,
        output_by_project,
    );
    output_source.level = Some(output_level.clone());
    output_source.active = Some(output_optimizer_active(&output_level));
    output_source.measurement_note = Some(OUTPUT_OPTIMIZER_MEASUREMENT_NOTE.to_string());

    let sources = vec![
        savings_source(
            "memory",
            "Memory",
            "SUM(length(body_full) - length(body)) from memories",
            memory_available,
            memory_by_project,
        ),
        output_source,
        savings_source(
            "command_optimizer",
            "Command Optimizer",
            "~/.rtrt/proxy-stats.sqlite proxy_runs saved_chars",
            command_available,
            command_by_project,
        ),
    ];
    let total_saved_chars: i64 = sources.iter().map(|source| source.saved_chars).sum();
    let total_original_chars: i64 = sources.iter().map(|source| source.original_chars).sum();
    let total_with_rtrt_chars: i64 = sources.iter().map(|source| source.with_rtrt_chars).sum();
    let total_saved_tokens = estimate_saved_tokens(total_saved_chars);
    let projects = projects_rollup(&sources);
    let available = sources.iter().any(|source| source.available);

    Json(serde_json::json!({
        "available": available,
        "scope": project.unwrap_or("all"),
        "window": window.label,
        "total_saved_chars": total_saved_chars,
        "total_saved_tokens": total_saved_tokens,
        "total_original_chars": total_original_chars,
        "total_with_rtrt_chars": total_with_rtrt_chars,
        "total_saved_pct": saved_pct(total_saved_chars, total_original_chars),
        "token_estimate": "chars/4",
        "sources": sources,
        "projects": projects,
        "note": notes.join(" "),
    }))
}

async fn list_templates() -> Json<Vec<TemplateSummary>> {
    Json(
        rtrt_templates::list_all()
            .into_iter()
            .map(TemplateSummary::from)
            .collect(),
    )
}

async fn get_template(
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<rtrt_templates::Template>, (StatusCode, String)> {
    rtrt_templates::find(&name)
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("template not found: {name}")))
}

async fn create_template(
    Json(req): Json<TemplateUpsertRequest>,
) -> std::result::Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    validate_template_name(&req.name)?;
    if is_builtin_template(&req.name) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("built-in template is read-only: {}", req.name),
        ));
    }
    let template = req.try_into_template()?;
    let path =
        rtrt_templates::custom::save_custom(&template).map_err(template_write_error_status)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "ok": true, "name": template.name, "path": path })),
    ))
}

async fn update_template(
    AxPath(name): AxPath<String>,
    Json(req): Json<TemplateUpsertRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    validate_template_name(&name)?;
    validate_template_name(&req.name)?;
    if req.name != name {
        return Err((
            StatusCode::BAD_REQUEST,
            "template body name must match route name".to_string(),
        ));
    }
    if is_builtin_template(&name) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("built-in template is read-only: {name}"),
        ));
    }
    let template = req.try_into_template()?;
    let path =
        rtrt_templates::custom::save_custom(&template).map_err(template_write_error_status)?;
    Ok(Json(
        serde_json::json!({ "ok": true, "name": template.name, "path": path }),
    ))
}

async fn delete_template(
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    validate_template_name(&name)?;
    if is_builtin_template(&name) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("built-in template is read-only: {name}"),
        ));
    }
    if !rtrt_templates::custom::is_custom(&name) {
        return Err((StatusCode::NOT_FOUND, format!("template not found: {name}")));
    }
    rtrt_templates::custom::delete_custom(&name).map_err(template_write_error_status)?;
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

async fn security_profiles() -> Json<Vec<String>> {
    Json(rtrt_security::list_profiles())
}

async fn security_profile(
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<Profile>, (StatusCode, String)> {
    rtrt_security::load_profile(&name)
        .map(Json)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(Debug, Deserialize)]
struct SecurityScanRequest {
    profile: String,
    #[serde(default)]
    path: Option<String>,
}

async fn security_scan(
    Json(req): Json<SecurityScanRequest>,
) -> std::result::Result<Json<ScanReport>, (StatusCode, String)> {
    let profile = rtrt_security::load_profile(&req.profile)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let path = req.path.unwrap_or_else(|| ".".to_string());
    rtrt_security::run(&profile, std::path::Path::new(&path))
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Debug, Deserialize)]
struct ProfileSaveReq {
    name: String,
    toml: String,
}

/// `POST /api/security/profile` — validate and persist a profile to the user
/// profile directory. Powers profile clone/edit-save in the UI.
async fn security_profile_save(
    Json(req): Json<ProfileSaveReq>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Validate the TOML by parsing it into a Profile first.
    Profile::from_toml(&req.toml).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let dir = rtrt_security::user_profile_dir().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "cannot determine profile directory".to_string(),
    ))?;
    std::fs::create_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create dir {}: {e}", dir.display()),
        )
    })?;
    let path = dir.join(format!("{}.toml", req.name));
    std::fs::write(&path, &req.toml).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Debug, Clone, Serialize)]
struct ProjectView {
    name: String,
    path: Option<String>,
    security_profile: Option<String>,
    /// Per-project embedding override (`None` = inherit global default).
    embeddings_enabled: Option<bool>,
    mem_count: usize,
}

/// `GET /api/projects` — union of registered config entries (path /
/// security_profile) and memory buckets (mem_count), merged by name.
async fn list_projects(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<ProjectView>>, (StatusCode, String)> {
    use std::collections::BTreeMap;

    let mut views: BTreeMap<String, ProjectView> = BTreeMap::new();

    // Registered config entries first.
    let cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    for entry in &cfg.projects {
        views.insert(
            entry.name.clone(),
            ProjectView {
                name: entry.name.clone(),
                path: entry.path.clone(),
                security_profile: entry.security_profile.clone(),
                embeddings_enabled: entry.embeddings_enabled,
                mem_count: 0,
            },
        );
    }

    // Memory buckets contribute counts (and may introduce memory-only names).
    if let Some(mem) = &state.memory {
        let guard = mem.lock().await;
        let projects = guard
            .projects()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        drop(guard);
        for (name, count, _last) in projects {
            // No name-pattern filtering: a row's project is decided by the
            // reattribution pass (transcript parent cwd), which folds stray
            // subagent / workflow captures under their real project, leaving
            // those buckets empty so they don't appear here at all.
            views
                .entry(name.clone())
                .and_modify(|v| v.mem_count = count)
                .or_insert(ProjectView {
                    name,
                    path: None,
                    security_profile: None,
                    embeddings_enabled: None,
                    mem_count: count,
                });
        }
    }

    // BTreeMap iteration is already sorted by name.
    Ok(Json(views.into_values().collect()))
}

#[derive(Debug, Deserialize)]
struct ProjectUpsertReq {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    security_profile: Option<String>,
    /// Per-project embedding override as a tri-state string so the handler can
    /// tell "field absent" (preserve existing) from an explicit choice:
    /// `"on"` -> Some(true), `"off"` -> Some(false), `"inherit"` -> None.
    /// (A bare `Option<bool>`/`Option<Option<bool>>` can't distinguish absent
    /// from JSON `null` — serde maps both to `None`.)
    #[serde(default)]
    embeddings_mode: Option<String>,
}

/// `PUT /api/projects` — upsert a project entry into the config registry.
async fn upsert_project(
    Json(req): Json<ProjectUpsertReq>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let name = req.name.clone();
    // Explicit choice wins; an absent field preserves the existing override.
    let embeddings_enabled = match req.embeddings_mode.as_deref() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some("inherit") => None,
        _ => cfg.project(&name).and_then(|p| p.embeddings_enabled),
    };
    cfg.upsert_project(rtrt_core::ProjectEntry {
        name: req.name,
        path: req.path,
        security_profile: req.security_profile,
        embeddings_enabled,
    });

    let path = rtrt_core::Config::default_path().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "cannot determine config path".to_string(),
    ))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create dir {}: {e}", parent.display()),
            )
        })?;
    }
    let s = toml::to_string_pretty(&cfg)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&path, s).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

#[derive(Debug, Clone, Serialize)]
struct TemplateSummary {
    name: String,
    description: String,
    source: rtrt_templates::TemplateSource,
    category: rtrt_templates::TemplateCategory,
    variables: Vec<rtrt_templates::TemplateVariable>,
}

impl From<rtrt_templates::Template> for TemplateSummary {
    fn from(t: rtrt_templates::Template) -> Self {
        Self {
            name: t.name,
            description: t.description,
            source: t.source,
            category: t.category,
            variables: t.variables,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TemplateUpsertRequest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_template_category")]
    category: String,
    #[serde(default)]
    variables: Vec<rtrt_templates::TemplateVariable>,
    #[serde(default)]
    files: Vec<rtrt_templates::TemplateFile>,
    #[serde(default)]
    post_hooks: Vec<String>,
}

impl TemplateUpsertRequest {
    fn try_into_template(
        self,
    ) -> std::result::Result<rtrt_templates::Template, (StatusCode, String)> {
        let template = rtrt_templates::Template {
            name: self.name,
            description: self.description,
            source: rtrt_templates::TemplateSource::Custom,
            category: parse_template_category(&self.category)?,
            variables: self.variables,
            files: self.files,
            post_hooks: self.post_hooks,
        };
        validate_template_manifest(&template)?;
        Ok(template)
    }
}

fn default_template_category() -> String {
    rtrt_templates::TemplateCategory::Development
        .as_str()
        .to_string()
}

fn parse_template_category(
    raw: &str,
) -> std::result::Result<rtrt_templates::TemplateCategory, (StatusCode, String)> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "development" | "scaffold" | "project" | "project-template" | "project-templates"
        | "rust-cli" | "rust-lib" | "rust-axum" | "node-typescript" | "python-uv" | "go-cli"
        | "custom" => Ok(rtrt_templates::TemplateCategory::Development),
        "design" => Ok(rtrt_templates::TemplateCategory::Design),
        "planning" | "doc-chain" | "document-chain" | "document-chains" => {
            Ok(rtrt_templates::TemplateCategory::Planning)
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("unknown template category: {raw}"),
        )),
    }
}

fn is_builtin_template(name: &str) -> bool {
    rtrt_templates::builtin::ALL
        .iter()
        .any(|template| template.name == name)
}

fn validate_template_name(name: &str) -> std::result::Result<(), (StatusCode, String)> {
    if is_safe_template_slug(name) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "template name must be a non-empty lowercase slug using letters, numbers, hyphens, or underscores"
                .to_string(),
        ))
    }
}

fn is_safe_template_slug(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('.')
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn validate_template_manifest(
    template: &rtrt_templates::Template,
) -> std::result::Result<(), (StatusCode, String)> {
    validate_template_name(&template.name)?;
    for variable in &template.variables {
        if variable.name.is_empty() || variable.name.chars().any(char::is_whitespace) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "template variable name must be non-empty and contain no spaces: {}",
                    variable.name
                ),
            ));
        }
    }
    for file in &template.files {
        if !is_safe_template_file_path(&file.path) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "template file path must be relative and stay inside the scaffold target: {}",
                    file.path
                ),
            ));
        }
    }
    Ok(())
}

fn is_safe_template_file_path(path: &str) -> bool {
    let path_ref = std::path::Path::new(path);
    !path.is_empty()
        && !path.contains('\\')
        && path_ref
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn template_write_error_status(err: rtrt_core::Error) -> (StatusCode, String) {
    match err {
        rtrt_core::Error::Config(message) => (StatusCode::BAD_REQUEST, message),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct ScaffoldRequest {
    template: String,
    target: PathBuf,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    manifest: Option<TemplateUpsertRequest>,
}

#[derive(Debug, Serialize)]
struct ScaffoldResponse {
    files_written: usize,
    root: PathBuf,
    post_hooks: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ScaffoldPreviewResponse {
    root: PathBuf,
    files: Vec<ScaffoldPreviewFile>,
    post_hooks: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ScaffoldPreviewFile {
    path: PathBuf,
    bytes: usize,
    executable: bool,
}

async fn scaffold_preview(
    Json(req): Json<ScaffoldRequest>,
) -> std::result::Result<Json<ScaffoldPreviewResponse>, (StatusCode, String)> {
    let tmpl = if let Some(manifest) = req.manifest {
        manifest.try_into_template()?
    } else {
        rtrt_templates::find(&req.template).ok_or((
            StatusCode::NOT_FOUND,
            format!("template not found: {}", req.template),
        ))?
    };
    let plan = rtrt_templates::render::plan(&tmpl, &req.target, req.variables)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let files = plan
        .files
        .iter()
        .map(|f| ScaffoldPreviewFile {
            path: f.path.clone(),
            bytes: f.content.len(),
            executable: f.executable,
        })
        .collect();
    Ok(Json(ScaffoldPreviewResponse {
        root: plan.root,
        files,
        post_hooks: plan.post_hooks,
    }))
}

async fn scaffold(
    Json(req): Json<ScaffoldRequest>,
) -> std::result::Result<Json<ScaffoldResponse>, (StatusCode, String)> {
    let tmpl = rtrt_templates::find(&req.template).ok_or((
        StatusCode::NOT_FOUND,
        format!("template not found: {}", req.template),
    ))?;
    let plan = rtrt_templates::render::plan(&tmpl, &req.target, req.variables)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    rtrt_templates::render::write(&plan, req.overwrite)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ScaffoldResponse {
        files_written: plan.files.len(),
        root: plan.root,
        post_hooks: plan.post_hooks,
    }))
}

#[derive(Debug, Deserialize)]
struct ChatHttpRequest {
    model: String,
    messages: Vec<ChatHttpMessage>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct ChatHttpMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatHttpResponse {
    provider: String,
    model: String,
    content: String,
    input_tokens: u64,
    output_tokens: u64,
}

async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatHttpRequest>,
) -> std::result::Result<Json<ChatHttpResponse>, (StatusCode, String)> {
    let messages = req
        .messages
        .into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            ChatMessage {
                role,
                content: m.content,
            }
        })
        .collect();
    let chat_req = ChatRequest {
        model: req.model,
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
    };
    let resp = state
        .gateway
        .chat(chat_req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "chat".into());
    meta.insert("provider".into(), resp.provider.clone());
    meta.insert("model".into(), resp.model.clone());
    meta.insert("input_tokens".into(), resp.usage.input_tokens.to_string());
    meta.insert("output_tokens".into(), resp.usage.output_tokens.to_string());
    state.capture("chat", &resp.content, &meta).await;
    Ok(Json(ChatHttpResponse {
        provider: resp.provider,
        model: resp.model,
        content: resp.content,
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
    }))
}

#[derive(Debug, Serialize)]
struct MetricsResponse {
    summary: rtrt_providers::GatewaySummary,
    by_provider: BTreeMap<String, rtrt_providers::GatewaySummary>,
    recent: Vec<RequestMetric>,
}

fn memory_store_path() -> PathBuf {
    std::env::var("RTRT_MEMORY_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".rtrt/memory.sqlite"))
}

fn open_memory_store() -> Option<Arc<Mutex<MemoryStore>>> {
    let path = memory_store_path();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    match MemoryStore::open(&path) {
        Ok(store) => Some(Arc::new(Mutex::new(store))),
        Err(e) => {
            tracing::warn!(?path, "memory store unavailable: {e}");
            None
        }
    }
}

#[derive(Debug, Deserialize)]
struct MemoryRecallRequest {
    project: String,
    query: String,
    #[serde(default = "default_recall_limit")]
    limit: u32,
    #[serde(default)]
    filter: Option<String>,
    /// `bm25` (default) — plain BM25; `hybrid` — BM25 + graph-neighbour blend.
    /// True dense-vector hybrid requires the `embeddings` feature; this path
    /// uses `recall_bm25_graph_blend` which needs no separate model.
    /// TODO: wire fastembed / Ollama bge-m3 for full vector hybrid.
    #[serde(default)]
    mode: Option<String>,
}

fn default_recall_limit() -> u32 {
    10
}

async fn memory_recall(
    State(state): State<AppState>,
    Json(req): Json<MemoryRecallRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;

    let mode = req.mode.as_deref().unwrap_or("bm25");

    match mode {
        "hybrid" => {
            // When an Ollama embedder is wired, run true BM25 + dense-vector
            // RRF. Otherwise fall back to the graph-blended BM25 path so the
            // endpoint stays usable without an embedding server.
            let (scored, effective_mode) = if let Some(emb) = state.embedder.as_ref() {
                let s = guard
                    .recall_hybrid(&req.project, &req.query, req.limit as usize, emb.as_ref())
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                (s, "hybrid-vector")
            } else {
                // No embedder available — use graph-blended BM25 as a
                // graceful degradation.
                let s = guard
                    .recall_bm25_graph_blend(&req.project, &req.query, req.limit as usize)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                (s, "hybrid-graph")
            };
            let hits: Vec<serde_json::Value> = scored
                .into_iter()
                .map(|sr| {
                    serde_json::json!({
                        "id": sr.record.id,
                        "project": sr.record.project,
                        "kind": sr.record.kind,
                        "body": sr.record.body,
                        "created_at": sr.record.created_at,
                        "scope": sr.record.scope,
                        "score": sr.score,
                    })
                })
                .collect();
            Ok(Json(
                serde_json::json!({ "hits": hits, "mode": effective_mode }),
            ))
        }
        // "bm25" or any unrecognised value falls through to plain BM25.
        _ => {
            let hits = match req.filter.as_deref() {
                Some(spec) if !spec.is_empty() => {
                    let f = PayloadFilter::parse(spec)
                        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
                    guard
                        .recall_bm25_with_filter(&req.project, &req.query, req.limit as usize, &f)
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                }
                _ => guard
                    .recall_bm25(&req.project, &req.query, req.limit as usize)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
            };
            Ok(Json(serde_json::json!({ "hits": hits, "mode": "bm25" })))
        }
    }
}

async fn memory_projects(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let rows = guard
        .projects()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let projects: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(name, count, latest)| {
            serde_json::json!({ "project": name, "count": count, "latest_ts": latest })
        })
        .collect();
    Ok(Json(serde_json::json!({ "projects": projects })))
}

#[derive(Debug, Deserialize)]
struct MemoryTimelineQuery {
    project: String,
    #[serde(default = "default_timeline_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    /// `recent` (default) — newest first; `importance` — deterministic score descending.
    #[serde(default)]
    sort: Option<String>,
    /// Restrict to a `source_kind` (`main` / `subagent`). Absent = all rows.
    #[serde(default)]
    source_kind: Option<String>,
}

fn default_timeline_limit() -> usize {
    50
}

async fn memory_timeline(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryTimelineQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;

    let sort = q.sort.as_deref().unwrap_or("recent");
    let sk = q.source_kind.as_deref().filter(|s| !s.is_empty());
    let total = guard
        .count_by_project_filtered(&q.project, sk)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let items: Vec<serde_json::Value> = if sort == "importance" {
        // Importance sort — returns DetailedRecord which already includes
        // body_full, metadata, and a pre-computed score.
        let rows = guard
            .recent_paged_by_importance_filtered(&q.project, q.limit, q.offset, sk)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                let saved_pct = compress_saved_pct_from_meta(Some(&r.metadata));
                serde_json::json!({
                    "id": r.id,
                    "kind": r.kind,
                    "scope": r.scope,
                    "body": r.body,
                    "body_full": r.body_full,
                    "compressed": r.compressed,
                    "created_at": r.created_at,
                    "importance": r.importance,
                    "metadata": r.metadata,
                    "saved_pct": saved_pct,
                })
            })
            .collect()
    } else {
        // Default: newest-first paged view.
        let rows = guard
            .recent_paged_filtered(&q.project, q.limit, q.offset, sk)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                // `body_full` is the preserved pre-compression original (None
                // when the row was never compressed). `body` is what recall
                // uses (terse when compressed).
                let full = guard.full_body(r.id).ok().flatten();
                let compressed = full.is_some();
                let meta = guard.get_metadata(r.id).ok();
                let saved_pct = if compressed {
                    compress_saved_pct_from_meta(meta.as_ref())
                } else {
                    None
                };
                // main | subagent — lets the UI split a project's own work from
                // its subagent / teammate captures.
                let source_kind = meta.as_ref().and_then(|m| m.get("source_kind")).cloned();
                serde_json::json!({
                    "id": r.id,
                    "kind": r.kind,
                    "scope": r.scope,
                    "body": r.body,
                    "body_full": full,
                    "compressed": compressed,
                    "created_at": r.created_at,
                    "saved_pct": saved_pct,
                    "source_kind": source_kind,
                })
            })
            .collect()
    };

    Ok(Json(serde_json::json!({
        "items": items,
        "total": total,
        "limit": q.limit,
        "offset": q.offset,
        "sort": sort,
    })))
}

#[derive(Debug, Deserialize)]
struct MemoryGraphQuery {
    #[serde(default)]
    project: String,
    #[serde(default = "default_graph_limit")]
    limit: usize,
    /// `similarity` (default — memory↔memory, no LLM), `entity` (bipartite
    /// memory↔entity, requires entity extraction), or `overview` (LOD cluster
    /// bubbles for large graphs).
    #[serde(default)]
    mode: Option<String>,
    /// Top-level grouping basis for `mode=overview`: `context` (semantic,
    /// default) | `file` | `kind` | `session` | `source`.
    #[serde(default)]
    group: Option<String>,
    /// Multi-level drill token (opaque, server-minted). Takes precedence over
    /// every other param: the server resolves it to a member-id set and either
    /// re-subclusters (deeper `group` level) or returns a `leaf`.
    #[serde(default)]
    token: Option<String>,
    /// Legacy drill-down by cluster root id (kept for the old frontend path);
    /// superseded by `token`.
    #[serde(default)]
    cluster: Option<i64>,
    /// Clustering basis the user picked on the map for `group=context`:
    /// `auto` (default — coverage decides) | `vector` (force semantic/embeddings)
    /// | `lexical` (force keyword). Overrides the per-project default.
    #[serde(default)]
    basis: Option<String>,
    /// Granularity ("세밀도"): overview bubble target. More bubbles = a smaller
    /// unclustered/"미분류" catch-all. Clamped server-side. `None` = default.
    #[serde(default)]
    target: Option<usize>,
    /// Drill depth ("깊이"): leaf cutoff — a bubble at/under this many members
    /// renders individual nodes. `None`/`0` = dynamic (~√total).
    #[serde(default)]
    leaf: Option<usize>,
    /// `mode=brain` concept drill: when set, return the memories containing this
    /// concept token (the `brain-concept` response) instead of the brain graph.
    #[serde(default)]
    concept: Option<String>,
    /// `mode=brain` community drill: the stable community id (the `mode=brain`
    /// community-level node's numeric id). When set — and `concept` is not — the
    /// response is that community's CONCEPT sub-graph (`level:"concept"`) instead
    /// of the top-level community overview.
    #[serde(default)]
    community: Option<i64>,
}

fn default_graph_limit() -> usize {
    // Keep the default modest: a force-directed graph past ~80 nodes is a
    // hairball, and the BM25 fallback path costs one FTS query per node.
    80
}

async fn memory_graph(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryGraphQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;

    // ── Token drill (highest precedence) ────────────────────────────────────
    // A drill token resolves to a member-id set. Tiny sets render as a `leaf`
    // (individual memory nodes); larger sets re-subcluster into sub-bubbles
    // (each minted a fresh child token). All resolved before holding the
    // `!Send` store guard across an `.await`.
    if let Some(tok) = q.token.clone() {
        return memory_graph_drill(&state, store, tok, q.leaf).await;
    }

    // LOD drill-down (`cluster=<root>`): return the members of one cluster from
    // the cached index. Rebuilds the index if missing/expired. Checked before
    // `mode` so a drill-down request needs no extra `mode=` param. Resolved
    // before the long-lived store guard below so the `!Send` store reference is
    // never held across an `.await`. Superseded by `token`, kept for the old UI.
    if let Some(root_id) = q.cluster {
        let index = cluster_index_cached(&state, store, &q.project).await?;
        let members = {
            let guard = store.lock().await;
            guard
                .cluster_members(&q.project, root_id, &index)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        };
        let nodes: Vec<serde_json::Value> = members
            .nodes
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": format!("m{}", m.id),
                    "node_type": "memory",
                    "label": m.preview,
                    "kind": m.kind,
                    "source_kind": m.source_kind,
                })
            })
            .collect();
        let edges: Vec<serde_json::Value> = members
            .edges
            .iter()
            .map(|(a, b, w)| serde_json::json!({"src": format!("m{a}"), "dst": format!("m{b}"), "weight": w}))
            .collect();
        return Ok(Json(serde_json::json!({
            "mode": "cluster",
            "root": root_id,
            "nodes": nodes,
            "edges": edges,
        })));
    }

    // LOD overview (`mode=overview&group=<basis>`): server-side top-level
    // grouping of the whole project. Returns ONLY bubble summaries (each with a
    // drill token) + aggregated inter-cluster edges, never individual nodes —
    // this scales to hundreds of thousands of rows. `group=context` (default)
    // clusters semantically; any other basis buckets by a metadata facet.
    if q.mode.as_deref() == Some("overview") {
        let group = q.group.as_deref().unwrap_or("context");
        let basis = q.basis.as_deref().unwrap_or("auto");
        return memory_graph_overview(&state, store, &q.project, group, basis, q.target).await;
    }

    // Brain mode (`mode=brain`): the three-level "digital brain" map. Scope
    // resolves the same way the project selector does — a present, non-sentinel
    // project is one project's brain; an empty string or the `__global__`
    // sentinel is the GLOBAL brain (all projects merged by concept token). Drill
    // precedence (per contract):
    //   1. `concept=TOKEN` → that concept's MEMORIES   (`mode:"brain-concept"`)
    //   2. `community=ID`   → that community's CONCEPTS (`level:"concept"`)
    //   3. neither          → TOP-LEVEL communities     (`level:"community"`)
    if q.mode.as_deref() == Some("brain") {
        let scope: Option<&str> = brain_scope(&q.project);
        if let Some(concept) = q.concept.as_deref().filter(|c| !c.is_empty()) {
            return memory_graph_brain_concept(store, scope, concept).await;
        }
        if let Some(community_id) = q.community {
            return memory_graph_brain_community(store, scope, community_id).await;
        }
        return memory_graph_brain(&state, store, &q.project, scope).await;
    }

    let guard = store.lock().await;

    // Entity mode: bipartite memory↔entity graph (needs extracted entities).
    if q.mode.as_deref() == Some("entity") {
        let graph = guard
            .graph_bipartite(&q.project, q.limit)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut nodes: Vec<serde_json::Value> =
            Vec::with_capacity(graph.memories.len() + graph.entities.len());
        for m in &graph.memories {
            nodes.push(serde_json::json!({
                "id": format!("m{}", m.id),
                "node_type": "memory",
                "label": m.preview,
                "kind": m.kind,
                "source_kind": m.source_kind,
            }));
        }
        for e in &graph.entities {
            nodes.push(serde_json::json!({
                "id": format!("e{}", e.id),
                "node_type": "entity",
                "label": e.name,
                "degree": e.degree,
            }));
        }
        let edges: Vec<serde_json::Value> = graph
            .links
            .iter()
            .map(|(mem_id, ent_id)| serde_json::json!({"src": format!("m{mem_id}"), "dst": format!("e{ent_id}")}))
            .collect();
        return Ok(Json(serde_json::json!({
            "mode": "entity",
            "nodes": nodes,
            "edges": edges,
        })));
    }

    // Default similarity mode: memory↔memory, no generative LLM (cosine over
    // stored embeddings, or BM25 lexical fallback).
    let graph = guard
        .graph_similarity(&q.project, q.limit, 4, 0.15)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let nodes: Vec<serde_json::Value> = graph
        .memories
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": format!("m{}", m.id),
                "node_type": "memory",
                "label": m.preview,
                "kind": m.kind,
                "source_kind": m.source_kind,
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = graph
        .edges
        .iter()
        .map(|(a, b, w)| serde_json::json!({"src": format!("m{a}"), "dst": format!("m{b}"), "weight": w}))
        .collect();
    Ok(Json(serde_json::json!({
        "mode": "similarity",
        "basis": graph.basis,
        "nodes": nodes,
        "edges": edges,
    })))
}

/// LOD parameters for whole-project clustering. `max_nodes` is a safety bound
/// (newest first); `top_k` peers per node feed union-find; `min_weight` is the
/// candidate-edge threshold for joining two nodes into one cluster.
const CLUSTER_MAX_NODES: usize = 200_000;
const CLUSTER_TOP_K: usize = 4;
const CLUSTER_MIN_WEIGHT: f32 = 0.15;

/// "Digital brain" min co-occurrence (`mode=brain`): 2 just means "co-occurred
/// in at least two memories" — a property, not a cap. The concept/edge budget
/// and the community-fold target are sized dynamically by rtrt-memory (AUTO).
const BRAIN_MIN_COOCCUR: usize = 2;

/// The global-scope sentinel value the project selector sends for "all projects
/// merged into one brain" (mirrors `GLOBAL_PROJECT_VALUE` in the frontend). An
/// empty project string is treated as global too.
const GLOBAL_PROJECT_SENTINEL: &str = "__global__";

/// Map a raw `project` query param to a [`MemoryStore::concept_graph`] scope:
/// `None` (GLOBAL, all projects merged) for an empty string or the global
/// sentinel, else `Some(project)` for one project's brain.
fn brain_scope(project: &str) -> Option<&str> {
    if project.is_empty() || project == GLOBAL_PROJECT_SENTINEL {
        None
    } else {
        Some(project)
    }
}

/// TOP LEVEL of the brain (`mode=brain`, no `community`/`concept` param): build
/// (or serve from the 60s cache) the per-scope TOPIC-COMMUNITY hierarchy and
/// return the documented `level:"community"` JSON. A few dozen super-nodes
/// (`kind:"community"`) replace the hundreds-of-concepts hairball; drilling a
/// node (`community=ID`) yields its concepts. Cached in
/// [`AppState::brainh_cache`] under `"brainh\x1f<scope>"`, the same TTL + style
/// as the LOD cluster cache.
///
/// The store is `!Send`, so the build runs inside a synchronous block with no
/// `.await` while the guard is held — keeping the future `Send` for axum.
async fn memory_graph_brain(
    state: &AppState,
    store: &Arc<Mutex<MemoryStore>>,
    project: &str,
    scope: Option<&str>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let hierarchy = brain_hierarchy_cached(state, store, project, scope).await?;

    let nodes: Vec<serde_json::Value> = hierarchy
        .communities
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": format!("k:{}", c.id),
                "label": c.label,
                "size": c.size,
                "concept_count": c.concept_count,
                "top_concepts": c.top_concepts,
                "kind": "community",
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = hierarchy
        .edges
        .iter()
        .map(|(a, b, w)| {
            serde_json::json!({"src": format!("k:{a}"), "dst": format!("k:{b}"), "weight": w})
        })
        .collect();
    Ok(Json(serde_json::json!({
        "mode": "brain",
        "level": "community",
        "scope": if scope.is_none() { "global" } else { "project" },
        "total_memories": hierarchy.total_memories,
        "total_concepts": hierarchy.total_concepts,
        "nodes": nodes,
        "edges": edges,
    })))
}

/// Build (or serve from the 60s cache) the per-scope [`ConceptHierarchy`].
/// Cached in [`AppState::brainh_cache`] under `"brainh\x1f<scope>"`, the same
/// TTL + style as the LOD cluster + flat-brain caches.
///
/// The store is `!Send`, so the build runs inside a synchronous block with no
/// `.await` while the guard is held — keeping the future `Send` for axum.
async fn brain_hierarchy_cached(
    state: &AppState,
    store: &Arc<Mutex<MemoryStore>>,
    project: &str,
    scope: Option<&str>,
) -> std::result::Result<ConceptHierarchy, (StatusCode, String)> {
    let scope_key = if scope.is_none() {
        GLOBAL_PROJECT_SENTINEL
    } else {
        project
    };
    let cache_key = format!("brainh\x1f{scope_key}");

    // Fast path: serve a still-fresh cached hierarchy.
    let cached = {
        let cache = state.brainh_cache.lock().await;
        match cache.get(&cache_key) {
            Some((built_at, h)) if built_at.elapsed() < CLUSTER_INDEX_TTL => Some(h.clone()),
            _ => None,
        }
    };
    if let Some(h) = cached {
        return Ok(h);
    }

    // Miss / expired: rebuild under the store lock (no await inside).
    let hierarchy = {
        let guard = store.lock().await;
        guard
            .concept_communities(scope, BRAIN_MIN_COOCCUR)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let mut cache = state.brainh_cache.lock().await;
    cache.insert(cache_key, (std::time::Instant::now(), hierarchy.clone()));
    Ok(hierarchy)
}

/// MIDDLE LEVEL of the brain (`mode=brain&community=ID`): the concept sub-graph
/// for ONE topic community — its member concepts + the intra-community edges,
/// via [`MemoryStore::community_concepts`]. Returns the documented
/// `level:"concept"` JSON (concept nodes keyed `c:<token>`). An unknown id
/// yields an empty graph (the memory layer's contract), rendered as zero nodes.
async fn memory_graph_brain_community(
    store: &Arc<Mutex<MemoryStore>>,
    scope: Option<&str>,
    community_id: i64,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let graph = {
        let guard = store.lock().await;
        guard
            .community_concepts(scope, community_id, BRAIN_MIN_COOCCUR)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    let nodes: Vec<serde_json::Value> = graph
        .nodes
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": format!("c:{}", c.name),
                "label": c.name,
                "degree": c.degree,
                "freq": c.freq,
                "projects": c.projects,
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = graph
        .edges
        .iter()
        .map(|(a, b, w)| {
            serde_json::json!({"src": format!("c:{a}"), "dst": format!("c:{b}"), "weight": w})
        })
        .collect();
    Ok(Json(serde_json::json!({
        "mode": "brain",
        "level": "concept",
        "community": community_id,
        "nodes": nodes,
        "edges": edges,
    })))
}

/// Drill one concept: return the memories containing `concept` (newest first,
/// capped) as the documented `mode=brain-concept` JSON.
async fn memory_graph_brain_concept(
    store: &Arc<Mutex<MemoryStore>>,
    scope: Option<&str>,
    concept: &str,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Show ALL of a concept's memories — the count is naturally its frequency,
    // not an arbitrary cap. usize::MAX = "no limit".
    let rows = {
        let guard = store.lock().await;
        guard
            .concept_memories(scope, concept, usize::MAX)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let nodes: Vec<serde_json::Value> = rows
        .iter()
        .map(|(m, project)| {
            serde_json::json!({
                "id": format!("m{}", m.id),
                "node_type": "memory",
                "label": m.preview,
                "kind": m.kind,
                "source_kind": m.source_kind,
                "project": project,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "mode": "brain-concept",
        "concept": concept,
        "nodes": nodes,
        "edges": [],
    })))
}

/// Return a fresh [`ClusterIndex`] for `project`, served from the per-project
/// cache when the entry is younger than [`CLUSTER_INDEX_TTL`]; otherwise rebuild
/// it via [`MemoryStore::graph_clusters`] and refresh the cache.
///
/// The store is `!Send` (it wraps a `rusqlite::Connection`), so the build runs
/// inside a synchronous block with no `.await` while the store guard is held —
/// keeping the returned future `Send` for axum.
async fn cluster_index_cached(
    state: &AppState,
    store: &Arc<Mutex<MemoryStore>>,
    project: &str,
) -> std::result::Result<ClusterIndex, (StatusCode, String)> {
    // Fast path: serve a still-fresh cached index.
    {
        let cache = state.cluster_cache.lock().await;
        if let Some((built_at, index)) = cache.get(project)
            && built_at.elapsed() < CLUSTER_INDEX_TTL
        {
            return Ok(index.clone());
        }
    }
    // Miss / expired: rebuild under the store lock (no await inside this block).
    let index = {
        let guard = store.lock().await;
        guard
            .graph_clusters(
                project,
                CLUSTER_MAX_NODES,
                CLUSTER_TOP_K,
                CLUSTER_MIN_WEIGHT,
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let mut cache = state.cluster_cache.lock().await;
    cache.insert(
        project.to_string(),
        (std::time::Instant::now(), index.clone()),
    );
    Ok(index)
}

/// Cache-key separator for the `(project, group)` overview cache.
const CACHE_KEY_SEP: char = '\u{1f}';

/// Build (or serve cached) a top-level overview keyed by `(project, group)`.
///
/// `group=context` clusters semantically via [`MemoryStore::graph_clusters`];
/// any other valid basis (`file`/`kind`/`session`/`source`) buckets by that
/// metadata facet via [`MemoryStore::group_meta`]. Each emitted bubble is
/// minted a fresh drill token (mapping to its member ids) so the client can
/// drill arbitrarily deep without ever shipping id lists. Expired tokens (and
/// stale tokens of this same project) are pruned on every build to bound growth.
async fn memory_graph_overview(
    state: &AppState,
    store: &Arc<Mutex<MemoryStore>>,
    project: &str,
    group: &str,
    basis_pref: &str,
    target_pref: Option<usize>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    // `context` is semantic; everything else is a metadata facet. Reject an
    // unknown basis up front (group_meta also rejects, but a 400 is clearer).
    let is_context = group == "context";
    if !is_context && !matches!(group, "file" | "kind" | "session" | "source" | "time") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown group `{group}` (expected context|file|kind|session|source|time)"),
        ));
    }

    // 자동 default = per-project embedding toggle else global `[embeddings] enabled`.
    let allow_vector_default = {
        let cfg = rtrt_core::Config::load().unwrap_or_default();
        cfg.project(project)
            .and_then(|p| p.embeddings_enabled)
            .unwrap_or_else(|| cfg.embeddings.is_enabled())
    };
    // The map's 기준 selector: auto | vector(의미) | lexical(어휘).
    let basis_pref = match basis_pref {
        "vector" | "lexical" => basis_pref,
        _ => "auto",
    };
    // 세밀도(granularity): bubble target. Explicit value (clamped) wins; otherwise
    // default to a clean, airy count — Memory stays uncluttered by distilling
    // to few-but-meaningful nodes, so scale gently with project size (~sqrt(n))
    // and clamp low: a 20k project lands ~140 bubbles, a small project ~60.
    let target = match target_pref {
        Some(t) => t.clamp(24, 1000),
        None => {
            let n = {
                let guard = store.lock().await;
                guard.count_by_project(project).unwrap_or(0)
            };
            (n as f64).sqrt().round().clamp(60.0, 200.0) as usize
        }
    };

    // Key the cache on every knob so a different selection never serves a stale
    // opposite-basis / opposite-granularity index.
    let cache_key = format!(
        "{project}{CACHE_KEY_SEP}{group}{CACHE_KEY_SEP}{basis_pref}{CACHE_KEY_SEP}{target}{CACHE_KEY_SEP}{}",
        allow_vector_default as u8
    );

    // Fast path: serve a still-fresh cached index (re-mint tokens against it).
    let cached = {
        let cache = state.cluster_cache.lock().await;
        cache.get(&cache_key).and_then(|(built_at, index)| {
            (built_at.elapsed() < CLUSTER_INDEX_TTL).then(|| index.clone())
        })
    };
    let index = match cached {
        Some(idx) => idx,
        None => {
            // Miss / expired: rebuild under the store lock (no await inside).
            let idx = {
                let guard = store.lock().await;
                if is_context {
                    match basis_pref {
                        // Force semantic: cluster on vectors even at low coverage
                        // (the map then covers only the embedded rows).
                        "vector" => guard
                            .graph_clusters_vec(project, CLUSTER_MAX_NODES, CLUSTER_TOP_K, target)
                            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
                        // Force keyword.
                        "lexical" => guard
                            .graph_clusters_opt(
                                project,
                                CLUSTER_MAX_NODES,
                                CLUSTER_TOP_K,
                                CLUSTER_MIN_WEIGHT,
                                false,
                                target,
                            )
                            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
                        // Auto: coverage decides (honours the per-project toggle).
                        _ => guard
                            .graph_clusters_opt(
                                project,
                                CLUSTER_MAX_NODES,
                                CLUSTER_TOP_K,
                                CLUSTER_MIN_WEIGHT,
                                allow_vector_default,
                                target,
                            )
                            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
                    }
                } else {
                    guard
                        .group_meta(project, CLUSTER_MAX_NODES, group, target)
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                }
            };
            let mut cache = state.cluster_cache.lock().await;
            cache.insert(cache_key, (std::time::Instant::now(), idx.clone()));
            idx
        }
    };

    // Invert `node_cluster` (member -> root) into `root -> member_ids`.
    let total_nodes = index.node_cluster.len();
    let mut members_by_root: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for (&mem_id, &root) in &index.node_cluster {
        members_by_root.entry(root).or_default().push(mem_id);
    }

    // Prune expired tokens + stale tokens of this same project, then mint one
    // token per bubble.
    let clusters = {
        let mut tokens = state.level_tokens.lock().await;
        tokens.retain(|_, (built_at, entry)| {
            built_at.elapsed() < LEVEL_TOKEN_TTL && entry.project != project
        });
        index
            .clusters
            .iter()
            .map(|c| {
                let member_ids = members_by_root.remove(&c.id).unwrap_or_default();
                let token = mint_level_token();
                tokens.insert(
                    token.clone(),
                    (
                        std::time::Instant::now(),
                        TokenEntry {
                            project: project.to_string(),
                            member_ids,
                            total_n: total_nodes,
                            label: c.label.clone(),
                        },
                    ),
                );
                serde_json::json!({
                    "id": c.id,
                    "token": token,
                    "size": c.size,
                    "label": c.label,
                    "dominant_source": c.dominant_source,
                    // Every bubble opens: > leaf_cut -> sub-bubbles, else a leaf
                    // of its members. Only a size-1 bubble is a dead end.
                    "drillable": c.size > 1,
                })
            })
            .collect::<Vec<_>>()
    };

    let cluster_edges: Vec<serde_json::Value> = index
        .cluster_edges
        .iter()
        .map(|(a, b, w)| serde_json::json!({"src": a, "dst": b, "weight": w}))
        .collect();

    // Report which signal the map is actually using so the UI can badge it:
    // a context overview is "vector" (semantic, embeddings) when the project is
    // mostly embedded — matching graph_clusters' own auto-dispatch — else
    // "lexical" (keyword fallback); metadata facets are neither.
    let (embedded, total_rows) = {
        let guard = store.lock().await;
        guard.embedding_coverage(project).unwrap_or((0, 0))
    };
    let basis = if !is_context {
        "metadata"
    } else {
        match basis_pref {
            "vector" => "vector",
            "lexical" => "lexical",
            // auto: coverage decides (honours the per-project toggle).
            _ if allow_vector_default && embedded > 0 && embedded * 2 >= total_rows => "vector",
            _ => "lexical",
        }
    };

    Ok(Json(serde_json::json!({
        "mode": "overview",
        "group": group,
        "total_nodes": total_nodes,
        "clusters": clusters,
        "cluster_edges": cluster_edges,
        "basis": basis,
        "embedded": embedded,
        "total_rows": total_rows,
    })))
}

/// Resolve a drill `token` to its member-id set and render the next level.
///
/// * missing / expired token  -> `410 stale` (client refetches the overview).
/// * `ids.len() <= LEAF_THRESHOLD` -> `leaf` (individual memory nodes).
/// * otherwise re-subcluster; if the subset does not split
///   (`clusters.len() <= 1`, e.g. all highly similar) fall back to a `leaf`
///   (anti-stall guard); else emit a `group` of sub-bubbles, each minted a
///   fresh child token.
async fn memory_graph_drill(
    state: &AppState,
    store: &Arc<Mutex<MemoryStore>>,
    token: String,
    leaf_pref: Option<usize>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Resolve the token (honouring TTL) and clone out its payload.
    let entry = {
        let tokens = state.level_tokens.lock().await;
        match tokens.get(&token) {
            Some((built_at, entry)) if built_at.elapsed() < LEVEL_TOKEN_TTL => entry.clone(),
            _ => return Err((StatusCode::GONE, "stale".into())),
        }
    };
    let total_n = entry.total_n;
    let project = entry.project.clone();
    let ids = entry.member_ids;
    // 깊이(depth): an explicit leaf cutoff from the map control, else dynamic.
    // A larger cutoff bottoms out sooner (shallower); smaller drills deeper.
    let leaf_cut = leaf_pref
        .filter(|&l| l > 0)
        .map(|l| l.clamp(8, 2000))
        .unwrap_or_else(|| dynamic_leaf(total_n));

    // Leaf: small enough to render individual memory nodes.
    if ids.len() <= leaf_cut {
        return memory_graph_leaf(store, &token, &ids).await;
    }

    // Deeper level: semantic re-cluster of the subset. Branch width scales with
    // the bucket size so drill depth grows with quantity.
    let branch = dynamic_branch(ids.len());
    let idx2 = {
        let guard = store.lock().await;
        guard
            .subcluster(&ids, CLUSTER_TOP_K, CLUSTER_MIN_WEIGHT, branch)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    // Anti-stall: if the semantic split did not actually break the set up — one
    // dominant child still holds most of it (a lexically-disjoint "unclustered"
    // mass) — recursing on it would peel only a few rows per level (the 90-deep
    // pathology). Re-partition by a metadata facet (session) so the mass becomes
    // navigable sub-bubbles instead of one blob.
    let largest = idx2.clusters.iter().map(|c| c.size).max().unwrap_or(0);
    let stalled = idx2.clusters.len() <= 1 || largest as f64 >= ids.len() as f64 * STALL_DOMINANCE;
    let level = if stalled {
        // Try metadata facets in turn until one actually distributes the mass
        // (more than one bucket, no single bucket still holding ~everything).
        // session -> time(hour) -> kind. `time` almost always splits a
        // same-session lexical mass chronologically, avoiding a truncated leaf.
        let chosen = {
            let guard = store.lock().await;
            let mut found: Option<ClusterIndex> = None;
            for facet in ["session", "time", "kind"] {
                let meta = guard
                    .group_meta_ids(&ids, facet, branch)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                let ml = meta.clusters.iter().map(|c| c.size).max().unwrap_or(0);
                if meta.clusters.len() > 1 && (ml as f64) < ids.len() as f64 * 0.9 {
                    found = Some(meta);
                    break;
                }
            }
            found
        };
        // No facet helped (a truly homogeneous mass). Rather than truncate it to
        // a capped leaf (silent node loss), split it into ordinal time-ordered
        // PAGE buckets of <= leaf_cut, each of which opens to a full leaf. This
        // guarantees every memory stays reachable while the graph never has to
        // render thousands of points at once.
        match chosen {
            Some(m) => m,
            None => {
                let pages = page_buckets(&ids, leaf_cut);
                return Ok(emit_group_response(
                    state,
                    &pages,
                    &project,
                    total_n,
                    &token,
                    ids.len(),
                    true,
                )
                .await);
            }
        }
    } else {
        idx2
    };

    Ok(emit_group_response(state, &level, &project, total_n, &token, ids.len(), false).await)
}

/// Last-resort partition for a homogeneous mass no facet could split: sort by id
/// (≈ chronological) and cut into ordinal pages of `page` members each. Each page
/// is its own cluster (root = min id) so it opens to a complete leaf — no node is
/// ever dropped.
fn page_buckets(ids: &[i64], page: usize) -> ClusterIndex {
    let page = page.max(1);
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut clusters = Vec::new();
    let mut node_cluster = std::collections::HashMap::new();
    for (ci, chunk) in sorted.chunks(page).enumerate() {
        let root = *chunk.iter().min().expect("non-empty chunk");
        for &id in chunk {
            node_cluster.insert(id, root);
        }
        let start = ci * page + 1;
        clusters.push(rtrt_memory::ClusterSummary {
            id: root,
            size: chunk.len(),
            label: format!("{}–{} (시간순)", start, start + chunk.len() - 1),
            dominant_source: "mixed".to_string(),
        });
    }
    ClusterIndex {
        clusters,
        cluster_edges: Vec::new(),
        node_cluster,
    }
}

/// Mint a child drill token per sub-bubble of `index` and render a `group`
/// level. Shared by the semantic and metadata-fallback drill paths. Prunes
/// expired tokens before minting so the cache stays bounded on deep drills
/// (overviews are not always rebuilt between drills).
async fn emit_group_response(
    state: &AppState,
    index: &ClusterIndex,
    project: &str,
    total_n: usize,
    parent: &str,
    parent_size: usize,
    force_drillable: bool,
) -> Json<serde_json::Value> {
    let mut members_by_root: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for (&mem_id, &root) in &index.node_cluster {
        members_by_root.entry(root).or_default().push(mem_id);
    }
    let clusters = {
        let mut tokens = state.level_tokens.lock().await;
        tokens.retain(|_, (built_at, _)| built_at.elapsed() < LEVEL_TOKEN_TTL);
        index
            .clusters
            .iter()
            .map(|c| {
                let member_ids = members_by_root.remove(&c.id).unwrap_or_default();
                let child = mint_level_token();
                tokens.insert(
                    child.clone(),
                    (
                        std::time::Instant::now(),
                        TokenEntry {
                            project: project.to_string(),
                            member_ids,
                            total_n,
                            label: c.label.clone(),
                        },
                    ),
                );
                serde_json::json!({
                    "id": c.id,
                    "token": child,
                    "size": c.size,
                    "label": c.label,
                    "dominant_source": c.dominant_source,
                    // Every bubble opens (sub-bubbles or a member leaf); only a
                    // size-1 bubble is a dead end. (force_drillable kept for
                    // symmetry with the overview path / page buckets.)
                    "drillable": force_drillable || c.size > 1,
                })
            })
            .collect::<Vec<_>>()
    };
    let cluster_edges: Vec<serde_json::Value> = index
        .cluster_edges
        .iter()
        .map(|(a, b, w)| serde_json::json!({"src": a, "dst": b, "weight": w}))
        .collect();

    Json(serde_json::json!({
        "mode": "group",
        "parent": parent,
        "total_nodes": parent_size,
        "clusters": clusters,
        "cluster_edges": cluster_edges,
    }))
}

/// Leaf render shared by the token-drill paths: load `ids` as individual
/// memory nodes + their intra-set edges (capped by the store).
async fn memory_graph_leaf(
    store: &Arc<Mutex<MemoryStore>>,
    parent: &str,
    ids: &[i64],
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let members = {
        let guard = store.lock().await;
        guard
            .members_for_ids(ids)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let nodes: Vec<serde_json::Value> = members
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": format!("m{}", n.id),
                "node_type": "memory",
                "label": n.preview,
                "kind": n.kind,
                "source_kind": n.source_kind,
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = members
        .edges
        .iter()
        .map(|(a, b, w)| serde_json::json!({"src": format!("m{a}"), "dst": format!("m{b}"), "weight": w}))
        .collect();
    Ok(Json(serde_json::json!({
        "mode": "leaf",
        "parent": parent,
        "nodes": nodes,
        "edges": edges,
    })))
}

#[derive(Debug, Deserialize)]
struct MemoryExportQuery {
    project: String,
}

async fn memory_export(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryExportQuery>,
) -> std::result::Result<axum::response::Response, (StatusCode, String)> {
    use axum::http::header;
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let mut buf = Vec::new();
    guard
        .export_jsonl(&q.project, &mut buf)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let filename = format!("{}.jsonl", q.project.replace(['/', '\\'], "_"));
    let disposition = format!("attachment; filename=\"{filename}\"");
    let resp = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(axum::body::Body::from(buf))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct MemorySaveRequest {
    project: String,
    #[serde(default = "default_kind")]
    kind: String,
    body: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

fn default_kind() -> String {
    "note".into()
}

async fn bearer_guard(
    expected: Option<Arc<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{HeaderValue, header::AUTHORIZATION};
    let path = req.uri().path().to_string();
    // Always allow the bundled HTML, health probe, and favicon so the UI can
    // bootstrap; the API routes still require the token.
    if matches!(path.as_str(), "/" | "/healthz" | "/favicon.ico") {
        return next.run(req).await;
    }
    let Some(expected) = expected else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.strip_prefix("Bearer "))
        .map(str::to_string);
    let ok = presented
        .as_deref()
        .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));
    if ok {
        return next.run(req).await;
    }
    let mut resp = axum::response::Response::new(axum::body::Body::from(
        "unauthorized: bearer token missing or invalid",
    ));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        "WWW-Authenticate",
        HeaderValue::from_static("Bearer realm=\"rtrt-dashboard\""),
    );
    resp
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Deserialize)]
struct SetBlockRequest {
    project: String,
    name: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ListBlocksQuery {
    project: String,
}

async fn list_blocks(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListBlocksQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let blocks = guard
        .list_blocks(&q.project)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "blocks": blocks })))
}

async fn set_block(
    State(state): State<AppState>,
    Json(req): Json<SetBlockRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let id = guard
        .set_block(&req.project, &req.name, &req.body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
struct GetBlockQuery {
    project: String,
}

async fn get_block(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<GetBlockQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let block = guard
        .get_block(&q.project, &name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match block {
        Some(b) => {
            Ok(Json(serde_json::to_value(&b).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?))
        }
        None => Err((StatusCode::NOT_FOUND, format!("block not found: {name}"))),
    }
}

async fn memory_save(
    State(state): State<AppState>,
    Json(req): Json<MemorySaveRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let id = if req.metadata.is_empty() {
        guard
            .save(&req.project, &req.kind, &req.body)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        guard
            .save_with_metadata(&req.project, &req.kind, &req.body, &req.metadata)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    Ok(Json(serde_json::json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
struct CompressRequest {
    text: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    ml: bool,
    #[serde(default)]
    ratio: Option<f32>,
    #[serde(default)]
    format: Option<String>,
    /// Engine selector: "rules" (default), "ml", or "llm".
    /// When "llm", `model` must be set; the gateway's configured base URL is used.
    #[serde(default)]
    engine: Option<String>,
    /// Model id passed to the gateway when `engine = "llm"`.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyRequest {
    /// Optional command label (e.g. `git status`, `cargo build`).
    #[serde(default)]
    command: Option<String>,
    /// Raw captured output to filter.
    raw: String,
    /// Filter mode: `command` (default — picks `filter_for(command)`),
    /// `errors_only`, or `ultra_compact`.
    #[serde(default)]
    mode: Option<String>,
    /// Context-line count for `errors_only` (default 3).
    #[serde(default = "default_context")]
    context: usize,
}

fn default_context() -> usize {
    3
}

async fn proxy_filter(
    State(state): State<AppState>,
    Json(req): Json<ProxyRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mode = req.mode.as_deref().unwrap_or("command");
    let original = req.raw.chars().count();
    let out = match mode {
        "command" => {
            let cmd = req.command.as_deref().ok_or((
                StatusCode::BAD_REQUEST,
                "command required for command-mode".into(),
            ))?;
            match rtrt_proxy::filter_for(cmd) {
                Some(f) => f.apply(&req.raw),
                None => req.raw.clone(),
            }
        }
        "errors_only" => rtrt_proxy::errors_only(&req.raw, req.context),
        "ultra_compact" => rtrt_proxy::ultra_compact(&req.raw),
        other => return Err((StatusCode::BAD_REQUEST, format!("unknown mode: {other}"))),
    };
    let filtered = out.chars().count();
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "proxy".into());
    meta.insert("mode".into(), mode.to_string());
    if let Some(cmd) = req.command.as_deref() {
        meta.insert("command".into(), cmd.to_string());
    }
    meta.insert(
        "saved_chars".into(),
        original.saturating_sub(filtered).to_string(),
    );
    state.capture("proxy", &out, &meta).await;
    let saved_pct = if original > 0 {
        ((original.saturating_sub(filtered)) as f64 / original as f64 * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };
    Ok(Json(serde_json::json!({
        "filtered": out,
        "mode": mode,
        "original_len": original,
        "filtered_len": filtered,
        "saved_chars": original.saturating_sub(filtered),
        "saved_pct": saved_pct,
    })))
}

#[derive(Debug, Deserialize)]
struct DiagnoseRequest {
    /// Raw captured output (typically build/test failure).
    raw: String,
    /// Model id to send to the gateway.
    model: String,
    /// errors_only context-line count.
    #[serde(default = "default_context")]
    context: usize,
    /// Optional system prompt override; defaults to the rtrt-cli triage prompt.
    #[serde(default)]
    system: Option<String>,
}

const DIAGNOSE_SYS: &str = "You are a senior engineer triaging a build / test failure. \
Read the captured error output and respond with: (1) one-sentence root cause; \
(2) the smallest concrete fix (file + change). No filler. Cite line numbers when present.";

async fn diagnose(
    State(state): State<AppState>,
    Json(req): Json<DiagnoseRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let filtered = rtrt_proxy::errors_only(&req.raw, req.context);
    let chat_req = ChatRequest {
        model: req.model.clone(),
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: req.system.unwrap_or_else(|| DIAGNOSE_SYS.into()),
            },
            ChatMessage {
                role: Role::User,
                content: filtered.clone(),
            },
        ],
        max_tokens: Some(512),
        temperature: Some(0.1),
    };
    let resp = state
        .gateway
        .chat(chat_req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "diagnose".into());
    meta.insert("provider".into(), resp.provider.clone());
    meta.insert("model".into(), resp.model.clone());
    state.capture("diagnose", &resp.content, &meta).await;
    Ok(Json(serde_json::json!({
        "diagnosis": resp.content,
        "filtered": filtered,
        "provider": resp.provider,
        "model": resp.model,
        "input_tokens": resp.usage.input_tokens,
        "output_tokens": resp.usage.output_tokens,
    })))
}

#[derive(Debug, Deserialize)]
struct RepoMapRequest {
    root: PathBuf,
    #[serde(default = "default_ext")]
    ext: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: u64,
}

fn default_ext() -> String {
    // Empty = auto-detect via `Language::from_filename` (.rs / .py / .ts / .tsx).
    String::new()
}
fn default_max_bytes() -> u64 {
    524_288
}

async fn repo_map(
    Json(req): Json<RepoMapRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !req.root.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("root not found: {}", req.root.display()),
        ));
    }
    let mut entries = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut signature_chars: usize = 0;
    let restrict_ext = req.ext.trim();
    for entry in walk_files(&req.root) {
        let name = entry.to_string_lossy();
        if !restrict_ext.is_empty() && !name.ends_with(restrict_ext) {
            continue;
        }
        let Some(lang) = rtrt_compress::Language::from_filename(&name) else {
            continue;
        };
        let size = std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
        if size > req.max_bytes {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let extractor = rtrt_compress::SignatureExtractor::new(lang);
        let Ok(sig) = extractor.extract(&src) else {
            continue;
        };
        total_bytes += src.len() as u64;
        signature_chars += sig.chars().count();
        let rel = entry
            .strip_prefix(&req.root)
            .unwrap_or(&entry)
            .display()
            .to_string();
        entries.push(serde_json::json!({
            "path": rel,
            "language": format!("{lang:?}"),
            "signatures": sig,
            "original_bytes": src.len(),
            "signature_bytes": sig.len(),
        }));
    }
    Ok(Json(serde_json::json!({
        "files": entries.len(),
        "total_bytes": total_bytes,
        "signature_chars": signature_chars,
        "entries": entries,
    })))
}

fn walk_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    let name = entry.file_name();
                    let nstr = name.to_string_lossy();
                    if nstr == "target" || nstr.starts_with('.') {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() {
                    out.push(path);
                }
            }
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct SetupRequest {
    agent: String,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    binary: Option<String>,
}

async fn setup_snippet(
    Json(req): Json<SetupRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let binary = req.binary.clone().unwrap_or_else(|| "rtrt-mcp".to_string());
    let memory = req
        .memory
        .clone()
        .unwrap_or_else(|| ".rtrt/memory.sqlite".to_string());
    let (target_path, snippet) = match req.agent.as_str() {
        "claude-code" => (
            "~/.claude/mcp.json".to_string(),
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "rtrt": {
                        "command": binary,
                        "args": ["--memory", memory]
                    }
                }
            }))
            .unwrap(),
        ),
        "cursor" => (
            "~/.cursor/mcp.json".to_string(),
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "rtrt": {
                        "command": binary,
                        "args": ["--memory", memory]
                    }
                }
            }))
            .unwrap(),
        ),
        "codex" => (
            "~/.codex/config.toml".to_string(),
            format!(
                "[mcp.rtrt]\ncommand = \"{}\"\nargs = [\"--memory\", \"{}\"]\n",
                binary, memory
            ),
        ),
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown agent: {other} (try claude-code / cursor / codex)"),
            ));
        }
    };
    Ok(Json(serde_json::json!({
        "agent": req.agent,
        "target_path": target_path,
        "snippet": snippet,
    })))
}

const LLM_COMPRESS_SYS: &str = "You are a lossless-meaning compressor. Rewrite the user message \
in the shortest form that preserves every fact, decision, file path, identifier, command, and \
number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary, no \
preamble, no quotes — emit only the compressed text.";

async fn compress(
    State(state): State<AppState>,
    Json(req): Json<CompressRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    use rtrt_core::CompressionLevel;
    let original = req.text.chars().count();

    // Resolve the effective engine: explicit `engine` field wins; fall back to
    // the legacy `ml: true` boolean for backward compat.
    let engine = req
        .engine
        .as_deref()
        .unwrap_or(if req.ml { "ml" } else { "rules" });

    match engine {
        "llm" => {
            let model = req.model.clone().ok_or((
                StatusCode::BAD_REQUEST,
                "llm engine requires a `model` field".into(),
            ))?;
            // Build a request-scoped gateway that honours Config::auto_compress.base_url
            // when neither RTRT_PROVIDER_BASE_URL nor RTRT_OPENAI_COMPAT_URL is set —
            // the same resolution order used by run_hook_compress in rtrt-cli. state.gateway
            // was constructed at startup before config base_url was available, so we rebuild
            // here. When the env vars are absent we temporarily set RTRT_PROVIDER_BASE_URL
            // from config so Gateway::from_env registers the openai-compat provider.
            let llm_gateway = {
                let env_has_url = std::env::var_os("RTRT_PROVIDER_BASE_URL").is_some()
                    || std::env::var_os("RTRT_OPENAI_COMPAT_URL").is_some();
                if env_has_url {
                    rtrt_providers::Gateway::from_env()
                } else {
                    let cfg_url = rtrt_core::Config::load()
                        .ok()
                        .and_then(|c| c.auto_compress.base_url);
                    if let Some(url) = cfg_url {
                        // SAFETY: no await between set_var and remove_var; the var was
                        // absent before this block so concurrent handlers that reach this
                        // branch independently each set-and-remove their own value.
                        unsafe { std::env::set_var("RTRT_PROVIDER_BASE_URL", &url) };
                        let gw = rtrt_providers::Gateway::from_env();
                        unsafe { std::env::remove_var("RTRT_PROVIDER_BASE_URL") };
                        gw
                    } else {
                        rtrt_providers::Gateway::from_env()
                    }
                }
            };
            let chat_req = ChatRequest {
                model: model.clone(),
                messages: vec![
                    ChatMessage {
                        role: Role::System,
                        content: LLM_COMPRESS_SYS.to_string(),
                    },
                    ChatMessage {
                        role: Role::User,
                        content: req.text.clone(),
                    },
                ],
                max_tokens: Some(
                    // Cap at 4× the original char count but at least 128 tokens.
                    (original as u32).saturating_mul(4).clamp(128, 4096),
                ),
                temperature: Some(0.0),
            };
            let resp = llm_gateway
                .chat(chat_req)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            let out = resp.content.trim().to_string();
            let compressed = out.chars().count();
            let mut meta = BTreeMap::new();
            meta.insert("source".into(), "compress".into());
            meta.insert("mode".into(), "llm".into());
            meta.insert("model".into(), model.clone());
            meta.insert(
                "saved_chars".into(),
                original.saturating_sub(compressed).to_string(),
            );
            state.capture("compress", &out, &meta).await;
            let saved = original.saturating_sub(compressed);
            let saved_pct = if original > 0 {
                (saved as f64 / original as f64 * 100.0 * 10.0).round() / 10.0
            } else {
                0.0
            };
            Ok(Json(serde_json::json!({
                "compressed": out,
                "mode": "llm",
                "model": model,
                "original_len": original,
                "compressed_len": compressed,
                "saved": saved,
                "saved_chars": saved,
                "saved_pct": saved_pct,
            })))
        }

        "ml" => {
            let target = rtrt_compress::CompressionTarget::new(req.ratio.unwrap_or(0.5))
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            let c = rtrt_compress::MlCompressor::heuristic();
            let scorer = c.scorer_name().to_string();
            let out = c.compress(&req.text, target);
            let compressed = out.chars().count();
            let mut meta = BTreeMap::new();
            meta.insert("source".into(), "compress".into());
            meta.insert("mode".into(), "ml".into());
            meta.insert("scorer".into(), scorer.clone());
            meta.insert(
                "saved_chars".into(),
                original.saturating_sub(compressed).to_string(),
            );
            state.capture("compress", &out, &meta).await;
            let saved = original.saturating_sub(compressed);
            let saved_pct = if original > 0 {
                (saved as f64 / original as f64 * 100.0 * 10.0).round() / 10.0
            } else {
                0.0
            };
            Ok(Json(serde_json::json!({
                "compressed": out,
                "mode": "ml",
                "scorer": scorer,
                "original_len": original,
                "compressed_len": compressed,
                "saved": saved,
                "saved_chars": saved,
                "saved_pct": saved_pct,
            })))
        }

        // "rules" or any unrecognised value — run the rule-based compressor.
        _ => {
            let level = match req.level.as_deref().unwrap_or("full") {
                "lite" => CompressionLevel::Lite,
                "full" => CompressionLevel::Full,
                "ultra" => CompressionLevel::Ultra,
                "extreme" => CompressionLevel::Extreme,
                other => {
                    return Err((StatusCode::BAD_REQUEST, format!("unknown level: {other}")));
                }
            };
            let compressor = rtrt_compress::Compressor::new(level);
            let format = req
                .format
                .as_deref()
                .and_then(rtrt_compress::OutputFormat::parse)
                .unwrap_or(rtrt_compress::OutputFormat::Plain);
            let out = compressor.compress_to(&req.text, format);
            let compressed = out.chars().count();
            let mut meta = BTreeMap::new();
            meta.insert("source".into(), "compress".into());
            meta.insert("mode".into(), "rules".into());
            meta.insert(
                "saved_chars".into(),
                original.saturating_sub(compressed).to_string(),
            );
            state.capture("compress", &out, &meta).await;
            let saved = original.saturating_sub(compressed);
            let saved_pct = if original > 0 {
                (saved as f64 / original as f64 * 100.0 * 10.0).round() / 10.0
            } else {
                0.0
            };
            Ok(Json(serde_json::json!({
                "compressed": out,
                "mode": "rules",
                "original_len": original,
                "compressed_len": compressed,
                "saved": saved,
                "saved_chars": saved,
                "saved_pct": saved_pct,
            })))
        }
    }
}

fn open_prompt_registry() -> Option<Arc<PromptRegistry>> {
    let root = match std::env::var("RTRT_PROMPTS_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => rtrt_templates::prompts::default_dir()?,
    };
    match PromptRegistry::open(&root) {
        Ok(reg) => Some(Arc::new(reg)),
        Err(e) => {
            tracing::warn!(?root, "prompts registry unavailable: {e}");
            None
        }
    }
}

#[derive(Debug, Serialize)]
struct PromptSummary {
    name: String,
    versions: Vec<u32>,
    latest: u32,
}

fn require_prompts(state: &AppState) -> std::result::Result<&PromptRegistry, (StatusCode, String)> {
    state
        .prompts
        .as_deref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "prompts disabled".into()))
}

async fn list_prompts(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<PromptSummary>>, (StatusCode, String)> {
    let reg = require_prompts(&state)?;
    let names = reg
        .list_names()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let versions = reg
            .list_versions(&name)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let latest = versions.iter().copied().max().unwrap_or(0);
        out.push(PromptSummary {
            name,
            versions,
            latest,
        });
    }
    Ok(Json(out))
}

async fn list_prompt_versions(
    State(state): State<AppState>,
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<PromptSummary>, (StatusCode, String)> {
    let reg = require_prompts(&state)?;
    let versions = reg
        .list_versions(&name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if versions.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("prompt not found: {name}")));
    }
    let latest = versions.iter().copied().max().unwrap_or(0);
    Ok(Json(PromptSummary {
        name,
        versions,
        latest,
    }))
}

async fn get_prompt(
    State(state): State<AppState>,
    AxPath((name, version)): AxPath<(String, u32)>,
) -> std::result::Result<Json<Prompt>, (StatusCode, String)> {
    let reg = require_prompts(&state)?;
    reg.get(&name, version)
        .map(Json)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(Debug, Serialize)]
struct BudgetResponse {
    cap_usd: Option<f64>,
    spent_usd: f64,
    remaining_usd: Option<f64>,
    cache_len: Option<usize>,
}

async fn sse_stream(
    State(state): State<AppState>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<
        Item = std::result::Result<axum::response::sse::Event, std::convert::Infallible>,
    >,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::StreamExt;
    let rx = state.events.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(line) => Some(Ok::<_, std::convert::Infallible>(
                Event::default().data(line),
            )),
            Err(_lag) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn tokens_summary(State(state): State<AppState>) -> Json<serde_json::Value> {
    let buf = state.gateway.metrics();
    let guard = buf.lock().unwrap_or_else(|p| p.into_inner());
    use rtrt_providers::MetricsView;
    let view = MetricsView::new(&guard);
    let summary = view.summary();
    let by_provider = view.by_provider();
    let recent = view.recent(usize::MAX);
    // Bucket by hour (Unix epoch / 3600). Stable across timezones.
    let mut hourly: std::collections::BTreeMap<u64, (u64, u64, u64)> = Default::default();
    let mut daily: std::collections::BTreeMap<u64, (u64, u64, u64)> = Default::default();
    for m in &recent {
        let h: u64 = m.started_at / 3600;
        let d: u64 = m.started_at / 86400;
        let e = hourly.entry(h).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += m.usage.input_tokens;
        e.2 += m.usage.output_tokens;
        let e = daily.entry(d).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += m.usage.input_tokens;
        e.2 += m.usage.output_tokens;
    }
    let hourly: Vec<_> = hourly
        .into_iter()
        .map(|(h, (c, i, o))| {
            serde_json::json!({"hour_ts": h*3600, "calls": c, "input_tokens": i, "output_tokens": o})
        })
        .collect();
    let daily: Vec<_> = daily
        .into_iter()
        .map(|(d, (c, i, o))| {
            serde_json::json!({"day_ts": d*86400, "calls": c, "input_tokens": i, "output_tokens": o})
        })
        .collect();
    Json(serde_json::json!({
        "summary": summary,
        "by_provider": by_provider,
        "hourly": hourly,
        "daily": daily,
    }))
}

async fn budget(State(state): State<AppState>) -> Json<BudgetResponse> {
    let cap = state.gateway.budget_cap_usd();
    let spent = state.gateway.budget_spent_usd();
    let remaining = cap.map(|c| (c - spent).max(0.0));
    Json(BudgetResponse {
        cap_usd: cap,
        spent_usd: spent,
        remaining_usd: remaining,
        cache_len: state.gateway.cache_len(),
    })
}

async fn metrics(State(state): State<AppState>) -> Json<MetricsResponse> {
    let metrics = state.gateway.metrics();
    let guard = metrics.lock().unwrap_or_else(|p| p.into_inner());
    let view = MetricsView::new(&guard);
    let by_provider = view
        .by_provider()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    Json(MetricsResponse {
        summary: view.summary(),
        by_provider,
        recent: view.recent(50),
    })
}

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ConfigResponse {
    capture: rtrt_core::config::CaptureConfig,
    auto_compress: rtrt_core::config::AutoCompressConfig,
    embeddings: rtrt_core::config::EmbeddingsConfig,
    security: rtrt_core::config::SecurityConfig,
    path: String,
}

async fn get_config() -> std::result::Result<Json<ConfigResponse>, (StatusCode, String)> {
    let cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let path = rtrt_core::Config::default_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Json(ConfigResponse {
        capture: cfg.capture,
        auto_compress: cfg.auto_compress,
        embeddings: cfg.embeddings,
        security: cfg.security,
        path,
    }))
}

// ---------------------------------------------------------------------------
// GET/POST /api/statusline/config + GET /api/statusline/preview
// ---------------------------------------------------------------------------

const STATUSLINE_SEGMENTS: &[&str] = &[
    "project", "branch", "wip", "sess", "ctx", "cache", "opt", "model", "usage", "agents",
    "savings",
];
const LEGACY_STATUSLINE_SEGMENTS: &[&str] = &[
    "project", "branch", "wip", "sess", "ctx", "cache", "model", "usage", "codex", "savings",
];
const STATUSLINE_DEFAULT_FORMAT: &str =
    "{project} [{branch}] {wip} {sess} {ctx} {cache} {opt} {model} {agents}";
const LEGACY_STATUSLINE_FORMAT: &str = "{project} [{branch}] {wip} {sess} {ctx} {cache} {model}";
const STATUSLINE_DEFAULT_LINE2_FORMAT: &str = "{usage}";
const STATUSLINE_DEFAULT_LINE3_FORMAT: &str = "{savings}";
const STATUSLINE_DEFAULT_CODEX_TIMEOUT_MS: u64 = 200;

#[derive(Debug, Deserialize, Serialize)]
struct StatuslineConfig {
    #[serde(default = "default_statusline_segments")]
    enabled_segments: Vec<String>,
    #[serde(default = "default_statusline_format")]
    format: String,
    #[serde(default = "default_statusline_line2_format")]
    line2_format: String,
    #[serde(default = "default_statusline_line3_format")]
    line3_format: String,
    #[serde(default = "default_statusline_codex_timeout_ms")]
    codex_check_timeout_ms: u64,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            enabled_segments: default_statusline_segments(),
            format: default_statusline_format(),
            line2_format: default_statusline_line2_format(),
            line3_format: default_statusline_line3_format(),
            codex_check_timeout_ms: default_statusline_codex_timeout_ms(),
        }
    }
}

fn default_statusline_segments() -> Vec<String> {
    STATUSLINE_SEGMENTS
        .iter()
        .map(|segment| segment.to_string())
        .collect()
}

fn default_statusline_format() -> String {
    STATUSLINE_DEFAULT_FORMAT.to_string()
}

fn default_statusline_line2_format() -> String {
    STATUSLINE_DEFAULT_LINE2_FORMAT.to_string()
}

fn default_statusline_line3_format() -> String {
    STATUSLINE_DEFAULT_LINE3_FORMAT.to_string()
}

fn default_statusline_codex_timeout_ms() -> u64 {
    STATUSLINE_DEFAULT_CODEX_TIMEOUT_MS
}

#[derive(Debug, Serialize)]
struct StatuslineOk {
    ok: bool,
}

/// GET response wrapping the effective statusline config plus a scope marker so
/// the UI can render the "Follow global / Custom (this project)" toggle.
///
/// `#[serde(flatten)]` keeps the config fields at the top level (back-compat
/// with the existing UI reads) while adding:
/// - `scope`: `"custom"` when THIS project carries its own override, else
///   `"global"` (inherits the global config — the default).
/// - `custom`: the same as a boolean for convenience.
#[derive(Debug, Serialize)]
struct StatuslineConfigResponse {
    #[serde(flatten)]
    config: StatuslineConfig,
    scope: &'static str,
    custom: bool,
}

#[derive(Debug, Serialize)]
struct StatuslinePreview {
    lines: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

type DashboardJsonResult<T> = std::result::Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

async fn get_statusline_config(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
) -> DashboardJsonResult<StatuslineConfigResponse> {
    // Per-project override wins: when the project carries its own `statusline`
    // value in `<repo>/.rtrt/config.toml`, return it ("Custom" scope); otherwise
    // fall through to the global statusline config ("Follow global" — inherit).
    if let Some(repo) = resolve_project_repo(q.project.as_deref()) {
        let project = rtrt_core::Config::load_project(&repo).map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load project config {}: {e}", repo.display()),
            )
        })?;
        if let Some(statusline) = project.statusline {
            let cfg = statusline.try_into().map_err(|e| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "parse [statusline] in {}/.rtrt/config.toml: {e}",
                        repo.display()
                    ),
                )
            })?;
            return Ok(Json(StatuslineConfigResponse {
                config: upgrade_legacy_statusline_config(cfg),
                scope: "custom",
                custom: true,
            }));
        }
        // no project override — inherit the global config below ("Follow global")
    }

    // Global scope, or a project that follows global: serve the effective
    // (global) config and report `global` scope.
    let config = read_global_statusline_config().await?;
    Ok(Json(StatuslineConfigResponse {
        config,
        scope: "global",
        custom: false,
    }))
}

/// Read and normalise the global statusline config from `~/.rtrt/config.toml`.
async fn read_global_statusline_config()
-> std::result::Result<StatuslineConfig, (StatusCode, Json<ApiError>)> {
    let path = statusline_config_path()?;
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let root = parse_config_toml(&content, &path)?;
            let Some(statusline) = root.get("statusline") else {
                return Ok(StatuslineConfig::default());
            };
            let cfg = statusline.clone().try_into().map_err(|e| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("parse [statusline] in {}: {e}", path.display()),
                )
            })?;
            Ok(upgrade_legacy_statusline_config(cfg))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StatuslineConfig::default()),
        Err(e) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read {}: {e}", path.display()),
        )),
    }
}

fn upgrade_legacy_statusline_config(mut cfg: StatuslineConfig) -> StatuslineConfig {
    let legacy_segments = LEGACY_STATUSLINE_SEGMENTS
        .iter()
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>();
    if cfg.enabled_segments == legacy_segments {
        cfg.enabled_segments = default_statusline_segments();
    }
    if cfg.format == LEGACY_STATUSLINE_FORMAT {
        cfg.format = default_statusline_format();
    }
    cfg
}

async fn post_statusline_config(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
    Json(req): Json<StatuslineConfig>,
) -> DashboardJsonResult<StatuslineOk> {
    // "Follow global" action: `?scope=global` CLEARS this project's statusline
    // override so it inherits the global config again. Only meaningful for a
    // resolved project; ignored for the global scope (there is nothing to clear).
    let follow_global = q
        .scope
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("global"));

    // Per-project override: persist the serialized statusline config into the
    // project's `<repo>/.rtrt/config.toml` (`ProjectConfig.statusline`) instead
    // of the global file.
    if let Some(repo) = resolve_project_repo(q.project.as_deref()) {
        let mut project = rtrt_core::Config::load_project(&repo).map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load project config {}: {e}", repo.display()),
            )
        })?;
        if follow_global {
            // Clear the override. `save_project` removes the file when the
            // ProjectConfig becomes empty, so the project falls back to global.
            project.statusline = None;
            rtrt_core::Config::save_project(&repo, &project).map_err(|e| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("write project config {}: {e}", repo.display()),
                )
            })?;
            return Ok(Json(StatuslineOk { ok: true }));
        }
        validate_statusline_segments(&req.enabled_segments)?;
        let statusline_value = toml::Value::try_from(&req).map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialize statusline config: {e}"),
            )
        })?;
        project.statusline = Some(statusline_value);
        rtrt_core::Config::save_project(&repo, &project).map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write project config {}: {e}", repo.display()),
            )
        })?;
        return Ok(Json(StatuslineOk { ok: true }));
    }

    validate_statusline_segments(&req.enabled_segments)?;

    let path = statusline_config_path()?;
    let mut root = match tokio::fs::read_to_string(&path).await {
        Ok(content) => parse_config_toml(&content, &path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::Table::new())
        }
        Err(e) => {
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read {}: {e}", path.display()),
            ));
        }
    };
    if !root.is_table() {
        root = toml::Value::Table(toml::Table::new());
    }
    let root_table = root.as_table_mut().ok_or_else(|| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config root is not a TOML table",
        )
    })?;
    let statusline_value = toml::Value::try_from(req).map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize statusline config: {e}"),
        )
    })?;
    root_table.insert("statusline".to_string(), statusline_value);

    let output = toml::to_string_pretty(&root).map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize {}: {e}", path.display()),
        )
    })?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create dir {}: {e}", parent.display()),
            )
        })?;
    }
    tokio::fs::write(&path, output).await.map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;

    Ok(Json(StatuslineOk { ok: true }))
}

async fn statusline_preview() -> Json<StatuslinePreview> {
    let fallback = || {
        Json(StatuslinePreview {
            lines: vec![
                "(rtrt binary not found or timed out — save config and ensure rtrt is installed)"
                    .to_string(),
            ],
        })
    };
    match run_statusline_preview("rtrt").await {
        Some(lines) => Json(StatuslinePreview { lines }),
        None => {
            let Some(home) = dirs::home_dir() else {
                return fallback();
            };
            let cargo_bin = home.join(".cargo").join("bin").join("rtrt");
            match run_statusline_preview(cargo_bin).await {
                Some(lines) => Json(StatuslinePreview { lines }),
                None => fallback(),
            }
        }
    }
}

fn statusline_config_path() -> std::result::Result<PathBuf, (StatusCode, Json<ApiError>)> {
    rtrt_core::Config::default_path().ok_or_else(|| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot determine config path",
        )
    })
}

fn parse_config_toml(
    content: &str,
    path: &std::path::Path,
) -> std::result::Result<toml::Value, (StatusCode, Json<ApiError>)> {
    content.parse::<toml::Value>().map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("parse {}: {e}", path.display()),
        )
    })
}

fn validate_statusline_segments(
    segments: &[String],
) -> std::result::Result<(), (StatusCode, Json<ApiError>)> {
    for segment in segments {
        if !STATUSLINE_SEGMENTS.contains(&segment.as_str()) {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("unknown statusline segment: {segment}"),
            ));
        }
    }
    Ok(())
}

async fn run_statusline_preview<P>(binary: P) -> Option<Vec<String>>
where
    P: AsRef<std::ffi::OsStr>,
{
    let sample = serde_json::json!({
        "model": { "display_name": "Opus 4.8" },
        "cwd": "/home/kernalix7/Desktop/00_Personal_Project/00G_rtrt",
        "transcript": [],
    });
    let sample = serde_json::to_vec(&sample).ok()?;
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("statusline")
        .arg("--rich")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(&sample).await.is_err() || stdin.shutdown().await.is_err() {
            return None;
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .take(3)
        .collect::<Vec<_>>();
    Some(lines)
}

// ---------------------------------------------------------------------------
// Environment / orchestration detection
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DetectedToolResponse {
    #[serde(flatten)]
    tool: rtrt_core::DetectedTool,
    has_api_key: bool,
}

async fn detect_tools_api() -> Json<Vec<DetectedToolResponse>> {
    Json(
        detected_tools_with_config_overrides()
            .into_iter()
            .map(|tool| {
                let has_api_key = provider_api_key_present(&tool);
                DetectedToolResponse { tool, has_api_key }
            })
            .collect(),
    )
}

#[derive(Debug, Deserialize)]
struct RouteQuery {
    prompt: String,
    #[serde(default)]
    prefer: Option<String>,
    #[serde(default)]
    capability: Option<String>,
}

#[derive(Debug, Serialize)]
struct RouteChosen {
    target: String,
    mode: rtrt_providers::Mode,
    model: Option<String>,
    cost_class: rtrt_core::CostClass,
    reason: String,
}

#[derive(Debug, Serialize)]
struct RouteUsageHeadroom {
    sources: Vec<String>,
    by_target: BTreeMap<String, RouteTargetHeadroom>,
}

#[derive(Debug, Serialize)]
struct RouteTargetHeadroom {
    used: Option<u64>,
    limit: Option<u64>,
    remaining: Option<u64>,
    label: String,
}

#[derive(Debug, Serialize)]
struct RouteApiResponse {
    chosen: RouteChosen,
    alternatives: Vec<RouteAlternative>,
    usage_headroom: RouteUsageHeadroom,
}

async fn route_api(
    axum::extract::Query(q): axum::extract::Query<RouteQuery>,
) -> axum::response::Response {
    if q.prompt.trim().is_empty() {
        return route_error(StatusCode::BAD_REQUEST, "prompt is required");
    }

    let prefer = match parse_route_prefer(q.prefer.as_deref()) {
        Ok(prefer) => prefer,
        Err(message) => return route_error(StatusCode::BAD_REQUEST, &message),
    };
    let capability = match parse_route_capability(q.capability.as_deref()) {
        Ok(capability) => capability,
        Err(message) => return route_error(StatusCode::BAD_REQUEST, &message),
    };

    let request = RouteRequest {
        capability,
        prefer,
        target: None,
        model: None,
        mode: None,
    };
    let tools = detected_tools_with_config_overrides();
    let usage = UsageSnapshot::load_best_effort();
    let decision = match select_route(&request, &tools, &usage) {
        Ok(decision) => decision,
        Err(e) => return route_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let usage_headroom = route_usage_headroom(&usage, &tools);
    let response = RouteApiResponse {
        chosen: RouteChosen {
            target: decision.target,
            mode: decision.mode,
            model: decision.model,
            cost_class: decision.cost_class,
            reason: decision.reason,
        },
        alternatives: decision.alternatives,
        usage_headroom,
    };
    Json(response).into_response()
}

fn route_error(status: StatusCode, message: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": message,
        })),
    )
        .into_response()
}

fn parse_route_prefer(value: Option<&str>) -> std::result::Result<Prefer, String> {
    match value
        .unwrap_or("cheapest")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "cheapest" => Ok(Prefer::Cheapest),
        "local" => Ok(Prefer::Local),
        "quality" => Ok(Prefer::Quality),
        other => Err(format!(
            "unknown prefer '{other}' (expected cheapest, local, or quality)"
        )),
    }
}

fn parse_route_capability(
    value: Option<&str>,
) -> std::result::Result<Option<rtrt_core::Capability>, String> {
    match value
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "chat" | "general" => Ok(None),
        "code" => Ok(Some(rtrt_core::Capability::Code)),
        "reasoning" => Ok(Some(rtrt_core::Capability::Reasoning)),
        "vision" => Ok(Some(rtrt_core::Capability::Vision)),
        "embed" | "embedding" => Ok(Some(rtrt_core::Capability::Embed)),
        "agentic" => Ok(Some(rtrt_core::Capability::Agentic)),
        "summarize" | "summary" | "cheap-bulk" | "cheap_bulk" => {
            Ok(Some(rtrt_core::Capability::CheapBulk))
        }
        other => Err(format!(
            "unknown capability '{other}' (expected code, reasoning, chat, summarize, vision, embed, or agentic)"
        )),
    }
}

fn route_usage_headroom(
    usage: &UsageSnapshot,
    tools: &[rtrt_core::DetectedTool],
) -> RouteUsageHeadroom {
    let mut by_target = BTreeMap::new();
    for tool in tools.iter().filter(|tool| tool.installed && tool.enabled) {
        let headroom = usage.headroom(&tool.name);
        let view = match headroom {
            Some(quota) => RouteTargetHeadroom {
                used: Some(quota.used),
                limit: Some(quota.limit),
                remaining: Some(quota.remaining),
                label: format!("{}/{} tokens remaining", quota.remaining, quota.limit),
            },
            None => RouteTargetHeadroom {
                used: None,
                limit: None,
                remaining: None,
                label: "unknown".to_string(),
            },
        };
        by_target.insert(tool.name.clone(), view);
    }
    RouteUsageHeadroom {
        sources: usage.sources.clone(),
        by_target,
    }
}

fn provider_api_key_present(tool: &rtrt_core::DetectedTool) -> bool {
    matches!(tool.kind, rtrt_core::ToolKind::ProviderApi) && tool.installed && tool.path.is_none()
}

fn detected_tools_with_config_overrides() -> Vec<rtrt_core::DetectedTool> {
    let mut tools = rtrt_core::detect_tools();
    let Ok(cfg) = rtrt_core::Config::load() else {
        return tools;
    };

    for tool in &mut tools {
        let override_value = match tool.kind {
            rtrt_core::ToolKind::ProviderApi => cfg.providers.enabled_override(&tool.name),
            rtrt_core::ToolKind::CodingAgent
            | rtrt_core::ToolKind::LocalRuntime
            | rtrt_core::ToolKind::McpServer => cfg.agents.enabled_override(&tool.name),
        };
        if let Some(enabled) = override_value {
            tool.enabled = enabled;
        }
    }
    tools
}

#[derive(Debug, Deserialize)]
struct DetectToggleRequest {
    name: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct DetectToggleResponse {
    ok: bool,
    name: String,
    enabled: bool,
    section: String,
    path: String,
}

async fn toggle_detected_tool(
    Json(req): Json<DetectToggleRequest>,
) -> std::result::Result<Json<DetectToggleResponse>, (StatusCode, String)> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".to_string()));
    }

    let detected = rtrt_core::detect_tools()
        .into_iter()
        .find(|tool| tool.name == name)
        .ok_or((StatusCode::NOT_FOUND, format!("tool not detected: {name}")))?;

    let mut cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let section = match detected.kind {
        rtrt_core::ToolKind::ProviderApi => {
            cfg.set_provider_enabled(name, req.enabled);
            "providers"
        }
        rtrt_core::ToolKind::CodingAgent
        | rtrt_core::ToolKind::LocalRuntime
        | rtrt_core::ToolKind::McpServer => {
            cfg.set_agent_enabled(name, req.enabled);
            "agents"
        }
    };

    let path = write_config_file(&cfg)?;
    Ok(Json(DetectToggleResponse {
        ok: true,
        name: name.to_string(),
        enabled: req.enabled,
        section: section.to_string(),
        path,
    }))
}

fn write_config_file(cfg: &rtrt_core::Config) -> std::result::Result<String, (StatusCode, String)> {
    let path = rtrt_core::Config::default_path().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "cannot determine config path".into(),
    ))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create dir {}: {e}", parent.display()),
            )
        })?;
    }

    let toml_str = toml::to_string_pretty(cfg)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&path, toml_str).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;
    Ok(path.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// POST /api/config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ConfigWriteRequest {
    capture: rtrt_core::config::CaptureConfig,
    auto_compress: rtrt_core::config::AutoCompressConfig,
    /// Optional so older dashboard builds (no embeddings UI) still POST cleanly;
    /// when absent the on-disk embeddings section is preserved untouched.
    #[serde(default)]
    embeddings: Option<rtrt_core::config::EmbeddingsConfig>,
    /// Optional global security defaults (default profile). Preserved when absent.
    #[serde(default)]
    security: Option<rtrt_core::config::SecurityConfig>,
}

#[derive(Debug, Serialize)]
struct ConfigWriteResponse {
    ok: bool,
    path: String,
}

async fn post_config(
    Json(req): Json<ConfigWriteRequest>,
) -> std::result::Result<Json<ConfigWriteResponse>, (StatusCode, String)> {
    // Build an updated Config preserving any non-exposed fields from disk.
    let mut cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    cfg.capture = req.capture;
    cfg.auto_compress = req.auto_compress;
    if let Some(emb) = req.embeddings {
        cfg.embeddings = emb;
    }
    if let Some(sec) = req.security {
        cfg.security = sec;
    }

    let path = rtrt_core::Config::default_path().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "cannot determine config path".into(),
    ))?;

    // Create parent directory if needed.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create dir {}: {e}", parent.display()),
            )
        })?;
    }

    // Back up the existing file before overwriting.
    if path.exists() {
        let bak = path.with_extension("toml.bak");
        std::fs::copy(&path, &bak).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("backup {}: {e}", path.display()),
            )
        })?;
    }

    let toml_str = toml::to_string_pretty(&cfg)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&path, toml_str).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;

    Ok(Json(ConfigWriteResponse {
        ok: true,
        path: path.to_string_lossy().into_owned(),
    }))
}

// ---------------------------------------------------------------------------
// GET /api/models
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ModelEntry {
    id: String,
    source: &'static str,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    models: Vec<ModelEntry>,
}

async fn get_models() -> Json<ModelsResponse> {
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    // Derive the Ollama host root: strip a trailing `/v1` (OpenAI-compat
    // path prefix) and any trailing slash so `/api/tags` lands at the right
    // place regardless of how base_url was configured.
    let ollama_host = cfg
        .auto_compress
        .base_url
        .as_deref()
        .unwrap_or("http://127.0.0.1:11434")
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/')
        .to_string();

    let mut models: Vec<ModelEntry> = Vec::new();

    // Attempt to list Ollama models; any failure is silently ignored.
    let ollama_url = format!("{ollama_host}/api/tags");
    if let Ok(resp) = reqwest::Client::new()
        .get(&ollama_url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        if let Ok(body) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(name) = m.get("name").and_then(|v| v.as_str()) {
                        models.push(ModelEntry {
                            id: name.to_string(),
                            source: "ollama",
                        });
                    }
                }
            }
        }
    }

    // Always append the cloud defaults.
    models.push(ModelEntry {
        id: "claude-haiku-4-5".to_string(),
        source: "cloud",
    });
    models.push(ModelEntry {
        id: "gpt-5.4-mini".to_string(),
        source: "cloud",
    });

    Json(ModelsResponse { models })
}

// ---------------------------------------------------------------------------
// POST /api/memory/compress
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryCompressRequest {
    project: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryCompressResponse {
    compressed: usize,
    skipped: usize,
    saved_chars: i64,
    /// Aggregate % reduction across the batch (0.0 when nothing was compressed).
    saved_pct: f64,
}

async fn memory_compress(
    State(state): State<AppState>,
    Json(req): Json<MemoryCompressRequest>,
) -> std::result::Result<Json<MemoryCompressResponse>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;

    let cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let ac = &cfg.auto_compress;

    let age_sec: i64 = ac.age_sec;
    let min_chars: usize = ac.min_chars;
    let batch: usize = ac.batch;
    let max_tokens: u32 = ac.max_tokens;
    let model = req.model.clone().unwrap_or_else(|| ac.model.clone());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now - age_sec;

    let candidates = {
        let guard = store.lock().await;
        guard
            .compress_candidates(&req.project, cutoff, min_chars, batch)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    let mut compressed_count = 0usize;
    let mut skipped_count = 0usize;
    let mut total_from_chars: i64 = 0;
    let mut total_saved_chars: i64 = 0;

    for (id, body) in candidates {
        let chat_req = ChatRequest {
            model: model.clone(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are a lossless-meaning compressor. Rewrite the user message in the shortest form that preserves every fact, decision, file path, identifier, command, and number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary, no preamble, no quotes — emit only the compressed text.".to_string(),
                },
                ChatMessage {
                    role: Role::User,
                    content: body.clone(),
                },
            ],
            max_tokens: Some(max_tokens),
            temperature: Some(0.0),
        };
        let resp = match state.gateway.chat(chat_req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("memory_compress {project}#{id}: {e}", project = req.project);
                skipped_count += 1;
                continue;
            }
        };
        let new_body = resp.content.trim().to_string();
        if new_body.is_empty() || new_body.len() >= body.len() {
            // No compression win; mark so the next sweep skips this row.
            let guard = store.lock().await;
            let mut meta = guard.get_metadata(id).unwrap_or_default();
            meta.insert("compressed_at".into(), now.to_string());
            meta.insert("compressed_skip".into(), "no-shrink".into());
            let _ = guard.set_metadata(id, &meta);
            skipped_count += 1;
            continue;
        }
        let guard = store.lock().await;
        if let Err(e) = guard.compress_in_place(id, &new_body) {
            tracing::warn!(
                "memory_compress {project}#{id}: compress_in_place: {e}",
                project = req.project
            );
            skipped_count += 1;
            continue;
        }
        let mut meta = guard.get_metadata(id).unwrap_or_default();
        meta.insert("compressed_at".into(), now.to_string());
        meta.insert("compressed_model".into(), model.clone());
        let from_chars = body.len() as i64;
        let to_chars = new_body.len() as i64;
        meta.insert("compressed_from_chars".into(), from_chars.to_string());
        meta.insert("compressed_to_chars".into(), to_chars.to_string());
        let _ = guard.set_metadata(id, &meta);
        total_from_chars += from_chars;
        total_saved_chars += (from_chars - to_chars).max(0);
        compressed_count += 1;
        tracing::info!(
            project = %req.project,
            id,
            from = body.len(),
            to = new_body.len(),
            "manual compress sweep"
        );
    }

    let saved_pct = if total_from_chars > 0 {
        (total_saved_chars as f64 / total_from_chars as f64 * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };
    Ok(Json(MemoryCompressResponse {
        compressed: compressed_count,
        skipped: skipped_count,
        saved_chars: total_saved_chars,
        saved_pct,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/memory/stats?project=X
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryStatsQuery {
    project: String,
}

#[derive(Debug, Serialize)]
struct KindCount {
    kind: String,
    count: i64,
}

#[derive(Debug, Serialize)]
struct DayCount {
    day: String,
    count: i64,
}

#[derive(Debug, Serialize)]
struct MemoryStatsResponse {
    total: i64,
    by_kind: Vec<KindCount>,
    compressed_count: i64,
    saved_chars: i64,
    /// Average % reduction across compressed rows (0.0 when none compressed).
    saved_pct: f64,
    by_day: Vec<DayCount>,
}

async fn memory_stats(
    axum::extract::Query(q): axum::extract::Query<MemoryStatsQuery>,
) -> std::result::Result<Json<MemoryStatsResponse>, (StatusCode, String)> {
    // Open a direct rusqlite connection to the same path used by open_memory_store().
    let path = std::env::var("RTRT_MEMORY_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".rtrt/memory.sqlite"));

    let conn = rusqlite::Connection::open(&path).map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("memory store: {e}"),
        )
    })?;

    // Total row count for the project.
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE project = ?1",
            rusqlite::params![q.project],
            |row| row.get(0),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Per-kind breakdown.
    let by_kind = {
        let mut stmt = conn
            .prepare(
                "SELECT kind, COUNT(*) AS cnt FROM memories WHERE project = ?1 GROUP BY kind ORDER BY cnt DESC",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![q.project], |row| {
                Ok(KindCount {
                    kind: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    // compressed_count / saved_chars / saved_pct:
    //   Source of truth = actual body vs body_full lengths for rows where
    //   body_full IS NOT NULL (the pre-compression original is stored there).
    //   saved_pct = 1 - sum(len(body)) / sum(len(body_full)) over those rows.
    let (compressed_count, saved_chars, saved_pct) = {
        let mut stmt = conn
            .prepare(
                "SELECT LENGTH(body), LENGTH(body_full) FROM memories \
                  WHERE project = ?1 AND body_full IS NOT NULL",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![q.project], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut count: i64 = 0;
        let mut sum_body: i64 = 0;
        let mut sum_full: i64 = 0;
        for r in rows.flatten() {
            let (body_len, full_len) = r;
            if full_len > 0 {
                count += 1;
                sum_body += body_len;
                sum_full += full_len;
            }
        }
        let saved = (sum_full - sum_body).max(0);
        let pct = if sum_full > 0 {
            ((1.0 - sum_body as f64 / sum_full as f64) * 100.0 * 10.0).round() / 10.0
        } else {
            0.0
        };
        (count, saved, pct)
    };

    // by_day: group created_at (Unix timestamp integer) by calendar date (YYYY-MM-DD).
    let by_day = {
        let mut stmt = conn
            .prepare(
                "SELECT date(datetime(created_at, 'unixepoch')) AS day, COUNT(*) AS cnt \
                 FROM memories WHERE project = ?1 GROUP BY day ORDER BY day",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![q.project], |row| {
                Ok(DayCount {
                    day: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    Ok(Json(MemoryStatsResponse {
        total,
        by_kind,
        compressed_count,
        saved_chars,
        saved_pct,
        by_day,
    }))
}

#[derive(Debug, Deserialize)]
struct MemoryQueueQuery {
    project: String,
}

/// Compression queue: rows that are eligible for LLM compression (body
/// length >= configured min_chars, not yet compressed) but haven't been
/// done. `ready` = the cool-off age has also passed, so the daemon /
/// "compress now" will pick it up; `waiting` rows are still too recent.
async fn memory_queue(
    axum::extract::Query(q): axum::extract::Query<MemoryQueueQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = std::env::var("RTRT_MEMORY_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".rtrt/memory.sqlite"));
    let cfg = rtrt_core::Config::load().unwrap_or_default().auto_compress;
    let conn = rusqlite::Connection::open(&path).map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("memory store: {e}"),
        )
    })?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now - cfg.age_sec;
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, LENGTH(body), created_at FROM memories \
              WHERE project = ?1 AND LENGTH(body) >= ?2 \
                AND (metadata IS NULL OR metadata NOT LIKE '%compressed_at%') \
              ORDER BY created_at ASC LIMIT 200",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![q.project, cfg.min_chars as i64], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let chars: i64 = row.get(2)?;
            let created_at: i64 = row.get(3)?;
            Ok((id, kind, chars, created_at))
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut items = Vec::new();
    let (mut ready, mut waiting) = (0i64, 0i64);
    for r in rows.flatten() {
        let (id, kind, chars, created_at) = r;
        let is_ready = created_at < cutoff;
        if is_ready {
            ready += 1;
        } else {
            waiting += 1;
        }
        // Queue rows are not yet compressed; saved_pct is null (pending).
        items.push(serde_json::json!({
            "id": id,
            "kind": kind,
            "chars": chars,
            "age_min": (now - created_at) / 60,
            "ready": is_ready,
            "saved_pct": serde_json::Value::Null,
        }));
    }
    Ok(Json(serde_json::json!({
        "items": items,
        "ready": ready,
        "waiting": waiting,
        "min_chars": cfg.min_chars,
        "age_sec": cfg.age_sec,
        "enabled": cfg.enabled,
        "model": cfg.model,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/memory/{id}  — full detail for a single memory row
// ---------------------------------------------------------------------------

async fn memory_detail(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> std::result::Result<Json<DetailedRecord>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    match guard
        .get_row(id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        Some(r) => Ok(Json(r)),
        None => Err((StatusCode::NOT_FOUND, format!("memory {id} not found"))),
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/memory/{id}  — governance delete for a single row
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DeleteOneResponse {
    deleted: bool,
    id: i64,
}

async fn memory_delete_one(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> std::result::Result<Json<DeleteOneResponse>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let deleted = guard
        .delete_row(id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if deleted {
        broadcast_event(
            &state.events,
            serde_json::json!({ "type": "memory.delete", "id": id }),
        );
        Ok(Json(DeleteOneResponse { deleted: true, id }))
    } else {
        Err((StatusCode::NOT_FOUND, format!("memory {id} not found")))
    }
}

// ---------------------------------------------------------------------------
// POST /api/memory/delete  — governance batch delete
// Body: { "ids": [i64, …] }
// Response: { "deleted": N }
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeleteBatchRequest {
    ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
struct DeleteBatchResponse {
    deleted: usize,
}

async fn memory_delete_batch(
    State(state): State<AppState>,
    Json(req): Json<DeleteBatchRequest>,
) -> std::result::Result<Json<DeleteBatchResponse>, (StatusCode, String)> {
    if req.ids.is_empty() {
        return Ok(Json(DeleteBatchResponse { deleted: 0 }));
    }
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let deleted = guard
        .delete_rows(&req.ids)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    broadcast_event(
        &state.events,
        serde_json::json!({ "type": "memory.delete_batch", "ids": req.ids, "deleted": deleted }),
    );
    Ok(Json(DeleteBatchResponse { deleted }))
}

// ---------------------------------------------------------------------------
// POST /api/memory/embed
// Body:  { "project": "…" }
// Response: { "embedded": N }
//
// Backfills embeddings for every memory row in `project` that does not yet
// have an entry in the `embeddings` table. Requires embeddings to be enabled
// in the config / env. Returns 503 when no embedder is available.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryEmbedRequest {
    project: String,
}

// Kicks off a NON-BLOCKING background backfill. A big project (20k rows) takes
// many minutes of Ollama calls; doing that under the shared store mutex would
// freeze the whole dashboard. Instead a dedicated thread opens its OWN SQLite
// connection (WAL mode = concurrent with the main one) and backfills there, so
// the UI stays live and `embedding_coverage` reflects progress as rows commit.
async fn memory_embed(
    State(state): State<AppState>,
    Json(req): Json<MemoryEmbedRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.embedder.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "embeddings not enabled".into(),
        ));
    }
    // Dedup: refuse a second concurrent job for the same project.
    {
        let mut jobs = state.embedding_jobs.lock().unwrap();
        if jobs.contains(&req.project) {
            return Ok(Json(
                serde_json::json!({ "started": false, "running": true }),
            ));
        }
        jobs.insert(req.project.clone());
    }

    let path = state.memory_path.clone();
    let project = req.project.clone();
    let jobs = state.embedding_jobs.clone();
    // Resolve embed config for the worker's own embedder.
    let ecfg = rtrt_core::Config::load().unwrap_or_default().embeddings;
    let base_url = ecfg.resolved_base_url(
        rtrt_core::Config::load()
            .ok()
            .and_then(|c| c.auto_compress.base_url)
            .as_deref(),
    );
    let model = ecfg.effective_model();

    std::thread::spawn(move || {
        match MemoryStore::open(&path) {
            Ok(store) => {
                let embedder = rtrt_memory::OllamaEmbedder::new(base_url, model);
                match store.backfill_embeddings(&project, &embedder) {
                    Ok(n) => tracing::info!("embedding backfill done: {project} (+{n})"),
                    Err(e) => tracing::warn!("embedding backfill failed for {project}: {e}"),
                }
            }
            Err(e) => tracing::warn!("embedding backfill: open store failed: {e}"),
        }
        jobs.lock().unwrap().remove(&project);
    });

    Ok(Json(
        serde_json::json!({ "started": true, "running": true }),
    ))
}

#[derive(Debug, Deserialize)]
struct MemoryCoverageQuery {
    project: String,
}

// GET /api/memory/coverage?project=X -> { embedded, total, running }
// Lets the UI poll embedding progress while a background backfill runs.
async fn memory_coverage(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryCoverageQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let (embedded, total) = {
        let guard = store.lock().await;
        guard
            .embedding_coverage(&q.project)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let running = state.embedding_jobs.lock().unwrap().contains(&q.project);
    Ok(Json(
        serde_json::json!({ "embedded": embedded, "total": total, "running": running }),
    ))
}

// ---------------------------------------------------------------------------
// POST /api/memory/entities
// Body:  { "project": "…", "model": "…" }  (model is optional)
// Response: { "edges": N }
//
// Runs the LLM-driven entity-extraction + edge-linking pass for `project`.
// The gateway's configured model (or the explicit `model` field) is used.
// Returns 503 when no gateway provider is available.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryEntitiesRequest {
    project: String,
    #[serde(default)]
    model: Option<String>,
}

async fn memory_entities(
    State(state): State<AppState>,
    Json(req): Json<MemoryEntitiesRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;

    // Resolve the model: explicit arg → config auto_compress model → fallback.
    let model = req.model.clone().unwrap_or_else(|| {
        rtrt_core::Config::load()
            .ok()
            .map(|c| c.auto_compress.model)
            .unwrap_or_else(|| "claude-haiku-4-5".to_string())
    });

    // Build a per-request gateway the same way the auto-compress daemon does,
    // so the config base_url is honoured even when env vars are absent.
    let llm_gateway = {
        let env_has_url = std::env::var_os("RTRT_PROVIDER_BASE_URL").is_some()
            || std::env::var_os("RTRT_OPENAI_COMPAT_URL").is_some();
        if env_has_url {
            Arc::new(rtrt_providers::Gateway::from_env())
        } else {
            let cfg_url = rtrt_core::Config::load()
                .ok()
                .and_then(|c| c.auto_compress.base_url);
            Arc::new(if let Some(url) = cfg_url {
                unsafe { std::env::set_var("RTRT_PROVIDER_BASE_URL", &url) };
                let gw = rtrt_providers::Gateway::from_env();
                unsafe { std::env::remove_var("RTRT_PROVIDER_BASE_URL") };
                gw
            } else {
                rtrt_providers::Gateway::from_env()
            })
        }
    };

    let summariser = rtrt_memory::LlmSummariser::new(Box::new(GatewayAdapter(llm_gateway)), model);

    // `MemoryStore` is `!Sync` (rusqlite `Connection`), so no `&MemoryStore`
    // borrow may live across an `.await`. Mirror the auto-compress daemon: do
    // the SQLite reads under the lock, drop it for the async LLM extraction,
    // then re-lock for the synchronous edge writes.
    let sources: Vec<(i64, String)> = {
        let guard = store.lock().await;
        guard
            .list_by_project(&req.project, 10_000)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .into_iter()
            .map(|m| (m.id, m.body))
            .collect()
    };

    let mut extracted: Vec<(i64, Vec<String>)> = Vec::with_capacity(sources.len());
    for (id, body) in &sources {
        let entities = summariser
            .extract_entities(body)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        extracted.push((*id, entities));
    }

    let new_edges = {
        let guard = store.lock().await;
        guard
            .link_extracted_bipartite(&req.project, &extracted)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    Ok(Json(serde_json::json!({ "edges": new_edges })))
}

// ---------------------------------------------------------------------------
// Ollama model management  (/api/ollama/*)
//
// All endpoints resolve the Ollama base URL via `ollama_base()`:
//   embeddings.base_url → auto_compress.base_url → http://127.0.0.1:11434
// Trailing `/v1` is stripped so the same URL works for both the
// OpenAI-compat chat path and the native Ollama API.
// ---------------------------------------------------------------------------

/// Resolve and normalise the Ollama base URL (no trailing slash, no `/v1`).
fn ollama_base() -> String {
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    let raw = cfg
        .embeddings
        .resolved_base_url(cfg.auto_compress.base_url.as_deref());
    let mut url = raw.trim_end_matches('/').to_string();
    if url.ends_with("/v1") {
        url.truncate(url.len() - 3);
    }
    url.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// GET /api/ollama/models  →  GET {base}/api/tags
// Response: [{ "name": String, "size_bytes": u64, "family": String, "modified_at": String }]
// ---------------------------------------------------------------------------

async fn ollama_models() -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let url = format!("{}/api/tags", ollama_base());
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama unreachable: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ollama /api/tags {code}: {body}"),
        ));
    }
    let raw: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama decode: {e}")))?;
    let models: Vec<serde_json::Value> = raw
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let family = m
                .get("details")
                .and_then(|d| d.get("family"))
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string();
            serde_json::json!({
                "name":        m.get("name").cloned().unwrap_or(serde_json::Value::Null),
                "size_bytes":  m.get("size").cloned().unwrap_or(serde_json::Value::Null),
                "family":      family,
                "modified_at": m.get("modified_at").cloned().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "models": models })))
}

// ---------------------------------------------------------------------------
// GET /api/ollama/ps  →  GET {base}/api/ps
// Response: [{ "name": String, "size_bytes": u64, "size_vram_bytes": u64, "until": String }]
// ---------------------------------------------------------------------------

async fn ollama_ps() -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let url = format!("{}/api/ps", ollama_base());
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama unreachable: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ollama /api/ps {code}: {body}"),
        ));
    }
    let raw: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama decode: {e}")))?;
    let models: Vec<serde_json::Value> = raw
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            // size_vram is in the `size_vram` field from Ollama /api/ps.
            let size_vram = m
                .get("size_vram")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "name":            m.get("name").cloned().unwrap_or(serde_json::Value::Null),
                "size_bytes":      m.get("size").cloned().unwrap_or(serde_json::Value::Null),
                "size_vram_bytes": size_vram,
                "until":           m.get("expires_at").cloned().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "models": models })))
}

// ---------------------------------------------------------------------------
// POST /api/ollama/pull
// Body:    { "name": "bge-m3" }
// Response: { "ok": bool, "status": String }
//
// Uses stream:false — Ollama returns a single JSON object when done.
// Client timeout is intentionally large (pulls take minutes).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OllamaNameRequest {
    name: String,
}

async fn ollama_pull(
    Json(req): Json<OllamaNameRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let url = format!("{}/api/pull", ollama_base());
    // No timeout — model pulls can take many minutes on slow connections.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let body = serde_json::json!({ "name": req.name, "stream": false });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama unreachable: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ollama /api/pull {code}: {text}"),
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama decode: {e}")))?;
    if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
        return Err((StatusCode::BAD_GATEWAY, err.to_string()));
    }
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("success")
        .to_string();
    Ok(Json(serde_json::json!({ "ok": true, "status": status })))
}

// ---------------------------------------------------------------------------
// DELETE /api/ollama/{name}
// Path param: model name (URL-encoded, e.g. `bge-m3%3Alatest`).
// Response: { "ok": bool }
// ---------------------------------------------------------------------------

async fn ollama_delete(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let url = format!("{}/api/delete", ollama_base());
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "name": name });
    let resp = client
        .delete(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ollama unreachable: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ollama /api/delete {code}: {text}"),
        ));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

const INDEX_HTML: &str = include_str!("../ui/index.html");

// Vendored graph libraries (served at /vendor/*) so the map needs no CDN.
const VENDOR_CYTOSCAPE: &str = include_str!("../ui/vendor/cytoscape.min.js");
const VENDOR_LAYOUT_BASE: &str = include_str!("../ui/vendor/layout-base.js");
const VENDOR_COSE_BASE: &str = include_str!("../ui/vendor/cose-base.js");
const VENDOR_FCOSE: &str = include_str!("../ui/vendor/cytoscape-fcose.js");
const VENDOR_COLA: &str = include_str!("../ui/vendor/cola.min.js");
const VENDOR_CYTO_COLA: &str = include_str!("../ui/vendor/cytoscape-cola.js");
