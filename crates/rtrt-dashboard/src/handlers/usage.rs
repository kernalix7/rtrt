//! Provider usage + headroom + the load-balancing routing decision — the P4
//! dashboard surface for the usage-aware router.
//!
//! Provider usage is *global* (one ledger at `~/.rtrt/provider-usage.tsv`), so
//! these handlers have no per-project scope toggle. They call the rtrt-providers
//! P1/P2 backend directly:
//! - [`rtrt_providers::provider_usage_windows`] for the 5h / 24h / 7d windows,
//! - [`rtrt_providers::target_headroom`] for the 24h headroom against `[limits]`,
//! - [`rtrt_providers::select_route`] for the ranked routing preview.
#![allow(unused_imports)]

use std::collections::BTreeMap;

use axum::{Json, http::StatusCode, response::IntoResponse};
use rtrt_providers::{
    RouteRequest, TargetHeadroom, TargetWindows, UsageSnapshot, provider_usage_windows,
    select_route, target_headroom,
};
use serde::Serialize;

use crate::prelude::*;

/// Token + request counts for one rolling window, as exchanged with the UI.
#[derive(Debug, Serialize)]
pub(crate) struct UsageWindowView {
    tokens: u64,
    requests: u64,
}

/// The three rolling windows surfaced per target.
#[derive(Debug, Serialize)]
pub(crate) struct UsageWindowsView {
    #[serde(rename = "5h")]
    last_5h: UsageWindowView,
    #[serde(rename = "24h")]
    last_24h: UsageWindowView,
    #[serde(rename = "7d")]
    last_7d: UsageWindowView,
}

/// The 24h headroom for one target against its `[limits]` cap. `null` caps mean
/// "no [limits] entry" — never a fabricated ceiling.
#[derive(Debug, Serialize)]
pub(crate) struct UsageHeadroomView {
    used_tokens: u64,
    limit_tokens: Option<u64>,
    remaining_tokens: Option<u64>,
    used_requests: u64,
    request_limit: Option<u64>,
    remaining_requests: Option<u64>,
}

/// One target row: windowed usage + headroom + whether any window carried an
/// estimated (CLI shell-out) token count.
#[derive(Debug, Serialize)]
pub(crate) struct UsageTargetView {
    target: String,
    windows: UsageWindowsView,
    /// Any of the three windows had at least one estimated request.
    estimated: bool,
    /// `None` when the target has no `[limits]` cap configured.
    headroom: Option<UsageHeadroomView>,
}

#[derive(Debug, Serialize)]
pub(crate) struct UsageResponse {
    targets: Vec<UsageTargetView>,
}

fn window_view(usage: &rtrt_providers::WindowUsage) -> UsageWindowView {
    UsageWindowView {
        tokens: usage.tokens,
        requests: usage.requests,
    }
}

fn headroom_view(headroom: &TargetHeadroom) -> Option<UsageHeadroomView> {
    // No cap on either axis → surface `null` so the UI shows "no [limits] cap".
    if headroom.limits_unknown() {
        return None;
    }
    Some(UsageHeadroomView {
        used_tokens: headroom.used_tokens,
        limit_tokens: headroom.limit_tokens,
        remaining_tokens: headroom.remaining_tokens,
        used_requests: headroom.used_requests,
        request_limit: headroom.request_limit,
        remaining_requests: headroom.remaining_requests,
    })
}

fn target_view(
    target: &str,
    windows: &TargetWindows,
    headroom: Option<&TargetHeadroom>,
) -> UsageTargetView {
    let estimated = windows.last_5h.has_estimates()
        || windows.last_24h.has_estimates()
        || windows.last_7d.has_estimates();
    UsageTargetView {
        target: target.to_string(),
        windows: UsageWindowsView {
            last_5h: window_view(&windows.last_5h),
            last_24h: window_view(&windows.last_24h),
            last_7d: window_view(&windows.last_7d),
        },
        estimated,
        headroom: headroom.and_then(headroom_view),
    }
}

/// `GET /api/usage` — per-target rolling-window usage + 24h `[limits]` headroom.
///
/// Global: provider usage is not per-project, so this takes no scope. The union
/// of "targets seen in the ledger" and "targets configured in `[limits]`" is
/// returned so a configured cap is visible even before any traffic, and a busy
/// target is visible even with no cap.
pub(crate) async fn usage_api() -> Json<UsageResponse> {
    let windows = provider_usage_windows();
    // `[limits]` are global; project overrides don't touch caps, so plain load.
    let headroom = rtrt_core::Config::load()
        .map(|cfg| target_headroom(&cfg))
        .unwrap_or_default();

    let mut names = windows.keys().cloned().collect::<Vec<_>>();
    names.extend(headroom.keys().cloned());
    names.sort();
    names.dedup();

    let empty = TargetWindows::default();
    let targets = names
        .into_iter()
        .map(|name| {
            let w = windows.get(&name).unwrap_or(&empty);
            target_view(&name, w, headroom.get(&name))
        })
        .collect();
    Json(UsageResponse { targets })
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct RoutePreviewQuery {
    #[serde(default)]
    prefer: Option<String>,
    #[serde(default)]
    capability: Option<String>,
}

/// `GET /api/route/preview` — the load-balancing decision for the *next*
/// request, with no prompt required (unlike `/api/route`). It answers "who would
/// serve the next request and why", so the routing is visible alongside usage.
/// Reuses the same `select_route` + headroom view as `/api/route`.
pub(crate) async fn route_preview_api(
    axum::extract::Query(q): axum::extract::Query<RoutePreviewQuery>,
) -> axum::response::Response {
    let prefer = match parse_route_prefer(q.prefer.as_deref()) {
        Ok(prefer) => prefer,
        Err(message) => return route_error(StatusCode::BAD_REQUEST, &message),
    };
    let capability = match parse_route_capability(q.capability.as_deref()) {
        Ok(capability) => capability,
        Err(message) => return route_error(StatusCode::BAD_REQUEST, &message),
    };

    let request = RouteRequest {
        capability,
        prefer,
        target: None,
        model: None,
        mode: None,
    };
    let tools = detected_tools_with_config_overrides();
    let usage = UsageSnapshot::load_best_effort();
    let decision = match select_route(&request, &tools, &usage) {
        Ok(decision) => decision,
        Err(e) => return route_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    // Match the `/api/route` response shape so the UI's route renderer is reused
    // verbatim: `chosen` (the chosen target, marked) + ranked `alternatives`.
    let response = serde_json::json!({
        "chosen": {
            "target": decision.target,
            "mode": decision.mode,
            "model": decision.model,
            "cost_class": decision.cost_class,
            "reason": decision.reason,
        },
        "alternatives": decision.alternatives,
    });
    Json(response).into_response()
}
