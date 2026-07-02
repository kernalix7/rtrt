//! Hermetic integration tests for the `rtrt` binary.
//!
//! Every invocation pins HOME (and the store, where applicable) to a temp
//! directory so tests never read or write the real `~/.rtrt` / `~/.claude`.

use assert_cmd::Command;
use predicates::prelude::*;

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
fn gain_survives_empty_stats() {
    let home = tempfile::tempdir().unwrap();
    rtrt(home.path()).arg("gain").assert().success();
}
