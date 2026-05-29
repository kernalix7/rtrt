//! `secrets` engine — STUB. Implementation filled by the security-engines workflow.

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::profile::Rule;

#[derive(Default)]
pub struct SecretsEngine;

impl Engine for SecretsEngine {
    fn name(&self) -> &'static str {
        "secrets"
    }

    fn scan(&self, _ctx: &ScanContext, _rules: &[&Rule]) -> EngineOutcome {
        EngineOutcome::Skipped("engine not yet implemented".into())
    }
}
