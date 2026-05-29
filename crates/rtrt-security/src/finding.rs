//! Findings: what a scan produces. A [`Finding`] is one rule violation at one
//! location, carrying the severity, a fix hint, and the standards the violated
//! rule maps to so reports can cite the control (CWE-78, OWASP A03, …).

use serde::{Deserialize, Serialize};

/// Severity ordering, low → critical. `Ord` so a profile's `severity_threshold`
/// can filter with a simple `>=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Compliance-standard references a rule (and therefore its findings) maps to.
/// All fields optional — a rule cites only the frameworks it actually enforces.
/// Free-form strings so new frameworks need no code change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standards {
    /// CWE id(s), e.g. `["CWE-78", "CWE-89"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cwe: Vec<String>,
    /// OWASP Top 10 category, e.g. `"A03:2021-Injection"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owasp: Vec<String>,
    /// OWASP ASVS requirement, e.g. `"5.3.4"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asvs: Vec<String>,
    /// NIST control, e.g. `"SI-10"` (800-53) or `"PW.4"` (800-218 SSDF).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nist: Vec<String>,
    /// CIS Controls v8, e.g. `"16.11"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cis: Vec<String>,
    /// SLSA requirement / level, e.g. `"provenance-L2"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slsa: Vec<String>,
    /// EU AI Act article, e.g. `"Art.15-accuracy-robustness"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eu_ai_act: Vec<String>,
    /// Catch-all for any other framework (ISO 27001, PCI-DSS, STIG, SOC2,
    /// HIPAA, …) as `"framework:control"` strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<String>,
}

impl Standards {
    pub fn is_empty(&self) -> bool {
        self.cwe.is_empty()
            && self.owasp.is_empty()
            && self.asvs.is_empty()
            && self.nist.is_empty()
            && self.cis.is_empty()
            && self.slsa.is_empty()
            && self.eu_ai_act.is_empty()
            && self.other.is_empty()
    }
}

/// One rule violation at one location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Rule id that produced this finding, e.g. `"secret.aws-access-key"`.
    pub rule_id: String,
    /// Engine that produced it: `secrets` / `licenses` / `deps` / `patterns` / `ai`.
    pub engine: String,
    pub severity: Severity,
    /// Human-readable description of the problem.
    pub title: String,
    /// File path relative to the scan root (empty for project-level findings
    /// like a forbidden dependency license).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    /// 1-based line number, when the engine pinpoints one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// The offending snippet, redacted where it would leak the secret itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt: String,
    /// Concrete remediation hint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fix_hint: String,
    /// Standards this finding's rule enforces.
    #[serde(default, skip_serializing_if = "Standards::is_empty")]
    pub standards: Standards,
}

/// Aggregate result of a scan run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReport {
    pub profile: String,
    pub root: String,
    pub findings: Vec<Finding>,
    /// Count per severity, highest first — convenient for dashboards / gates.
    pub counts: SeverityCounts,
    /// Engines that ran (some may be skipped when their tool/data is absent).
    pub engines_run: Vec<String>,
    /// Engines skipped, with the reason (e.g. "deps: no Cargo.lock").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engines_skipped: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub info: usize,
}

impl SeverityCounts {
    pub fn add(&mut self, sev: Severity) {
        match sev {
            Severity::Critical => self.critical += 1,
            Severity::High => self.high += 1,
            Severity::Medium => self.medium += 1,
            Severity::Low => self.low += 1,
            Severity::Info => self.info += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.critical + self.high + self.medium + self.low + self.info
    }
}

impl ScanReport {
    /// True when any finding meets or exceeds `threshold` — the gate predicate.
    pub fn fails_gate(&self, threshold: Severity) -> bool {
        self.findings.iter().any(|f| f.severity >= threshold)
    }
}
