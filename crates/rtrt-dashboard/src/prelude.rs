//! Crate-internal prelude — re-exports every module's `pub(crate)` items so
//! handler modules can `use crate::prelude::*;` and see the same flat namespace
//! the original single-file binary had. Pure refactor glue.
#![allow(unused_imports)]

pub(crate) use crate::assets::{
    ASSET_JS_API, ASSET_JS_APP, ASSET_JS_COMPONENTS, ASSET_JS_PAGES, ASSET_STYLES_CSS, INDEX_HTML,
    VENDOR_COLA, VENDOR_COSE_BASE, VENDOR_CYTO_COLA, VENDOR_CYTOSCAPE, VENDOR_FCOSE,
    VENDOR_LAYOUT_BASE, asset_js_api, asset_js_app, asset_js_components, asset_js_pages,
    asset_response, asset_styles_css, index, vendor_asset,
};
pub(crate) use crate::daemons::{
    spawn_auto_compress_daemon, spawn_auto_embed_daemon, spawn_consolidation_daemon,
};
pub(crate) use crate::handlers::chat::{
    BudgetResponse, ChatHttpMessage, ChatHttpRequest, ChatHttpResponse, MetricsResponse, budget,
    chat, metrics, sse_stream, tokens_summary,
};
pub(crate) use crate::handlers::compress::{
    CompressRequest, DIAGNOSE_SYS, DiagnoseRequest, LLM_COMPRESS_SYS, ProxyRequest, RepoMapRequest,
    SetupRequest, compress, default_context, default_ext, default_max_bytes, diagnose,
    proxy_filter, repo_map, setup_snippet,
};
pub(crate) use crate::handlers::config::{
    ConfigResponse, ConfigWriteRequest, ConfigWriteResponse, MemorySettingsResponse, ModelEntry,
    ModelsResponse, SetMemorySettingsRequest, get_config, get_memory_settings, get_models,
    post_config, post_memory_settings,
};
pub(crate) use crate::handlers::limits::{
    LimitTargetView, LimitsConfigResponse, SetLimitsRequest, get_limits_config, post_limits_config,
};
pub(crate) use crate::handlers::memgraph::{
    BRAIN_MIN_COOCCUR, CACHE_KEY_SEP, CLUSTER_MAX_NODES, CLUSTER_MIN_WEIGHT, CLUSTER_TOP_K,
    GLOBAL_PROJECT_SENTINEL, MemoryGraphQuery, brain_hierarchy_cached, brain_scope,
    cluster_index_cached, default_graph_limit, emit_group_response, memory_graph,
    memory_graph_brain, memory_graph_brain_community, memory_graph_brain_concept,
    memory_graph_drill, memory_graph_leaf, memory_graph_overview, page_buckets,
};
pub(crate) use crate::handlers::memory::{
    DayCount, DeleteBatchRequest, DeleteBatchResponse, DeleteOneResponse, GetBlockQuery, KindCount,
    ListBlocksQuery, MemoryCompressRequest, MemoryCompressResponse, MemoryCoverageQuery,
    MemoryEmbedRequest, MemoryEntitiesRequest, MemoryExportQuery, MemoryQueueQuery,
    MemoryRecallRequest, MemorySaveRequest, MemoryStatsQuery, MemoryStatsResponse,
    MemoryTimelineQuery, SetBlockRequest, default_kind, default_recall_limit,
    default_timeline_limit, get_block, list_blocks, memory_compress, memory_coverage,
    memory_delete_batch, memory_delete_one, memory_detail, memory_embed, memory_entities,
    memory_export, memory_projects, memory_queue, memory_recall, memory_save, memory_stats,
    memory_timeline, set_block,
};
pub(crate) use crate::handlers::ollama::{
    OllamaNameRequest, ollama_base, ollama_delete, ollama_models, ollama_ps, ollama_pull,
};
pub(crate) use crate::handlers::orch::{
    DetectToggleRequest, DetectToggleResponse, DetectedToolResponse, RouteApiResponse, RouteChosen,
    RouteQuery, RouteTargetHeadroom, RouteUsageHeadroom, detect_tools_api,
    detected_tools_with_config_overrides, parse_route_capability, parse_route_prefer,
    provider_api_key_present, route_api, route_error, route_usage_headroom, toggle_detected_tool,
};
pub(crate) use crate::handlers::projects::{
    ProjectUpsertReq, ProjectView, list_projects, upsert_project,
};
pub(crate) use crate::handlers::prompts::{
    PromptSummary, get_prompt, list_prompt_versions, list_prompts, require_prompts,
};
pub(crate) use crate::handlers::savings::{
    DashboardWindow, GAIN_MAX_ROWS, GAIN_MIN_ROWS, GainCommand, GainProject, GainQuery, GainRun,
    OUTPUT_OPTIMIZER_MEASUREMENT_NOTE, OverviewQuery, ProjectRollup, ProjectSourceRollup,
    SavingsCoverage, SavingsProject, SavingsSource, command_savings_by_project, dynamic_gain_count,
    gain, integer_sqrt_ceil, load_gain_projects, load_gain_recent, load_gain_top_commands,
    memory_savings_by_project, nonnegative_i64, optimizer_overview, output_optimizer_active,
    output_savings_by_project, projects_rollup, savings_source, usize_to_i64,
};
pub(crate) use crate::handlers::scope::{
    ProjectQuery, SetAgentsRequest, SetCompressionRequest, SetEmbeddingsProjectRequest,
    SetLevelRequest, SetProvidersRequest, SetSecurityProjectRequest, agent_kind_label,
    compression_level_label, effective_agent_tools, effective_provider_tools, get_agents_config,
    get_compression_config, get_embeddings_project, get_optimizer_level, get_providers_config,
    get_security_project, parse_compression_level, post_agents_config, post_compression_config,
    post_embeddings_project, post_optimizer_level, post_providers_config, post_security_project,
    resolve_project_name, resolve_project_repo,
};
pub(crate) use crate::handlers::security::{
    ProfileSaveReq, SecurityScanRequest, security_profile, security_profile_save,
    security_profiles, security_scan,
};
pub(crate) use crate::handlers::statusline::{
    LEGACY_STATUSLINE_FORMAT, LEGACY_STATUSLINE_SEGMENTS, STATUSLINE_DEFAULT_CODEX_TIMEOUT_MS,
    STATUSLINE_DEFAULT_FORMAT, STATUSLINE_DEFAULT_LINE2_FORMAT, STATUSLINE_DEFAULT_LINE3_FORMAT,
    STATUSLINE_SEGMENTS, StatuslineConfig, StatuslineConfigResponse, StatuslineOk,
    StatuslinePreview, default_statusline_codex_timeout_ms, default_statusline_format,
    default_statusline_line2_format, default_statusline_line3_format, default_statusline_segments,
    get_statusline_config, parse_config_toml, post_statusline_config,
    read_global_statusline_config, run_statusline_preview, statusline_config_path,
    statusline_preview, upgrade_legacy_statusline_config, validate_statusline_segments,
};
pub(crate) use crate::handlers::templates::{
    ScaffoldPreviewFile, ScaffoldPreviewResponse, ScaffoldRequest, ScaffoldResponse,
    TemplateSummary, TemplateUpsertRequest, create_template, default_template_category,
    delete_template, get_template, is_builtin_template, is_safe_template_file_path,
    is_safe_template_slug, list_templates, parse_template_category, scaffold, scaffold_preview,
    template_write_error_status, update_template, validate_template_manifest,
    validate_template_name,
};
pub(crate) use crate::handlers::usage::{
    RoutePreviewQuery, UsageHeadroomView, UsageResponse, UsageTargetView, UsageWindowView,
    UsageWindowsView, route_preview_api, usage_api,
};
pub(crate) use crate::state::{
    AppState, CLUSTER_INDEX_TTL, GatewayAdapter, LEVEL_TOKEN_SEQ, LEVEL_TOKEN_TTL, STALL_DOMINANCE,
    TokenEntry, broadcast_event, compress_saved_pct_from_meta, dynamic_branch, dynamic_leaf,
    memory_store_path, mint_level_token, open_memory_store, open_prompt_registry,
};
pub(crate) use crate::util::{
    ApiError, DashboardJsonResult, SECS_PER_DAY, SECS_PER_HOUR, api_error, bearer_guard,
    clear_field_error, constant_time_eq, estimate_saved_tokens, healthz, json_i64,
    metadata_savings, normalize_project, proxy_stats_path, saved_pct, saved_pct_or_zero, stats,
    walk_files, write_config_file,
};
