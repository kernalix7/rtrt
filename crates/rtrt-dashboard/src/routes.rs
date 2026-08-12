//! The single place that wires every route to its handler. Pure refactor — the
//! route table is byte-for-byte the same as the original monolithic `main.rs`
//! Router build (same paths, same methods, same handler fns).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use axum::{
    Router,
    http::{Method, StatusCode, header},
    response::IntoResponse,
    routing::{delete, get, post},
};

use crate::prelude::*;
use crate::state::AppState;
use crate::util::bearer_guard;

/// Test/default builder. Production uses [`router_for_bind`] so browser origins
/// are derived once from startup configuration, never from `Host`.
#[cfg(test)]
pub(crate) fn router(state: AppState, token: Option<String>) -> Router {
    router_with_origins(state, token, fixed_origins("127.0.0.1:7311").unwrap())
}

pub(crate) fn router_for_bind(
    state: AppState,
    token: String,
    bind: &str,
) -> anyhow::Result<Router> {
    Ok(router_with_origins(
        state,
        Some(token),
        fixed_origins(bind)?,
    ))
}

fn router_with_origins(
    state: AppState,
    token: Option<String>,
    allowed_origins: Arc<std::collections::HashSet<String>>,
) -> Router {
    let token_arc = token.map(Arc::new);
    let bootstrap_token = token_arc.clone();
    let bootstrap_replays = Arc::new(tokio::sync::Mutex::new(BootstrapReplayCache::default()));
    let selector_state = state.clone();
    Router::new()
        .route("/", get(index))
        .route("/assets/styles.css", get(asset_styles_css))
        .route("/assets/js/api.js", get(asset_js_api))
        .route("/assets/js/components.js", get(asset_js_components))
        .route("/assets/js/pages.js", get(asset_js_pages))
        .route("/assets/js/orchestration.js", get(asset_js_orchestration))
        .route("/assets/js/app.js", get(asset_js_app))
        .route("/vendor/{file}", get(vendor_asset))
        .route("/healthz", get(healthz))
        .route(
            "/api/auth/bootstrap",
            post(move |request: axum::extract::Request| {
                exchange_bootstrap(
                    bootstrap_token.clone(),
                    bootstrap_replays.clone(),
                    request,
                )
            })
            .layer(axum::extract::DefaultBodyLimit::max(256)),
        )
        .route("/api/stats", get(stats))
        .route("/api/overview", get(optimizer_overview))
        .route("/api/optimizer/overview", get(optimizer_overview))
        .route("/api/gain", get(gain))
        .route("/api/detect", get(detect_tools_api))
        .route("/api/route", get(route_api))
        .route("/api/route/preview", get(route_preview_api))
        .route("/api/usage", get(usage_api))
        .route("/api/detect/toggle", post(toggle_detected_tool))
        .route(
            "/api/optimizer/level",
            get(get_optimizer_level).post(post_optimizer_level),
        )
        .route(
            "/api/compression/config",
            get(get_compression_config).post(post_compression_config),
        )
        .route(
            "/api/providers/config",
            get(get_providers_config).post(post_providers_config),
        )
        .route(
            "/api/agents/config",
            get(get_agents_config).post(post_agents_config),
        )
        .route(
            "/api/embeddings/project",
            get(get_embeddings_project).post(post_embeddings_project),
        )
        .route(
            "/api/security/project",
            get(get_security_project).post(post_security_project),
        )
        .route(
            "/api/limits/config",
            get(get_limits_config).post(post_limits_config),
        )
        // Provider failover markers.
        .route(
            "/api/failover/config",
            get(get_failover_config).post(post_failover_config),
        )
        .route(
            "/api/memory/settings",
            get(get_memory_settings).post(post_memory_settings),
        )
        .route("/api/templates", get(list_templates).post(create_template))
        .route(
            "/api/templates/{name}",
            get(get_template)
                .put(update_template)
                .delete(delete_template),
        )
        // Scaffolding accepts arbitrary destinations in the legacy handler.
        // Keep it unavailable until that surface takes AppState containment.
        .route("/api/templates/scaffold", post(safe_scaffold))
        .route("/api/templates/scaffold/preview", post(safe_scaffold_preview))
        .route("/api/chat", post(chat))
        .route("/api/metrics", get(metrics))
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts/{name}", get(list_prompt_versions))
        .route("/api/prompts/{name}/{version}", get(get_prompt))
        .route("/api/budget", get(budget))
        .route("/api/memory/projects", get(memory_projects))
        .route("/api/memory/timeline", get(memory_timeline))
        .route("/api/memory/sessions", get(memory_sessions))
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
        .route("/api/projects", get(list_projects).put(project_mode_unavailable))
        .route("/api/projects/overview", get(projects_overview))
        .route("/api/projects/hidden", get(project_mode_unavailable))
        .route("/api/projects/reassign", post(project_mode_unavailable))
        .route(
            "/api/memory/{id}",
            get(memory_detail).delete(memory_delete_one),
        )
        // SPA deep-link catch-all: any path that didn't match an explicit route
        // above (and isn't /api/* or /assets/*) serves the same index.html shell
        // as `GET /`, so refreshing a deep URL (e.g. /memory/search) returns the
        // app, which then restores the page from the path. Declared before the
        // bearer layer so the guard still wraps it; the guard already allows SPA
        // shell paths to bootstrap while keeping /api + /assets token-protected.
        .fallback(spa_fallback)
        .layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let state = selector_state.clone();
                async move {
                    if is_global_route(req.method(), req.uri().path()) {
                        req.extensions_mut().insert(state);
                        return next.run(req).await;
                    }
                    let slug = match selected_slug(&req) {
                        Ok(slug) => slug,
                        Err(response) => return response.into_response(),
                    };
                    let mut context = state.catalog.get(&slug);
                    if context.is_none() {
                        state.catalog.refresh();
                        state.start_project_daemons();
                        context = state.catalog.get(&slug);
                    }
                    let Some(context) = context else {
                        return (StatusCode::NOT_FOUND, "unknown project selector").into_response();
                    };
                    req.extensions_mut().insert(state.with_context(&context));
                    next.run(req).await
                }
            },
        ))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
            let allowed_origins = allowed_origins.clone();
            async move {
                if let Err(status) = validate_origin(&req, &allowed_origins) {
                    return (status, "origin validation failed").into_response();
                }
                next.run(req).await
            }
            },
        ))
        // Last layer is outermost: authenticate before revealing origin policy.
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                bearer_guard(token_arc.clone(), req, next)
            },
        ))
        .with_state(state)
}

