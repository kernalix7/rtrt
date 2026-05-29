//! `ai` engine — AI-artifact-specific checks.
//!
//! Each routed rule selects one `check` (via `rule.param_str("check")`):
//!   - `hallucinated-import` : imports/requires of packages not in declared deps
//!     (the "slopsquatting" supply-chain risk from LLM-invented package names).
//!   - `base64-blob`         : long base64 literals (>=120 chars) in source.
//!   - `eval-usage`          : eval()/exec()/Function()/child_process/pickle.loads.
//!   - `todo-secret`         : TODO/FIXME/XXX adjacent to auth/secret/password/token.
//!   - `unsafe-block`        : Rust `unsafe` blocks.
//!
//! Engines are synchronous and offline: deps come from on-disk manifests, never
//! the network.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use walkdir::WalkDir;

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::finding::Finding;
use crate::profile::Rule;

#[derive(Default)]
pub struct AiEngine;

impl Engine for AiEngine {
    fn name(&self) -> &'static str {
        "ai"
    }

    fn scan(&self, ctx: &ScanContext, rules: &[&Rule]) -> EngineOutcome {
        let mut findings = Vec::new();

        // Lazily computed shared inputs.
        let rust_files = collect_files(ctx, &["rs"]);
        let js_files = collect_files(ctx, &["js", "jsx", "mjs", "cjs", "ts", "tsx"]);

        for rule in rules {
            let check = rule.param_str("check").unwrap_or("");
            match check {
                "hallucinated-import" => {
                    findings.extend(check_hallucinated_imports(
                        ctx,
                        rule,
                        &rust_files,
                        &js_files,
                    ));
                }
                "base64-blob" => {
                    findings.extend(check_base64_blob(ctx, rule, &rust_files, &js_files));
                }
                "eval-usage" => {
                    findings.extend(check_eval_usage(ctx, rule));
                }
                "todo-secret" => {
                    findings.extend(check_todo_secret(ctx, rule));
                }
                "unsafe-block" => {
                    findings.extend(check_unsafe_block(ctx, rule, &rust_files));
                }
                _ => {
                    // Unknown / missing check: skip this rule silently rather
                    // than erroring the whole engine.
                }
            }
        }

        EngineOutcome::Ran(findings)
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Walk `ctx.root`, honouring excludes, collecting files with one of `exts`.
/// Returns (relative-path, contents) pairs; unreadable files are skipped.
fn collect_files(ctx: &ScanContext, exts: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in WalkDir::new(&ctx.root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = match path.strip_prefix(&ctx.root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if ctx.is_excluded(rel) {
            continue;
        }
        let has_ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
            .unwrap_or(false);
        if !has_ext {
            continue;
        }
        if let Ok(contents) = std::fs::read_to_string(path) {
            out.push((rel.to_string_lossy().into_owned(), contents));
        }
    }
    out
}

/// Build a finding off `rule`, copying its severity and standards.
fn make_finding(
    rule: &Rule,
    file: &str,
    line: Option<usize>,
    title: String,
    excerpt: String,
    fix_hint: &str,
) -> Finding {
    Finding {
        rule_id: rule.id.clone(),
        engine: "ai".to_string(),
        severity: rule.severity,
        title,
        file: file.to_string(),
        line,
        excerpt: trim_excerpt(&excerpt),
        fix_hint: fix_hint.to_string(),
        standards: rule.standards.clone(),
    }
}

/// Trim an excerpt to a sane single-line length.
fn trim_excerpt(s: &str) -> String {
    let one_line: String = s.trim().chars().take(160).collect();
    one_line
}

// ---------------------------------------------------------------------------
// hallucinated-import
// ---------------------------------------------------------------------------

static RUST_USE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*use\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
static RUST_EXTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*extern\s+crate\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
// `import ... from "x"` and `import "x"`.
static JS_IMPORT_FROM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\bfrom\s+['"]([^'"]+)['"]"#).unwrap());
static JS_IMPORT_BARE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^\s*import\s+['"]([^'"]+)['"]"#).unwrap());
static JS_REQUIRE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\brequire\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap());

/// Rust paths that are never external crates.
const RUST_BUILTINS: &[&str] = &["std", "core", "alloc", "self", "crate", "super"];

fn check_hallucinated_imports(
    ctx: &ScanContext,
    rule: &Rule,
    rust_files: &[(String, String)],
    js_files: &[(String, String)],
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // --- Rust ---
    if !rust_files.is_empty() {
        let declared = declared_rust_deps(ctx);
        // Only check when we actually found a manifest; otherwise we'd flag
        // everything in a sub-crate that imports workspace siblings, etc.
        if !declared.is_empty() {
            for (rel, contents) in rust_files {
                for (i, line) in contents.lines().enumerate() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("*") {
                        continue;
                    }
                    let cap = RUST_USE
                        .captures(line)
                        .or_else(|| RUST_EXTERN.captures(line));
                    let Some(cap) = cap else { continue };
                    let krate = normalise_rust_crate(&cap[1]);
                    if RUST_BUILTINS.contains(&krate.as_str()) {
                        continue;
                    }
                    if declared.contains(&krate) {
                        continue;
                    }
                    findings.push(make_finding(
                        rule,
                        rel,
                        Some(i + 1),
                        format!("import of undeclared crate `{krate}` (possible AI hallucination)"),
                        line.to_string(),
                        "Verify the crate exists and add it to Cargo.toml, or remove the import.",
                    ));
                }
            }
        }
    }

    // --- JS / TS ---
    if !js_files.is_empty() {
        let declared = declared_js_deps(ctx);
        if !declared.is_empty() {
            for (rel, contents) in js_files {
                for (i, line) in contents.lines().enumerate() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("*") {
                        continue;
                    }
                    let mut specs: Vec<&str> = Vec::new();
                    if let Some(c) = JS_IMPORT_FROM.captures(line) {
                        specs.push(c.get(1).unwrap().as_str());
                    }
                    if let Some(c) = JS_IMPORT_BARE.captures(line) {
                        specs.push(c.get(1).unwrap().as_str());
                    }
                    if let Some(c) = JS_REQUIRE.captures(line) {
                        specs.push(c.get(1).unwrap().as_str());
                    }
                    for spec in specs {
                        // Relative / absolute path imports are local, not packages.
                        if spec.starts_with('.') || spec.starts_with('/') {
                            continue;
                        }
                        let pkg = js_package_root(spec);
                        if NODE_BUILTINS.contains(&pkg.as_str()) {
                            continue;
                        }
                        if declared.contains(&pkg) {
                            continue;
                        }
                        findings.push(make_finding(
                            rule,
                            rel,
                            Some(i + 1),
                            format!(
                                "import of undeclared package `{pkg}` (possible AI hallucination)"
                            ),
                            line.to_string(),
                            "Verify the package exists on the registry and add it to package.json, or remove the import.",
                        ));
                    }
                }
            }
        }
    }

    findings
}

