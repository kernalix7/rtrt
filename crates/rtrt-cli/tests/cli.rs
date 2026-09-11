//! Hermetic integration tests for the `rtrt` binary.
//!
//! Every invocation pins HOME (and the store, where applicable) to a temp
//! directory so tests never read or write the real `~/.rtrt` / `~/.claude`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// A `rtrt` command with HOME isolated to `home`.
/// Canonical only where it matters. macOS reaches the temp dir through
/// `/var -> /private/var`, which breaks comparisons against canonical paths.
/// Windows canonicalization instead yields a `\\?\` verbatim path, which the
/// production code rejects, so the plain temp path is the correct fixture there.
fn canonical_for_tests(path: &std::path::Path) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::fs::canonicalize(path).expect("canonicalize temp path")
    }
    #[cfg(not(unix))]
    {
        path.to_path_buf()
    }
}

/// A temp HOME whose path is canonical.
///
/// macOS reaches the system temp dir through `/var -> /private/var`, and the
/// OpenCode store deliberately refuses any database reached through a symlink.
/// Exposing `path()` keeps every call site identical to a plain `TempDir`.
struct CanonicalHome {
    _guard: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl CanonicalHome {
    fn new() -> Self {
        let guard = tempfile::tempdir().unwrap();
        let path = canonical_for_tests(guard.path());
        Self {
            _guard: guard,
            path,
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn rtrt(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("rtrt").expect("rtrt binary builds");
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("RTRT_CONFIG", home.join(".rtrt").join("config.toml"))
        .env_remove("RTRT_MEMORY_PATH")
        .env_remove("RTRT_CLAUDE_RATE_LIMIT_CACHE")
        .env_remove("RTRT_CLAUDE_RATE_LIMIT_MAX_AGE_SEC")
        .env_remove("OPENCODE_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME");
    cmd
}

fn read_http_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= body_start + content_length {
            break;
        }
    }
}

#[test]
fn version_prints_version_string() {
    let home = CanonicalHome::new();
    rtrt(home.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn project_refresh_retires_exact_owned_legacy_orchestration() {
    const LEGACY_SECTION: &str = r#"## 11. Agent Teams

| Agent | Owned Paths | Domain | Model |
|-------|-------------|--------|-------|
| tech-lead | All | orchestration, planning, integration | claude-opus-4-5 |
| explorer | All | read-only code discovery and symbol mapping | claude-opus-4-5 |
| code-reviewer | All | diff review, conventions, security, tests | claude-sonnet-4-5 |
| log-analyzer | logs, traces, CI output | failure diagnosis and root cause analysis | claude-sonnet-4-5 |
"#;
    const LEGACY_TECH_LEAD: &str = r#"---
name: tech-lead
description: Orchestrates cross-cutting work, assigns sub-agents, integrates results, and enforces conventions.
tools: Read, Bash, Glob, Grep, Edit, Write
model: claude-opus-4-5
---

Break down cross-cutting tasks, assign focused sub-agent work, integrate results, and enforce this repository's conventions. Keep changes scoped, resolve conflicts deliberately, and make verification explicit before handoff.
"#;

    // Given a repository containing exact bytes emitted by the legacy standardization template.
    let home = CanonicalHome::new();
    let project = home.path().join("project");
    let agents = project.join(".claude/agents");
    std::fs::create_dir_all(&agents).unwrap();
    let contract = format!("# project\n\n## 1. Project Identity\n\nKeep me.\n\n{LEGACY_SECTION}");
    std::fs::write(project.join("CLAUDE.md"), &contract).unwrap();
    std::fs::write(agents.join("tech-lead.md"), LEGACY_TECH_LEAD).unwrap();

    // When project refresh is applied.
    rtrt(home.path())
        .args([
            "project",
            "refresh",
            "--path",
            project.to_str().unwrap(),
            "--apply",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("retired legacy orchestration"));

    // Then owned orchestration is absent and its original bytes are backed up.
    assert!(
        !std::fs::read_to_string(project.join("CLAUDE.md"))
            .unwrap()
            .contains("## 11. Agent Teams")
    );
    assert_eq!(
        std::fs::read_to_string(project.join("CLAUDE.md.bak")).unwrap(),
        contract
    );
    assert!(!agents.join("tech-lead.md").exists());
    assert_eq!(
        std::fs::read_to_string(agents.join("tech-lead.md.bak")).unwrap(),
        LEGACY_TECH_LEAD
    );
}

#[test]
fn opencode_session_status_and_dry_run_are_cwd_independent_and_read_only() {
    let home = CanonicalHome::new();
    let data = home.path().join("xdg-data");
    std::fs::create_dir_all(data.join("opencode")).unwrap();
    let source = data.join("opencode/opencode.db");
    let connection = rusqlite::Connection::open(&source).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, directory TEXT);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, data TEXT);
             INSERT INTO session VALUES('lost','/deleted');",
        )
        .unwrap();
    drop(connection);
    let before = std::fs::read(&source).unwrap();

    for action in ["status", "dry-run"] {
        rtrt(home.path())
            .env("XDG_DATA_HOME", &data)
            .current_dir(home.path())
            .args(["opencode", "sessions", action])
            .assert()
            .success()
            .stdout(predicate::str::contains("sessions=1"));
    }
    assert_eq!(std::fs::read(source).unwrap(), before);
    assert!(!home.path().join(".rtrt/projects").exists());
}

#[test]
fn compress_ultra_preserves_paths_and_negations() {
    let home = CanonicalHome::new();
    rtrt(home.path())
        .args(["compress", "--level", "ultra"])
        .write_stdin("Make sure you do not delete docs/reference/api.md")
        .assert()
        .success()
        .stdout(predicate::str::contains("docs/reference/api.md"))
        .stdout(predicate::str::contains("not"));
}

#[test]
fn memory_save_then_recall_roundtrip() {
    let home = CanonicalHome::new();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "save",
            "--admin-legacy-store",
            store_s,
            "--project",
            "itest",
            "the gateway binds loopback by default",
        ])
        .assert()
        .success();

