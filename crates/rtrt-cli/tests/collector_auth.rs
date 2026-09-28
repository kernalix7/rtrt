#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, time::Duration};

use rusqlite::{Connection, OpenFlags};

struct Collector(std::process::Child);

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn token_bound_to_pair_rejects_cross_store_and_flush_keeps_foreign_spool_rows() {
    // Given two project mappings, separate credentials and an isolated operator home.
    let tmp_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.rtrt/tmp");
    fs::create_dir_all(&tmp_root).unwrap();
    let temp = tempfile::tempdir_in(tmp_root).unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let a = temp.path().join("project-a");
    let b = temp.path().join("project-b");
    fs::create_dir_all(a.join(".git")).unwrap();
    fs::create_dir_all(b.join(".git")).unwrap();
    let a_identity = rtrt_core::ProjectIdentity::derive(&a).unwrap();
    let b_identity = rtrt_core::ProjectIdentity::derive(&b).unwrap();
    let b_db = home
        .join(".rtrt/projects")
        .join(b_identity.slug())
        .join("memory.sqlite");
    let credentials = temp.path().join("credentials.toml");
    fs::write(&credentials, "[[credential]]\nguest_id='a'\nproject='remote'\ntoken='a-secret'\n[[credential]]\nguest_id='b'\nproject='remote'\ntoken='b-secret'\n").unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let collector = Collector(
        Command::new(env!("CARGO_BIN_EXE_rtrt"))
            .env("HOME", &home)
            .env_remove("RTRT_COLLECTOR_TOKEN")
            .arg("collector")
            .arg("serve")
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--credentials")
            .arg(&credentials)
            .arg("--map")
            .arg(format!("a:remote={}", a.display()))
            .arg("--map")
            .arg(format!("b:remote={}", b.display()))
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let event = |id: &str, guest: &str| {
        serde_json::json!({
            "event_id": id, "guest_id": guest, "project": "remote", "kind": "note", "body": "fact"
        })
    };
    let endpoint = format!("{url}/v1/events");
    let mut ready = false;
    for _ in 0..100 {
        if client.post(&endpoint).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "collector did not bind");

    // When A's bearer claims B's event, the request is refused without creating B's store.
    let denied = client
        .post(&endpoint)
        .bearer_auth("a-secret")
        .json(&event("wrong", "b"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(!b_db.exists());
    // Then A and B with their own credentials each write only their store.
    for (token, id, guest) in [("a-secret", "evt-a", "a"), ("b-secret", "evt-b", "b")] {
        let response = client
            .post(&endpoint)
            .bearer_auth(token)
            .json(&event(id, guest))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    for (identity, id) in [(a_identity, "evt-a"), (b_identity, "evt-b")] {
        let path = home
            .join(".rtrt/projects")
            .join(identity.slug())
            .join("memory.sqlite");
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM forwarded_event_receipts WHERE event_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // Given queued B and A events, unfiltered flush cannot consume either with A's token.
    let spool = temp.path().join("spool/forward.sqlite");
    fs::create_dir(spool.parent().unwrap()).unwrap();
    fs::set_permissions(spool.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let db = Connection::open(&spool).unwrap();
    db.execute_batch("CREATE TABLE forward_spool (event_id TEXT PRIMARY KEY, payload TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_ms INTEGER NOT NULL DEFAULT 0)").unwrap();
    for (id, guest) in [("queued-b", "b"), ("queued-a", "a")] {
        db.execute(
            "INSERT INTO forward_spool (event_id, payload) VALUES (?1, ?2)",
            rusqlite::params![id, event(id, guest).to_string()],
        )
        .unwrap();
    }
    drop(db);
    fs::set_permissions(&spool, fs::Permissions::from_mode(0o600)).unwrap();
    let run_flush = |guest: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rtrt"));
        command
            .env("HOME", &home)
            .env("RTRT_COLLECTOR_TOKEN", "a-secret")
            .arg("forward")
            .arg("flush")
            .arg("--endpoint")
            .arg(&url)
            .arg("--spool")
            .arg(&spool);
        if let Some(guest) = guest {
            command
                .arg("--guest-id")
                .arg(guest)
                .arg("--project")
                .arg("remote");
        }
        command.output().unwrap()
    };
    // When unfiltered flush fails, the complete spool remains intact.
    assert!(!run_flush(None).status.success());
    let db = Connection::open(&spool).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM forward_spool", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    drop(db);
    // Then filtered flush sends only A; B remains queued without backoff mutation.
    assert!(run_flush(Some("a")).status.success());
    let db = Connection::open(&spool).unwrap();
    let remaining: (String, String, i64, i64) = db
        .query_row(
            "SELECT event_id, payload, attempts, next_attempt_ms FROM forward_spool",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        remaining,
        ("queued-b".into(), event("queued-b", "b").to_string(), 0, 0)
    );
    db.execute(
        "INSERT INTO forward_spool (event_id, payload) VALUES (?1, ?2)",
        rusqlite::params!["older-a", event("older-a", "a").to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO forward_spool (event_id, payload) VALUES (?1, ?2)",
        rusqlite::params!["newer-b", event("newer-b", "b").to_string()],
    )
    .unwrap();
    drop(db);
    // When re-enqueueing an existing A event older than newest B, immediate delivery uses A's ID.
    let enqueue = Command::new(env!("CARGO_BIN_EXE_rtrt"))
        .env("HOME", &home)
        .env("RTRT_COLLECTOR_TOKEN", "a-secret")
        .arg("forward")
        .arg("enqueue")
        .arg("--endpoint")
        .arg(&url)
        .arg("--spool")
        .arg(&spool)
        .arg("--guest-id")
        .arg("a")
        .arg("--project")
        .arg("remote")
        .arg("--kind")
        .arg("note")
        .arg("--event-id")
        .arg("older-a")
        .arg("fact")
        .output()
        .unwrap();
    assert!(enqueue.status.success());
    let db = Connection::open(&spool).unwrap();
    let remaining_a: i64 = db
        .query_row(
            "SELECT count(*) FROM forward_spool WHERE event_id = 'older-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_b: i64 = db
        .query_row(
            "SELECT count(*) FROM forward_spool WHERE event_id = 'newer-b' AND attempts = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((remaining_a, remaining_b), (0, 1));
    drop(collector);
}
