use std::path::{Component, Path, PathBuf};

use rtrt_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::{Template, TemplateCategory, TemplateFile, TemplateSource, TemplateVariable};

const MANIFEST_FILE: &str = "manifest.toml";

#[derive(Debug, Deserialize, Serialize)]
struct ManifestToml {
    name: String,
    description: String,
    #[serde(default)]
    category: TemplateCategory,
    #[serde(default)]
    variables: Vec<TemplateVariable>,
    #[serde(default)]
    post_hooks: Vec<String>,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize, Serialize)]
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
        let manifest = dir.join(MANIFEST_FILE);
        if !manifest.exists() {
            continue;
        }
        out.push(load_one(&dir)?);
    }
    Ok(out)
}

pub fn load_one(dir: &Path) -> Result<Template> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let raw = std::fs::read_to_string(&manifest_path).map_err(Error::Io)?;
    let parsed: ManifestToml = toml::from_str(&raw)
        .map_err(|e| Error::Config(format!("{}: {e}", manifest_path.display())))?;

    let mut files = Vec::with_capacity(parsed.files.len());
    for f in parsed.files {
        validate_file_path(&f.path)?;
        let content = match (f.content, f.source) {
            (Some(c), None) => c,
            (None, Some(rel)) => {
                validate_file_path(&rel)?;
                std::fs::read_to_string(dir.join(rel)).map_err(Error::Io)?
            }
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
        category: parsed.category,
        variables: parsed.variables,
        files,
        post_hooks: parsed.post_hooks,
    })
}

pub fn save_custom(template: &Template) -> Result<PathBuf> {
    validate_name(&template.name)?;
    for file in &template.files {
        validate_file_path(&file.path)?;
    }

    let root = default_dir()
        .ok_or_else(|| Error::Config("cannot determine custom template directory".to_string()))?;
    let dir = root.join(&template.name);
    ensure_strict_child(&root, &dir)?;

    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest = ManifestToml {
        name: template.name.clone(),
        description: template.description.clone(),
        category: template.category,
        variables: template.variables.clone(),
        post_hooks: template.post_hooks.clone(),
        files: template
            .files
            .iter()
            .map(|file| ManifestFile {
                path: file.path.clone(),
                executable: file.executable,
                content: Some(file.content.clone()),
                source: None,
            })
            .collect(),
    };
    let encoded = toml::to_string_pretty(&manifest).map_err(|e| Error::Config(e.to_string()))?;
    std::fs::write(&manifest_path, encoded).map_err(Error::Io)?;
    Ok(manifest_path)
}

pub fn delete_custom(name: &str) -> Result<()> {
    validate_name(name)?;
    let root = default_dir()
        .ok_or_else(|| Error::Config("cannot determine custom template directory".to_string()))?;
    let dir = root.join(name);
    ensure_strict_child(&root, &dir)?;
    std::fs::remove_dir_all(&dir).map_err(Error::Io)
}

pub fn is_custom(name: &str) -> bool {
    validate_name(name)
        .ok()
        .and_then(|()| default_dir())
        .map(|root| root.join(name).join(MANIFEST_FILE).is_file())
        .unwrap_or(false)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('.')
        || name.contains(std::path::MAIN_SEPARATOR)
        || name.contains('/')
        || name.contains('\\')
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(Error::Config(
            "template name must be a non-empty slug using letters, numbers, hyphens, or underscores"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_file_path(path: &str) -> Result<()> {
    let path_ref = Path::new(path);
    if path.is_empty()
        || path.contains('\\')
        || path_ref
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Config(format!(
            "template file path must be relative and stay inside the scaffold target: {path}"
        )));
    }
    Ok(())
}

fn ensure_strict_child(root: &Path, child: &Path) -> Result<()> {
    if child.parent() != Some(root) || child == root {
        return Err(Error::Config(format!(
            "template path must stay under {}",
            root.display()
        )));
    }
    Ok(())
}
