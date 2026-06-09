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