fn is_global_route(method: &Method, path: &str) -> bool {
    !path.starts_with("/api/")
        || path == "/api/auth/bootstrap"
        || (method == Method::GET
            && (path == "/api/projects"
                || path == "/api/projects/overview"
                || path == "/api/templates"
                || path.starts_with("/api/templates/")
                || path == "/api/prompts"
                || path.starts_with("/api/prompts/")
                || path == "/api/metrics"
                || path == "/api/budget"
                || path == "/api/models"
                || path == "/api/ollama/models"
                || path == "/api/ollama/ps"
                || path == "/api/security/profiles"
                || path.starts_with("/api/security/profile/")))
}

fn selected_slug(request: &axum::extract::Request) -> Result<String, (StatusCode, &'static str)> {
    let header_slug = match request.headers().get("X-RTRT-Project") {
        Some(value) => match value.to_str() {
            Ok(value) if crate::project_catalog::valid_slug(value) => Some(value.to_string()),
            _ => {
                return Err((StatusCode::BAD_REQUEST, "malformed project selector"));
            }
        },
        None => None,
    };
    let query_slug = match query_project(request.uri().query()) {
        Ok(value) => value,
        Err(()) => {
            return Err((StatusCode::BAD_REQUEST, "malformed project selector"));
        }
    };
    if let (Some(header), Some(query)) = (&header_slug, &query_slug)
        && header != query
    {
        return Err((StatusCode::BAD_REQUEST, "project selectors do not match"));
    }
    header_slug
        .or(query_slug)
        .ok_or((StatusCode::BAD_REQUEST, "project selector required"))
}

