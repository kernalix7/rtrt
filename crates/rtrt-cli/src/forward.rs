pub(crate) mod spool;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::collector::GuestProject;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use spool::pending;
use spool::{Pending, Selection, delivered, enqueue, open_spool, retry, select};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireEvent {
    pub event_id: String,
    pub guest_id: String,
    pub project: String,
    pub kind: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
}

impl WireEvent {
    pub fn validate(&self) -> Result<()> {
        if !valid_identifier(&self.event_id)
            || !valid_identifier(&self.guest_id)
            || !valid_identifier(&self.project)
            || !valid_identifier(&self.kind)
            || self
                .session_id
                .as_deref()
                .is_some_and(|value| !valid_identifier(value))
        {
            bail!("event identifiers must be non-empty and contain no controls");
        }
        if self.body.is_empty()
            || serde_json::to_vec(self)?.len() > crate::collector::MAX_BODY_BYTES
        {
            bail!("serialized event must be non-empty and at most 1 MiB");
        }
        Ok(())
    }
}

pub(super) fn valid_identifier(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

pub(super) const fn retry_delay(attempts: u32) -> Duration {
    Duration::from_secs(if attempts >= 9 {
        300
    } else {
        1_u64 << attempts
    })
}

fn collector_url(endpoint: &str) -> Result<String> {
    let url = reqwest::Url::parse(endpoint).context("invalid collector endpoint")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("collector endpoint must be HTTP(S) without credentials, query or fragment");
    }
    Ok(if endpoint.trim_end_matches('/').ends_with("/v1/events") {
        endpoint.trim_end_matches('/').to_string()
    } else {
        format!("{}/v1/events", endpoint.trim_end_matches('/'))
    })
}

pub(super) fn default_spool_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("cannot determine operator home directory")?
        .join(".rtrt/forward-spool.sqlite"))
}

/// Strict acknowledgement the collector must return for a delivered event:
/// both fields required, no unknown fields, and `memory_id` strictly
/// positive. Anything else keeps the event queued.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectorAck {
    memory_id: i64,
    inserted: bool,
}

/// Delivery client for the collector: redirects are never followed, so a
/// 3xx hop counts as a failed delivery instead of silently acknowledging
/// through some other endpoint.
pub(super) fn delivery_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .build()
        .context("build collector delivery client")
}

pub(super) struct EnqueueArgs {
    pub endpoint: String,
    pub token: String,
    pub spool: PathBuf,
    pub guest_id: String,
    pub project: String,
    pub kind: String,
    pub body: String,
    pub event_id: Option<String>,
    pub session_id: Option<String>,
    pub metadata: Option<String>,
}

pub(super) async fn enqueue_and_deliver(args: EnqueueArgs) -> Result<String> {
    crate::collector::validate_token(&args.token)?;
    let url = collector_url(&args.endpoint)?;
    let event = WireEvent {
        event_id: args
            .event_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        guest_id: args.guest_id,
        project: args.project,
        kind: args.kind,
        body: args.body,
        session_id: args.session_id,
        metadata: args
            .metadata
            .map(|raw| serde_json::from_str(&raw))
            .transpose()?,
    };
    event.validate()?;
    let id = event.event_id.clone();
    let spool = args.spool;
    let path = spool.clone();
    tokio::task::spawn_blocking(move || enqueue(&open_spool(&path)?, &event))
        .await
        .context("join spool writer")??;
    let client = delivery_client()?;
    if let Err(error) = flush_to_url(
        &spool,
        &client,
        &url,
        &args.token,
        Selection::EventId(id.clone()),
    )
    .await
    {
        eprintln!("collector delivery deferred; event remains queued: {error}");
    }
    Ok(id)
}

pub(super) async fn flush(
    spool: &Path,
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    force: bool,
) -> Result<usize> {
    crate::collector::validate_token(token)?;
    let url = collector_url(endpoint)?;
    flush_to_url(
        spool,
        client,
        &url,
        token,
        Selection::Due {
            pair: None,
            limit: if force { 1 } else { 100 },
            force,
        },
    )
    .await
}

