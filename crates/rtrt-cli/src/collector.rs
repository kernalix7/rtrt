use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
#[cfg(unix)]
use std::{fs, io::Read};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use rtrt_core::ProjectIdentity;
use rtrt_memory::{ForwardedEvent, MemoryStore};
use serde::{Deserialize, Serialize};

use crate::forward::{WireEvent, valid_identifier};

pub(super) const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct CollectorState {
    bindings: Vec<Binding>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct GuestProject {
    pub guest_id: String,
    pub project: String,
}

#[derive(Clone)]
struct Binding {
    pair: GuestProject,
    identity: ProjectIdentity,
    token: String,
}

#[derive(Clone)]
struct AuthorizedBinding {
    pair: GuestProject,
    identity: ProjectIdentity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    credential: Vec<CredentialRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialRecord {
    guest_id: String,
    project: String,
    token: String,
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

fn parse_mapping(value: &str) -> Result<(GuestProject, ProjectIdentity)> {
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
    Ok((
        GuestProject {
            guest_id: guest.to_string(),
            project: remote.to_string(),
        },
        identity,
    ))
}

#[cfg(test)]
pub(super) fn router(token: &str, mappings: impl IntoIterator<Item = String>) -> Result<Router> {
    router_with_credentials(Some(token), None, mappings)
}

#[cfg(unix)]
fn read_credentials(path: &Path) -> Result<CredentialFile> {
    use std::os::unix::fs::MetadataExt;

    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).context("inspect credentials path")?;
        if metadata.file_type().is_symlink() {
            bail!("credentials path contains symlink");
        }
    }
    let before = fs::symlink_metadata(path).context("inspect credentials file")?;
    if !before.is_file()
        || !crate::forward::spool::owned_with_mode(
            &before,
            crate::forward::spool::effective_uid()?,
            0o600,
        )
    {
        bail!("credentials file must be operator-owned with mode 0600");
    }
    let mut file = fs::File::open(path).context("open credentials file")?;
    let opened = file.metadata().context("inspect opened credentials file")?;
    if !opened.is_file()
        || opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || !crate::forward::spool::owned_with_mode(
            &opened,
            crate::forward::spool::effective_uid()?,
            0o600,
        )
    {
        bail!("credentials file changed during open");
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .context("read credentials file")?;
    toml::from_str(&contents).map_err(|_| anyhow::anyhow!("invalid credentials file"))
}

#[cfg(not(unix))]
fn read_credentials(_path: &Path) -> Result<CredentialFile> {
    bail!("credentials file requires Unix file ownership and permissions")
}

pub(super) fn router_with_credentials(
    token: Option<&str>,
    credentials: Option<&Path>,
    mappings: impl IntoIterator<Item = String>,
) -> Result<Router> {
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
    if token.is_some() && credentials.is_some() {
        bail!("--token and --credentials cannot be combined");
    }
    let bindings = match credentials {
        Some(path) => {
            let file = read_credentials(path)?;
            if file.credential.len() != projects.len() {
                bail!("credentials must match mappings one-to-one");
            }
            let mut bindings = Vec::with_capacity(projects.len());
            for record in file.credential {
                validate_token(&record.token)?;
                let pair = GuestProject {
                    guest_id: record.guest_id,
                    project: record.project,
                };
                let identity = projects
                    .remove(&pair)
                    .context("credentials must match mappings one-to-one")?;
                if bindings.iter().any(|binding: &Binding| {
                    constant_time_equal(binding.token.as_bytes(), record.token.as_bytes())
                }) {
                    bail!("duplicate credentials token");
                }
                bindings.push(Binding {
                    pair,
                    identity,
                    token: record.token,
                });
            }
            bindings
        }
        None => {
            let token = token.context("collector requires --token or --credentials")?;
            validate_token(token)?;
            if projects.len() != 1 {
                bail!("multiple mappings require --credentials with unique per-pair tokens");
            }
            projects
                .into_iter()
                .map(|(pair, identity)| Binding {
                    pair,
                    identity,
                    token: token.to_string(),
                })
                .collect()
        }
    };
    let state = CollectorState { bindings };
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
async fn authorize(
    State(state): State<CollectorState>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "));
    let mut matched = None;
    for binding in &state.bindings {
        let equal = constant_time_equal(
            bearer.unwrap_or_default().as_bytes(),
            binding.token.as_bytes(),
        );
        if equal {
            matched = Some(AuthorizedBinding {
                pair: binding.pair.clone(),
                identity: binding.identity.clone(),
            });
        }
    }
    let Some(binding) = matched else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    request.extensions_mut().insert(binding);
    next.run(request).await
}

async fn ingest(
    Extension(binding): Extension<AuthorizedBinding>,
    event: std::result::Result<Json<WireEvent>, axum::extract::rejection::JsonRejection>,
) -> std::result::Result<Json<IngestResponse>, StatusCode> {
    let Json(event) = event.map_err(|error| error.status())?;
    event.validate().map_err(|_| StatusCode::BAD_REQUEST)?;
    if event.guest_id != binding.pair.guest_id || event.project != binding.pair.project {
        return Err(StatusCode::FORBIDDEN);
    }
    let identity = binding.identity;
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

pub(super) async fn serve(
    bind: SocketAddr,
    token: Option<&str>,
    credentials: Option<&Path>,
    mappings: Vec<String>,
) -> Result<()> {
    let app = router_with_credentials(token, credentials, mappings)?;
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
    fn authorized_binding_carries_only_pair_and_identity() {
        // Given a resolved project identity and its authorized guest pair.
        let identity = ProjectIdentity::derive(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let authorized = AuthorizedBinding {
            pair: GuestProject {
                guest_id: "guest".into(),
                project: "remote".into(),
            },
            identity,
        };
        // When the request extension is destructured without an omitted field.
        let AuthorizedBinding { pair, identity: _ } = authorized;
        // Then its type admits no credential field.
        assert_eq!(pair.guest_id, "guest");
    }

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

    #[test]
    fn router_rejects_shared_bearer_for_distinct_projects_before_bind() {
        // Given two independently rooted projects mapped under one bearer.
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.rtrt/tmp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let home = tempfile::Builder::new()
            .prefix("collector-shared-token-")
            .tempdir_in(temp_root)
            .unwrap();
        let project_a = home.path().join("project-a");
        let project_b = home.path().join("project-b");
        std::fs::create_dir_all(project_a.join(".git")).unwrap();
        std::fs::create_dir_all(project_b.join(".git")).unwrap();
        assert_ne!(
            ProjectIdentity::derive(&project_a).unwrap().fingerprint(),
            ProjectIdentity::derive(&project_b).unwrap().fingerprint()
        );

        // When constructing the router (before a network listener is bound).
        let result = router(
            "shared",
            [
                format!("guest-a:remote-a={}", project_a.display()),
                format!("guest-b:remote-b={}", project_b.display()),
            ],
        );

        // Then one bearer cannot authorize writes to both host projects.
        assert!(
            result.is_err(),
            "shared bearer accepted distinct project mappings"
        );
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_requires_exact_pairs_and_distinct_tokens() {
        use std::os::unix::fs::PermissionsExt;

        // Given two independent mapped repositories and a private credential file.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.rtrt/tmp");
        fs::create_dir_all(&root).unwrap();
        let temp = tempfile::tempdir_in(root).unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        fs::create_dir_all(a.join(".git")).unwrap();
        fs::create_dir_all(b.join(".git")).unwrap();
        let mappings = || {
            [
                format!("a:p={}", a.display()),
                format!("b:p={}", b.display()),
            ]
        };
        let path = temp.path().join("credentials.toml");
        let write = |contents: &str| {
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        };
        // When records are missing, extra, duplicated or share a bearer.
        for contents in [
            "[[credential]]\nguest_id='a'\nproject='p'\ntoken='first'\n",
            "[[credential]]\nguest_id='a'\nproject='p'\ntoken='first'\n[[credential]]\nguest_id='b'\nproject='p'\ntoken='second'\n[[credential]]\nguest_id='c'\nproject='p'\ntoken='third'\n",
            "[[credential]]\nguest_id='a'\nproject='p'\ntoken='first'\n[[credential]]\nguest_id='c'\nproject='p'\ntoken='other'\n",
            "[[credential]]\nguest_id='a'\nproject='p'\ntoken='first'\n[[credential]]\nguest_id='a'\nproject='p'\ntoken='other'\n",
            "[[credential]]\nguest_id='a'\nproject='p'\ntoken='shared'\n[[credential]]\nguest_id='b'\nproject='p'\ntoken='shared'\n",
        ] {
            write(contents);
            assert!(router_with_credentials(None, Some(&path), mappings()).is_err());
        }
        write(
            "[[credential]]\nguest_id='a'\nproject='p'\ntoken='first'\n[[credential]]\nguest_id='b'\nproject='p'\ntoken='second'\n",
        );
        // Then only a bijection starts, and argv token cannot accompany it.
        assert!(router_with_credentials(None, Some(&path), mappings()).is_ok());
        assert!(router_with_credentials(Some("first"), Some(&path), mappings()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(router_with_credentials(None, Some(&path), mappings()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("credentials-link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(router_with_credentials(None, Some(&link), mappings()).is_err());
    }

    #[cfg(not(unix))]
    #[test]
    fn credential_file_is_refused_without_opening_on_non_unix() {
        // Given a nonexistent credentials path.
        // When the non-Unix reader is called.
        let error = read_credentials(Path::new("absent-credential-file"))
            .err()
            .unwrap();
        // Then unsupported ACLs reject before filesystem access.
        assert!(error.to_string().contains("Unix"));
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
