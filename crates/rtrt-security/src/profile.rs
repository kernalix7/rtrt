//! Profiles & rules: the declarative layer. A [`Profile`] is a named set of
//! [`Rule`]s plus a severity gate threshold, loaded from TOML. Built-in
//! profiles are embedded at compile time; user profiles live under
//! `~/.rtrt/security/profiles/*.toml` and override built-ins by name.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::finding::{Severity, Standards};

/// A single rule: routes to one engine, carries engine-specific params, a
/// severity, and the standards it enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Stable id, dotted by convention: `secret.aws-access-key`,
    /// `license.copyleft`, `ai.hallucinated-dep`, `pattern.command-injection`.
    pub id: String,
    /// Engine name: `secrets` / `licenses` / `deps` / `patterns` / `ai`.
    pub engine: String,
    pub severity: Severity,
    /// Human-readable description; surfaces in `profile show` and reports.
    #[serde(default)]
    pub description: String,
    /// Whether this rule is active. Profiles can ship a rule disabled so users
    /// flip it on without re-authoring it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Standards this rule maps to (CWE / OWASP / NIST / …).
    #[serde(default)]
    pub standards: Standards,
    /// Engine-specific parameters. Free-form so each engine reads what it needs
    /// without the profile schema knowing engine internals — e.g.
    /// `match = "..."` for patterns, `forbidden = [...]` for licenses,
    /// `check = "crate-exists"` for ai.
    #[serde(flatten, default)]
    pub params: BTreeMap<String, toml::Value>,
}

fn default_true() -> bool {
    true
}

impl Rule {
    /// Read a string param, if present.
    pub fn param_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(|v| v.as_str())
    }

    /// Read a string-array param, returning an empty vec when absent.
    pub fn param_str_list(&self, key: &str) -> Vec<String> {
        self.params
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Read a bool param with a default.
    pub fn param_bool(&self, key: &str, default: bool) -> bool {
        self.params
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }
}

/// A named set of rules + a gate threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Findings at or above this severity fail `rtrt security gate`.
    #[serde(default = "default_threshold")]
    pub severity_threshold: Severity,
    /// Glob-ish ignore patterns (substring match on the relative path) applied
    /// on top of the always-on ignores (.git, target, node_modules, …).
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_threshold() -> Severity {
    Severity::High
}

impl Profile {
    /// Parse a profile from TOML text.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("parse security profile TOML")
    }

    /// Active rules grouped by engine name, in declaration order.
    pub fn rules_by_engine(&self) -> BTreeMap<String, Vec<&Rule>> {
        let mut map: BTreeMap<String, Vec<&Rule>> = BTreeMap::new();
        for r in self.rules.iter().filter(|r| r.enabled) {
            map.entry(r.engine.clone()).or_default().push(r);
        }
        map
    }
}

/// Built-in profiles embedded at compile time. The workflow that authors the
/// `profiles/*.toml` files keeps this list in sync.
pub const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("ai-default", include_str!("../profiles/ai-default.toml")),
    ("ai-strict", include_str!("../profiles/ai-strict.toml")),
    (
        "owasp-top-10",
        include_str!("../profiles/owasp-top-10.toml"),
    ),
    ("asvs-l2", include_str!("../profiles/asvs-l2.toml")),
    (
        "cis-baseline",
        include_str!("../profiles/cis-baseline.toml"),
    ),
    ("nist-ssdf", include_str!("../profiles/nist-ssdf.toml")),
];

/// User profile directory: `~/.rtrt/security/profiles/`.
pub fn user_profile_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".rtrt")
            .join("security")
            .join("profiles"),
    )
}

/// Resolve a profile by name: user dir wins over built-in.
pub fn load_profile(name: &str) -> Result<Profile> {
    if let Some(dir) = user_profile_dir() {
        let path = dir.join(format!("{name}.toml"));
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            return Profile::from_toml(&text);
        }
    }
    if let Some((_, text)) = BUILTIN_PROFILES.iter().find(|(n, _)| *n == name) {
        return Profile::from_toml(text);
    }
    anyhow::bail!("unknown security profile: {name}")
}

/// List all available profile names (built-in + user), deduped, sorted.
pub fn list_profiles() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_PROFILES
        .iter()
        .map(|(n, _)| n.to_string())
        .collect();
    if let Some(dir) = user_profile_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}
