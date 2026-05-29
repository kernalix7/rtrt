//! `licenses` engine — flags license-policy violations on the project's own
//! manifests (and any lockfile / `node_modules` license files present),
//! entirely offline.
//!
//! Per-rule params (read from each routed rule):
//!   - `forbidden`     : list of SPDX ids that are never allowed.
//!   - `allowed`       : allowlist; when non-empty, any license NOT in it is flagged.
//!   - `require_header`: bool (default false) — flag source files missing an
//!     `SPDX-License-Identifier` comment in the first 5 lines.
//!
//! A license is flagged when it is in `forbidden`, OR (`allowed` non-empty AND
//! the license is not in `allowed`), OR the manifest has no license field.
//! All SPDX comparisons are case-insensitive.

use serde::Deserialize;
use walkdir::WalkDir;

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::finding::Finding;
use crate::profile::Rule;

#[derive(Default)]
pub struct LicensesEngine;

impl Engine for LicensesEngine {
    fn name(&self) -> &'static str {
        "licenses"
    }

    fn scan(&self, ctx: &ScanContext, rules: &[&Rule]) -> EngineOutcome {
        if rules.is_empty() {
            return EngineOutcome::Skipped("no licenses rules routed".into());
        }

        let mut findings = Vec::new();

        // Discover the project's own manifests under the scan root.
        let manifests = discover_manifests(ctx);
        if manifests.is_empty() {
            return EngineOutcome::Skipped("no Cargo.toml or package.json found".into());
        }

        for rule in rules {
            let forbidden: Vec<String> = lower_all(rule.param_str_list("forbidden"));
            let allowed: Vec<String> = lower_all(rule.param_str_list("allowed"));
            let require_header = rule.param_bool("require_header", false);

            // License-field policy check across every discovered manifest.
            for m in &manifests {
                match &m.license {
                    None => findings.push(make_finding(
                        rule,
                        &m.rel_path,
                        None,
                        format!("manifest `{}` declares no license field", m.rel_path),
                        m.kind.missing_hint(),
                        "<missing license field>".into(),
                    )),
                    Some(license) => {
                        let lc = license.to_ascii_lowercase();
                        if forbidden.contains(&lc) {
                            findings.push(make_finding(
                                rule,
                                &m.rel_path,
                                None,
                                format!("forbidden license `{license}` in `{}`", m.rel_path),
                                format!(
                                    "remove or replace the `{license}` license; it is on the forbidden list"
                                ),
                                license.clone(),
                            ));
                        } else if !allowed.is_empty() && !allowed.contains(&lc) {
                            findings.push(make_finding(
                                rule,
                                &m.rel_path,
                                None,
                                format!(
                                    "license `{license}` in `{}` is not on the allowlist",
                                    m.rel_path
                                ),
                                format!("use one of the allowed licenses: {}", allowed.join(", ")),
                                license.clone(),
                            ));
                        }
                    }
                }
            }

            // Optional SPDX-header requirement on source files.
            if require_header {
                findings.extend(scan_headers(ctx, rule, &forbidden, &allowed));
            }
        }

        EngineOutcome::Ran(findings)
    }
}

/// What a manifest is, so we can tailor the fix hint.
#[derive(Clone, Copy)]
enum ManifestKind {
    Cargo,
    Npm,
}

impl ManifestKind {
    fn missing_hint(&self) -> String {
        match self {
            ManifestKind::Cargo => {
                "add `license = \"MIT\"` (or your SPDX id) under [package] in Cargo.toml".into()
            }
            ManifestKind::Npm => "add a top-level \"license\" field to package.json".into(),
        }
    }
}

/// A manifest plus the license string we parsed out of it.
struct ManifestLicense {
    rel_path: String,
    kind: ManifestKind,
    license: Option<String>,
}

#[derive(Deserialize)]
struct CargoToml {
    package: Option<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    license: Option<String>,
}

