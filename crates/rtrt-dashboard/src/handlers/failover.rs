//! `[failover]` policy config — the retry/fallback markers behind
//! `rtrt route --failover` and `rtrt call --failover`.
//!
//! Endpoint: `GET/POST /api/failover/config`. Takes the same `?project=` /
//! `?scope=` selector as every other config endpoint (see `handlers::scope`)
//! and answers with the same `scope` / `custom` / `inherited` triple.

use std::path::Path;

use axum::{Json, http::StatusCode, response::IntoResponse};
use rtrt_core::config::FailoverConfig;
use serde::Deserialize;

use crate::prelude::*;

fn clean_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Drop unnamed invocation flags. A flag's VALUE may legitimately be empty (a
/// valueless switch), so only the key is required.
fn config_path_string(repo: Option<&Path>, custom: bool) -> String {
    match repo.filter(|_| custom) {
        Some(path) => rtrt_core::Config::project_config_path(path)
            .to_string_lossy()
            .into_owned(),
        None => rtrt_core::Config::default_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Which sections a project pins for itself. Read once per request so a GET
/// never parses `<repo>/.rtrt/config.toml` twice.
#[derive(Debug, Clone, Copy, Default)]
struct ProjectOverrides {
    failover: bool,
}

fn project_overrides(repo: Option<&Path>) -> ProjectOverrides {
    let Some(path) = repo else {
        return ProjectOverrides::default();
    };
    // An unreadable/invalid override file reports "no override"; the read of the
    // section itself goes through `load_effective`, which surfaces the real
    // error rather than letting this probe swallow it.
    rtrt_core::Config::load_project(path)
        .map(|p| ProjectOverrides {
            failover: p.failover.is_some(),
        })
        .unwrap_or_default()
}

/// The shared scope triple, identical in shape and meaning to the other
/// per-project settings: `custom` is true when THIS project carries its own
/// override, `inherited` when a project is selected but follows the global.
fn scope_fields(repo: Option<&Path>, custom: bool) -> serde_json::Value {
    serde_json::json!({
        "scope": if custom { "custom" } else { "global" },
        "custom": custom,
        "inherited": repo.is_some() && !custom,
    })
}

/// Merge the scope triple into a response object.
fn with_scope(
    mut value: serde_json::Value,
    repo: Option<&Path>,
    custom: bool,
) -> serde_json::Value {
    if let (Some(target), Some(scope)) = (
        value.as_object_mut(),
        scope_fields(repo, custom).as_object(),
    ) {
        for (key, val) in scope {
            target.insert(key.clone(), val.clone());
        }
    }
    value
}

fn error_response(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}
// ---------------------------------------------------------------------------
// GET/POST /api/failover/config
// ---------------------------------------------------------------------------

fn failover_json(
    failover: &FailoverConfig,
    repo: Option<&Path>,
    custom: bool,
) -> serde_json::Value {
    serde_json::json!({
        "fatal": failover.fatal,
        "quota": failover.quota,
        "transient": failover.transient,
        "transient_retries": failover.transient_retries,
        "backoff_divisor": failover.backoff_divisor,
        "backoff_ms": failover.backoff_ms,
        "path": config_path_string(repo, custom),
    })
}

pub(crate) async fn get_failover_config(
    axum::Extension(state): axum::Extension<AppState>,
    axum::extract::Query(_q): axum::extract::Query<ProjectQuery>,
) -> axum::response::Response {
    let repo = Some(state.project.memory_root().to_path_buf());
    let custom = project_overrides(repo.as_deref()).failover;
    let cfg = match rtrt_core::Config::load_effective(repo.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    Json(with_scope(
        failover_json(&cfg.failover, repo.as_deref(), custom),
        repo.as_deref(),
        custom,
    ))
    .into_response()
}

/// Full-replace write. An omitted marker list clears that class; the numeric
/// knobs are optional and `null` restores the built-in behaviour.
#[derive(Debug, Deserialize)]
pub(crate) struct SetFailoverRequest {
    #[serde(default)]
    fatal: Option<Vec<String>>,
    #[serde(default)]
    quota: Option<Vec<String>>,
    #[serde(default)]
    transient: Option<Vec<String>>,
    #[serde(default)]
    transient_retries: crate::util::JsonPatch<u64>,
    #[serde(default)]
    backoff_divisor: crate::util::JsonPatch<u64>,
    #[serde(default)]
    backoff_ms: crate::util::JsonPatch<u64>,
}

pub(crate) async fn post_failover_config(
    axum::Extension(state): axum::Extension<AppState>,
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
    body: Option<Json<SetFailoverRequest>>,
) -> axum::response::Response {
    let repo = Some(state.project.memory_root().to_path_buf());
    let follow_global = q
        .scope
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("global"));

    // "Follow global": clear only this project's `[failover]` override; a
    // coexisting `[team]` override (or any other) in the same file survives.
    if follow_global && repo.is_some() {
        let path = repo.as_deref().expect("repo is some");
        let mut project = match rtrt_core::Config::load_project(path) {
            Ok(p) => p,
            Err(e) => return clear_field_error(e),
        };
        project.failover = None;
        if let Err(e) = crate::util::write_project_config(path, &project) {
            return clear_field_error(e);
        }
        let cfg = match rtrt_core::Config::load_effective(Some(path)) {
            Ok(cfg) => cfg,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        return Json(with_scope(
            failover_json(&cfg.failover, Some(path), false),
            Some(path),
            false,
        ))
        .into_response();
    }

    let Some(Json(req)) = body else {
        // `?scope=global` in the global scope has nothing to clear and carries
        // no body; report the global policy rather than erroring.
        if follow_global {
            let cfg = match rtrt_core::Config::load() {
                Ok(cfg) => cfg,
                Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            return Json(with_scope(
                failover_json(&cfg.failover, None, false),
                None,
                false,
            ))
            .into_response();
        }
        return error_response(StatusCode::BAD_REQUEST, "missing body");
    };

    let current = rtrt_core::Config::load_effective(repo.as_deref())
        .or_else(|_| rtrt_core::Config::load())
        .unwrap_or_default()
        .failover;
    let transient_retries = match patch_unsigned(
        req.transient_retries,
        current.transient_retries.map(u64::from),
    ) {
        Ok(v) => v.map(|v| v as u32),
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let backoff_divisor =
        match patch_unsigned(req.backoff_divisor, current.backoff_divisor.map(u64::from)) {
            Ok(v) => v.map(|v| v as u32),
            Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
        };
    let backoff_ms = match patch_unsigned(req.backoff_ms, current.backoff_ms) {
        Ok(v) => v,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    // A zero divisor would divide the per-call timeout by zero when deriving
    // the backoff; reject it here rather than shipping a config that panics or
    // silently falls back at invoke time. Checked BEFORE any write, for both
    // scopes.
    if backoff_divisor == Some(0) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "failover.backoff_divisor must be greater than 0",
        );
    }

    let failover = FailoverConfig {
        fatal: req.fatal.map(clean_list).unwrap_or(current.fatal),
        quota: req.quota.map(clean_list).unwrap_or(current.quota),
        transient: req.transient.map(clean_list).unwrap_or(current.transient),
        transient_retries,
        backoff_divisor,
        backoff_ms,
    };

    // Per-project write.
    if let Some(path) = repo.as_deref() {
        let mut project = match rtrt_core::Config::load_project(path) {
            Ok(p) => p,
            Err(e) => return clear_field_error(e),
        };
        project.failover = Some(failover.clone());
        if let Err(e) = crate::util::write_project_config(path, &project) {
            return clear_field_error(e);
        }
        return Json(with_scope(
            failover_json(&failover, Some(path), true),
            Some(path),
            true,
        ))
        .into_response();
    }

    // Global write.
    let (cfg, _) = match crate::util::update_config_file(|cfg| {
        cfg.failover = failover;
        Ok(())
    }) {
        Ok(result) => result,
        Err((status, msg)) => return error_response(status, msg),
    };
    Json(with_scope(
        failover_json(&cfg.failover, None, false),
        None,
        false,
    ))
    .into_response()
}
fn patch_unsigned(
    value: crate::util::JsonPatch<u64>,
    existing: Option<u64>,
) -> Result<Option<u64>, String> {
    match value {
        crate::util::JsonPatch::Missing => Ok(existing),
        crate::util::JsonPatch::Null => Ok(None),
        crate::util::JsonPatch::Value(value) => Ok(Some(value)),
    }
}
