//! `secrets` engine — scans every text file under the scan root for committed
//! credentials. The profile only needs to route >=1 rule here; the engine then
//! runs the FULL builtin pattern set. Findings get the highest severity among
//! the routed secrets rules and that rule's standards, so reports cite the
//! control the profile chose to enforce.

use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;
use walkdir::WalkDir;

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::finding::{Finding, Severity, Standards};
use crate::profile::Rule;

#[derive(Default)]
pub struct SecretsEngine;

/// A builtin secret pattern. `entropy_gated` patterns only fire when the matched
/// (or captured) token clears the Shannon-entropy bar — used for the noisy
/// generic-assignment rule.
struct Pattern {
    /// Becomes `secret.<name>` in the finding rule_id.
    name: &'static str,
    re: Regex,
    /// Human-readable title for the finding.
    title: &'static str,
    /// Apply the entropy gate to capture group 1 (or the whole match if none).
    entropy_gated: bool,
}

/// Shannon entropy (bits/char) of a byte string.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let bytes = s.as_bytes();
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    let mut entropy = 0.0;
    for &c in counts.iter() {
        if c == 0 {
            continue;
        }
        let p = c as f64 / len;
        entropy -= p * p.log2();
    }
    entropy
}

const ENTROPY_MIN: f64 = 3.5;

/// Build the builtin pattern set once. Each regex is anchored on the structure
/// of the credential, not its surroundings, so it matches inside any text file.
static PATTERNS: Lazy<Vec<Pattern>> = Lazy::new(|| {
    fn p(name: &'static str, title: &'static str, re: &str) -> Pattern {
        Pattern {
            name,
            re: Regex::new(re).expect("builtin secret regex compiles"),
            title,
            entropy_gated: false,
        }
    }
    vec![
        p(
            "aws-access-key",
            "AWS access key id",
            r"AKIA[0-9A-Z]{16}",
        ),
        // 40-char base64 secret appearing near an aws_secret hint.
        p(
            "aws-secret",
            "AWS secret access key",
            r#"(?i)aws_secret[a-z_]*\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})["']?"#,
        ),
        p(
            "gh-token",
            "GitHub token",
            r"gh[poshr]_[A-Za-z0-9]{36}",
        ),
        p(
            "gh-pat-fine",
            "GitHub fine-grained PAT",
            r"github_pat_[A-Za-z0-9_]{22,}",
        ),
        p(
            "slack-token",
            "Slack token",
            r"xox[baprs]-[A-Za-z0-9-]{10,}",
        ),
        p(
            "stripe-key",
            "Stripe live key",
            r"(?:sk|rk)_live_[A-Za-z0-9]{16,}",
        ),
        p(
            "google-api",
            "Google API key",
            r"AIza[0-9A-Za-z_-]{35}",
        ),
        p(
            "private-key",
            "Private key block",
            r"-----BEGIN (?:RSA|EC|OPENSSH|PGP) PRIVATE KEY-----",
        ),
        p(
            "jwt",
            "JSON Web Token",
            r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        ),
        // Generic high-entropy assignment to a key/token/secret/passwd var.
        Pattern {
            name: "generic-high-entropy",
            re: Regex::new(
                r#"(?i)(?:key|token|secret|passwd|password|apikey|api_key)[a-z0-9_]*\s*[:=]\s*["']?([A-Za-z0-9/+=_-]{20,})["']?"#,
            )
            .expect("builtin secret regex compiles"),
            title: "High-entropy secret assignment",
            entropy_gated: true,
        },
        // KEY=secret style line in a committed dotenv file (path-gated below).
        Pattern {
            name: "dotenv-value",
            re: Regex::new(r#"(?m)^\s*[A-Z][A-Z0-9_]{2,}\s*=\s*["']?([^\s"'#]{8,})["']?\s*$"#)
                .expect("builtin secret regex compiles"),
            title: "Hardcoded value in committed .env file",
            entropy_gated: false,
        },
    ]
});

/// `true` if the first 1KB of `bytes` contains a NUL — treat as binary, skip.
fn looks_binary(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(1024)];
    window.contains(&0)
}

