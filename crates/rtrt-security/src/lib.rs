//! rtrt-security — security & license profiles for AI-generated artifacts.
//!
//! Modeled on RHEL/OpenSCAP security profiles: a [`Profile`] is a declarative
//! TOML set of [`Rule`]s, each routed to one pluggable [`Engine`]
//! (secrets / licenses / deps / patterns / ai). Every rule carries a
//! [`Standards`] mapping so findings cite the control they enforce
//! (CWE, OWASP Top 10 / ASVS, NIST 800-53 / 800-218 SSDF, CIS Controls v8,
//! SLSA, EU AI Act, and more).
//!
//! ```no_run
//! use rtrt_security::{load_profile, run};
//! use std::path::Path;
//!
//! let profile = load_profile("ai-default").unwrap();
//! let report = run(&profile, Path::new(".")).unwrap();
//! println!("{} findings", report.findings.len());
//! ```

pub mod engine;
pub mod engines;
pub mod finding;
pub mod profile;
pub mod runner;

pub use engine::{Engine, EngineOutcome, ScanContext};
pub use finding::{Finding, ScanReport, Severity, SeverityCounts, Standards};
pub use profile::{BUILTIN_PROFILES, Profile, Rule, list_profiles, load_profile, user_profile_dir};
pub use runner::run;
