//! `patterns` engine — regex source scanner. Each routed rule supplies a
//! `match` regex (required), an optional `lang` extension filter, and an
//! optional `path_glob` substring the file path must contain. The engine walks
//! every text file under `ctx.root` (honouring `ctx.is_excluded`) and emits one
//! [`Finding`] per matching line: 1-based line number, excerpt trimmed to 160
//! chars. Rules without a `match` param are skipped silently.

use std::collections::BTreeSet;

use regex::Regex;
use walkdir::WalkDir;

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::finding::Finding;
use crate::profile::Rule;

/// Cap on a finding excerpt, in chars (not bytes), per the param contract.
const EXCERPT_MAX: usize = 160;

#[derive(Default)]
pub struct PatternsEngine;

/// A rule that compiled successfully, paired with its file filters.
struct Compiled<'a> {
    rule: &'a Rule,
    regex: Regex,
    /// Lowercased file extensions this rule applies to; empty = any.
    extensions: Vec<&'static str>,
    /// Optional substring the relative path must contain.
    path_glob: Option<String>,
}

/// Map a `lang` param to the extensions it covers. `any` / unknown -> no filter.
fn extensions_for_lang(lang: &str) -> Vec<&'static str> {
    match lang.to_ascii_lowercase().as_str() {
        "rust" => vec!["rs"],
        "python" => vec!["py"],
        "js" => vec!["js", "jsx", "mjs"],
        "ts" => vec!["ts", "tsx"],
        _ => Vec::new(),
    }
}

