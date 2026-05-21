//! rtrt-templates — standardized project scaffolding.
//!
//! Each template is a list of files (with `{{var}}` substitution) plus optional post-init
//! shell hooks. Built-in templates ship with the crate; custom templates live in
//! `~/.rtrt/templates/<name>/` with a `manifest.toml`.

pub mod builtin;
#[cfg(feature = "chains")]
pub mod chains;
pub mod custom;
pub mod prompts;
pub mod render;

#[cfg(feature = "chains")]
pub use chains::{PromptChain, PromptStep, StepOutput};
pub use prompts::{Prompt, PromptRegistry};

use std::collections::BTreeMap;

use rtrt_core::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub source: TemplateSource,
    /// Project kind. Drives the grouping shown in `rtrt templates` and the
    /// dashboard Tools tab. Defaults to `Development` so older custom
    /// manifests that don't set the field still land in a sensible bucket.
    #[serde(default)]
    pub category: TemplateCategory,
    #[serde(default)]
    pub variables: Vec<TemplateVariable>,
    pub files: Vec<TemplateFile>,
    #[serde(default)]
    pub post_hooks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateSource {
    BuiltIn,
    Custom,
}

/// Project kind. Keeps the surface organised around what the user is trying
/// to start, not which programming language fronts the scaffold.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateCategory {
    /// Code projects (CLI / lib / service across any language).
    #[default]
    Development,
    /// UI / brand / wireframe assets.
    Design,
    /// Specs, decision records, roadmaps, agent definitions.
    Planning,
}

impl TemplateCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Design => "design",
            Self::Planning => "planning",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateVariable {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateFile {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub executable: bool,
}

pub fn list_all() -> Vec<Template> {
    let mut all: Vec<Template> = builtin::ALL.iter().map(|b| (*b).clone()).collect();
    if let Ok(custom) = custom::scan_default_dir() {
        all.extend(custom);
    }
    all
}

pub fn find(name: &str) -> Option<Template> {
    list_all().into_iter().find(|t| t.name == name)
}

pub fn validate_vars(template: &Template, vars: &BTreeMap<String, String>) -> Result<()> {
    for v in &template.variables {
        if v.required && !vars.contains_key(&v.name) && v.default.is_none() {
            return Err(Error::Config(format!(
                "missing required variable: {}",
                v.name
            )));
        }
    }
    Ok(())
}

pub fn resolve_vars(
    template: &Template,
    user_vars: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for v in &template.variables {
        if let Some(val) = user_vars.get(&v.name) {
            out.insert(v.name.clone(), val.clone());
        } else if let Some(def) = &v.default {
            out.insert(v.name.clone(), def.clone());
        }
    }
    for (k, v) in user_vars {
        out.entry(k).or_insert(v);
    }
    out
}
