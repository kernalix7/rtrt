//! Daily usage limits — global `[limits]` editor.
//!
//! `Config.limits` is a `BTreeMap<target, { daily_tokens, daily_requests, pools }>`.
//! These handlers expose the whole map for read and replace it wholesale on
//! write, persisting to the global `~/.rtrt/config.toml` via `write_config_file`.
//! Plain global settings — no per-project scope toggle.
#![allow(unused_imports)]

use std::collections::BTreeMap;

use axum::{Json, http::StatusCode, response::IntoResponse};
use rtrt_core::config::{PoolLimit, TargetLimit};
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
    /// Per-pool ceilings nested under this target
    /// (`[limits.<target>.pools.<pool>]`). Omitted from the response entirely
    /// when the target has none, so a config without pool caps reads exactly as
    /// it always did. On write, an absent field PRESERVES whatever is already
    /// configured (the UI does not edit pools yet); an explicit list — empty
    /// included — replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pools: Option<Vec<LimitPoolView>>,
}

/// One pool row inside a target. Same optional-ceiling semantics as the target
/// row; the pool name is the model prefix rtrt derives its quota bucket from
/// (`opencode-go/glm-5.2` → `opencode-go`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LimitPoolView {
    pub(crate) pool: String,
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
            pools: pools_to_views(lim),
        })
        .collect()
}

/// `None` for a target with no pool caps, so the field disappears from the
/// response instead of showing up as an empty list.
fn pools_to_views(limit: &TargetLimit) -> Option<Vec<LimitPoolView>> {
    if limit.pools.is_empty() {
        return None;
    }
    Some(
        limit
            .pools
            .iter()
            .map(|(pool, cap)| LimitPoolView {
                pool: pool.clone(),
                daily_tokens: cap.daily_tokens,
                daily_requests: cap.daily_requests,
            })
            .collect(),
    )
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
        // Per-pool caps: an absent `pools` field carries the configured ones
        // across (a wholesale replace must not silently delete caps the sender
        // never saw), while an explicit list replaces them.
        let pools = match view.pools {
            Some(views) => pool_views_to_limits(views),
            None => cfg
                .limits
                .target(&name)
                .map(|existing| existing.pools.clone())
                .unwrap_or_default(),
        };
        // Skip a row that pins nothing at all — it would persist as an empty,
        // meaningless `[limits.<name>]` table. A target whose only ceilings are
        // per-pool ones is NOT empty.
        if view.daily_tokens.is_none() && view.daily_requests.is_none() && pools.is_empty() {
            continue;
        }
        targets.insert(
            name,
            TargetLimit {
                daily_tokens: view.daily_tokens,
                daily_requests: view.daily_requests,
                pools,
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

/// Same tidiness rule as targets: unnamed pools and pools that pin neither axis
/// are dropped rather than written as empty tables.
fn pool_views_to_limits(views: Vec<LimitPoolView>) -> BTreeMap<String, PoolLimit> {
    let mut pools = BTreeMap::new();
    for view in views {
        let name = view.pool.trim().to_string();
        if name.is_empty() || (view.daily_tokens.is_none() && view.daily_requests.is_none()) {
            continue;
        }
        pools.insert(
            name,
            PoolLimit {
                daily_tokens: view.daily_tokens,
                daily_requests: view.daily_requests,
            },
        );
    }
    pools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(daily_tokens: Option<u64>, pools: &[(&str, u64)]) -> TargetLimit {
        TargetLimit {
            daily_tokens,
            daily_requests: None,
            pools: pools
                .iter()
                .map(|(pool, tokens)| {
                    (
                        (*pool).to_string(),
                        PoolLimit {
                            daily_tokens: Some(*tokens),
                            daily_requests: None,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn a_target_without_pool_caps_serializes_exactly_as_before() {
        let mut limits = rtrt_core::config::LimitsConfig::default();
        limits
            .targets
            .insert("openai".to_string(), target(Some(1_000), &[]));
        let views = limits_to_views(&limits);
        let json = serde_json::to_string(&views).unwrap();
        assert_eq!(json, r#"[{"target":"openai","daily_tokens":1000}]"#);
    }

    #[test]
    fn pool_caps_are_exposed_when_configured() {
        let mut limits = rtrt_core::config::LimitsConfig::default();
        limits.targets.insert(
            "opencode".to_string(),
            target(Some(1_000), &[("opencode-go", 400)]),
        );
        let views = limits_to_views(&limits);
        let pools = views[0].pools.as_ref().expect("pools exposed");
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pool, "opencode-go");
        assert_eq!(pools[0].daily_tokens, Some(400));
    }

    #[test]
    fn a_write_without_pools_preserves_the_configured_ones() {
        // The UI does not send `pools`, so a save must not delete them.
        let body: LimitTargetView =
            serde_json::from_str(r#"{"target":"opencode","daily_tokens":2000}"#).unwrap();
        assert!(body.pools.is_none());
        let existing = target(Some(1_000), &[("opencode-go", 400)]);
        let pools = match body.pools {
            Some(views) => pool_views_to_limits(views),
            None => existing.pools.clone(),
        };
        assert_eq!(pools["opencode-go"].daily_tokens, Some(400));
    }

    #[test]
    fn an_explicit_pool_list_replaces_and_drops_empty_rows() {
        let views = vec![
            LimitPoolView {
                pool: "ollama".to_string(),
                daily_tokens: Some(10),
                daily_requests: None,
            },
            // No ceiling on either axis: dropped rather than written as an
            // empty `[limits.<t>.pools.<p>]` table.
            LimitPoolView {
                pool: "opencode-go".to_string(),
                daily_tokens: None,
                daily_requests: None,
            },
            LimitPoolView {
                pool: "   ".to_string(),
                daily_tokens: Some(5),
                daily_requests: None,
            },
        ];
        let pools = pool_views_to_limits(views);
        assert_eq!(pools.keys().collect::<Vec<_>>(), vec!["ollama"]);
    }
}
