//! rtrt-core — shared types for the RTRT toolkit.
//!
//! Stable surface: [`Error`], [`Result`], [`CompressionLevel`], [`TokenCount`],
//! [`Plugin`], [`Config`].

pub mod config;
pub mod detect;
pub mod error;
pub mod output_style;
pub mod plugin;
pub mod project;
pub mod token;

pub use config::{
    AgentsConfig, Config, DEFAULT_API_MAX_TOKENS, LimitsConfig, ProjectConfig, ProjectEntry,
    ProvidersConfig, TargetLimit, repo_root_from,
};
pub use detect::{
    Capability, CostClass, DetectedTool, InvocationMode, ToolKind, detect_tools,
    detect_tools_with_config,
};
pub use error::{Error, Result};
pub use output_style::{
    OutputStyleLevel, output_style_path, read_output_style_level, read_output_style_level_for,
    write_output_style_level, write_output_style_level_for,
};
pub use plugin::{Plugin, PluginKind, PluginMetadata};
pub use project::{project_for_cwd, project_for_cwd_str};
pub use token::{CompressionLevel, TokenCount, TokenStats};
