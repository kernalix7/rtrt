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

pub use config::{Config, ProjectConfig, ProjectEntry};
pub use detect::{Capability, CostClass, DetectedTool, InvocationMode, ToolKind, detect_tools};
pub use error::{Error, Result};
pub use output_style::{
    OutputStyleLevel, output_style_path, read_output_style_level, read_output_style_level_for,
    write_output_style_level, write_output_style_level_for,
};
pub use plugin::{Plugin, PluginKind, PluginMetadata};
pub use project::{project_for_cwd, project_for_cwd_str};
pub use token::{CompressionLevel, TokenCount, TokenStats};
