//! The engine contract. Each scan engine takes the active rules routed to it
//! plus the scan context, and returns findings. Engines are synchronous and
//! self-contained: they read the filesystem under `ctx.root`, never the
//! network (dep advisory data is bundled / read from lockfiles), so a scan is
//! deterministic and offline.

use std::path::{Path, PathBuf};

use crate::finding::Finding;
use crate::profile::Rule;

/// Shared scan context handed to every engine.
pub struct ScanContext {
    /// Absolute root the scan walks.
    pub root: PathBuf,
    /// Relative-path substrings to skip (from profile.exclude + built-in set).
    pub exclude: Vec<String>,
}

impl ScanContext {
    /// True when `rel` (a path relative to `root`) should be skipped.
    pub fn is_excluded(&self, rel: &Path) -> bool {
        let s = rel.to_string_lossy();
        const ALWAYS: &[&str] = &[
            "/.git/",
            "/target/",
            "/node_modules/",
            "/.venv/",
            "/dist/",
            "/build/",
            "/.next/",
            "/vendor/",
        ];
        let padded = format!("/{s}/");
        if ALWAYS.iter().any(|a| padded.contains(a)) {
            return true;
        }
        self.exclude.iter().any(|e| s.contains(e.as_str()))
    }
}

/// A pluggable scan engine. `name()` must match the `engine` field rules route
/// on. `scan` receives only the rules that named this engine.
pub trait Engine {
    fn name(&self) -> &'static str;

    /// Run every routed rule against the context. Best-effort: an engine that
    /// can't run (missing lockfile, unreadable file) returns `Skipped` rather
    /// than erroring the whole scan.
    fn scan(&self, ctx: &ScanContext, rules: &[&Rule]) -> EngineOutcome;
}

/// Result of one engine run.
pub enum EngineOutcome {
    Ran(Vec<Finding>),
    /// Engine deliberately did not run; the string is the user-facing reason.
    Skipped(String),
}