#[derive(Deserialize)]
struct PackageJson {
    // npm "license" is normally a string but historically can be an object
    // ({ "type": "MIT", ... }); accept either via untagged enum.
    license: Option<NpmLicense>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NpmLicense {
    Str(String),
    Obj {
        #[serde(rename = "type")]
        type_: Option<String>,
    },
}

impl NpmLicense {
    fn into_string(self) -> Option<String> {
        match self {
            NpmLicense::Str(s) => Some(s),
            NpmLicense::Obj { type_ } => type_,
        }
    }
}

/// Walk the scan root, parse every `Cargo.toml` / `package.json` we can read.
fn discover_manifests(ctx: &ScanContext) -> Vec<ManifestLicense> {
    let mut out = Vec::new();
    for entry in WalkDir::new(&ctx.root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let rel = match path.strip_prefix(&ctx.root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if ctx.is_excluded(rel) {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        match name {
            "Cargo.toml" => {
                if let Ok(text) = std::fs::read_to_string(path) {
                    let license = toml::from_str::<CargoToml>(&text)
                        .ok()
                        .and_then(|c| c.package)
                        .and_then(|p| p.license)
                        .filter(|s| !s.trim().is_empty());
                    out.push(ManifestLicense {
                        rel_path,
                        kind: ManifestKind::Cargo,
                        license,
                    });
                }
            }
            "package.json" => {
                if let Ok(text) = std::fs::read_to_string(path) {
                    let license = serde_json::from_str::<PackageJson>(&text)
                        .ok()
                        .and_then(|p| p.license)
                        .and_then(|l| l.into_string())
                        .filter(|s| !s.trim().is_empty());
                    out.push(ManifestLicense {
                        rel_path,
                        kind: ManifestKind::Npm,
                        license,
                    });
                }
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

/// When `require_header`, flag source files whose first 5 lines lack an
/// `SPDX-License-Identifier:` comment (or carry a forbidden / non-allowed one).
fn scan_headers(
    ctx: &ScanContext,
    rule: &Rule,
    forbidden: &[String],
    allowed: &[String],
) -> Vec<Finding> {
    const EXTS: &[&str] = &[
        "rs", "py", "js", "jsx", "mjs", "ts", "tsx", "go", "java", "c", "h",
    ];
    let mut findings = Vec::new();
    for entry in WalkDir::new(&ctx.root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let rel = match path.strip_prefix(&ctx.root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if ctx.is_excluded(rel) {
            continue;
        }
        let is_src = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| EXTS.contains(&e))
            .unwrap_or(false);
        if !is_src {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        let spdx = first_lines_spdx(&text, 5);
        match spdx {
            None => findings.push(make_finding(
                rule,
                &rel_path,
                Some(1),
                format!("source file `{rel_path}` missing SPDX-License-Identifier header"),
                "add a `// SPDX-License-Identifier: MIT` comment to the file header".into(),
                "<no SPDX header>".into(),
            )),
            Some(id) => {
                let lc = id.to_ascii_lowercase();
                if forbidden.contains(&lc) {
                    findings.push(make_finding(
                        rule,
                        &rel_path,
                        Some(1),
                        format!("forbidden SPDX header `{id}` in `{rel_path}`"),
                        format!("the `{id}` license is on the forbidden list"),
                        id,
                    ));
                } else if !allowed.is_empty() && !allowed.contains(&lc) {
                    findings.push(make_finding(
                        rule,
                        &rel_path,
                        Some(1),
                        format!("SPDX header `{id}` in `{rel_path}` not on the allowlist"),
                        format!("use one of the allowed licenses: {}", allowed.join(", ")),
                        id,
                    ));
                }
            }
        }
    }
    findings
}

/// Extract the SPDX id from a `SPDX-License-Identifier:` line within the first
/// `n` lines, if present.
fn first_lines_spdx(text: &str, n: usize) -> Option<String> {
    for line in text.lines().take(n) {
        if let Some(idx) = line.find("SPDX-License-Identifier:") {
            let rest = &line[idx + "SPDX-License-Identifier:".len()..];
            // Trim comment trailers (*/, --, #) and whitespace.
            let id = rest
                .trim()
                .trim_end_matches("*/")
                .trim_end_matches("-->")
                .trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Lowercase every entry of a list, for case-insensitive SPDX comparison.
fn lower_all(v: Vec<String>) -> Vec<String> {
    v.into_iter().map(|s| s.to_ascii_lowercase()).collect()
}

/// Build a finding inheriting the rule's severity + standards.
fn make_finding(
    rule: &Rule,
    file: &str,
    line: Option<usize>,
    title: String,
    fix_hint: String,
    excerpt: String,
) -> Finding {
    Finding {
        rule_id: rule.id.clone(),
        engine: "licenses".to_string(),
        severity: rule.severity,
        title,
        file: file.to_string(),
        line,
        excerpt,
        fix_hint,
        standards: rule.standards.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;
    use std::path::{Path, PathBuf};

    /// Make a unique temp fixture dir under the system temp dir.
    fn fixture_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rtrt-lic-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(root: &Path) -> ScanContext {
        ScanContext {
            root: root.to_path_buf(),
            exclude: Vec::new(),
        }
    }

    fn rule(params: &str) -> Rule {
        let toml = format!(
            "id = \"license.policy\"\nengine = \"licenses\"\nseverity = \"high\"\n{params}"
        );
        toml::from_str(&toml).expect("parse test rule")
    }

    fn findings_of(outcome: EngineOutcome) -> Vec<Finding> {
        match outcome {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(reason) => panic!("engine skipped: {reason}"),
        }
    }

    #[test]
    fn forbidden_license_fires() {
        let dir = fixture_dir("forbidden");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0\"\n",
        )
        .unwrap();

        let r = rule("forbidden = [\"GPL-3.0\", \"AGPL-3.0\"]");
        let engine = LicensesEngine;
        let findings = findings_of(engine.scan(&ctx(&dir), &[&r]));

        assert_eq!(findings.len(), 1, "exactly one forbidden-license finding");
        assert_eq!(findings[0].rule_id, "license.policy");
        assert_eq!(findings[0].engine, "licenses");
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].file, "Cargo.toml");
        assert!(findings[0].title.contains("forbidden license"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allowed_license_is_clean() {
        let dir = fixture_dir("clean");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nlicense = \"mit\"\n",
        )
        .unwrap();

        // Case-insensitive: declared "mit" matches allowlist "MIT".
        let r = rule("allowed = [\"MIT\", \"Apache-2.0\"]");
        let engine = LicensesEngine;
        let findings = findings_of(engine.scan(&ctx(&dir), &[&r]));

        assert!(
            findings.is_empty(),
            "clean case yields no findings: {findings:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_license_field_fires() {
        let dir = fixture_dir("missing");
        std::fs::write(
            dir.join("package.json"),
            "{\n  \"name\": \"x\",\n  \"version\": \"1.0.0\"\n}\n",
        )
        .unwrap();

        let r = rule("allowed = [\"MIT\"]");
        let engine = LicensesEngine;
        let findings = findings_of(engine.scan(&ctx(&dir), &[&r]));

        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("no license field"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn not_on_allowlist_fires() {
        let dir = fixture_dir("notallowed");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nlicense = \"BSD-3-Clause\"\n",
        )
        .unwrap();

        let r = rule("allowed = [\"MIT\", \"Apache-2.0\"]");
        let engine = LicensesEngine;
        let findings = findings_of(engine.scan(&ctx(&dir), &[&r]));

        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("not on the allowlist"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn require_header_flags_missing_spdx() {
        let dir = fixture_dir("header");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("clean.rs"),
            "// SPDX-License-Identifier: MIT\nfn a() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad.rs"), "fn b() {}\n").unwrap();

        let r = rule("allowed = [\"MIT\"]\nrequire_header = true");
        let engine = LicensesEngine;
        let findings = findings_of(engine.scan(&ctx(&dir), &[&r]));

        // Only bad.rs (no SPDX header) fires; manifest + clean.rs are fine.
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].file, "bad.rs");
        assert!(findings[0].title.contains("missing SPDX"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
