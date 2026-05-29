//! `deps` engine — dependency hygiene + RustSec advisory matching.
//!
//! Offline, deterministic. Parses `Cargo.lock` (TOML) and `package-lock.json`
//! under `ctx.root` and flags, per the routed rules' params:
//!   - `forbid_git`      : git / path sourced dependencies in the lockfile.
//!   - `forbid_wildcard` : `"*"` version requirements in manifests.
//!   - `forbid_yanked`   : packages whose lock entry is marked yanked.
//!   - RustSec advisories: when an advisory DB is reachable (`$RTRT_ADVISORY_DB`
//!     or `~/.cargo/advisory-db`), match locked crate versions against the
//!     `RUSTSEC-*.md` front-matter (`package` + `versions { patched/unaffected }`).
//!
//! The advisory sub-check is skipped silently when no DB is present, but
//! lockfile hygiene still runs. When neither lockfile exists at all, the whole
//! engine returns `Skipped("no lockfile")`.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

use crate::engine::{Engine, EngineOutcome, ScanContext};
use crate::finding::Finding;
use crate::profile::Rule;

#[derive(Default)]
pub struct DepsEngine;

impl Engine for DepsEngine {
    fn name(&self) -> &'static str {
        "deps"
    }

    fn scan(&self, ctx: &ScanContext, rules: &[&Rule]) -> EngineOutcome {
        let cargo_lock = ctx.root.join("Cargo.lock");
        let npm_lock = ctx.root.join("package-lock.json");
        let has_cargo = cargo_lock.is_file();
        let has_npm = npm_lock.is_file();

        if !has_cargo && !has_npm {
            return EngineOutcome::Skipped("no lockfile".into());
        }

        let mut findings = Vec::new();

        // Cargo lock entries: collected once, reused by every routed rule's
        // checks so we parse the file a single time.
        let cargo_pkgs = if has_cargo {
            std::fs::read_to_string(&cargo_lock)
                .ok()
                .and_then(|t| parse_cargo_lock(&t))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let npm_pkgs = if has_npm {
            std::fs::read_to_string(&npm_lock)
                .ok()
                .and_then(|t| parse_npm_lock(&t))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        for rule in rules {
            // --- forbid_git ---------------------------------------------------
            if rule.param_bool("forbid_git", false) {
                for (p, lockfile) in cargo_pkgs
                    .iter()
                    .map(|p| (p, "Cargo.lock"))
                    .chain(npm_pkgs.iter().map(|p| (p, "package-lock.json")))
                {
                    if let Some(src) = &p.source {
                        if src.starts_with("git+") {
                            findings.push(mk(
                                rule,
                                lockfile,
                                format!(
                                    "git dependency `{}` ({}) bypasses registry review",
                                    p.name, src
                                ),
                                format!("{} {} = {}", p.name, p.version, src),
                                "Pin to a published release instead of a git source.",
                            ));
                        }
                    }
                }
                // Path dependencies surface as sourceless lock entries plus a
                // manifest `path = ` key. Flag manifest path/git deps too.
                for (mfile, dep, kind) in scan_manifest_sources(&ctx.root) {
                    findings.push(mk(
                        rule,
                        &mfile,
                        format!("{kind} dependency `{dep}` bypasses registry review"),
                        format!("{dep} ({kind} source)"),
                        "Pin to a published release instead of a git/path source.",
                    ));
                }
            }

            // --- forbid_wildcard ---------------------------------------------
            if rule.param_bool("forbid_wildcard", false) {
                for (mfile, dep) in scan_manifest_wildcards(&ctx.root) {
                    findings.push(mk(
                        rule,
                        &mfile,
                        format!("wildcard version requirement for `{dep}`"),
                        format!("{dep} = \"*\""),
                        "Pin a concrete or caret version range; `*` accepts any future breaking release.",
                    ));
                }
            }

            // --- forbid_yanked -----------------------------------------------
            if rule.param_bool("forbid_yanked", false) {
                for p in &cargo_pkgs {
                    if p.yanked {
                        findings.push(mk(
                            rule,
                            "Cargo.lock",
                            format!("yanked dependency `{}` {}", p.name, p.version),
                            format!("{} {} (yanked)", p.name, p.version),
                            "Upgrade off the yanked version; it was pulled for a reason.",
                        ));
                    }
                }
            }

            // --- RustSec advisory match --------------------------------------
            if rule
                .param_str("db")
                .map(|d| d == "rustsec")
                .unwrap_or(false)
            {
                if let Some(dir) = advisory_db_dir() {
                    let advisories = load_advisories(&dir);
                    for p in &cargo_pkgs {
                        for adv in &advisories {
                            if adv.package == p.name && adv.affects(&p.version) {
                                findings.push(mk(
                                    rule,
                                    "Cargo.lock",
                                    format!(
                                        "{} {} affected by {}: {}",
                                        p.name, p.version, adv.id, adv.title
                                    ),
                                    format!("{} {} <= {}", p.name, p.version, adv.id),
                                    "Upgrade to a patched release listed in the advisory.",
                                ));
                            }
                        }
                    }
                }
                // No DB -> advisory sub-check silently skipped; hygiene already ran.
            }
        }

        EngineOutcome::Ran(findings)
    }
}

/// Build a finding carrying the rule's severity + standards.
fn mk(rule: &Rule, file: &str, title: String, excerpt: String, fix_hint: &str) -> Finding {
    Finding {
        rule_id: rule.id.clone(),
        engine: "deps".into(),
        severity: rule.severity,
        title,
        file: file.into(),
        line: None,
        excerpt: trim160(&excerpt),
        fix_hint: fix_hint.into(),
        standards: rule.standards.clone(),
    }
}

fn trim160(s: &str) -> String {
    if s.chars().count() <= 160 {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(157).collect();
        format!("{truncated}...")
    }
}

// ---------------------------------------------------------------------------
// Cargo.lock parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct LockPkg {
    name: String,
    version: String,
    source: Option<String>,
    yanked: bool,
}

