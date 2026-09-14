#![cfg(unix)]

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

struct CanonicalHome {
    _guard: tempfile::TempDir,
    path: PathBuf,
}

impl CanonicalHome {
    fn new() -> Self {
        let guard = tempfile::tempdir().unwrap();
        // macOS may expose its temp directory through /var -> /private/var.
        let path = fs::canonicalize(guard.path()).unwrap();
        Self {
            _guard: guard,
            path,
        }
    }
}

fn gain(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("rtrt").expect("rtrt binary builds");
    cmd.current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("RTRT_CONFIG", home.join(".rtrt/config.toml"))
        .env_remove("RTRT_PROXY_STATS_PATH")
        .env_remove("RTRT_MEMORY_PATH")
        .env_remove("RTRT_CLAUDE_RATE_LIMIT_CACHE")
        .env_remove("RTRT_CLAUDE_RATE_LIMIT_MAX_AGE_SEC")
        .env_remove("OPENCODE_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .arg("gain");
    cmd
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn creates_private_store_when_default_store_is_fresh() {
    // Given: an isolated HOME without a default store.
    let home = CanonicalHome::new();
    let state = home.path.join(".rtrt");
    let db = state.join("proxy-stats.sqlite");

    // When: gain opens the default store.
    gain(&home.path)
        .assert()
        .success()
        .stdout(predicate::str::contains("db_status:").not());

    // Then: both newly created paths are owner-only.
    assert!(state.is_dir());
    assert!(db.is_file());
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&db), 0o600);
}

#[test]
fn repairs_permissions_when_legacy_store_has_live_sidecars() {
    // Given: an owner-owned legacy store with committed data and live WAL/SHM.
    let home = CanonicalHome::new();
    let state = home.path.join(".rtrt");
    fs::create_dir(&state).unwrap();
    let db = state.join("proxy-stats.sqlite");
    let wal = state.join("proxy-stats.sqlite-wal");
    let shm = state.join("proxy-stats.sqlite-shm");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE proxy_runs (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             ts TEXT NOT NULL, project TEXT NOT NULL,
             original_cmd TEXT NOT NULL, mode TEXT NOT NULL,
             input_chars INTEGER NOT NULL, output_chars INTEGER NOT NULL,
             saved_chars INTEGER NOT NULL, saved_pct REAL NOT NULL,
             exec_ms INTEGER NOT NULL
         );
         INSERT INTO proxy_runs VALUES (
             1, '2026-01-01 00:00:00', 'fixture', 'git status', 'normal',
             100, 20, 80, 80.0, 7
         );",
    )
    .unwrap();
    assert!(wal.is_file());
    assert!(shm.is_file());
    chmod(&state, 0o755);
    for path in [&db, &wal, &shm] {
        chmod(path, 0o644);
    }

    // When: another process reads the store while this connection keeps sidecars alive.
    let result = gain(&home.path)
        .args(["--format", "json"])
        .assert()
        .success();

    // Then: data remains visible and all store files are private before connection teardown.
    let summary: serde_json::Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert!(summary["db_status"].is_null());
    assert_eq!(summary["total_runs"], 1);
    assert_eq!(summary["total_saved_chars"], 80);
    assert_eq!(mode(&state), 0o700);
    for path in [&db, &wal, &shm] {
        assert!(path.is_file());
        assert_eq!(mode(path), 0o600);
    }
    let saved: (String, i64, i64, i64) = conn
        .query_row(
            "SELECT original_cmd, input_chars, output_chars, saved_chars
             FROM proxy_runs WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(saved, ("git status".to_owned(), 100, 20, 80));
    drop(conn);
}

#[test]
fn reports_unavailable_when_default_database_is_a_symlink() {
    // Given: a default DB symlink pointing at a non-private victim.
    let home = CanonicalHome::new();
    let state = home.path.join(".rtrt");
    fs::create_dir(&state).unwrap();
    chmod(&state, 0o700);
    let victim = home.path.join("victim.sqlite");
    fs::write(&victim, b"victim").unwrap();
    chmod(&victim, 0o644);
    symlink(&victim, state.join("proxy-stats.sqlite")).unwrap();

    // When: gain encounters the DB symlink.
    let result = gain(&home.path).assert();

    // Then: best-effort reporting succeeds, identifies the rejection and preserves victim mode.
    result
        .success()
        .stdout(predicate::str::contains("db_status: unavailable ("))
        .stdout(predicate::str::contains("not a real regular file"));
    assert_eq!(mode(&victim), 0o644);
}

#[test]
fn reports_unavailable_when_default_state_directory_is_a_symlink() {
    // Given: the default state directory links to a non-private target.
    let home = CanonicalHome::new();
    let target = home.path.join("target");
    fs::create_dir(&target).unwrap();
    chmod(&target, 0o755);
    symlink(&target, home.path.join(".rtrt")).unwrap();

    // When: gain encounters the state directory symlink.
    let result = gain(&home.path).assert();

    // Then: best-effort reporting succeeds, identifies the rejection and preserves target mode.
    result
        .success()
        .stdout(predicate::str::contains("db_status: unavailable ("))
        .stdout(predicate::str::contains("not a real directory"));
    assert_eq!(mode(&target), 0o755);
}

#[test]
fn preserves_parent_permissions_when_database_path_is_overridden() {
    // Given: an explicit DB override inside an existing shared parent.
    let home = CanonicalHome::new();
    let parent = home.path.join("shared");
    fs::create_dir(&parent).unwrap();
    chmod(&parent, 0o755);
    let db = parent.join("custom.sqlite");
    fs::write(&db, []).unwrap();
    chmod(&db, 0o644);

    // When: gain opens the explicitly overridden store.
    gain(&home.path)
        .env("RTRT_PROXY_STATS_PATH", &db)
        .assert()
        .success()
        .stdout(predicate::str::contains("db_status:").not());

    // Then: only the database becomes private, not its existing parent.
    assert!(db.is_file());
    assert_eq!(mode(&db), 0o600);
    assert_eq!(mode(&parent), 0o755);
}
