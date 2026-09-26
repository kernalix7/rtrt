use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use rtrt_core::ProjectIdentity;
use rtrt_memory::{ForwardedEvent, MemoryStore};
use serde::Serialize;

use crate::forward::{WireEvent, valid_identifier};

pub(super) const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct CollectorState {
    token: String,
    projects: HashMap<(String, String), ProjectIdentity>,
}

#[derive(Serialize)]
struct IngestResponse {
    memory_id: i64,
    inserted: bool,
}

pub(super) fn validate_token(token: &str) -> Result<()> {
    if !valid_identifier(token) || token.trim() != token {
        bail!("collector token must be non-empty and contain no control characters");
    }
    Ok(())
}

fn parse_mapping(value: &str) -> Result<((String, String), ProjectIdentity)> {
    let (guest, rest) = value
        .split_once(':')
        .context("mapping must have syntax guest:remote_project=HOST_PROJECT_PATH")?;
    let (remote, path) = rest
        .split_once('=')
        .context("mapping must have syntax guest:remote_project=HOST_PROJECT_PATH")?;
    if !valid_identifier(guest) || !valid_identifier(remote) || path.is_empty() {
        bail!("mapping requires non-empty, control-free guest and remote project and a host path");
    }
    let path = PathBuf::from(path);
    if !path.is_dir() {
        bail!("mapped host project must be an existing directory");
    }
    let identity = ProjectIdentity::derive(&path).context("derive mapped host project identity")?;
    Ok(((guest.to_string(), remote.to_string()), identity))
}

pub(super) fn router(token: &str, mappings: impl IntoIterator<Item = String>) -> Result<Router> {
    validate_token(token)?;
    let mut projects = HashMap::new();
    for mapping in mappings {
        let (key, project) = parse_mapping(&mapping)?;
        if projects.insert(key, project).is_some() {
            bail!("duplicate guest and remote project mapping");
        }
    }
    if projects.is_empty() {
        bail!("collector requires at least one explicit --map mapping");
    }
    let state = CollectorState {
        token: token.to_string(),
        projects,
    };
    Ok(Router::new()
        .route("/v1/events", post(ingest))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for (index, byte) in right.iter().enumerate() {
        difference |= usize::from(left.get(index).copied().unwrap_or(0) ^ byte);
    }
    difference == 0
}

/// Authorization gate mounted in front of the ingest handler so a request
/// is refused before any of its body is buffered: rejects every Origin
/// header outright and requires a constant-time bearer match.
async fn authorize(State(state): State<CollectorState>, request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "));
    if !constant_time_equal(
        bearer.unwrap_or_default().as_bytes(),
        state.token.as_bytes(),
    ) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn ingest(
    State(state): State<CollectorState>,
    event: std::result::Result<Json<WireEvent>, axum::extract::rejection::JsonRejection>,
) -> std::result::Result<Json<IngestResponse>, StatusCode> {
    let Json(event) = event.map_err(|error| error.status())?;
    event.validate().map_err(|_| StatusCode::BAD_REQUEST)?;
    let identity = state
        .projects
        .get(&(event.guest_id.clone(), event.project.clone()))
        .cloned()
        .ok_or(StatusCode::FORBIDDEN)?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let store = MemoryStore::open_project(&identity)?;
            let metadata = event.metadata.unwrap_or_default();
            store.ingest_forwarded(&ForwardedEvent {
                event_id: &event.event_id,
                source_guest: &event.guest_id,
                source_project: &event.project,
                kind: &event.kind,
                body: &event.body,
                session_id: event.session_id.as_deref(),
                metadata: &metadata,
            })
        }),
    )
    .await
    .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(IngestResponse {
        memory_id: outcome.memory_id,
        inserted: outcome.inserted,
    }))
}

