//! The single place that wires every route to its handler. Pure refactor — the
//! route table is byte-for-byte the same as the original monolithic `main.rs`
//! Router build (same paths, same methods, same handler fns).

use std::sync::Arc;

use axum::{
    Router,
    routing::{delete, get, post},
};

use crate::prelude::*;
use crate::state::AppState;
use crate::util::bearer_guard;

/// Build the application router for `state`, applying the optional bearer-token
/// middleware (`RTRT_DASHBOARD_TOKEN`) exactly as the original binary did.
pub(crate) fn router(state: AppState, token: Option<String>) -> Router {
    let token_arc = token.map(Arc::new);
    Router::new()
        .route("/", get(index))
        .route("/assets/styles.css", get(asset_styles_css))
        .route("/assets/js/api.js", get(asset_js_api))
        .route("/assets/js/components.js", get(asset_js_components))
        .route("/assets/js/pages.js", get(asset_js_pages))
        .route("/assets/js/app.js", get(asset_js_app))
        .route("/vendor/{file}", get(vendor_asset))
        .route("/healthz", get(healthz))
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
        .with_state(state)
}