pub(super) async fn flush_pair(
    spool: &Path,
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    pair: GuestProject,
) -> Result<usize> {
    crate::collector::validate_token(token)?;
    if !valid_identifier(&pair.guest_id) || !valid_identifier(&pair.project) {
        bail!("forward filter requires valid guest and project identifiers");
    }
    let url = collector_url(endpoint)?;
    flush_to_url(
        spool,
        client,
        &url,
        token,
        Selection::Due {
            pair: Some(pair),
            limit: 100,
            force: false,
        },
    )
    .await
}

async fn send_one(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    event: &WireEvent,
) -> Result<CollectorAck> {
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(event)
        .send()
        .await
        .with_context(|| format!("post event {} to collector", event.event_id))?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "collector rejected event {} with HTTP {status}",
            event.event_id
        );
    }
    let ack: CollectorAck = response
        .json()
        .await
        .with_context(|| format!("event {} acknowledged with invalid JSON", event.event_id))?;
    // `inserted` decides nothing (duplicates are still acknowledgements),
    // but requiring it above makes forged partial acknowledgements fail
    // deserialization instead of deleting the queued event.
    let _ = ack.inserted;
    if ack.memory_id <= 0 {
        bail!(
            "event {} acknowledged without a positive memory_id",
            event.event_id
        );
    }
    Ok(ack)
}