pub(super) async fn serve(bind: SocketAddr, token: &str, mappings: Vec<String>) -> Result<()> {
    let app = router(token, mappings)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind collector to {bind}"))?;
    axum::serve(listener, app).await.context("serve collector")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use clap::Parser;
    use tower::ServiceExt;

    #[test]
    fn cli_parses_collector_mapping_when_explicit() {
        // Given an explicit repeated guest/remote mapping.
        // When parsing the serve subcommand.
        let cli = crate::Cli::try_parse_from([
            "rtrt",
            "collector",
            "serve",
            "--token",
            "secret",
            "--map",
            "guest:remote=/tmp",
        ]);
        // Then the command is available through the CLI.
        assert!(cli.is_ok());
    }

    #[test]
    fn mapping_rejects_invalid_when_missing_guest_or_project() {
        // Given malformed explicit guest mappings.
        // When parsed at CLI startup.
        // Then invalid identifiers and missing delimiters fail closed.
        assert!(parse_mapping(":remote=/tmp").is_err());
        assert!(parse_mapping("guest:remote").is_err());
        assert!(parse_mapping("guest:remote=").is_err());
        assert!(parse_mapping("guest\nforged:remote=/tmp").is_err());
    }

    #[test]
    fn token_rejected_when_empty_or_controls() {
        // Given malformed token values.
        // When validating before server startup.
        // Then no unauthenticated server can start.
        assert!(validate_token("").is_err());
        assert!(validate_token("  ").is_err());
        assert!(validate_token("one\ntwo").is_err());
        assert!(validate_token("valid-secret").is_ok());
    }

    #[tokio::test]
    async fn router_rejects_origin_even_when_authorized() {
        // Given a mapped project and an otherwise authorized request.
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let app = router("secret", [format!("guest:remote={}", project.display())]).unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/events")
            .header("authorization", "Bearer secret")
            .header("origin", "https://example.com")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();

        // When routed without binding a network socket.
        let response = app.oneshot(request).await.unwrap();
        // Then Origin is rejected before event ingestion.
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn router_rejects_missing_bearer_when_no_origin() {
        // Given an explicitly mapped project and no bearer header.
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let app = router("secret", [format!("guest:remote={}", project.display())]).unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/events")
            .body(Body::from("{}"))
            .unwrap();

        // When routed without binding a network socket.
        let response = app.oneshot(request).await.unwrap();
        // Then authorization fails, not the JSON parser.
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn router_rejects_oversize_body_when_authorized() {
        // Given an authorized event larger than the collector's 1 MiB limit.
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let app = router("secret", [format!("guest:remote={}", project.display())]).unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/events")
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .body(Body::from(vec![b'x'; MAX_BODY_BYTES + 1]))
            .unwrap();

        // When routing the oversized request.
        let response = app.oneshot(request).await.unwrap();
        // Then the server rejects it without opening a project database.
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn app_with_project() -> Router {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();
        router("secret", [format!("guest:remote={}", project.display())]).unwrap()
    }

    fn never_completing_body() -> Body {
        Body::from_stream(futures_util::stream::pending::<
            std::result::Result<axum::body::Bytes, std::io::Error>,
        >())
    }

    #[tokio::test]
    async fn router_answers_unauthorized_before_reading_request_body() {
        // Given an unauthenticated request whose body never completes.
        let app = app_with_project();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/events")
            .header("content-type", "application/json")
            .body(never_completing_body())
            .unwrap();

        // When routing, the authorization gate must not wait for the body.
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), app.oneshot(request))
                .await
                .expect("unauthorized requests must be refused before the body is read")
                .unwrap();
        // Then unauthorized is answered without consuming the body.
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn router_answers_wrong_bearer_before_reading_request_body() {
        // Given an incorrectly authorized request whose body never completes.
        let app = app_with_project();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/events")
            .header("authorization", "Bearer wrong-secret")
            .header("content-type", "application/json")
            .body(never_completing_body())
            .unwrap();

        // When routing, the authorization gate must not wait for the body.
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), app.oneshot(request))
                .await
                .expect("wrong bearer must be refused before the body is read")
                .unwrap();
        // Then unauthorized is answered without consuming the body.
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn router_answers_forbidden_origin_before_reading_request_body() {
        // Given an otherwise authorized request carrying an Origin header
        // whose body never completes.
        let app = app_with_project();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/events")
            .header("authorization", "Bearer secret")
            .header("origin", "https://example.com")
            .header("content-type", "application/json")
            .body(never_completing_body())
            .unwrap();

        // When routing, the Origin gate must not wait for the body.
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), app.oneshot(request))
                .await
                .expect("Origin-bearing requests must be refused before the body is read")
                .unwrap();
        // Then forbidden is answered without consuming the body.
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }
}
