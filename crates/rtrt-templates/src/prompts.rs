//! Versioned prompt registry (langfuse-style).
//!
//! Stores named prompts with monotonically increasing version numbers on the
//! filesystem under `~/.rtrt/prompts/<name>/<version>.toml`. Each version is
//! a stand-alone TOML file with the prompt body, an optional parent version
//! (so you can trace evolution), and free-form metadata.
//!
//! The registry is intentionally file-backed rather than SQLite-backed so
//! prompts diff cleanly under git and survive a `rtrt-memory` schema reset.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rtrt_core::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub name: String,
    pub version: u32,
    pub body: String,
    pub created_at: i64,
    #[serde(default)]
    pub parent_version: Option<u32>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

pub struct PromptRegistry {
    root: PathBuf,
}

impl PromptRegistry {
    /// Opens (or creates) a registry rooted at `path`. Use [`default_dir`] to
    /// stay under `~/.rtrt/prompts/`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(Error::Io)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persists a new version of `name`. Picks the next version automatically;
    /// the caller's `version` field is overwritten.
    pub fn save(
        &self,
        name: &str,
        body: &str,
        metadata: BTreeMap<String, String>,
    ) -> Result<Prompt> {
        validate_name(name)?;
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).map_err(Error::Io)?;
        let next = self.next_version(name)?;
        let prompt = Prompt {
            name: name.to_string(),
            version: next,
            body: body.to_string(),
            created_at: now_unix(),
            parent_version: if next > 1 { Some(next - 1) } else { None },
            metadata,
        };
        let path = dir.join(format!("{:04}.toml", next));
        let rendered = toml::to_string_pretty(&prompt)
            .map_err(|e| Error::Config(format!("prompt serialize: {e}")))?;
        std::fs::write(&path, rendered).map_err(Error::Io)?;
        Ok(prompt)
    }

    /// Returns the latest version of `name`, or `None` when the prompt is unknown.
    pub fn latest(&self, name: &str) -> Result<Option<Prompt>> {
        validate_name(name)?;
        let max = self.next_version(name)?.saturating_sub(1);
        if max == 0 {
            return Ok(None);
        }
        self.get(name, max).map(Some)
    }

    /// Returns the requested version of `name`.
    pub fn get(&self, name: &str, version: u32) -> Result<Prompt> {
        validate_name(name)?;
        let path = self.root.join(name).join(format!("{:04}.toml", version));
        if !path.exists() {
            return Err(Error::Config(format!("prompt {name} v{version} not found")));
        }
        let raw = std::fs::read_to_string(&path).map_err(Error::Io)?;
        toml::from_str(&raw).map_err(|e| Error::Config(format!("prompt deserialize: {e}")))
    }

    /// Lists every registered prompt name.
    pub fn list_names(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::Io(e)),
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(s) = entry.file_name().to_str() {
                    if validate_name(s).is_ok() {
                        out.push(s.to_string());
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Lists every version of `name`, in ascending order.
    pub fn list_versions(&self, name: &str) -> Result<Vec<u32>> {
        validate_name(name)?;
        let dir = self.root.join(name);
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(Error::Io(e)),
        };
        let mut versions = Vec::new();
        for entry in entries.flatten() {
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str())
                && let Ok(v) = stem.parse::<u32>()
            {
                versions.push(v);
            }
        }
        versions.sort();
        Ok(versions)
    }

    fn next_version(&self, name: &str) -> Result<u32> {
        Ok(self.list_versions(name)?.iter().copied().max().unwrap_or(0) + 1)
    }
}

pub fn default_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".rtrt").join("prompts"))
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Config(format!("invalid prompt name: {name:?}")));
    }
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_get_latest_roundtrip() {
        let tmp = tempdir();
        let reg = PromptRegistry::open(&tmp).unwrap();
        reg.save("hello", "say hi", BTreeMap::new()).unwrap();
        let v2 = reg.save("hello", "say hi v2", BTreeMap::new()).unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.parent_version, Some(1));
        let latest = reg.latest("hello").unwrap().unwrap();
        assert_eq!(latest.version, 2);
        assert_eq!(latest.body, "say hi v2");
        let v1 = reg.get("hello", 1).unwrap();
        assert_eq!(v1.body, "say hi");
        let versions = reg.list_versions("hello").unwrap();
        assert_eq!(versions, vec![1, 2]);
        let names = reg.list_names().unwrap();
        assert_eq!(names, vec!["hello".to_string()]);
    }

    #[test]
    fn rejects_invalid_names() {
        let tmp = tempdir();
        let reg = PromptRegistry::open(&tmp).unwrap();
        assert!(reg.save("../escape", "x", BTreeMap::new()).is_err());
        assert!(reg.save("", "x", BTreeMap::new()).is_err());
        assert!(reg.save("with space", "x", BTreeMap::new()).is_err());
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rtrt-prompts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }
}