async fn flush_to_url(
    spool: &Path,
    client: &reqwest::Client,
    url: &str,
    token: &str,
    selection: Selection,
) -> Result<usize> {
    let path = spool.to_path_buf();
    let batch = tokio::task::spawn_blocking(move || select(&open_spool(&path)?, selection))
        .await
        .context("read forward spool")??;
    let mut sent = 0;
    let mut failures = 0;
    let mut last_error = None;
    for item in batch {
        let outcome = send_one(client, url, token, &item.event).await;
        let success = outcome.is_ok();
        let Pending { event, attempts } = item;
        let event_id = event.event_id;
        let path = spool.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let conn = open_spool(&path)?;
            if success {
                delivered(&conn, &event_id)
            } else {
                retry(&conn, &event_id, attempts.saturating_add(1))
            }
        })
        .await
        .context("update forward spool")??;
        if success {
            sent += 1;
        } else {
            failures += 1;
            last_error = outcome.err().map(|error| error.to_string());
        }
    }
    if failures > 0 {
        match last_error {
            Some(reason) => {
                bail!(
                    "{failures} event(s) remain queued after failed delivery; {sent} delivered; last error: {reason}"
                )
            }
            None => {
                bail!("{failures} event(s) remain queued after failed delivery; {sent} delivered")
            }
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(dir: &tempfile::TempDir) -> PathBuf {
        #[cfg(unix)]
        {
            std::fs::canonicalize(dir.path()).unwrap()
        }
        #[cfg(not(unix))]
        {
            dir.path().to_path_buf()
        }
    }

    #[test]
    fn validates_wire_identifiers_when_controls_or_empty() {
        // Given malformed identifiers and a valid identifier.
        // When the wire-boundary validation runs.
        // Then only non-empty, control-free values are accepted.
        assert!(valid_identifier("guest-a"));
        assert!(!valid_identifier(""));
        assert!(!valid_identifier(" \t"));
        assert!(!valid_identifier("guest\nforged"));
    }

    #[test]
    fn wire_event_rejects_when_json_envelope_exceeds_collector_limit() {
        // Given a body that fits 1 MiB but exceeds it once wrapped in JSON.
        let event = WireEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            guest_id: "guest".into(),
            project: "remote".into(),
            kind: "note".into(),
            body: "a".repeat(1024 * 1024),
            session_id: None,
            metadata: None,
        };
        // When checking the outbound event against the collector boundary.
        // Then enqueue fails before creating an undeliverable spool row.
        assert!(event.validate().is_err());
    }

    #[test]
    fn backoff_is_bounded_when_attempts_grow() {
        // Given an arbitrarily large retry count.
        // When the exponential backoff is calculated.
        // Then the delay is capped.
        assert_eq!(retry_delay(100), std::time::Duration::from_secs(300));
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_symlink_when_spool_file_is_link() {
        use std::os::unix::fs::PermissionsExt;

        // Given a spool symlink pointing at a separate file.
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(&dir);
        let target = root.join("target.sqlite");
        std::fs::write(&target, "do not touch").unwrap();
        let spool = root.join(".rtrt/forward.sqlite");
        std::fs::create_dir(root.join(".rtrt")).unwrap();
        std::fs::set_permissions(root.join(".rtrt"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::os::unix::fs::symlink(&target, &spool).unwrap();
        // When the spool is opened.
        // Then it is rejected without touching the target.
        assert!(
            open_spool(&spool)
                .unwrap_err()
                .to_string()
                .contains("spool must be a regular file, not a symlink")
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "do not touch");
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_symlinked_directory_without_populating_target() {
        use std::os::unix::fs::PermissionsExt;

        // Given a symlink at the spool directory pointing to a separate private directory.
        let home = tempfile::tempdir().unwrap();
        let root = test_root(&home);
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.join(".rtrt");
        std::os::unix::fs::symlink(&target, &directory).unwrap();
        let path = directory.join("forward.sqlite");

        // When the spool is opened through the directory symlink.
        // Then the symlink is rejected and the target directory stays empty.
        assert!(
            open_spool(&path)
                .unwrap_err()
                .to_string()
                .contains("spool path contains symlink")
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_symlinked_intermediate_directory_without_populating_target() {
        use std::os::unix::fs::PermissionsExt;

        // Given a symlink above the spool directory pointing to a separate directory.
        let home = tempfile::tempdir().unwrap();
        let root = test_root(&home);
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let intermediate = root.join("link");
        std::os::unix::fs::symlink(&target, &intermediate).unwrap();
        let path = intermediate.join(".rtrt/forward.sqlite");

        // When the spool is opened through the intermediate symlink.
        // Then the symlink is rejected before creating any target subdirectory.
        assert!(
            open_spool(&path)
                .unwrap_err()
                .to_string()
                .contains("spool path contains symlink")
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_directory_at_file_path_without_populating_it() {
        use std::os::unix::fs::PermissionsExt;

        // Given a directory where the spool file would be created.
        let home = tempfile::tempdir().unwrap();
        let root = test_root(&home);
        let directory = root.join(".rtrt");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("forward.sqlite");
        std::fs::create_dir(&path).unwrap();

        // When the spool is opened at the directory path.
        // Then opening is rejected without writing into the directory.
        assert!(
            open_spool(&path)
                .unwrap_err()
                .to_string()
                .contains("spool must be a regular file, not a symlink")
        );
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn spool_preserves_event_id_when_reopened() {
        // Given a private spool and a queued event.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        let event = WireEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            guest_id: "guest-a".into(),
            project: "remote".into(),
            kind: "note".into(),
            body: "hello".into(),
            session_id: None,
            metadata: None,
        };
        let conn = open_spool(&path).unwrap();
        enqueue(&conn, &event).unwrap();
        drop(conn);

        // When the process opens the spool again.
        let pending = pending(&open_spool(&path).unwrap(), 10, false).unwrap();
        // Then exactly the same stable event identifier is available for replay.
        assert_eq!(pending[0].event.event_id, event.event_id);
    }

    #[test]
    fn spool_rejects_duplicate_event_id_with_different_payload() {
        // Given a queued event.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        let event = WireEvent {
            event_id: "evt-same".into(),
            guest_id: "guest-a".into(),
            project: "remote".into(),
            kind: "note".into(),
            body: "hello".into(),
            session_id: None,
            metadata: None,
        };
        let conn = open_spool(&path).unwrap();
        enqueue(&conn, &event).unwrap();

        // When the same event id is enqueued with a different payload.
        let mut conflicting = event.clone();
        conflicting.body = "changed body".into();
        assert!(enqueue(&conn, &conflicting).is_err());
        // Then identical re-enqueue stays idempotent and the original payload wins.
        assert!(enqueue(&conn, &event).is_ok());
        let pending = pending(&conn, 10, true).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event.body, "hello");
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_directory_when_mode_not_exactly_0700() {
        use std::os::unix::fs::PermissionsExt;

        // Given an operator-owned spool directory whose mode is not exactly 0700.
        let home = tempfile::tempdir().unwrap();
        let dir = test_root(&home).join(".rtrt");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("forward-spool.sqlite");
        for mode in [0o755, 0o701, 0o1700, 0o600, 0o000] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            // When the spool is opened.
            // Then every mode other than 0700 is rejected without creating the file.
            assert!(
                open_spool(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("spool directory must be operator-owned with mode 0700"),
                "mode {mode:o} must be rejected"
            );
        }
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_file_when_mode_not_exactly_0600() {
        use std::os::unix::fs::PermissionsExt;

        // Given a created private spool file.
        let home = tempfile::tempdir().unwrap();
        let path = test_root(&home).join(".rtrt/forward-spool.sqlite");
        drop(open_spool(&path).unwrap());
        for mode in [0o644, 0o400, 0o4600, 0o2600, 0o700] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            // When the spool is reopened.
            // Then every mode other than 0600, including special bits, is rejected.
            assert!(
                open_spool(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("spool file must be operator-owned with mode 0600"),
                "mode {mode:o} must be rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn spool_creates_private_directory_and_file_when_missing() {
        use std::os::unix::fs::PermissionsExt;

        // Given a missing spool directory nested below a private home.
        let home = tempfile::tempdir().unwrap();
        let path = test_root(&home).join(".rtrt/forward-spool.sqlite");
        // When the spool is opened.
        let conn = open_spool(&path).unwrap();
        drop(conn);
        // Then the directory and SQLite file are inaccessible to other users.
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[tokio::test]
    async fn flush_preserves_event_when_endpoint_refuses_delivery() {
        // Given a pending event and an unreachable collector endpoint.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        let event = WireEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            guest_id: "guest-a".into(),
            project: "remote".into(),
            kind: "note".into(),
            body: "hello".into(),
            session_id: None,
            metadata: None,
        };
        enqueue(&open_spool(&path).unwrap(), &event).unwrap();
        let client = delivery_client().unwrap();
        // When one bounded retry pass fails.
        let result = flush(&path, &client, "http://127.0.0.1:0", "secret", true).await;
        // Then the durable row survives for a later flush.
        assert!(result.is_err());
        assert_eq!(
            pending(&open_spool(&path).unwrap(), 10, true)
                .unwrap()
                .len(),
            1
        );
    }

    async fn collector_returning(status: reqwest::StatusCode, body: &'static str) -> String {
        let app = axum::Router::new().route(
            "/v1/events",
            axum::routing::post(move || async move {
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        endpoint
    }

    fn spool_with_queued_event(path: &Path) -> WireEvent {
        let event = WireEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            guest_id: "guest-a".into(),
            project: "remote".into(),
            kind: "note".into(),
            body: "hello".into(),
            session_id: None,
            metadata: None,
        };
        enqueue(&open_spool(path).unwrap(), &event).unwrap();
        event
    }

    async fn acknowledge() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({"memory_id": 1, "inserted": true}))
    }

    #[tokio::test]
    async fn flush_retains_event_when_acknowledgement_json_is_invalid() {
        // Given a queued event and a collector that answers 200 with non-JSON.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        spool_with_queued_event(&path);
        let endpoint =
            collector_returning(reqwest::StatusCode::OK, "this is not an acknowledgement").await;
        let client = delivery_client().unwrap();

        // When one bounded retry pass sees an invalid acknowledgement.
        let result = flush(&path, &client, &endpoint, "secret", true).await;
        // Then the durable row survives with the failure recorded.
        assert!(result.is_err());
        let pending = pending(&open_spool(&path).unwrap(), 10, true).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempts, 1);
    }

    #[tokio::test]
    async fn flush_retains_event_when_acknowledgement_misses_fields() {
        // Given a queued event and a collector that answers 200 with JSON
        // missing the inserted field.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        spool_with_queued_event(&path);
        let endpoint = collector_returning(reqwest::StatusCode::OK, r#"{"memory_id": 1}"#).await;
        let client = delivery_client().unwrap();

        // When one bounded retry pass sees an incomplete acknowledgement.
        let result = flush(&path, &client, &endpoint, "secret", true).await;
        // Then the durable row survives.
        assert!(result.is_err());
        assert_eq!(
            pending(&open_spool(&path).unwrap(), 10, true)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn flush_retains_event_when_acknowledgement_memory_id_is_not_positive() {
        // Given a queued event and collectors answering 200 with zero and
        // negative memory ids.
        for body in [
            r#"{"memory_id": 0, "inserted": true}"#,
            r#"{"memory_id": -1, "inserted": true}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = test_root(&dir).join(".rtrt/forward.sqlite");
            spool_with_queued_event(&path);
            let endpoint = collector_returning(reqwest::StatusCode::OK, body).await;
            let client = delivery_client().unwrap();

            // When one bounded retry pass sees a non-positive memory id.
            let result = flush(&path, &client, &endpoint, "secret", true).await;
            // Then the durable row survives.
            assert!(result.is_err());
            assert_eq!(
                pending(&open_spool(&path).unwrap(), 10, true)
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn flush_retains_event_when_collector_answers_non_2xx() {
        // Given a queued event and a collector answering 500.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        spool_with_queued_event(&path);
        let endpoint = collector_returning(reqwest::StatusCode::INTERNAL_SERVER_ERROR, r#""#).await;
        let client = delivery_client().unwrap();

        // When one bounded retry pass sees the server error.
        let result = flush(&path, &client, &endpoint, "secret", true).await;
        // Then the durable row survives.
        assert!(result.is_err());
        assert_eq!(
            pending(&open_spool(&path).unwrap(), 10, true)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn flush_retains_event_when_collector_redirects() {
        // Given a queued event and a collector that redirects delivery to
        // another endpoint which would acknowledge a followed request.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        spool_with_queued_event(&path);
        let app = axum::Router::new()
            .route(
                "/v1/events",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::FOUND,
                        [("location", "/v1/elsewhere")],
                    )
                }),
            )
            .route(
                "/v1/elsewhere",
                axum::routing::get(acknowledge).post(acknowledge),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = delivery_client().unwrap();

        // When one bounded retry pass sees the redirect.
        let result = flush(&path, &client, &endpoint, "secret", true).await;
        server.abort();
        // Then the redirect is not followed and the durable row survives.
        assert!(result.is_err());
        assert_eq!(
            pending(&open_spool(&path).unwrap(), 10, true)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn flush_removes_event_when_collector_reports_duplicate_success() {
        // Given a queued event and a collector returning duplicate success.
        let dir = tempfile::tempdir().unwrap();
        let path = test_root(&dir).join(".rtrt/forward.sqlite");
        let event = WireEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            guest_id: "guest-a".into(),
            project: "remote".into(),
            kind: "note".into(),
            body: "hello".into(),
            session_id: None,
            metadata: None,
        };
        enqueue(&open_spool(&path).unwrap(), &event).unwrap();
        let app = axum::Router::new().route(
            "/v1/events",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"memory_id": 1, "inserted": false}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = delivery_client().unwrap();

        // When replaying the durable row to the collector.
        let sent = flush(&path, &client, &endpoint, "secret", true)
            .await
            .unwrap();
        server.abort();
        // Then a valid duplicate acknowledgement removes the event.
        assert_eq!(sent, 1);
        assert!(
            pending(&open_spool(&path).unwrap(), 10, true)
                .unwrap()
                .is_empty()
        );
    }
}