fn query_project(query: Option<&str>) -> Result<Option<String>, ()> {
    let mut found = None;
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|value| !value.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "project" {
            continue;
        }
        if found.is_some() || !crate::project_catalog::valid_slug(value) {
            return Err(());
        }
        found = Some(value.to_string());
    }
    Ok(found)
}

fn fixed_origins(bind: &str) -> anyhow::Result<Arc<std::collections::HashSet<String>>> {
    let (port, configured_origin) = if let Ok(address) = bind.parse::<std::net::SocketAddr>() {
        (
            address.port(),
            (!address.ip().is_unspecified()).then(|| format!("http://{address}")),
        )
    } else {
        let (host, port) = bind
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("RTRT_DASHBOARD_BIND must include a port: {bind}"))?;
        anyhow::ensure!(
            !host.is_empty()
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')),
            "invalid RTRT_DASHBOARD_BIND host: {host}"
        );
        let port = port
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid RTRT_DASHBOARD_BIND port: {port}"))?;
        (port, Some(format!("http://{host}:{port}")))
    };
    let mut origins = std::collections::HashSet::from([
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        format!("http://[::1]:{port}"),
    ]);
    if let Some(origin) = configured_origin {
        origins.insert(origin);
    }
    Ok(Arc::new(origins))
}

async fn project_mode_unavailable() -> impl IntoResponse {
    (
        StatusCode::FORBIDDEN,
        "unavailable in project dashboard mode",
    )
}

async fn safe_scaffold(
    axum::Extension(state): axum::Extension<AppState>,
    axum::Json(mut value): axum::Json<serde_json::Value>,
) -> Result<axum::Json<ScaffoldResponse>, (StatusCode, String)> {
    pin_scaffold_target(&state, &mut value)?;
    let request = serde_json::from_value::<ScaffoldRequest>(value)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    scaffold(axum::Json(request)).await
}

async fn safe_scaffold_preview(
    axum::Extension(state): axum::Extension<AppState>,
    axum::Json(mut value): axum::Json<serde_json::Value>,
) -> Result<axum::Json<ScaffoldPreviewResponse>, (StatusCode, String)> {
    pin_scaffold_target(&state, &mut value)?;
    let request = serde_json::from_value::<ScaffoldRequest>(value)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    scaffold_preview(axum::Json(request)).await
}

fn pin_scaffold_target(
    state: &AppState,
    value: &mut serde_json::Value,
) -> Result<(), (StatusCode, String)> {
    let target = value
        .get("target")
        .and_then(serde_json::Value::as_str)
        .ok_or((StatusCode::BAD_REQUEST, "target is required".into()))?;
    let target = state.contained_write_path(std::path::Path::new(target))?;
    value["target"] = serde_json::Value::String(target.to_string_lossy().into_owned());
    Ok(())
}

fn validate_origin<B>(
    request: &axum::http::Request<B>,
    allowed_origins: &std::collections::HashSet<String>,
) -> Result<(), StatusCode> {
    if !request.uri().path().starts_with("/api/") {
        return Ok(());
    }
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    match origin {
        None if request.uri().path() == "/api/auth/bootstrap" => Err(StatusCode::FORBIDDEN),
        None => Ok(()), // Explicit non-browser bearer client.
        Some(origin) if allowed_origins.contains(origin) => Ok(()),
        Some(_) => Err(StatusCode::FORBIDDEN),
    }
}

const BOOTSTRAP_REPLAY_CAPACITY: usize = 1024;

#[derive(Default)]
struct BootstrapReplayCache {
    order: VecDeque<([u8; 16], u64)>,
    active: HashSet<[u8; 16]>,
}

