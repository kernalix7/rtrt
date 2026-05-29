//! `ai` engine — STUB. Implementation filled by the security-engines workflow.

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::profile::Rule;

#[derive(Default)]
pub struct AiEngine;

impl Engine for AiEngine {
    fn name(&self) -> &'static str {
        "ai"
    }

    fn scan(&self, _ctx: &ScanContext, _rules: &[&Rule]) -> EngineOutcome {
        EngineOutcome::Skipped("engine not yet implemented".into())
    }
}
