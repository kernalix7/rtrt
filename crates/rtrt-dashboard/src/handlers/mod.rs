//! HTTP handler modules, one file per domain. Pure refactor — every handler
//! is the same code that previously lived in the monolithic `main.rs`.

pub(crate) mod chat;
pub(crate) mod compress;
pub(crate) mod config;
pub(crate) mod limits;
pub(crate) mod memgraph;
pub(crate) mod memory;
pub(crate) mod ollama;
pub(crate) mod orch;
pub(crate) mod projects;
pub(crate) mod prompts;
pub(crate) mod savings;
pub(crate) mod scope;
pub(crate) mod security;
pub(crate) mod statusline;
pub(crate) mod templates;
pub(crate) mod usage;