    rtrt(home.path())
        .args([
            "memory",
            "recall",
            "--admin-legacy-store",
            store_s,
            "--project",
            "itest",
            "--query",
            "gateway loopback",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("loopback"));
}

#[test]
fn punctuated_recall_query_does_not_error() {
    let home = CanonicalHome::new();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "save",
            "--admin-legacy-store",
            store_s,
            "--project",
            "itest",
            "auth notes",
        ])
        .assert()
        .success();

    // FTS5 metacharacters must not surface as SQL errors (PR #62 sanitizer).
    rtrt(home.path())
        .args([
            "memory",
            "recall",
            "--admin-legacy-store",
            store_s,
            "--project",
            "itest",
            "--query",
            "don't C++ (auth)",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("fts5").not());
}

#[test]
fn memory_reembed_dry_run_honours_project_scope() {
    let home = CanonicalHome::new();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    for (project, body) in [("p1", "project one row"), ("p2", "project two row")] {
        rtrt(home.path())
            .args([
                "memory",
                "save",
                "--admin-legacy-store",
                store_s,
                "--project",
                project,
                body,
            ])
            .assert()
            .success();
    }

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "--model",
            "bge-m3",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 row(s) pending"))
        .stdout(predicate::str::contains("project=`p1`"))
        .stdout(predicate::str::contains("project two row").not());

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--all",
            "--model",
            "bge-m3",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 row(s) pending"))
        .stdout(predicate::str::contains("all projects"));
}