/// Redact a secret: keep the first 4 chars, replace the rest with `***`.
fn redact(secret: &str) -> String {
    let head: String = secret.chars().take(4).collect();
    format!("{head}***")
}

/// 1-based line number of byte offset `at` within `text`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

impl SecretsEngine {
    /// Highest severity among routed rules, with the standards/source from the
    /// rule that carries it. Defaults to High if (impossibly) none are routed.
    fn pick_severity_and_standards<'a>(rules: &[&'a Rule]) -> (Severity, Standards) {
        let mut best: Option<&'a Rule> = None;
        for r in rules {
            match best {
                Some(b) if r.severity <= b.severity => {}
                _ => best = Some(r),
            }
        }
        match best {
            Some(r) => (r.severity, r.standards.clone()),
            None => (Severity::High, Standards::default()),
        }
    }
}

impl Engine for SecretsEngine {
    fn name(&self) -> &'static str {
        "secrets"
    }

    fn scan(&self, ctx: &ScanContext, rules: &[&Rule]) -> EngineOutcome {
        if rules.is_empty() {
            return EngineOutcome::Skipped("no secrets rules routed".into());
        }
        let (severity, standards) = Self::pick_severity_and_standards(rules);
        let mut findings = Vec::new();

        for entry in WalkDir::new(&ctx.root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let abs = entry.path();
            let rel = abs.strip_prefix(&ctx.root).unwrap_or(abs);
            if ctx.is_excluded(rel) {
                continue;
            }
            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if looks_binary(&bytes) {
                continue;
            }
            let text = match String::from_utf8(bytes) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().to_string();
            let is_dotenv = is_dotenv_file(rel);

            for pat in PATTERNS.iter() {
                // dotenv-value is meaningful only inside a committed .env file.
                if pat.name == "dotenv-value" && !is_dotenv {
                    continue;
                }
                for caps in pat.re.captures_iter(&text) {
                    let whole = caps.get(0).expect("group 0 always present");
                    // Token = capture group 1 if present, else the full match.
                    let token = caps.get(1).unwrap_or(whole).as_str();

                    if pat.entropy_gated && shannon_entropy(token) < ENTROPY_MIN {
                        continue;
                    }

                    let line = line_of(&text, whole.start());
                    findings.push(Finding {
                        rule_id: format!("secret.{}", pat.name),
                        engine: "secrets".to_string(),
                        severity,
                        title: pat.title.to_string(),
                        file: rel_str.clone(),
                        line: Some(line),
                        excerpt: redact(token),
                        fix_hint:
                            "Remove the credential from source, rotate it, and load it from a secret manager or environment variable at runtime."
                                .to_string(),
                        standards: standards.clone(),
                    });
                }
            }
        }

        EngineOutcome::Ran(findings)
    }
}