/// Parse `Cargo.lock` into package entries. Honours both the v2/v3 array-of-
/// tables `[[package]]` layout.
fn parse_cargo_lock(text: &str) -> Option<Vec<LockPkg>> {
    let doc: TomlValue = toml::from_str(text).ok()?;
    let pkgs = doc.get("package")?.as_array()?;
    let mut out = Vec::new();
    for p in pkgs {
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let version = p
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let source = p.get("source").and_then(|v| v.as_str()).map(String::from);
        // Some tooling annotates yanked entries; accept a bool or string flag.
        let yanked = p
            .get("yanked")
            .map(|v| v.as_bool().unwrap_or(false) || v.as_str() == Some("true"))
            .unwrap_or(false);
        out.push(LockPkg {
            name: name.to_string(),
            version: version.to_string(),
            source,
            yanked,
        });
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// package-lock.json parsing
// ---------------------------------------------------------------------------

/// Parse npm `package-lock.json` into lock packages. Supports lockfile v2/v3
/// (`packages` map keyed by "node_modules/<name>") and legacy v1
/// (`dependencies` map).
fn parse_npm_lock(text: &str) -> Option<Vec<LockPkg>> {
    let doc: JsonValue = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();

    if let Some(map) = doc.get("packages").and_then(|v| v.as_object()) {
        for (key, meta) in map {
            if key.is_empty() {
                continue; // the root project entry
            }
            let name = key.rsplit("node_modules/").next().unwrap_or(key);
            let version = meta
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let resolved = meta
                .get("resolved")
                .and_then(|v| v.as_str())
                .map(String::from);
            let source = match &resolved {
                Some(r) if r.starts_with("git+") || r.contains("git+ssh") => {
                    Some(format!("git+{r}"))
                }
                other => other.clone(),
            };
            out.push(LockPkg {
                name: name.to_string(),
                version: version.to_string(),
                source,
                yanked: false,
            });
        }
    } else if let Some(map) = doc.get("dependencies").and_then(|v| v.as_object()) {
        for (name, meta) in map {
            let version = meta
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            out.push(LockPkg {
                name: name.clone(),
                version: version.to_string(),
                source: None,
                yanked: false,
            });
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Manifest scanning (wildcard + git/path sources)
// ---------------------------------------------------------------------------

/// Returns `(file, dep, kind)` for every git/path-sourced dependency declared
/// in a `Cargo.toml` under the root. (npm git deps are caught via the lockfile.)
fn scan_manifest_sources(root: &Path) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for manifest in walk_manifests(root, "Cargo.toml") {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<TomlValue>(&text) else {
            continue;
        };
        let rel = rel_str(root, &manifest);
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = doc.get(section).and_then(|v| v.as_table()) {
                for (name, spec) in deps {
                    if let Some(t) = spec.as_table() {
                        if t.contains_key("git") {
                            out.push((rel.clone(), name.clone(), "git".to_string()));
                        } else if t.contains_key("path") {
                            out.push((rel.clone(), name.clone(), "path".to_string()));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Returns `(file, dep)` for every `"*"` version requirement in a Cargo.toml.
fn scan_manifest_wildcards(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for manifest in walk_manifests(root, "Cargo.toml") {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<TomlValue>(&text) else {
            continue;
        };
        let rel = rel_str(root, &manifest);
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = doc.get(section).and_then(|v| v.as_table()) {
                for (name, spec) in deps {
                    let is_wild = match spec {
                        TomlValue::String(s) => s == "*",
                        TomlValue::Table(t) => {
                            t.get("version").and_then(|v| v.as_str()) == Some("*")
                        }
                        _ => false,
                    };
                    if is_wild {
                        out.push((rel.clone(), name.clone()));
                    }
                }
            }
        }
    }
    // package.json wildcard requirements.
    for manifest in walk_manifests(root, "package.json") {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<JsonValue>(&text) else {
            continue;
        };
        let rel = rel_str(root, &manifest);
        for section in ["dependencies", "devDependencies"] {
            if let Some(deps) = doc.get(section).and_then(|v| v.as_object()) {
                for (name, spec) in deps {
                    if spec.as_str() == Some("*") {
                        out.push((rel.clone(), name.clone()));
                    }
                }
            }
        }
    }
    out
}

/// Collect manifest files named `file_name` under `root`, honouring excludes.
fn walk_manifests(root: &Path, file_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() != file_name {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if rel.as_os_str().is_empty() || is_excluded(root, entry.path()) {
            continue;
        }
        out.push(entry.path().to_path_buf());
    }
    out
}

fn is_excluded(root: &Path, abs: &Path) -> bool {
    let rel = abs.strip_prefix(root).unwrap_or(abs);
    let s = rel.to_string_lossy();
    const ALWAYS: &[&str] = &[
        "/.git/",
        "/target/",
        "/node_modules/",
        "/.venv/",
        "/dist/",
        "/build/",
        "/.next/",
        "/vendor/",
    ];
    let padded = format!("/{s}/");
    ALWAYS.iter().any(|a| padded.contains(a))
}

fn rel_str(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// RustSec advisory DB
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Advisory {
    id: String,
    package: String,
    title: String,
    /// Versions explicitly patched/unaffected; a locked version that does NOT
    /// satisfy any of these (i.e. is below all of them) is treated as affected.
    patched: Vec<String>,
}

impl Advisory {
    /// Conservative offline check: with no semver crate available, an advisory
    /// is considered to affect `ver` when `ver` is strictly less than every
    /// listed patched version. If no patched versions are listed, every version
    /// of the package is affected.
    fn affects(&self, ver: &str) -> bool {
        if self.patched.is_empty() {
            return true;
        }
        self.patched.iter().all(|p| version_lt(ver, p))
    }
}

/// Resolve the advisory DB directory: `$RTRT_ADVISORY_DB` wins, else
/// `~/.cargo/advisory-db`. Returns `None` when neither exists.
fn advisory_db_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("RTRT_ADVISORY_DB") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
        return None;
    }
    let home = dirs::home_dir()?;
    let p = home.join(".cargo").join("advisory-db");
    if p.is_dir() { Some(p) } else { None }
}

/// Load every `RUSTSEC-*.md` advisory under `dir`, parsing the TOML front-matter
/// fenced by a leading ```` ```toml ```` block.
fn load_advisories(dir: &Path) -> Vec<Advisory> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !(name.starts_with("RUSTSEC-") && name.ends_with(".md")) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if let Some(adv) = parse_advisory(&text) {
                out.push(adv);
            }
        }
    }
    out
}

/// Extract the TOML front-matter from an advisory markdown file and read the
/// `[advisory]` id/package/title plus `[versions] patched` list.
fn parse_advisory(text: &str) -> Option<Advisory> {
    let front = extract_front_matter(text)?;
    let doc: TomlValue = toml::from_str(&front).ok()?;
    let adv = doc.get("advisory")?;
    let id = adv.get("id").and_then(|v| v.as_str())?.to_string();
    let package = adv.get("package").and_then(|v| v.as_str())?.to_string();
    let title = adv
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut patched = Vec::new();
    if let Some(versions) = doc.get("versions") {
        for key in ["patched", "unaffected"] {
            if let Some(arr) = versions.get(key).and_then(|v| v.as_array()) {
                for r in arr {
                    if let Some(s) = r.as_str() {
                        // Reqs look like ">= 1.2.3"; keep just the version core.
                        if let Some(v) = extract_version_core(s) {
                            patched.push(v);
                        }
                    }
                }
            }
        }
    }

    Some(Advisory {
        id,
        package,
        title,
        patched,
    })
}

/// Pull the front-matter between the first pair of ```` ``` ```` fences (RustSec
/// advisories wrap their TOML in a ```` ```toml ```` block at the top).
fn extract_front_matter(text: &str) -> Option<String> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    // skip an optional language tag line ("toml\n")
    let body_start = after.find('\n')? + 1;
    let body = &after[body_start..];
    let end = body.find("```")?;
    Some(body[..end].to_string())
}

/// Strip comparison operators / whitespace from a version requirement, keeping
/// the dotted numeric core (e.g. ">= 1.2.3" -> "1.2.3").
fn extract_version_core(req: &str) -> Option<String> {
    let core: String = req
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if core.is_empty() { None } else { Some(core) }
}

/// Numeric dotted-version less-than. Parses each component as u64; missing
/// components are treated as 0. Non-numeric tails (pre-release) are ignored.
fn version_lt(a: &str, b: &str) -> bool {
    let pa = parse_ver(a);
    let pb = parse_ver(b);
    pa < pb
}

fn parse_ver(v: &str) -> Vec<u64> {
    let core: String = v
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts: Vec<u64> = core
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect();
    // pad to 3 components for stable comparison
    while parts.len() < 3 {
        parts.push(0);
    }
    parts
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Severity, Standards};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Create a unique temp fixture dir.
    fn fixture_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("rtrt-deps-test-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rule(id: &str, params: &[(&str, TomlValue)]) -> Rule {
        let mut map: BTreeMap<String, TomlValue> = BTreeMap::new();
        for (k, v) in params {
            map.insert((*k).to_string(), v.clone());
        }
        Rule {
            id: id.to_string(),
            engine: "deps".to_string(),
            severity: Severity::High,
            description: String::new(),
            enabled: true,
            standards: Standards::default(),
            params: map,
        }
    }

    fn ctx(dir: &Path) -> ScanContext {
        ScanContext {
            root: dir.to_path_buf(),
            exclude: Vec::new(),
        }
    }

    #[test]
    fn no_lockfile_is_skipped() {
        let dir = fixture_dir();
        let engine = DepsEngine;
        let r = rule("deps.git", &[("forbid_git", TomlValue::Boolean(true))]);
        match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Skipped(reason) => assert_eq!(reason, "no lockfile"),
            EngineOutcome::Ran(_) => panic!("expected Skipped for missing lockfile"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn forbid_git_fires_on_git_locked_dep() {
        let dir = fixture_dir();
        let lock = r#"
version = 3

[[package]]
name = "good"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "sketchy"
version = "0.1.0"
source = "git+https://github.com/evil/sketchy#abc123"
"#;
        std::fs::write(dir.join("Cargo.lock"), lock).unwrap();

        let engine = DepsEngine;
        let r = rule("deps.git", &[("forbid_git", TomlValue::Boolean(true))]);
        let findings = match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(s) => panic!("unexpected skip: {s}"),
        };
        assert!(
            findings.iter().any(|f| f.title.contains("sketchy")),
            "expected a git-source finding for `sketchy`, got: {findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.title.contains("good")),
            "registry dep should not be flagged"
        );
        assert_eq!(findings[0].engine, "deps");
        assert_eq!(findings[0].severity, Severity::High);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clean_lockfile_yields_no_findings() {
        let dir = fixture_dir();
        let lock = r#"
version = 3

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        std::fs::write(dir.join("Cargo.lock"), lock).unwrap();
        // a clean manifest too — no wildcards, no git/path sources
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nserde = \"1.0\"\n",
        )
        .unwrap();

        let engine = DepsEngine;
        let r = rule(
            "deps.all",
            &[
                ("forbid_git", TomlValue::Boolean(true)),
                ("forbid_wildcard", TomlValue::Boolean(true)),
                ("forbid_yanked", TomlValue::Boolean(true)),
            ],
        );
        let findings = match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(s) => panic!("unexpected skip: {s}"),
        };
        assert!(
            findings.is_empty(),
            "clean project should yield no findings, got: {findings:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn forbid_wildcard_fires_on_star_req() {
        let dir = fixture_dir();
        std::fs::write(dir.join("Cargo.lock"), "version = 3\n").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nrand = \"*\"\n",
        )
        .unwrap();

        let engine = DepsEngine;
        let r = rule(
            "deps.wildcard",
            &[("forbid_wildcard", TomlValue::Boolean(true))],
        );
        let findings = match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Ran(f) => f,
            EngineOutcome::Skipped(s) => panic!("unexpected skip: {s}"),
        };
        assert!(
            findings.iter().any(|f| f.title.contains("rand")),
            "expected wildcard finding for `rand`, got: {findings:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn npm_lockfile_alone_is_not_skipped() {
        let dir = fixture_dir();
        let pkg_lock = r#"{
  "name": "demo",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "demo", "version": "1.0.0" },
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"
    }
  }
}"#;
        std::fs::write(dir.join("package-lock.json"), pkg_lock).unwrap();
        let engine = DepsEngine;
        let r = rule("deps.git", &[("forbid_git", TomlValue::Boolean(true))]);
        match engine.scan(&ctx(&dir), &[&r]) {
            EngineOutcome::Ran(_) => {}
            EngineOutcome::Skipped(s) => panic!("npm lockfile should run, got skip: {s}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn version_compare_is_numeric() {
        assert!(version_lt("1.2.3", "1.2.4"));
        assert!(version_lt("0.9.0", "1.0.0"));
        assert!(!version_lt("2.0.0", "1.9.9"));
        assert!(!version_lt("1.0.0", "1.0.0"));
    }
}
