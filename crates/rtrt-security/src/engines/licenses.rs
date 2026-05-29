//! `licenses` engine — STUB. Implementation filled by the security-engines workflow.

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::profile::Rule;

#[derive(Default)]
pub struct LicensesEngine;

impl Engine for LicensesEngine {
    fn name(&self) -> &'static str {
        "licenses"
    }

    fn scan(&self, _ctx: &ScanContext, _rules: &[&Rule]) -> EngineOutcome {
        EngineOutcome::Skipped("engine not yet implemented".into())
    }
}