/// True when the relative path is a committed dotenv file (`.env`,
/// `.env.local`, `config/.env.prod`, …) but not an `.env.example` template.
fn is_dotenv_file(rel: &Path) -> bool {
    let name = match rel.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if !name.starts_with(".env") {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    !(lower.contains("example") || lower.contains("sample") || lower.contains("template"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Build a unique temp fixture dir, returning its path.
    fn fixture_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "rtrt-secrets-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn one_rule() -> Rule {
        Rule {
            id: "secret.any".to_string(),
            engine: "secrets".to_string(),
            severity: Severity::Critical,
            description: String::new(),
            enabled: true,
            standards: Standards {
                cwe: vec!["CWE-798".to_string()],
                ..Default::default()
            },
            params: BTreeMap::new(),
        }
    }

    fn run(dir: &Path) -> Vec<Finding> {
        let ctx = ScanContext {
            root: dir.to_path_buf(),
            exclude: vec![],
        };
        let rule = one_rule();
        let rules = vec![&rule];
        match SecretsEngine.scan(&ctx, &rules) {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(r) => panic!("unexpectedly skipped: {r}"),
        }
    }

    #[test]
    fn fires_on_aws_access_key() {
        let dir = fixture_dir("aws");
        write(
            &dir,
            "config.rs",
            "let k = \"AKIAIOSFODNN7EXAMPLE\"; // do not commit\n",
        );
        let findings = run(&dir);
        let hit = findings
            .iter()
            .find(|f| f.rule_id == "secret.aws-access-key")
            .expect("aws access key should fire");
        assert_eq!(hit.severity, Severity::Critical);
        assert_eq!(hit.engine, "secrets");
        assert_eq!(hit.file, "config.rs");
        assert_eq!(hit.line, Some(1));
        // Excerpt is redacted: first 4 chars + ***
        assert_eq!(hit.excerpt, "AKIA***");
        assert!(!hit.excerpt.contains("IOSFODNN7EXAMPLE"));
        // Standards propagated from the routed rule.
        assert_eq!(hit.standards.cwe, vec!["CWE-798".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fires_on_private_key_and_jwt() {
        let dir = fixture_dir("multi");
        write(
            &dir,
            "id_rsa",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----\n",
        );
        write(
            &dir,
            "token.txt",
            "auth: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N\n",
        );
        let findings = run(&dir);
        assert!(findings.iter().any(|f| f.rule_id == "secret.private-key"));
        assert!(findings.iter().any(|f| f.rule_id == "secret.jwt"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clean_project_yields_nothing() {
        let dir = fixture_dir("clean");
        write(
            &dir,
            "main.rs",
            "fn main() {\n    println!(\"hello world\");\n}\n",
        );
        write(&dir, "README.md", "# A normal project\n\nNothing to see.\n");
        let findings = run(&dir);
        assert!(
            findings.is_empty(),
            "clean project should have no findings, got: {findings:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn entropy_gate_skips_low_entropy_assignment() {
        let dir = fixture_dir("entropy");
        // 20+ chars but extremely low entropy (single repeated char) -> skipped.
        write(
            &dir,
            "low.rs",
            "let api_key = \"aaaaaaaaaaaaaaaaaaaaaaaa\";\n",
        );
        let findings = run(&dir);
        assert!(
            !findings
                .iter()
                .any(|f| f.rule_id == "secret.generic-high-entropy"),
            "low-entropy value must not fire generic-high-entropy"
        );

        // A genuinely high-entropy value DOES fire.
        let dir2 = fixture_dir("entropy-hi");
        write(
            &dir2,
            "hi.rs",
            "let secret = \"aB3xZ9qLmP2vT7wK4nR8sD1f\";\n",
        );
        let findings2 = run(&dir2);
        assert!(
            findings2
                .iter()
                .any(|f| f.rule_id == "secret.generic-high-entropy"),
            "high-entropy value should fire generic-high-entropy"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[test]
    fn dotenv_gated_to_env_files() {
        let dir = fixture_dir("dotenv");
        // Same KEY=VALUE in a .env (fires) and an .env.example (does not).
        write(&dir, ".env", "DATABASE_PASSWORD=supersecretvalue123\n");
        write(
            &dir,
            ".env.example",
            "DATABASE_PASSWORD=changeme_placeholder\n",
        );
        let findings = run(&dir);
        let dotenv_hits: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == "secret.dotenv-value")
            .collect();
        assert!(
            dotenv_hits.iter().all(|f| f.file == ".env"),
            "dotenv-value should only fire in .env, not .env.example: {dotenv_hits:?}"
        );
        assert!(
            dotenv_hits.iter().any(|f| f.file == ".env"),
            "dotenv-value should fire in committed .env"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = fixture_dir("binary");
        // Contains a NUL in the first KB plus an embedded AWS key.
        let mut data = b"\x00\x01\x02 AKIAIOSFODNN7EXAMPLE".to_vec();
        data.extend_from_slice(&[0u8; 16]);
        std::fs::write(dir.join("blob.bin"), &data).unwrap();
        let findings = run(&dir);
        assert!(
            findings.is_empty(),
            "binary file should be skipped, got: {findings:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