/// Crate names are dashed on the registry but underscored in `use` paths.
/// Normalise to the underscore form for set comparison.
fn normalise_rust_crate(name: &str) -> String {
    name.replace('-', "_")
}

/// Read declared crate deps from the nearest `Cargo.toml` at `ctx.root`.
/// Includes `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`.
fn declared_rust_deps(ctx: &ScanContext) -> HashSet<String> {
    let mut set = HashSet::new();
    let manifest = ctx.root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return set;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return set;
    };
    for table_name in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(tbl) = value.get(table_name).and_then(|v| v.as_table()) {
            for (name, spec) in tbl {
                set.insert(normalise_rust_crate(name));
                // Honour `package = "real-name"` renames.
                if let Some(real) = spec.get("package").and_then(|v| v.as_str()) {
                    set.insert(normalise_rust_crate(real));
                }
            }
        }
    }
    // Also accept workspace-internal references named in [package].
    if let Some(pkg) = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        set.insert(normalise_rust_crate(pkg));
    }
    set
}

/// Node builtin modules (subset that matters; never flagged).
const NODE_BUILTINS: &[&str] = &[
    "assert",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "crypto",
    "dgram",
    "dns",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "net",
    "os",
    "path",
    "process",
    "querystring",
    "readline",
    "stream",
    "string_decoder",
    "timers",
    "tls",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

/// The package "root" of an import specifier: `lodash/fp` -> `lodash`,
/// `@scope/pkg/sub` -> `@scope/pkg`, `node:fs` -> `fs`.
fn js_package_root(spec: &str) -> String {
    let spec = spec.strip_prefix("node:").unwrap_or(spec);
    if let Some(rest) = spec.strip_prefix('@') {
        // scoped: keep first two segments.
        let mut it = rest.splitn(3, '/');
        let scope = it.next().unwrap_or("");
        let name = it.next().unwrap_or("");
        if name.is_empty() {
            format!("@{scope}")
        } else {
            format!("@{scope}/{name}")
        }
    } else {
        spec.split('/').next().unwrap_or(spec).to_string()
    }
}

/// Read declared package deps from `package.json` at `ctx.root`.
fn declared_js_deps(ctx: &ScanContext) -> HashSet<String> {
    let mut set = HashSet::new();
    let manifest = ctx.root.join("package.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return set;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return set;
    };
    for table_name in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = value.get(table_name).and_then(|v| v.as_object()) {
            for name in obj.keys() {
                set.insert(name.clone());
            }
        }
    }
    set
}