/// True when `path` (the relative path string) has one of `exts`. Empty exts
/// means "any file".
fn extension_matches(path: &str, exts: &[&'static str]) -> bool {
    if exts.is_empty() {
        return true;
    }
    let ext = path
        .rsplit('.')
        .next()
        .filter(|_| path.contains('.'))
        .unwrap_or("")
        .to_ascii_lowercase();
    exts.iter().any(|e| *e == ext)
}

/// Trim to at most `EXCERPT_MAX` chars (char-boundary safe), single-lined.
fn excerpt(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= EXCERPT_MAX {
        trimmed.to_string()
    } else {
        trimmed.chars().take(EXCERPT_MAX).collect()
    }
}

impl Engine for PatternsEngine {
    fn name(&self) -> &'static str {
        "patterns"
    }

    fn scan(&self, ctx: &ScanContext, rules: &[&Rule]) -> EngineOutcome {
        // Compile each rule's `match` regex once. Rules lacking a valid `match`
        // are skipped silently (a missing or uncompilable pattern is a profile
        // authoring issue, not a finding).
        let mut compiled: Vec<Compiled> = Vec::new();
        for rule in rules {
            let Some(pat) = rule.param_str("match") else {
                continue;
            };
            let Ok(regex) = Regex::new(pat) else {
                continue;
            };
            let extensions = match rule.param_str("lang") {
                Some(lang) => extensions_for_lang(lang),
                None => Vec::new(),
            };
            let path_glob = rule.param_str("path_glob").map(str::to_string);
            compiled.push(Compiled {
                rule,
                regex,
                extensions,
                path_glob,
            });
        }

        if compiled.is_empty() {
            return EngineOutcome::Skipped("no patterns rules with a valid `match`".into());
        }

        // Track which file extensions any rule still needs, so we never read a
        // file no rule could match. When some rule wants "any", we read all.
        let any_lang = compiled.iter().any(|c| c.extensions.is_empty());
        let wanted_exts: BTreeSet<&'static str> = compiled
            .iter()
            .flat_map(|c| c.extensions.iter().copied())
            .collect();

        let mut findings: Vec<Finding> = Vec::new();

        for entry in WalkDir::new(&ctx.root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let abs = entry.path();
            let rel = match abs.strip_prefix(&ctx.root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if ctx.is_excluded(rel) {
                continue;
            }
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            // Cheap pre-filter: skip files no compiled rule's extension wants.
            if !any_lang {
                let ext = rel_str
                    .rsplit('.')
                    .next()
                    .filter(|_| rel_str.contains('.'))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !wanted_exts.contains(ext.as_str()) {
                    continue;
                }
            }

            let Ok(text) = std::fs::read_to_string(abs) else {
                // Binary / unreadable file — skip, this is a text-only engine.
                continue;
            };

            // Pre-split lines once; reused across every rule for this file.
            let lines: Vec<&str> = text.lines().collect();

            for c in &compiled {
                if !extension_matches(&rel_str, &c.extensions) {
                    continue;
                }
                if let Some(glob) = &c.path_glob {
                    if !rel_str.contains(glob.as_str()) {
                        continue;
                    }
                }
                for (idx, line) in lines.iter().enumerate() {
                    if c.regex.is_match(line) {
                        findings.push(Finding {
                            rule_id: c.rule.id.clone(),
                            engine: "patterns".to_string(),
                            severity: c.rule.severity,
                            title: if c.rule.description.is_empty() {
                                format!("pattern `{}` matched", c.rule.id)
                            } else {
                                c.rule.description.clone()
                            },
                            file: rel_str.clone(),
                            line: Some(idx + 1),
                            excerpt: excerpt(line),
                            fix_hint:
                                "Review this match; remove or refactor the flagged construct."
                                    .to_string(),
                            standards: c.rule.standards.clone(),
                        });
                    }
                }
            }
        }

        EngineOutcome::Ran(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Severity, Standards};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Build a unique scratch dir under the system temp dir.
    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("rtrt-patterns-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rule(id: &str, params: &[(&str, &str)]) -> Rule {
        let mut map: BTreeMap<String, toml::Value> = BTreeMap::new();
        for (k, v) in params {
            map.insert((*k).to_string(), toml::Value::String((*v).to_string()));
        }
        Rule {
            id: id.to_string(),
            engine: "patterns".to_string(),
            severity: Severity::High,
            description: "test rule".to_string(),
            enabled: true,
            standards: Standards::default(),
            params: map,
        }
    }

    fn ctx(root: &std::path::Path) -> ScanContext {
        ScanContext {
            root: root.to_path_buf(),
            exclude: Vec::new(),
        }
    }

    #[test]
    fn fires_on_match() {
        let dir = scratch();
        std::fs::write(
            dir.join("app.rs"),
            "fn main() {\n    let q = format!(\"SELECT * FROM t WHERE id = {}\", id);\n}\n",
        )
        .unwrap();

        let r = rule(
            "pattern.sql-format",
            &[("match", r"format!\(.*SELECT"), ("lang", "rust")],
        );
        let engine = PatternsEngine;
        let outcome = engine.scan(&ctx(&dir), &[&r]);

        let findings = match outcome {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(s) => panic!("unexpectedly skipped: {s}"),
        };
        assert_eq!(findings.len(), 1, "expected exactly one finding");
        let f = &findings[0];
        assert_eq!(f.rule_id, "pattern.sql-format");
        assert_eq!(f.engine, "patterns");
        assert_eq!(f.file, "app.rs");
        assert_eq!(f.line, Some(2));
        assert_eq!(f.severity, Severity::High);
        assert!(f.excerpt.contains("SELECT"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clean_case_is_empty() {
        let dir = scratch();
        std::fs::write(
            dir.join("clean.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let r = rule(
            "pattern.sql-format",
            &[("match", r"format!\(.*SELECT"), ("lang", "rust")],
        );
        let engine = PatternsEngine;
        let outcome = engine.scan(&ctx(&dir), &[&r]);

        match outcome {
            EngineOutcome::Ran(f) => assert!(f.is_empty(), "expected no findings, got {f:?}"),
            EngineOutcome::Skipped(s) => panic!("unexpectedly skipped: {s}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lang_filter_excludes_other_extensions() {
        let dir = scratch();
        // A .py file would match the regex content, but the rule targets rust.
        std::fs::write(dir.join("script.py"), "x = exec_query('SELECT 1')\n").unwrap();
        std::fs::write(dir.join("ok.rs"), "// SELECT but no eval\n").unwrap();

        let r = rule("pattern.select", &[("match", "SELECT"), ("lang", "rust")]);
        let engine = PatternsEngine;
        let outcome = engine.scan(&ctx(&dir), &[&r]);

        let findings = match outcome {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(s) => panic!("unexpectedly skipped: {s}"),
        };
        // Only the .rs file is eligible, and it does contain SELECT.
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "ok.rs");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_match_param_skips() {
        let dir = scratch();
        std::fs::write(dir.join("a.rs"), "anything\n").unwrap();
        let r = rule("pattern.broken", &[("lang", "rust")]); // no `match`
        let engine = PatternsEngine;
        match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Skipped(_) => {}
            EngineOutcome::Ran(f) => panic!("expected skip, got {f:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_glob_filters() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("src_main.rs"), "let token = 1;\n").unwrap();
        std::fs::write(dir.join("tests").join("t.rs"), "let token = 2;\n").unwrap();

        let r = rule(
            "pattern.token",
            &[("match", "token"), ("path_glob", "tests/")],
        );
        let engine = PatternsEngine;
        let findings = match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(s) => panic!("skipped: {s}"),
        };
        assert_eq!(findings.len(), 1);
        assert!(findings[0].file.contains("tests/"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
