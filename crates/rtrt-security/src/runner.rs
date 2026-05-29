//! The runner: takes a [`Profile`] and a root path, routes its rules to the
//! registered engines, collects findings into a [`ScanReport`].

use std::path::Path;

use anyhow::Result;

use crate::engine::{EngineOutcome, ScanContext};
use crate::engines;
use crate::finding::{ScanReport, SeverityCounts};
use crate::profile::Profile;

/// Run `profile` against `root`. Engines whose rules aren't in the profile are
/// not instantiated; engines that can't run are recorded in `engines_skipped`.
pub fn run(profile: &Profile, root: &Path) -> Result<ScanReport> {
    let ctx = ScanContext {
        root: root.to_path_buf(),
        exclude: profile.exclude.clone(),
    };
    let by_engine = profile.rules_by_engine();
    let registry = engines::registry();

    let mut report = ScanReport {
        profile: profile.name.clone(),
        root: root.to_string_lossy().into_owned(),
        ..Default::default()
    };

    for (engine_name, rules) in &by_engine {
        let Some(engine) = registry.iter().find(|e| e.name() == engine_name) else {
            report
                .engines_skipped
                .push(format!("{engine_name}: no such engine"));
            continue;
        };
        match engine.scan(&ctx, rules) {
            EngineOutcome::Ran(findings) => {
                report.engines_run.push(engine_name.clone());
                report.findings.extend(findings);
            }
            EngineOutcome::Skipped(reason) => {
                report
                    .engines_skipped
                    .push(format!("{engine_name}: {reason}"));
            }
        }
    }

    // Severity-rank findings: critical first, then by file for stable output.
    report
        .findings
        .sort_by(|a, b| b.severity.cmp(&a.severity).then(a.file.cmp(&b.file)));

    let mut counts = SeverityCounts::default();
    for f in &report.findings {
        counts.add(f.severity);
    }
    report.counts = counts;

    Ok(report)
}
