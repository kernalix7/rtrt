//! Daily usage limits — global `[limits]` editor.
//!
//! `Config.limits` is a `BTreeMap<target, { daily_tokens, daily_requests }>`.
//! These handlers expose the whole map for read and replace it wholesale on
//! write, persisting to the global `~/.rtrt/config.toml` via `write_config_file`.
//! Plain global settings — no per-project scope toggle.
#![allow(unused_imports)]

use std::collections::BTreeMap;

use axum::{Json, http::StatusCode, response::IntoResponse};
use rtrt_core::config::TargetLimit;
use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// One target row as exchanged with the UI. `daily_tokens` / `daily_requests`
/// are optional ceilings; an absent (null) value means "no limit on that axis".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LimitTargetView {
    pub(crate) target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daily_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daily_requests: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LimitsConfigResponse {
    targets: Vec<LimitTargetView>,
    path: String,
}

/// Flatten `Config.limits` into a stable, name-sorted list for the UI.
fn limits_to_views(limits: &rtrt_core::config::LimitsConfig) -> Vec<LimitTargetView> {
    limits
        .targets
        .iter()
        .map(|(name, lim)| LimitTargetView {
            target: name.clone(),
            daily_tokens: lim.daily_tokens,
            daily_requests: lim.daily_requests,
        })
        .collect()
}

pub(crate) async fn get_limits_config()
-> std::result::Result<Json<LimitsConfigResponse>, (StatusCode, String)> {
    let cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let path = rtrt_core::Config::default_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Json(LimitsConfigResponse {
        targets: limits_to_views(&cfg.limits),
        path,
    }))
}

/// Full-replace write: the body carries the complete desired target set, so
/// removing a target is just omitting it. Empty/whitespace target names and
/// rows with no ceiling at all are dropped to keep the config tidy.
#[derive(Debug, Deserialize)]
pub(crate) struct SetLimitsRequest {
    #[serde(default)]
    targets: Vec<LimitTargetView>,
}

pub(crate) async fn post_limits_config(
    Json(req): Json<SetLimitsRequest>,
) -> std::result::Result<Json<LimitsConfigResponse>, (StatusCode, String)> {
    let mut cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut targets: BTreeMap<String, TargetLimit> = BTreeMap::new();
    for view in req.targets {
        let name = view.target.trim().to_string();
        if name.is_empty() {
            continue;
        }
        // Skip a row that pins neither axis — it would persist as an empty,
        // meaningless `[limits.<name>]` table.
        if view.daily_tokens.is_none() && view.daily_requests.is_none() {
            continue;
        }
        targets.insert(
            name,
            TargetLimit {
                daily_tokens: view.daily_tokens,
                daily_requests: view.daily_requests,
            },
        );
    }
    cfg.limits = rtrt_core::config::LimitsConfig { targets };

    let path = write_config_file(&cfg)?;
    Ok(Json(LimitsConfigResponse {
        targets: limits_to_views(&cfg.limits),
        path,
    }))
}
