//! rtrt-core — shared types for the RTRT toolkit.
//!
//! Stable surface: [`Error`], [`Result`], [`CompressionLevel`], [`TokenCount`],
//! [`Plugin`], [`Config`].

pub mod config;
pub mod dashboard_bootstrap;
pub mod detect;
pub mod error;
mod model_cache;
pub mod output_style;
pub mod plugin;
pub mod pool;
pub mod project;
pub mod token;

pub use config::{
    AgentsConfig, Balance, ClaudeContinuity, Config, DEFAULT_API_MAX_TOKENS,
    DEFAULT_CLAUDE_CONTINUITY_MAX_RESUMED_TURNS, DEFAULT_CLAUDE_CONTINUITY_TTL_SECS,
    DEFAULT_TEAM_MAX_RETRIES, Delegation, LimitsConfig, MAX_CLAUDE_CONTINUITY_RESUMED_TURNS,
    MAX_CLAUDE_CONTINUITY_TTL_SECS, MIN_CLAUDE_CONTINUITY_TTL_SECS, PoolLimit, ProjectConfig,
    ProjectEntry, ProvidersConfig, RosterPreset, TargetLimit, TeamConfig, TeamMember, TeamMode,
    TeamPolicy, TierMap, default_memory_store_path, legacy_memory_store_path, repo_root_from,
};
pub use detect::{
    Capability, CostClass, DetectedTool, InvocationMode, ToolKind, detect_tools,
    detect_tools_with_config, refresh_cli_model_cache,
};
pub use error::{Error, Result};
pub use output_style::{
    OutputStyleLevel, output_style_path, read_output_style_level, read_output_style_level_for,
    write_output_style_level, write_output_style_level_for,
};
pub use plugin::{Plugin, PluginKind, PluginMetadata};
pub use pool::{PoolKey, pool_from_model};
pub use project::{
    CLAUDE_AF_UNIX_PATH_MAX_BYTES, ProjectIdentity, claude_runtime_tmp_dir, project_for_cwd,
    project_for_cwd_str, project_memory_db_path, project_memory_db_path_in, project_storage_dir,
    project_storage_dir_in, resolve_runtime_tmp_dir, runtime_tmp_dir, write_private_file_atomic,
};
pub use token::{CompressionLevel, TokenCount, TokenStats};
