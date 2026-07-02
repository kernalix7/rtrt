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

mod assets;
mod daemons;
mod handlers;
mod prelude;
mod routes;
mod state;
mod transcripts;
mod util;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::Result;
use rtrt_memory::Embedder;
use rtrt_providers::Gateway;
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use crate::daemons::{
    spawn_auto_compress_daemon, spawn_auto_embed_daemon, spawn_consolidation_daemon,
};
use crate::state::{AppState, memory_store_path, open_memory_store, open_prompt_registry};

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

    let app = routes::router(state, token);

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
