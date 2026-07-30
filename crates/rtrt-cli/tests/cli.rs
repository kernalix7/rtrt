//! Hermetic integration tests for the `rtrt` binary.
//!
//! Every invocation pins HOME (and the store, where applicable) to a temp
//! directory so tests never read or write the real `~/.rtrt` / `~/.claude`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::{Read, Write};
use std::net::TcpListener;

/// A `rtrt` command with HOME isolated to `home`.
fn rtrt(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("rtrt").expect("rtrt binary builds");
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("RTRT_MEMORY_PATH");
    cmd
}

#[test]
fn version_prints_version_string() {
    let home = tempfile::tempdir().unwrap();
    rtrt(home.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn compress_ultra_preserves_paths_and_negations() {
    let home = tempfile::tempdir().unwrap();
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
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "save",
            "--store",
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
            "--store",
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
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "save",
            "--store",
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
            "--store",
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
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    for (project, body) in [("p1", "project one row"), ("p2", "project two row")] {
        rtrt(home.path())
            .args([
                "memory",
                "save",
                "--store",
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
            "--store",
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
            "--store",
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
    let home = tempfile::tempdir().unwrap();

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
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    rtrt(home.path())
        .args([
            "memory",
            "save",
            "--store",
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
            "--store",
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
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".rtrt");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "not valid = [toml").unwrap();

    rtrt(home.path())
        .args(["memory", "reembed", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("config"));
}

#[test]
fn memory_reembed_persists_successful_rows_before_mid_batch_failure() {
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    for body in ["first row", "second row", "third row"] {
        rtrt(home.path())
            .args([
                "memory",
                "save",
                "--store",
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
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
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
            "--store",
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
            "--store",
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
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("mem.sqlite");
    let store_s = store.to_str().unwrap();

    for body in ["first row", "second row"] {
        rtrt(home.path())
            .args([
                "memory",
                "save",
                "--store",
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
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
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
            "--store",
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
            "--store",
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
    let home = tempfile::tempdir().unwrap();
    rtrt(home.path()).arg("gain").assert().success();
}
