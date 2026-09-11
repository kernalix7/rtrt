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
//! All `/api/*` routes require a bearer token. The bundled SPA assets and
//! `/healthz` remain open so the UI can bootstrap and request that token.

mod assets;
mod daemons;
mod handlers;
mod opencode_transcripts;
mod prelude;
mod project_catalog;
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
use tokio::sync::broadcast;

use crate::project_catalog::ProjectCatalog;
use crate::state::{AppState, open_prompt_registry};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info,tower_http=info")
        .init();

    let startup = MachineStartup::from_process()?;
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    let bind = std::env::var("RTRT_DASHBOARD_BIND").unwrap_or_else(|_| cfg.dashboard.bind.clone());
    validate_loopback_bind(&bind)?;
    let token = startup.token;
    let catalog = Arc::new(ProjectCatalog::new(
        startup.home.clone(),
        startup.projects_root,
    ));
    catalog.refresh();
    let gateway = Arc::new(Gateway::from_env());
    let prompts = open_prompt_registry();
    let auto_capture = std::env::var("RTRT_AUTO_CAPTURE")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(cfg.capture.enabled);
    let auto_redact = std::env::var("RTRT_AUTO_REDACT")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(cfg.capture.redact);
    let dedup_window_sec: i64 = std::env::var("RTRT_AUTO_DEDUP_WINDOW_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.capture.dedup_window_sec);
    let session_id = uuid::Uuid::new_v4().to_string();
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
    let state = AppState::machine(
        gateway,
        prompts,
        auto_capture,
        auto_redact,
        session_id,
        dedup_window_sec,
        events_tx,
        embedder,
        catalog.clone(),
        startup.home,
    )?;
    state.start_project_daemons();

    let app = routes::router_for_bind(state, token, &bind)?;

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

#[derive(Debug)]
struct MachineStartup {
    home: std::path::PathBuf,
    projects_root: std::path::PathBuf,
    token: String,
}

impl MachineStartup {
    fn from_process() -> Result<Self> {
        Self::parse(std::env::args_os().skip(1))
    }

    fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let args: Vec<_> = args.into_iter().collect();
        anyhow::ensure!(
            args.len() == 3 && args[0] == "--machine" && args[1] == "--state-dir",
            "usage: rtrt-dashboard --machine --state-dir <home>/.rtrt/dashboard"
        );
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve operator home"))?;
        let home = std::fs::canonicalize(home)?;
        let expected = home.join(".rtrt/dashboard");
        anyhow::ensure!(
            args[2].as_os_str() == expected.as_os_str(),
            "invalid dashboard state directory"
        );
        validate_private_directory(&home.join(".rtrt"))?;
        validate_private_directory(&expected)?;
        anyhow::ensure!(
            std::fs::canonicalize(&expected)? == expected,
            "dashboard state directory must not traverse links"
        );
        let env_path = expected.join("dashboard.env");
        let metadata = std::fs::symlink_metadata(&env_path)
            .map_err(|_| anyhow::anyhow!("dashboard credential unavailable"))?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "dashboard credential unavailable"
        );
        validate_private_mode(&metadata, 0o600)?;
        anyhow::ensure!(
            metadata.len() <= 16 * 1024,
            "dashboard credential unavailable"
        );
        let raw = std::fs::read_to_string(&env_path)
            .map_err(|_| anyhow::anyhow!("dashboard credential unavailable"))?;
        let token = raw
            .lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(key, value)| {
                (key.trim() == "RTRT_DASHBOARD_TOKEN")
                    .then(|| value.trim().trim_matches(['\'', '"']).to_string())
            })
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("dashboard credential unavailable"))?;
        Ok(Self {
            projects_root: home.join(".rtrt/projects"),
            home,
            token,
        })
    }
}

fn validate_private_directory(path: &std::path::Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "dashboard state directory must be a real directory"
    );
    validate_private_mode(&metadata, 0o700)
}

#[cfg(unix)]
fn validate_private_mode(metadata: &std::fs::Metadata, expected: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    anyhow::ensure!(
        metadata.uid() == unsafe { libc_geteuid() },
        "dashboard state ownership mismatch"
    );
    anyhow::ensure!(
        metadata.permissions().mode() & 0o777 == expected,
        "dashboard state permissions are not private"
    );
    Ok(())
}

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}
#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    unsafe { geteuid() }
}

#[cfg(not(unix))]
fn validate_private_mode(_: &std::fs::Metadata, _: u32) -> Result<()> {
    Ok(())
}

fn validate_loopback_bind(bind: &str) -> Result<()> {
    if let Ok(address) = bind.parse::<std::net::SocketAddr>() {
        anyhow::ensure!(
            address.ip().is_loopback(),
            "dashboard bind must be loopback"
        );
    } else {
        let host = bind.rsplit_once(':').map(|(host, _)| host).unwrap_or("");
        anyhow::ensure!(
            matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]"),
            "dashboard bind must be loopback"
        );
    }
    Ok(())
}

#[cfg(test)]
fn dashboard_token() -> Result<String> {
    std::env::var("RTRT_DASHBOARD_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("dashboard requires nonempty RTRT_DASHBOARD_TOKEN"))
}

#[cfg(test)]
fn validate_scope(scope: &str, token: Option<&str>) -> Result<()> {
    if scope == "admin" {
        if token.is_none_or(|value| value.trim().is_empty()) {
            anyhow::bail!("RTRT_DASHBOARD_SCOPE=admin requires nonempty RTRT_DASHBOARD_TOKEN");
        }
        anyhow::bail!("admin dashboard store routing is not implemented; refusing to start");
    }
    if scope != "project" {
        anyhow::bail!("RTRT_DASHBOARD_SCOPE must be `project` (admin is unavailable)");
    }
    Ok(())
}