// ---------------------------------------------------------------------------
// base64-blob
// ---------------------------------------------------------------------------

// A standalone base64 run of >=120 chars (optionally padded).
static BASE64_BLOB: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z0-9+/]{120,}={0,2}").unwrap());

fn check_base64_blob(
    ctx: &ScanContext,
    rule: &Rule,
    rust_files: &[(String, String)],
    js_files: &[(String, String)],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    // Scan all source-ish text; reuse the gathered rust+js sets plus python.
    let py_files = collect_files(ctx, &["py"]);
    for (rel, contents) in rust_files.iter().chain(js_files).chain(py_files.iter()) {
        for (i, line) in contents.lines().enumerate() {
            if let Some(m) = BASE64_BLOB.find(line) {
                let blob = m.as_str();
                let preview = format!("{}…(+{} chars)", &blob[..blob.len().min(24)], blob.len());
                findings.push(make_finding(
                    rule,
                    rel,
                    Some(i + 1),
                    format!("suspicious base64 blob ({} chars) in source", blob.len()),
                    preview,
                    "Inspect this literal; large embedded base64 can smuggle a payload. Load such data from a vetted external resource instead.",
                ));
            }
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// eval-usage
// ---------------------------------------------------------------------------

static EVAL_PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            Regex::new(r"\beval\s*\(").unwrap(),
            "dynamic eval() of untrusted input",
        ),
        (
            Regex::new(r"\bexec\s*\(").unwrap(),
            "dynamic exec() of untrusted input",
        ),
        (
            Regex::new(r"\bnew\s+Function\s*\(").unwrap(),
            "new Function() builds code from a string",
        ),
        (
            Regex::new(r"\bFunction\s*\(").unwrap(),
            "Function() constructor builds code from a string",
        ),
        (
            Regex::new(r"child_process").unwrap(),
            "child_process invocation",
        ),
        (
            Regex::new(r"pickle\.loads\s*\(").unwrap(),
            "pickle.loads() deserialises arbitrary objects",
        ),
    ]
});

fn check_eval_usage(ctx: &ScanContext, rule: &Rule) -> Vec<Finding> {
    let mut findings = Vec::new();
    let files = collect_files(ctx, &["rs", "py", "js", "jsx", "mjs", "cjs", "ts", "tsx"]);
    for (rel, contents) in &files {
        for (i, line) in contents.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
                continue;
            }
            for (re, desc) in EVAL_PATTERNS.iter() {
                if re.is_match(line) {
                    findings.push(make_finding(
                        rule,
                        rel,
                        Some(i + 1),
                        format!("{desc} without validation"),
                        line.to_string(),
                        "Avoid dynamic code execution; validate/allowlist inputs or use a safe parser.",
                    ));
                    break; // one finding per line is enough.
                }
            }
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// todo-secret
// ---------------------------------------------------------------------------

static TODO_SECRET: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(TODO|FIXME|XXX)\b.*\b(auth|secret|password|passwd|token|credential|api[_-]?key)\b",
    )
    .unwrap()
});

