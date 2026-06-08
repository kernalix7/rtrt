//! rtrt-core — shared types for the RTRT toolkit.
//!
//! Stable surface: [`Error`], [`Result`], [`CompressionLevel`], [`TokenCount`],
//! [`Plugin`], [`Config`].

pub mod config;
pub mod error;
pub mod plugin;
pub mod project;
pub mod token;

pub use config::{Config, ProjectEntry};
pub use error::{Error, Result};
pub use plugin::{Plugin, PluginKind, PluginMetadata};
pub use project::{project_for_cwd, project_for_cwd_str};
pub use token::{CompressionLevel, TokenCount, TokenStats};