impl BootstrapReplayCache {
    fn consume(&mut self, nonce: [u8; 16], expires_at: u64, now: u64) -> bool {
        while self.order.front().is_some_and(|(_, expiry)| *expiry < now) {
            if let Some((expired, _)) = self.order.pop_front() {
                self.active.remove(&expired);
            }
        }
        if self.active.contains(&nonce) {
            return false;
        }
        // Fail closed rather than evicting an unexpired nonce: eviction would
        // make that still-valid credential replayable.
        if self.order.len() >= BOOTSTRAP_REPLAY_CAPACITY {
            return false;
        }
        self.active.insert(nonce);
        // Keep expiry order independent of arrival order so an early,
        // long-lived credential cannot block cleanup of later short-lived ones.
        let position = self
            .order
            .iter()
            .position(|(_, expiry)| *expiry > expires_at)
            .unwrap_or(self.order.len());
        self.order.insert(position, (nonce, expires_at));
        true
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapRequest {
    credential: String,
}

#[derive(serde::Serialize)]
struct BootstrapResponse {
    token: String,
}

async fn exchange_bootstrap(
    token: Option<Arc<String>>,
    replay_cache: Arc<tokio::sync::Mutex<BootstrapReplayCache>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};

    let content_type_ok = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("application/json"));
    if !content_type_ok {
        return bootstrap_rejected(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let body = match axum::body::to_bytes(request.into_body(), 256).await {
        Ok(body) => body,
        Err(_) => return bootstrap_rejected(StatusCode::BAD_REQUEST),
    };
    let payload: BootstrapRequest = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return bootstrap_rejected(StatusCode::BAD_REQUEST),
    };
    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => return bootstrap_rejected(StatusCode::UNAUTHORIZED),
    };
    let Some(token) = token else {
        return bootstrap_rejected(StatusCode::UNAUTHORIZED);
    };
    let verified = match rtrt_core::dashboard_bootstrap::verify(&token, &payload.credential, now) {
        Ok(verified) => verified,
        Err(_) => return bootstrap_rejected(StatusCode::UNAUTHORIZED),
    };
    if !replay_cache
        .lock()
        .await
        .consume(verified.nonce, verified.expires_at, now)
    {
        return bootstrap_rejected(StatusCode::UNAUTHORIZED);
    }
    let mut response = axum::Json(BootstrapResponse {
        token: token.as_str().to_owned(),
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

fn bootstrap_rejected(status: StatusCode) -> axum::response::Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        (status, "bootstrap rejected"),
    )
        .into_response()
}

#[cfg(test)]
mod bootstrap_cache_tests {
    use super::*;

    #[test]
    fn replay_cache_is_single_use_and_bounded() {
        let mut cache = BootstrapReplayCache::default();
        assert!(cache.consume([1; 16], 200, 100));
        assert!(!cache.consume([1; 16], 200, 100));
        for value in 0..(BOOTSTRAP_REPLAY_CAPACITY - 1) {
            let mut nonce = [0; 16];
            nonce[..8].copy_from_slice(&(value as u64).to_be_bytes());
            assert!(cache.consume(nonce, 200, 100));
        }
        assert_eq!(cache.order.len(), BOOTSTRAP_REPLAY_CAPACITY);
        assert_eq!(cache.active.len(), BOOTSTRAP_REPLAY_CAPACITY);
        assert!(!cache.consume([2; 16], 200, 100));
    }

    #[test]
    fn replay_cache_cleans_out_of_order_expiries() {
        let mut cache = BootstrapReplayCache::default();
        let long_lived = [1; 16];
        let short_lived = [2; 16];

        assert!(cache.consume(long_lived, 300, 100));
        assert!(cache.consume(short_lived, 150, 100));
        assert!(cache.consume([3; 16], 400, 200));

        assert!(!cache.active.contains(&short_lived));
        assert!(cache.active.contains(&long_lived));
        assert!(!cache.consume(long_lived, 300, 200));
    }

    #[test]
    fn expired_out_of_order_entries_release_capacity() {
        let mut cache = BootstrapReplayCache::default();
        let long_lived = [0xff; 16];
        assert!(cache.consume(long_lived, 1_000, 100));

        for value in 0..(BOOTSTRAP_REPLAY_CAPACITY - 1) {
            let mut nonce = [0; 16];
            nonce[..8].copy_from_slice(&(value as u64).to_be_bytes());
            assert!(cache.consume(nonce, 150, 100));
        }
        assert!(!cache.consume([0xfe; 16], 1_000, 100));

        assert!(cache.consume([0xfe; 16], 1_000, 200));
        assert_eq!(cache.order.len(), 2);
        assert_eq!(cache.active.len(), 2);
        assert!(!cache.consume(long_lived, 1_000, 200));
    }
}