fn check_todo_secret(ctx: &ScanContext, rule: &Rule) -> Vec<Finding> {
    let mut findings = Vec::new();
    let files = collect_files(
        ctx,
        &[
            "rs", "py", "js", "jsx", "mjs", "cjs", "ts", "tsx", "go", "java", "rb", "php",
        ],
    );
    for (rel, contents) in &files {
        for (i, line) in contents.lines().enumerate() {
            if TODO_SECRET.is_match(line) {
                findings.push(make_finding(
                    rule,
                    rel,
                    Some(i + 1),
                    "security TODO/FIXME left near auth/secret handling".to_string(),
                    line.to_string(),
                    "Resolve the security stub before shipping; AI-generated code often leaves auth gaps marked as TODO.",
                ));
            }
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// unsafe-block
// ---------------------------------------------------------------------------

// `unsafe {` or `unsafe fn` / `unsafe impl` — the `\b` ensures we don't catch
// identifiers containing "unsafe".
static UNSAFE_BLOCK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bunsafe\s*(\{|fn\b|impl\b|trait\b)").unwrap());

fn check_unsafe_block(
    _ctx: &ScanContext,
    rule: &Rule,
    rust_files: &[(String, String)],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (rel, contents) in rust_files {
        for (i, line) in contents.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if UNSAFE_BLOCK.is_match(line) {
                findings.push(make_finding(
                    rule,
                    rel,
                    Some(i + 1),
                    "Rust `unsafe` block/item".to_string(),
                    line.to_string(),
                    "Confirm the unsafe is necessary and sound; AI often emits unneeded unsafe. Prefer safe abstractions.",
                ));
            }
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Severity, Standards};
    use std::path::PathBuf;

    /// Create a unique temp fixture dir; caller writes files under it.
    fn fixture_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rtrt-ai-test-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rule(check: &str) -> Rule {
        let toml_text = format!(
            r#"
id = "ai.test"
engine = "ai"
severity = "high"
check = "{check}"
"#
        );
        toml::from_str(&toml_text).unwrap()
    }

    fn ctx(root: PathBuf) -> ScanContext {
        ScanContext {
            root,
            exclude: Vec::new(),
        }
    }

    fn run(engine: &AiEngine, ctx: &ScanContext, rules: &[&Rule]) -> Vec<Finding> {
        match engine.scan(ctx, rules) {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(why) => panic!("unexpected skip: {why}"),
        }
    }

    #[test]
    fn hallucinated_import_fires_and_clean_is_empty() {
        let dir = fixture_dir("halluc");
        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("good.rs"),
            "use serde::Serialize;\nuse std::collections::HashMap;\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("bad.rs"),
            "use serde::Deserialize;\nuse totally_made_up_crate::Thing;\n",
        )
        .unwrap();

        let r = rule("hallucinated-import");
        let engine = AiEngine;
        let findings = run(&engine, &ctx(dir.clone()), &[&r]);

        // exactly one finding: the made-up crate. serde + std must not fire.
        assert_eq!(findings.len(), 1, "findings: {findings:?}");
        let f = &findings[0];
        assert!(f.title.contains("totally_made_up_crate"));
        assert_eq!(f.engine, "ai");
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.rule_id, "ai.test");
        assert_eq!(f.file, "bad.rs");
        assert_eq!(f.line, Some(2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clean_rust_project_has_no_hallucinations() {
        let dir = fixture_dir("clean");
        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1"
regex = "1"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "use serde::Serialize;\nuse regex::Regex;\nuse crate::foo;\nuse std::io;\n",
        )
        .unwrap();

        let r = rule("hallucinated-import");
        let findings = run(&AiEngine, &ctx(dir.clone()), &[&r]);
        assert!(findings.is_empty(), "expected clean, got: {findings:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsafe_block_fires() {
        let dir = fixture_dir("unsafe");
        std::fs::write(
            dir.join("x.rs"),
            "fn safe() {}\nfn risky() { unsafe { std::ptr::null::<u8>(); } }\n",
        )
        .unwrap();

        let r = rule("unsafe-block");
        let findings = run(&AiEngine, &ctx(dir.clone()), &[&r]);
        assert_eq!(findings.len(), 1, "findings: {findings:?}");
        assert_eq!(findings[0].line, Some(2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn todo_secret_fires_and_plain_todo_is_ignored() {
        let dir = fixture_dir("todo");
        std::fs::write(
            dir.join("a.py"),
            "# TODO: refactor this loop\n# FIXME: validate the auth token before use\n",
        )
        .unwrap();

        let r = rule("todo-secret");
        let findings = run(&AiEngine, &ctx(dir.clone()), &[&r]);
        assert_eq!(findings.len(), 1, "findings: {findings:?}");
        assert_eq!(findings[0].line, Some(2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eval_usage_fires() {
        let dir = fixture_dir("eval");
        std::fs::write(dir.join("a.js"), "const safe = 1;\neval(userInput);\n").unwrap();

        let r = rule("eval-usage");
        let findings = run(&AiEngine, &ctx(dir.clone()), &[&r]);
        assert_eq!(findings.len(), 1, "findings: {findings:?}");
        assert_eq!(findings[0].line, Some(2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base64_blob_fires() {
        let dir = fixture_dir("b64");
        let blob = "A".repeat(150);
        std::fs::write(dir.join("a.py"), format!("data = \"{blob}\"\n")).unwrap();

        let r = rule("base64-blob");
        let findings = run(&AiEngine, &ctx(dir.clone()), &[&r]);
        assert_eq!(findings.len(), 1, "findings: {findings:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn standards_are_copied_from_rule() {
        // sanity: a rule with standards propagates them to the finding.
        let dir = fixture_dir("std");
        std::fs::write(dir.join("x.rs"), "fn f() { unsafe { 1; } }\n").unwrap();
        let mut r = rule("unsafe-block");
        r.standards = Standards {
            cwe: vec!["CWE-242".to_string()],
            ..Default::default()
        };
        let findings = run(&AiEngine, &ctx(dir.clone()), &[&r]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].standards.cwe, vec!["CWE-242".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
