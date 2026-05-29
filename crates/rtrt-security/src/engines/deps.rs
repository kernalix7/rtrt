//! `deps` engine — STUB. Implementation filled by the security-engines workflow.

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::profile::Rule;

#[derive(Default)]
pub struct DepsEngine;

impl Engine for DepsEngine {
    fn name(&self) -> &'static str {
        "deps"
    }

    fn scan(&self, _ctx: &ScanContext, _rules: &[&Rule]) -> EngineOutcome {
        EngineOutcome::Skipped("engine not yet implemented".into())
    }
}
