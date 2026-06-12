use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStyleLevel {
    #[default]
    Off,
    Lite,
    Full,
    Ultra,
}

impl OutputStyleLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "lite" => Some(Self::Lite),
            "full" => Some(Self::Full),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
        }
    }

    pub fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl fmt::Display for OutputStyleLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn output_style_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rtrt")
        .join("output-style")
}

pub fn read_output_style_level() -> OutputStyleLevel {
    let path = output_style_path();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return OutputStyleLevel::Off;
    };
    OutputStyleLevel::parse(&raw).unwrap_or(OutputStyleLevel::Off)
}

pub fn write_output_style_level(level: OutputStyleLevel) -> std::io::Result<()> {
    let path = output_style_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", level.as_str()))
}

/// Read the effective terse level for a repo: a project override in
/// `<repo>/.rtrt/config.toml` wins, else the global `~/.rtrt/output-style`.
/// `repo = None` reads the global level only.
pub fn read_output_style_level_for(repo: Option<&std::path::Path>) -> OutputStyleLevel {
    if let Some(repo) = repo
        && let Ok(over) = crate::Config::load_project(repo)
        && let Some(level) = over
            .output_level
            .as_deref()
            .and_then(OutputStyleLevel::parse)
    {
        return level;
    }
    read_output_style_level()
}

/// Write the terse level for a repo as a project override
/// (`<repo>/.rtrt/config.toml`). `repo = None` writes the global level.
pub fn write_output_style_level_for(
    repo: Option<&std::path::Path>,
    level: OutputStyleLevel,
) -> std::io::Result<()> {
    match repo {
        Some(repo) => {
            let mut over = crate::Config::load_project(repo).unwrap_or_default();
            over.output_level = Some(level.as_str().to_string());
            crate::Config::save_project(repo, &over)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }
        None => write_output_style_level(level),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(
            OutputStyleLevel::parse("full"),
            Some(OutputStyleLevel::Full)
        );
        assert_eq!(
            OutputStyleLevel::parse(" FULL\n"),
            Some(OutputStyleLevel::Full)
        );
        assert_eq!(OutputStyleLevel::parse("verbose"), None);
    }
}
