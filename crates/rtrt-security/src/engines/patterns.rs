//! `patterns` engine — STUB. Implementation filled by the security-engines workflow.

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::profile::Rule;

#[derive(Default)]
pub struct PatternsEngine;

impl Engine for PatternsEngine {
    fn name(&self) -> &'static str {
        "patterns"
    }

    fn scan(&self, _ctx: &ScanContext, _rules: &[&Rule]) -> EngineOutcome {
        EngineOutcome::Skipped("engine not yet implemented".into())
    }
}