#[test]
fn memory_reembed_rejects_invalid_batch_and_conflicting_scope() {
    let home = CanonicalHome::new();

    rtrt(home.path())
        .args(["memory", "reembed", "--batch", "0", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--batch"));

    rtrt(home.path())
        .args(["memory", "reembed", "--batch", "257", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must not exceed 256"));

    rtrt(home.path())
        .args(["memory", "reembed", "--workers", "33", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must not exceed 32"));

    rtrt(home.path())
        .args(["memory", "reembed", "--project", "p1", "--all", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn memory_reembed_probe_reports_pending_rows() {
    let home = CanonicalHome::new();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "save",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "pending row",
        ])
        .assert()
        .success();

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "--model",
            "bge-m3",
            "--probe",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stale rows still present"));
}

#[test]
fn memory_reembed_rejects_malformed_config() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let home = CanonicalHome::new();
    let config_dir = home.path().join(".rtrt");
    std::fs::create_dir(&config_dir).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(config_dir.join("config.toml"), "not valid = [toml").unwrap();

    rtrt(home.path())
        .args(["memory", "reembed", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("config"));
}

#[test]
fn memory_reembed_persists_successful_rows_before_mid_batch_failure() {
    let home = CanonicalHome::new();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    for body in ["first row", "second row", "third row"] {
        rtrt(home.path())
            .args([
                "memory",
                "save",
                "--admin-legacy-store",
                store_s,
                "--project",
                "p1",
                body,
            ])
            .assert()
            .success();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (status, body) in [
            ("200 OK", r#"{"embeddings":[[0.1,0.2],[0.3,0.4]]}"#),
            ("500 Internal Server Error", r#"{"error":"failed"}"#),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "--model",
            "bge-m3",
            "--base-url",
            &base_url,
            "--batch",
            "2",
            "--workers",
            "1",
        ])
        .assert()
        .failure();
    server.join().unwrap();

    let conn = rusqlite::Connection::open(&store).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.vector FROM embeddings e
               JOIN memories m ON m.id = e.memory_id
              WHERE m.project = 'p1' AND e.model = 'bge-m3'
              ORDER BY m.id",
        )
        .unwrap();
    let blobs = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let vectors = blobs
        .iter()
        .map(|blob| rtrt_memory::vector_from_blob(blob).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(vectors, [vec![0.1, 0.2], vec![0.3, 0.4]]);

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "--model",
            "bge-m3",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 row(s) pending"));
}

#[test]
fn memory_reembed_falls_back_to_legacy_ollama_endpoint() {
    let home = CanonicalHome::new();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    for body in ["first row", "second row"] {
        rtrt(home.path())
            .args([
                "memory",
                "save",
                "--admin-legacy-store",
                store_s,
                "--project",
                "p1",
                body,
            ])
            .assert()
            .success();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (status, body) in [
            ("404 Not Found", r#"{"error":"not found"}"#),
            ("200 OK", r#"{"embedding":[0.1,0.2]}"#),
            ("200 OK", r#"{"embedding":[0.3,0.4]}"#),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "--model",
            "bge-m3",
            "--base-url",
            &base_url,
            "--batch",
            "2",
            "--workers",
            "1",
        ])
        .assert()
        .success();
    server.join().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "reembed",
            "--admin-legacy-store",
            store_s,
            "--project",
            "p1",
            "--model",
            "bge-m3",
            "--probe",
        ])
        .assert()
        .success();
}

#[test]
fn gain_survives_empty_stats() {
    let home = CanonicalHome::new();
    rtrt(home.path()).arg("gain").assert().success();
}

fn repo(root: &std::path::Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
}

#[test]
fn normal_memory_isolates_same_basename_repositories_and_rejects_spoofing() {
    let home = CanonicalHome::new();
    let first = home.path().join("one/repo");
    let second = home.path().join("two/repo");
    repo(&first);
    repo(&second);
    let foreign_store = home.path().join("foreign.sqlite");

    rtrt(home.path())
        .current_dir(&first)
        .args(["memory", "save", "first repository secret"])
        .assert()
        .success();
    rtrt(home.path())
        .current_dir(&second)
        .args(["memory", "recall", "--query", "repository secret"])
        .assert()
        .success()
        .stdout(predicate::str::contains("first repository secret").not());

    rtrt(home.path())
        .current_dir(&first)
        .args([
            "memory",
            "recall",
            "--project",
            "foreign",
            "--query",
            "secret",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "foreign project assertion rejected",
        ));
    rtrt(home.path())
        .current_dir(&first)
        .env("RTRT_PROJECT", "foreign")
        .args(["memory", "recall", "--query", "secret"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RTRT_PROJECT"));
    rtrt(home.path())
        .current_dir(&first)
        .env("RTRT_DEFAULT_PROJECT", "foreign")
        .args(["memory", "recall", "--query", "secret"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RTRT_DEFAULT_PROJECT"));
    rtrt(home.path())
        .current_dir(&first)
        .env("RTRT_PARENT_PROJECT", "foreign")
        .args(["memory", "recall", "--query", "secret"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RTRT_PARENT_PROJECT"));
    rtrt(home.path())
        .current_dir(&first)
        .args([
            "memory",
            "recall",
            "--store",
            foreign_store.to_str().unwrap(),
            "--query",
            "secret",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--admin-legacy-store"));
    rtrt(home.path())
        .current_dir(&first)
        .env("RTRT_PROJECT", "foreign")
        .args(["hook", "capture", "user-prompt-submit"])
        .write_stdin(r#"{"prompt":"must not be captured"}"#)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "foreign project assertion rejected",
        ));
}

#[test]
fn linked_worktree_uses_main_repository_memory() {
    let home = CanonicalHome::new();
    let main = home.path().join("main");
    let linked = home.path().join("linked");
    std::fs::create_dir(&main).unwrap();
    let git = |args: &[&str], cwd: &std::path::Path| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "rtrt-test")
            .env("GIT_AUTHOR_EMAIL", "rtrt@example.invalid")
            .env("GIT_COMMITTER_NAME", "rtrt-test")
            .env("GIT_COMMITTER_EMAIL", "rtrt@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"], &main);
    std::fs::write(main.join("README"), "test").unwrap();
    git(&["add", "README"], &main);
    git(&["commit", "-q", "-m", "init"], &main);
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked-test",
            linked.to_str().unwrap(),
        ],
        &main,
    );

    rtrt(home.path())
        .current_dir(&main)
        .args(["memory", "save", "shared worktree fact"])
        .assert()
        .success();
    rtrt(home.path())
        .current_dir(&linked)
        .args(["memory", "recall", "--query", "worktree fact"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shared worktree fact"));
}

#[test]
fn opencode_setup_has_no_shared_memory_argv_and_restores_history_keybinds() {
    let home = CanonicalHome::new();
    let opencode = home.path().join(".config/opencode");
    std::fs::create_dir_all(&opencode).unwrap();
    let config = opencode.join("opencode.json");
    let tui = opencode.join("tui.json");
    std::fs::write(&config, r#"{"foreign":true}"#).unwrap();
    std::fs::write(
        &tui,
        r#"{"keybinds":{"history_previous":"ctrl+p","history_next":"ctrl+n","other":"keep"}}"#,
    )
    .unwrap();

    rtrt(home.path())
        .args(["setup", "--agent", "opencode", "--apply"])
        .assert()
        .success();
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    let command = root["mcp"]["rtrt"]["command"].as_array().unwrap();
    assert!(
        !command
            .iter()
            .any(|arg| matches!(arg.as_str(), Some("--memory" | "--admin-legacy-memory")))
    );
    assert_eq!(
        root["plugin"],
        serde_json::json!([concat!("rtrt-agent@", env!("CARGO_PKG_VERSION"))])
    );
    let installed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&tui).unwrap()).unwrap();
    assert_eq!(installed["keybinds"]["history_previous"], "none");
    assert_eq!(installed["keybinds"]["history_next"], "none");
    assert_eq!(installed["keybinds"]["other"], "keep");
    assert!(!opencode.join("agents").exists());

    rtrt(home.path())
        .args(["uninstall", "--agent", "opencode", "--apply"])
        .assert()
        .success();
    let removed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&tui).unwrap()).unwrap();
    assert_eq!(removed["keybinds"]["history_previous"], "ctrl+p");
    assert_eq!(removed["keybinds"]["history_next"], "ctrl+n");
    assert_eq!(removed["keybinds"]["other"], "keep");
}

#[test]
fn opencode_history_quarantine_is_dry_run_by_default() {
    let home = CanonicalHome::new();
    let history = home
        .path()
        .join(".local/state/opencode/prompt-history.jsonl");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, "private prompt\n").unwrap();

    rtrt(home.path())
        .args(["opencode", "history-quarantine"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[dry-run] would quarantine"));
    assert_eq!(
        std::fs::read_to_string(&history).unwrap(),
        "private prompt\n"
    );
    assert!(
        !history
            .with_file_name("prompt-history.jsonl.rtrt-quarantine")
            .exists()
    );
}

#[test]
fn opencode_launcher_refuses_nested_session_before_resolving_binary() {
    let home = CanonicalHome::new();
    let project = home.path().join("project");
    repo(&project);
    rtrt(home.path())
        .current_dir(&project)
        .env("OPENCODE_SESSION_ID", "nested")
        .args(["opencode", "--", "--model", "provider/model"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing nested OpenCode launch: OPENCODE_SESSION_ID is set",
        ));
    assert!(!home.path().join(".rtrt/projects").exists());
}

#[test]
fn legacy_isolation_requires_claim_and_quarantines_cross_row_state() {
    let home = CanonicalHome::new();
    let project = home.path().join("project");
    repo(&project);
    let identity = rtrt_core::ProjectIdentity::derive(&project).unwrap();
    let source = home.path().join("legacy.sqlite");
    let legacy = rtrt_memory::MemoryStore::open(&source).unwrap();
    let proven = legacy.save(identity.slug(), "note", "proven row").unwrap();
    let ambiguous = legacy
        .save(identity.label(), "note", "ambiguous row")
        .unwrap();
    let foreign = legacy.save("foreign", "note", "foreign row").unwrap();
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("source".into(), "legacy-test".into());
    legacy.set_metadata(proven, &metadata).unwrap();
    legacy
        .tag_row(proven, Some("session-1"), Some("sha-1"))
        .unwrap();
    drop(legacy);
    let source_db = rusqlite::Connection::open(&source).unwrap();
    source_db
        .execute(
            "INSERT INTO embeddings(memory_id,model,vector) VALUES (?1,'legacy-model',?2)",
            rusqlite::params![proven, rtrt_memory::vector_to_blob(&[0.1_f32, 0.2])],
        )
        .unwrap();
    source_db
        .execute(
            "INSERT INTO edges(src_id,dst_id,relation) VALUES (?1,?2,'related')",
            rusqlite::params![proven, foreign],
        )
        .unwrap();
    drop(source_db);

    rtrt(home.path())
        .current_dir(&project)
        .args([
            "memory",
            "legacy-isolate",
            "--source",
            source.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run"))
        .stdout(predicate::str::contains("ambiguous_basename=1"))
        .stdout(predicate::str::contains("quarantined_embeddings=1"))
        .stdout(predicate::str::contains("quarantined_relations=1"));
    rtrt(home.path())
        .current_dir(&project)
        .args([
            "memory",
            "legacy-isolate",
            "--source",
            source.to_str().unwrap(),
            "--apply",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("copied=1"));
    rtrt(home.path())
        .current_dir(&project)
        .args([
            "memory",
            "legacy-isolate",
            "--source",
            source.to_str().unwrap(),
            "--apply",
            "--claim-basename",
            "--accept-mixed-history",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "historical basename mixing cannot be disentangled",
        ));

    let destination = rtrt_core::project_memory_db_path_in(home.path(), &identity);
    let destination_db = rusqlite::Connection::open(destination).unwrap();
    let rows: usize = destination_db
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2);
    let preserved: (String, String) = destination_db
        .query_row(
            "SELECT session_id, json_extract(metadata,'$.source') FROM memories WHERE body='proven row'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(preserved, ("session-1".into(), "legacy-test".into()));
    assert_eq!(
        destination_db
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        destination_db
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        0
    );
    let source_rows: usize = rusqlite::Connection::open(&source)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source_rows, 3);
    let _ = ambiguous;
}

#[cfg(target_os = "linux")]
#[test]
fn service_machine_mode_has_no_project_working_directory_or_legacy_memory_path() {
    let home = CanonicalHome::new();
    let project = home.path().join("project");
    repo(&project);
    rtrt(home.path())
        .current_dir(&project)
        .args(["service", "install", "--binary", "/bin/true"])
        .assert()
        .success()
        .stdout(predicate::str::contains("WorkingDirectory=").not())
        .stdout(predicate::str::contains("EnvironmentFile=").not())
        .stdout(predicate::str::contains("\"--machine\""))
        .stdout(predicate::str::contains("\"--state-dir\""))
        .stdout(predicate::str::contains("/dashboard/dashboard.env"))
        .stdout(predicate::str::contains("RTRT_MEMORY_PATH").not())
        .stdout(predicate::str::contains("memory.sqlite").not());
}

#[test]
fn opencode_setup_dry_run_lists_tui_targets_without_writing() {
    let home = CanonicalHome::new();
    let opencode = home.path().join(".config/opencode");

    rtrt(home.path())
        .args(["setup", "--agent", "opencode"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            opencode
                .join("tui/rtrt-statusline.tsx")
                .display()
                .to_string(),
        ))
        .stdout(predicate::str::contains(
            opencode
                .join("tui/rtrt-statusline-core.mjs")
                .display()
                .to_string(),
        ))
        .stdout(predicate::str::contains(
            opencode.join("tui.json").display().to_string(),
        ))
        .stdout(predicate::str::contains("./tui/rtrt-statusline.tsx"))
        .stdout(predicate::str::contains(
            "disables TUI history navigation, not OpenCode global history writes",
        ));

    assert!(!opencode.exists());
}

#[test]
fn opencode_setup_and_uninstall_ignore_all_global_claude_config() {
    let home = CanonicalHome::new();
    let claude_json = home.path().join(".claude.json");
    let claude_dir = home.path().join(".claude");
    let claude_settings = claude_dir.join("settings.json");
    std::fs::create_dir(&claude_dir).unwrap();
    let conflicting = br#"{"mcpServers":{"rtrt":{"command":"foreign"}},not-json"#;
    let hook_conflict = br#"{"hooks":{"SessionStart":"foreign"},not-json"#;
    std::fs::write(&claude_json, conflicting).unwrap();
    std::fs::write(&claude_settings, hook_conflict).unwrap();

    rtrt(home.path())
        .args(["setup", "--agent", "opencode", "--apply"])
        .assert()
        .success();
    assert!(!home.path().join(".config/opencode/agents").exists());
    assert_eq!(std::fs::read(&claude_json).unwrap(), conflicting);
    assert_eq!(std::fs::read(&claude_settings).unwrap(), hook_conflict);

    rtrt(home.path())
        .args(["uninstall", "--agent", "opencode", "--apply"])
        .assert()
        .success();
    assert_eq!(std::fs::read(&claude_json).unwrap(), conflicting);
    assert_eq!(std::fs::read(&claude_settings).unwrap(), hook_conflict);
}

#[cfg(target_os = "linux")]
#[test]
fn opencode_sandbox_rejects_project_executable_before_home_writes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(_backend) = ["/usr/bin/bwrap", "/bin/bwrap"].into_iter().find(|path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.uid() == 0
                && metadata.permissions().mode() & 0o111 != 0
                && metadata.permissions().mode() & 0o022 == 0
        })
    }) else {
        return;
    };
    let home = CanonicalHome::new();
    let project = statusline_workspace_root();

    rtrt(home.path())
        .current_dir(&project)
        .args(["setup", "--agent", "opencode", "--sandbox", "--apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "must be outside the writable authorized project root",
        ));

    assert!(!home.path().join(".config/opencode").exists());
    assert!(!home.path().join(".claude.json").exists());
    assert!(!home.path().join(".claude").exists());
}

#[test]
fn opencode_machine_only_rejects_invalid_scope_combinations() {
    let home = CanonicalHome::new();
    rtrt(home.path())
        .args(["setup", "--agent", "opencode", "--machine-only"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--machine-only requires"));
    rtrt(home.path())
        .args(["setup", "--agent", "claude", "--sandbox", "--machine-only"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("only valid with --agent opencode"));
    rtrt(home.path())
        .args([
            "setup",
            "--agent",
            "opencode",
            "--sandbox",
            "--no-sandbox",
            "--machine-only",
        ])
        .assert()
        .failure();
}

#[cfg(target_os = "linux")]
#[test]
fn opencode_machine_only_never_requires_git_cwd() {
    let home = CanonicalHome::new();
    rtrt(home.path())
        .current_dir(home.path())
        .args([
            "setup",
            "--agent",
            "opencode",
            "--sandbox",
            "--machine-only",
        ])
        .assert()
        .stderr(predicate::str::contains("requires current directory inside").not());
}

#[cfg(target_os = "linux")]
#[test]
fn opencode_sandbox_accepts_trusted_executable_outside_project() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(_backend) = ["/usr/bin/bwrap", "/bin/bwrap"].into_iter().find(|path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.uid() == 0
                && metadata.permissions().mode() & 0o111 != 0
                && metadata.permissions().mode() & 0o022 == 0
        })
    }) else {
        return;
    };
    let parent = statusline_workspace_root().parent().unwrap().to_path_buf();
    let home = tempfile::tempdir_in(parent).unwrap();
    let project = home.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(project.join(".git")).unwrap();
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_rtrt")).unwrap();

    let mut command = Command::new(&executable);
    command
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("RTRT_CONFIG", home.path().join(".rtrt/config.toml"))
        .current_dir(&project)
        .args(["setup", "--agent", "opencode", "--sandbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "shell={}",
            executable.display()
        )));

    assert!(!home.path().join(".config/opencode").exists());
    assert!(!project.join(".rtrt").exists());
}

#[cfg(unix)]
#[test]
fn setup_never_probes_ollama_or_mutates_embeddings() {
    use std::os::unix::fs::PermissionsExt;

    let home = CanonicalHome::new();
    let bin = home.path().join("bin");
    let marker = home.path().join("ollama-probed");
    std::fs::create_dir_all(&bin).unwrap();
    let ollama = bin.join("ollama");
    std::fs::write(
        &ollama,
        format!("#!/bin/sh\ntouch \"{}\"\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&ollama, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config_dir = home.path().join(".rtrt");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    let original = "[embeddings]\nenabled = false\nmodel = \"operator-model\"\n";
    std::fs::write(&config, original).unwrap();

    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::split_paths(&path)
        .chain(std::iter::once(bin.clone()))
        .collect::<Vec<_>>();
    rtrt(home.path())
        .env("PATH", std::env::join_paths(path).unwrap())
        .args(["setup", "--agent", "aider", "--apply"])
        .assert()
        .success();

    assert!(!marker.exists());
    assert_eq!(std::fs::read_to_string(config).unwrap(), original);
}

#[test]
fn opencode_unsandboxed_setup_preserves_config_and_does_not_create_agents() {
    let home = CanonicalHome::new();
    let opencode = home.path().join(".config/opencode");
    let config = opencode.join("opencode.json");
    std::fs::create_dir_all(&opencode).unwrap();
    std::fs::write(
        &config,
        r#"{"foreign":{"keep":true},"provider":{"ollama":{"name":"operator"}}}"#,
    )
    .unwrap();
    rtrt(home.path())
        .args(["setup", "--agent", "opencode", "--apply"])
        .assert()
        .success();

    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(root["foreign"]["keep"], true);
    assert!(!opencode.join("agents").exists());
}

#[test]
fn hidden_shell_dispatch_fails_closed_without_managed_state() {
    let home = CanonicalHome::new();
    let project = home.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    rtrt(home.path())
        .current_dir(&project)
        .args(["-c", "printf unsafe"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("unsafe").not());

    rtrt(home.path())
        .current_dir(&project)
        .args(["-c"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("expects exactly"));
    rtrt(home.path())
        .current_dir(&project)
        .args(["-c", "true", "extra"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("expects exactly"));
}

#[cfg(target_os = "linux")]
#[test]
fn opencode_linux_sandbox_dry_run_and_boundary_rejections() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Some(backend) = ["/usr/bin/bwrap", "/bin/bwrap"].into_iter().find(|path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.uid() == 0
                && metadata.permissions().mode() & 0o111 != 0
                && metadata.permissions().mode() & 0o022 == 0
        })
    }) else {
        return;
    };
    let home = CanonicalHome::new();
    let project = home.path().join("project");
    let config_dir = home.path().join(".config/opencode");
    let config = config_dir.join("opencode.jsonc");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    let original = "{\n  // retain me\n  \"shell\": \"/bin/zsh\",\n  \"unrelated\": true,\n}\n";
    std::fs::write(&config, original).unwrap();

    rtrt(home.path())
        .current_dir("/")
        .args(["-c", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("filesystem root"));
    let symlink_project = home.path().join("symlink-project");
    let real_git = home.path().join("real-git");
    std::fs::create_dir(&symlink_project).unwrap();
    std::fs::create_dir(&real_git).unwrap();
    std::os::unix::fs::symlink(&real_git, symlink_project.join(".git")).unwrap();
    rtrt(home.path())
        .current_dir(&symlink_project)
        .args(["-c", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("symlinked Git metadata"));
    rtrt(home.path())
        .current_dir(&symlink_project)
        .args(["setup", "--agent", "opencode", "--sandbox", "--apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("symlinked Git metadata"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);

    rtrt(home.path())
        .current_dir(&project)
        .args(["setup", "--agent", "opencode", "--sandbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains(backend))
        .stdout(predicate::str::contains("shell="));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    assert!(!project.join(".rtrt").exists());

    rtrt(home.path())
        .current_dir(&project)
        .args(["-c", "claude -p nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires exact RTRT argv"));

    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
}

fn parse_statusline_json(stdout: &[u8]) -> serde_json::Value {
    let stdout = std::str::from_utf8(stdout).expect("statusline is UTF-8");
    assert!(stdout.ends_with('\n'));
    assert_eq!(stdout.lines().count(), 1, "expected one compact JSON line");
    serde_json::from_str(stdout.trim_end()).expect("valid statusline JSON")
}

#[test]
fn opencode_statusline_returns_partial_json_at_zero_budget_and_honors_no_git() {
    let home = CanonicalHome::new();
    let output = rtrt(home.path())
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            home.path().to_str().unwrap(),
            "--width",
            "100",
            "--budget-ms",
            "0",
            "--no-git",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = parse_statusline_json(&output.stdout);
    assert_eq!(value["v"], 1);
    assert!(value["ts"].is_u64());
    assert!(value["took_ms"].is_u64());
    assert_eq!(value["stale"], true);
    assert!(value["project"].is_string());
    assert!(value["cwd"].is_string());
    assert!(value["data"].is_object());
    assert!(value["segments"].is_array());
    let degraded = value["degraded"].as_array().unwrap();
    assert!(degraded.contains(&serde_json::json!("budget")));
    assert!(degraded.contains(&serde_json::json!("savings")));
    assert!(
        !degraded
            .iter()
            .any(|item| { item.as_str().is_some_and(|item| item.starts_with("git")) })
    );
    assert!(value["data"].get("git").is_none());
    assert!(
        value["segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|segment| segment["id"] != "git")
    );
}

#[test]
fn opencode_statusline_sanitizes_malicious_cwd_controls() {
    let home = CanonicalHome::new();
    let cwd = home.path().join("bad\n\t\u{1b}[31m");
    std::fs::create_dir(&cwd).unwrap();
    let output = rtrt(home.path())
        .arg("statusline")
        .arg("--opencode")
        .arg("--cwd")
        .arg(&cwd)
        .args(["--width", "40", "--budget-ms", "0", "--no-git"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    let value = parse_statusline_json(&output.stdout);
    for field in [
        value["cwd"].as_str().unwrap(),
        value["project"].as_str().unwrap(),
    ] {
        assert!(!field.chars().any(char::is_control), "{field:?}");
    }
    for segment in value["segments"].as_array().unwrap() {
        assert!(
            !segment["text"]
                .as_str()
                .unwrap()
                .chars()
                .any(char::is_control)
        );
    }
}

#[test]
fn opencode_statusline_never_waits_for_stdin() {
    let home = CanonicalHome::new();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rtrt"))
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            home.path().to_str().unwrap(),
            "--width",
            "40",
            "--budget-ms",
            "0",
            "--no-git",
        ])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("RTRT_CONFIG", home.path().join(".rtrt/config.toml"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(2) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            panic!("OpenCode statusline waited for stdin EOF");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success());
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).unwrap();
    drop(stdin);
    let _ = parse_statusline_json(&output);
}

#[test]
fn official_claude_rate_limits_roundtrip_without_changing_claude_bytes() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let home = CanonicalHome::new();
    let config_dir = home.path().join(".rtrt");
    std::fs::create_dir(&config_dir).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        r#"[statusline]
enabled_segments = []
format = "unused"
line2_format = ""
line3_format = ""
codex_check_timeout_ms = 1
"#,
    )
    .unwrap();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let five_reset = before + 3_660;
    let week_reset = before + 2 * 86_400;
    let official = serde_json::json!({
        "session_id": "must-not-be-cached",
        "prompt_id": "must-not-be-cached",
        "transcript_path": "/must/not/be/cached.jsonl",
        "context_window": {"total_input_tokens": 123456},
        "rate_limits": {
            "five_hour": {"used_percentage": 23.5, "resets_at": five_reset},
            "seven_day": {"used_percentage": 91.2, "resets_at": week_reset},
        }
    })
    .to_string();

    rtrt(home.path())
        .args(["statusline", "--rich", "--format", "claude-byte-golden"])
        .write_stdin(official)
        .assert()
        .success()
        .stdout("claude-byte-golden\n");

    let statusline_dir = config_dir.join("statusline");
    let cache_path = statusline_dir.join("claude-rate-limits.json");
    let raw = std::fs::read_to_string(&cache_path).unwrap();
    assert!(!config_dir.join("claude-rate-limits.json").exists());
    assert!(!raw.contains("session_id"));
    assert!(!raw.contains("prompt_id"));
    assert!(!raw.contains("transcript"));
    assert!(!raw.contains("token"));
    let cache: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let root = cache.as_object().unwrap();
    assert_eq!(root.len(), 3);
    assert!(root.contains_key("v"));
    assert!(root.contains_key("captured_at"));
    assert!(root.contains_key("windows"));
    assert_eq!(cache["v"], 1);
    assert_eq!(cache["windows"]["five_hour"]["used_percentage"], 23.5);
    assert_eq!(cache["windows"]["five_hour"]["resets_at"], five_reset);
    assert_eq!(cache["windows"]["seven_day"]["used_percentage"], 91.2);
    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::symlink_metadata(&config_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        assert_eq!(
            std::fs::symlink_metadata(&statusline_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&cache_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }

    let output = rtrt(home.path())
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            home.path().to_str().unwrap(),
            "--width",
            "80",
            "--budget-ms",
            "120",
            "--no-git",
        ])
        .env(
            "RTRT_PROVIDER_USAGE_PATH",
            home.path().join("missing-usage.tsv"),
        )
        .env(
            "RTRT_PROXY_STATS_PATH",
            home.path().join("missing-proxy.sqlite"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = parse_statusline_json(&output.stdout);
    assert_eq!(status["data"]["quota"]["source"], "claude_statusline");
    assert_eq!(status["data"]["quota"]["fresh"], true);
    assert_eq!(
        status["data"]["quota"]["windows"]["five_hour"]["used_percentage"],
        23.5
    );
    let segments = status["segments"].as_array().unwrap();
    let five = segments
        .iter()
        .find(|segment| segment["id"] == "limit_5h")
        .unwrap();
    assert_eq!(five["tone"], "good");
    assert!(five["text"].as_str().unwrap().starts_with("5h:24% ↻"));
    let week = segments
        .iter()
        .find(|segment| segment["id"] == "limit_week")
        .unwrap();
    assert_eq!(week["tone"], "bad");
    assert!(week["text"].as_str().unwrap().starts_with("wk:91% ↻"));
}

#[cfg(unix)]
#[test]
fn opencode_statusline_reads_and_migrates_secure_legacy_cache_under_0755_state_dir() {
    use std::os::unix::fs::PermissionsExt;

    let home = CanonicalHome::new();
    let state = home.path().join(".rtrt");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let legacy = state.join("claude-rate-limits.json");
    std::fs::write(
        &legacy,
        serde_json::json!({
            "v": 1,
            "captured_at": now,
            "windows": {
                "five_hour": {"used_percentage": 42.0, "resets_at": now + 3600}
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = rtrt(home.path())
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            home.path().to_str().unwrap(),
            "--width",
            "40",
            "--budget-ms",
            "120",
            "--no-git",
        ])
        .env(
            "RTRT_PROVIDER_USAGE_PATH",
            home.path().join("missing-usage.tsv"),
        )
        .env(
            "RTRT_PROXY_STATS_PATH",
            home.path().join("missing-proxy.sqlite"),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    let value = parse_statusline_json(&output.stdout);
    assert_eq!(
        value["data"]["quota"]["windows"]["five_hour"]["used_percentage"],
        42.0
    );
    let migrated_dir = state.join("statusline");
    let migrated = migrated_dir.join("claude-rate-limits.json");
    assert!(migrated.is_file());
    assert_eq!(
        std::fs::symlink_metadata(&migrated_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    assert_eq!(
        std::fs::symlink_metadata(&migrated)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn opencode_statusline_rejects_insecure_legacy_cache() {
    use std::os::unix::fs::PermissionsExt;

    let home = CanonicalHome::new();
    let state = home.path().join(".rtrt");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let legacy = state.join("claude-rate-limits.json");
    std::fs::write(
        &legacy,
        serde_json::json!({
            "v": 1,
            "captured_at": now,
            "windows": {
                "five_hour": {"used_percentage": 42.0, "resets_at": now + 3600}
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = rtrt(home.path())
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            home.path().to_str().unwrap(),
            "--width",
            "40",
            "--budget-ms",
            "120",
            "--no-git",
        ])
        .env(
            "RTRT_PROVIDER_USAGE_PATH",
            home.path().join("missing-usage.tsv"),
        )
        .env(
            "RTRT_PROXY_STATS_PATH",
            home.path().join("missing-proxy.sqlite"),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    let value = parse_statusline_json(&output.stdout);
    assert!(value["data"].get("quota").is_none());
    assert!(
        value["degraded"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("quota"))
    );
    assert!(!state.join("statusline").exists());
}

#[test]
fn opencode_statusline_degrades_quota_when_cache_is_absent() {
    let home = CanonicalHome::new();
    let output = rtrt(home.path())
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            home.path().to_str().unwrap(),
            "--width",
            "40",
            "--budget-ms",
            "120",
            "--no-git",
        ])
        .env(
            "RTRT_PROVIDER_USAGE_PATH",
            home.path().join("missing-usage.tsv"),
        )
        .env(
            "RTRT_PROXY_STATS_PATH",
            home.path().join("missing-proxy.sqlite"),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    let value = parse_statusline_json(&output.stdout);
    assert!(value["data"].get("quota").is_none());
    assert!(
        value["degraded"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("quota"))
    );
    assert!(
        value["segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|segment| { !matches!(segment["id"].as_str(), Some("limit_5h" | "limit_week")) })
    );
}

fn statusline_workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn seed_statusline_savings_cache(home: &std::path::Path) {
    let state = home.join(".rtrt");
    std::fs::create_dir_all(&state).unwrap();
    // The cache key is the project name with every non-alphanumeric byte mapped
    // to `_`, so it follows whatever the checkout directory happens to be named.
    let key: String = statusline_workspace_root()
        .file_name()
        .expect("workspace directory name")
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::fs::write(
        state.join(format!("statusline-savings-{key}.cache")),
        "⚡cmd:25% 💯Σ:40%",
    )
    .unwrap();
}

fn cached_statusline_command(home: &std::path::Path, runtime: &std::path::Path) -> Command {
    cached_statusline_command_with_budget(home, runtime, "120")
}

/// Priming runs need a budget that a cold runner can actually meet, otherwise
/// the probe they are meant to cache is abandoned and never written.
fn cached_statusline_command_with_budget(
    home: &std::path::Path,
    runtime: &std::path::Path,
    budget_ms: &str,
) -> Command {
    let mut command = rtrt(home);
    command
        .args([
            "statusline",
            "--opencode",
            "--cwd",
            statusline_workspace_root().to_str().unwrap(),
            "--width",
            "120",
            "--budget-ms",
            budget_ms,
        ])
        .env("RTRT_TMP_DIR", runtime)
        .env("RTRT_PROVIDER_USAGE_PATH", home.join("missing-usage.tsv"))
        .env("RTRT_PROXY_STATS_PATH", home.join("missing-proxy.sqlite"))
        .env("RTRT_MEMORY_PATH", home.join("missing-memory.sqlite"));
    command
}

/// A private directory holding a Git stand-in the statusline probe will trust.
///
/// The probe rejects any Git whose path is not private, which the Git shipped on
/// some runners is not, so priming through the real one is not reproducible. The
/// stand-in emits the single porcelain line the probe reads.
#[cfg(unix)]
fn trusted_git_bin(home: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin = home.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let git = bin.join("git");
    std::fs::write(&git, "#!/bin/sh\nprintf '# branch.head main\\n'\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

#[cfg(unix)]
#[test]
fn opencode_statusline_uses_normal_cached_data_within_wall_budget() {
    let home = CanonicalHome::new();
    let runtime = home.path().join("runtime");
    seed_statusline_savings_cache(home.path());

    // Establishing the cache is setup, not the thing being timed; only the second
    // call below runs under the budget this test asserts against.
    let first = cached_statusline_command_with_budget(home.path(), &runtime, "5000")
        .env("PATH", trusted_git_bin(home.path()))
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json = parse_statusline_json(&first.stdout);
    assert_eq!(
        first_json["data"]["savings"]["cached"], true,
        "statusline did not use the seeded savings cache: {first_json}"
    );

    let started = std::time::Instant::now();
    let second = cached_statusline_command(home.path(), &runtime)
        .output()
        .unwrap();
    let wall = started.elapsed();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        wall < std::time::Duration::from_millis(500),
        "wall={wall:?}"
    );
    let value = parse_statusline_json(&second.stdout);
    assert_eq!(
        value["data"]["savings"]["cached"], true,
        "second run did not reuse the savings cache: {value}"
    );
    assert_eq!(
        value["data"]["git"]["cached"], true,
        "second run did not reuse the Git cache: {value}"
    );
    assert!(value["took_ms"].as_u64().unwrap() < 500);
}

#[cfg(unix)]
#[test]
fn opencode_statusline_bounds_locked_sqlite_and_slow_stale_git() {
    use std::os::unix::fs::PermissionsExt;

    let home = CanonicalHome::new();
    let runtime = home.path().join("runtime");
    seed_statusline_savings_cache(home.path());

    let bin = trusted_git_bin(home.path());

    let prime = cached_statusline_command_with_budget(home.path(), &runtime, "5000")
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(
        prime.status.success(),
        "{}",
        String::from_utf8_lossy(&prime.stderr)
    );

    let git_cache = std::fs::read_dir(&runtime)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rtrt-opencode-git-"))
        })
        .unwrap_or_else(|| {
            panic!(
                "priming run wrote no Git cache into {}: {}",
                runtime.display(),
                String::from_utf8_lossy(&prime.stdout)
            )
        });
    let mut cache: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&git_cache).unwrap()).unwrap();
    cache["ts"] = serde_json::json!(0);
    std::fs::write(&git_cache, serde_json::to_vec(&cache).unwrap()).unwrap();

    let memory = home.path().join("locked-memory.sqlite");
    let lock = rusqlite::Connection::open(&memory).unwrap();
    lock.execute_batch(
        "PRAGMA journal_mode = DELETE; \
         CREATE TABLE memories(project TEXT NOT NULL, body TEXT NOT NULL, body_full TEXT); \
         BEGIN EXCLUSIVE; \
         INSERT INTO memories(project, body, body_full) VALUES ('00G_rtrt', 'stored', 'original');",
    )
    .unwrap();

    let fake_git = bin.join("git");
    let git_pid = home.path().join("git.pid");
    std::fs::write(
        &fake_git,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > \"{}\"\nwhile :; do :; done\n",
            git_pid.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = cached_statusline_command(home.path(), &runtime);
    command
        .env("PATH", &bin)
        .env("RTRT_MEMORY_PATH", &memory)
        .timeout(std::time::Duration::from_secs(2));
    let started = std::time::Instant::now();
    let output = command.output().unwrap();
    let wall = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        wall < std::time::Duration::from_millis(500),
        "wall={wall:?}"
    );
    let value = parse_statusline_json(&output.stdout);
    assert!(value["took_ms"].as_u64().unwrap() < 500);
    assert_eq!(value["stale"], true);
    assert_eq!(value["data"]["savings"]["cached"], true);
    assert_eq!(value["data"]["git"]["cached"], true);
    assert!(value["data"].get("memory").is_none());
    assert!(
        value["degraded"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("git_stale"))
    );
    assert!(
        value["degraded"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("memory"))
    );

    #[cfg(target_os = "linux")]
    {
        let pid = std::fs::read_to_string(&git_pid).unwrap();
        assert!(!std::path::Path::new("/proc").join(pid).exists());
    }
}
