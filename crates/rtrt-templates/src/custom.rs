use std::path::{Path, PathBuf};

use rtrt_core::{Error, Result};
use serde::Deserialize;

use crate::{Template, TemplateFile, TemplateSource, TemplateVariable};

#[derive(Debug, Deserialize)]
struct ManifestToml {
    name: String,
    description: String,
    #[serde(default)]
    variables: Vec<TemplateVariable>,
    #[serde(default)]
    post_hooks: Vec<String>,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: String,
    #[serde(default)]
    executable: bool,
    /// Inline content. Mutually exclusive with `source`.
    #[serde(default)]
    content: Option<String>,
    /// Path to file on disk (relative to manifest dir). Mutually exclusive with `content`.
    #[serde(default)]
    source: Option<String>,
}

pub fn default_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".rtrt").join("templates"))
}

pub fn scan_default_dir() -> Result<Vec<Template>> {
    let Some(root) = default_dir() else {
        return Ok(vec![]);
    };
    if !root.exists() {
        return Ok(vec![]);
    }
    scan_dir(&root)
}

pub fn scan_dir(root: &Path) -> Result<Vec<Template>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(root).map_err(Error::Io)?;
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        if !entry.file_type().map_err(Error::Io)?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let manifest = dir.join("manifest.toml");
        if !manifest.exists() {
            continue;
        }
        out.push(load_one(&dir)?);
    }
    Ok(out)
}

pub fn load_one(dir: &Path) -> Result<Template> {
    let manifest_path = dir.join("manifest.toml");
    let raw = std::fs::read_to_string(&manifest_path).map_err(Error::Io)?;
    let parsed: ManifestToml = toml::from_str(&raw)
        .map_err(|e| Error::Config(format!("{}: {e}", manifest_path.display())))?;

    let mut files = Vec::with_capacity(parsed.files.len());
    for f in parsed.files {
        let content = match (f.content, f.source) {
            (Some(c), None) => c,
            (None, Some(rel)) => std::fs::read_to_string(dir.join(rel)).map_err(Error::Io)?,
            (Some(_), Some(_)) => {
                return Err(Error::Config(format!(
                    "{}: file '{}' has both content and source",
                    manifest_path.display(),
                    f.path
                )));
            }
            (None, None) => {
                return Err(Error::Config(format!(
                    "{}: file '{}' missing content/source",
                    manifest_path.display(),
                    f.path
                )));
            }
        };
        files.push(TemplateFile {
            path: f.path,
            content,
            executable: f.executable,
        });
    }

    Ok(Template {
        name: parsed.name,
        description: parsed.description,
        source: TemplateSource::Custom,
        variables: parsed.variables,
        files,
        post_hooks: parsed.post_hooks,
    })
}
