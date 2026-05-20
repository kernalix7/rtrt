use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionLevel {
    Lite,
    Full,
    Ultra,
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::Full
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount(pub u64);

impl TokenCount {
    pub fn new(n: u64) -> Self {
        Self(n)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenStats {
    pub input_before: TokenCount,
    pub input_after: TokenCount,
    pub output_before: TokenCount,
    pub output_after: TokenCount,
}

impl TokenStats {
    pub fn saved_input(self) -> u64 {
        self.input_before.0.saturating_sub(self.input_after.0)
    }

    pub fn saved_output(self) -> u64 {
        self.output_before.0.saturating_sub(self.output_after.0)
    }

    pub fn saved_total(self) -> u64 {
        self.saved_input() + self.saved_output()
    }
}
