//! Orchestration config — the `[team]` roster and the `[failover]` marker
//! overrides, exposed for editing from the dashboard.
//!
//! The roster rtrt ships is only a DEFAULT. Everything a `[team]` section can
//! express — lanes, the tier ladder, the routing policy — is data, so all of it
//! is editable here rather than being frozen into the binary. Nothing in this
//! module names a lane, a tier, a target or a model: the shipped values arrive
//! from `rtrt_core::Config` and the UI's choices come from `/api/detect`.
//!
//! Endpoints:
//!   * `GET/POST /api/team/config`     — `[team]` (+ `[[team.members]]`,
//!     `[team.tiers]`, `[team.policy]`).
//!   * `GET/POST /api/failover/config` — `[failover]`.
//!
//! Both accept the same `?project=` / `?scope=` selector as every other
//! config endpoint (see `handlers::scope`) and answer with the same
//! `scope` / `custom` / `inherited` triple, so the UI's shared
//! "Follow global / Custom (this project)" helper drives them unchanged:
//!
//!   * `GET` reports whether THIS project pins its own `[team]` / `[failover]`
//!     in `<repo>/.rtrt/config.toml`, and serves the effective section either
//!     way (`Config::load_effective`).
//!   * `POST` with a project writes that project's override.
//!   * `POST ?scope=global` clears only this project's override, preserving any
//!     coexisting overrides in the same file.
//!   * `POST` with no project writes the global section, as before.
//!
//! A project override REPLACES the global section wholesale rather than merging
//! into it (see `rtrt_core::config::ProjectConfig::team`), which is what keeps
//! the validation below meaningful: the roster this handler checks is exactly
//! the roster that becomes effective.
//!
//! Every write runs [`TeamConfig::validate`] BEFORE touching any config file:
//! an invalid roster (fallback cycle, unknown lane name, mismatched sibling,
//! design-only lane in an implementing tier…) comes back as a 400 carrying the
//! validator's own message and nothing is persisted. `Config::save_project`
//! re-checks the same invariant at the core boundary.
#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::path::Path;

use axum::{Json, http::StatusCode, response::IntoResponse};
use rtrt_core::config::{
    Balance, Delegation, FailoverConfig, NativePermissions, RecursionPolicy, RosterPreset,
    TeamConfig, TeamMember, TeamMode, TeamPolicy, TierMap,
};
use serde::{Deserialize, Serialize};

use crate::prelude::*;

// ---------------------------------------------------------------------------
// Wire views
// ---------------------------------------------------------------------------

/// One lane, with every field always present.
///
/// `TeamMember` itself skips serializing its defaults (so an untouched config
/// round-trips byte for byte); a form needs the opposite, so this view spells
/// every field out in both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TeamMemberView {
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) model: Option<String>,
    #[serde(default = "default_team_mode")]
    pub(crate) mode: TeamMode,
    #[serde(default)]
    pub(crate) roles: Vec<String>,
    #[serde(default)]
    pub(crate) delegation: Delegation,
    #[serde(default)]
    pub(crate) host_agent: Option<String>,
    pub(crate) logical: Option<String>,
    pub(crate) sibling: Option<String>,
    /// Self-declared tier. Round-tripped even though the ladder editor writes
    /// `[team.tiers]`: dropping it here would silently delete a lane-declared
    /// tier from a hand-written config on the first save from the UI.
    pub(crate) tier: Option<String>,
    #[serde(default)]
    pub(crate) fallback: Vec<String>,
    #[serde(default = "default_allow_impl")]
    pub(crate) allow_impl: bool,
    #[serde(default)]
    pub(crate) flags: BTreeMap<String, String>,
    /// Typed Native worker permissions. The core roster does not interpret
    /// these yet; keep the wire field explicit so dashboard clients can edit
    /// and round-trip the shape without confusing it with invocation flags.
    #[serde(default)]
    pub(crate) permissions: Option<NativePermissions>,
}

fn default_team_mode() -> TeamMode {
    TeamMode::Cli
}

fn default_allow_impl() -> bool {
    true
}

fn default_worker_summary_max_lines() -> u8 {
    TeamPolicy::default().worker_summary_max_lines
}

impl TeamMemberView {
    fn from_member(member: &TeamMember) -> Self {
        Self {
            name: member.name.clone(),
            target: member.target.clone(),
            model: member.model.clone(),
            mode: member.mode,
            roles: member.roles.clone(),
            delegation: member.delegation,
            host_agent: member.host_agent.clone(),
            logical: member.logical.clone(),
            sibling: member.sibling.clone(),
            tier: member.tier.clone(),
            fallback: member.fallback.clone(),
            allow_impl: member.allow_impl,
            flags: member.flags.clone(),
            permissions: member.permissions.clone(),
        }
    }

    /// Tidy a submitted lane: trim every value, and turn a blank optional into
    /// "unset" rather than an empty string the validator would reject with a
    /// message about NUL-free non-empty values the user never typed.
    fn into_member(self) -> TeamMember {
        TeamMember {
            name: self.name.trim().to_string(),
            target: self.target.trim().to_string(),
            model: non_empty(self.model),
            mode: self.mode,
            roles: clean_list(self.roles),
            delegation: self.delegation,
            host_agent: non_empty(self.host_agent),
            logical: non_empty(self.logical),
            sibling: non_empty(self.sibling),
            tier: non_empty(self.tier),
            fallback: clean_list(self.fallback),
            allow_impl: self.allow_impl,
            flags: clean_flags(self.flags),
            permissions: self.permissions,
        }
    }
}

/// One rung of the ladder. A list, not a map, because the order of the rungs is
/// the difficulty ordering the leader climbs — `TierMap` preserves it and a
/// JSON object would leave it to the consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TierView {
    pub(crate) tier: String,
    #[serde(default)]
    pub(crate) members: Vec<String>,
}

/// A rung of the *resolved* ladder: what is actually in force once the shipped
/// default, the configured table, and lane self-declarations are combined.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct EffectiveTierView {
    tier: String,
    members: Vec<String>,
    /// Whether this rung plans rather than implements. Read from
    /// `TeamConfig::is_design_only_tier` so the UI never has to know the
    /// shipped name.
    design_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TeamPolicyView {
    pub(crate) max_retries: u32,
    pub(crate) redo_on_fallback: bool,
    pub(crate) prefer_sibling_on_quota: bool,
    pub(crate) record_provenance: bool,
    /// `null` derives the cap from the roster size.
    pub(crate) max_fallback_depth: Option<usize>,
    /// `null` starts from the first rung of the effective ladder.
    pub(crate) default_tier: Option<String>,
    #[serde(default)]
    pub(crate) explore_tier: Option<String>,
    #[serde(default)]
    pub(crate) review_tier: Option<String>,
    #[serde(default)]
    pub(crate) balance: Balance,
    #[serde(default = "default_worker_summary_max_lines")]
    pub(crate) worker_summary_max_lines: u8,
    #[serde(default = "default_allow_impl")]
    pub(crate) isolate_conflicting: bool,
    /// `null` follows the shipped design-only tier name(s); an explicit list
    /// (empty included) pins them.
    pub(crate) design_only_tiers: Option<Vec<String>>,
}

impl TeamPolicyView {
    fn from_policy(policy: &TeamPolicy) -> Self {
        Self {
            max_retries: policy.max_retries,
            redo_on_fallback: policy.redo_on_fallback,
            prefer_sibling_on_quota: policy.prefer_sibling_on_quota,
            record_provenance: policy.record_provenance,
            max_fallback_depth: policy.max_fallback_depth,
            default_tier: policy.default_tier.clone(),
            explore_tier: policy.explore_tier.clone(),
            review_tier: policy.review_tier.clone(),
            balance: policy.balance,
            worker_summary_max_lines: policy.worker_summary_max_lines,
            isolate_conflicting: policy.isolate_conflicting,
            design_only_tiers: policy.design_only_tiers.clone(),
        }
    }

    #[cfg(test)]
    fn into_policy(self) -> TeamPolicy {
        TeamPolicy {
            max_retries: self.max_retries,
            redo_on_fallback: self.redo_on_fallback,
            prefer_sibling_on_quota: self.prefer_sibling_on_quota,
            record_provenance: self.record_provenance,
            max_fallback_depth: self.max_fallback_depth,
            default_tier: non_empty(self.default_tier),
            explore_tier: non_empty(self.explore_tier),
            review_tier: non_empty(self.review_tier),
            balance: self.balance,
            worker_summary_max_lines: self.worker_summary_max_lines,
            isolate_conflicting: self.isolate_conflicting,
            design_only_tiers: self.design_only_tiers.map(clean_list),
            recursion: RecursionPolicy::default(),
        }
    }
}

/// The parts of the roster rtrt *derives* rather than stores: the resolved
/// ladder, the resolved policy defaults, and each lane's fallback walk. Shown
/// read-only so the user can see what their config actually means before the
/// leader acts on it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TeamEffectiveView {
    tiers: Vec<EffectiveTierView>,
    default_tier: Option<String>,
    max_fallback_depth: usize,
    design_only_tiers: Vec<String>,
    /// lane name -> the lanes a failure walks through, in order.
    chains: BTreeMap<String, Vec<String>>,
}

fn effective_view(team: &TeamConfig) -> TeamEffectiveView {
    let tiers = team.effective_tiers();
    let rungs: Vec<EffectiveTierView> = tiers
        .iter()
        .map(|(tier, members)| EffectiveTierView {
            tier: tier.to_string(),
            members: members.to_vec(),
            design_only: team.is_design_only_tier(tier),
        })
        .collect();
    let design_only_tiers = rungs
        .iter()
        .filter(|rung| rung.design_only)
        .map(|rung| rung.tier.clone())
        .collect();
    let chains = team
        .members
        .iter()
        .map(|member| (member.name.clone(), team.fallback_chain(&member.name)))
        .collect();
    TeamEffectiveView {
        tiers: rungs,
        default_tier: team.effective_default_tier(),
        max_fallback_depth: team.effective_max_fallback_depth(),
        design_only_tiers,
        chains,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Trim a submitted optional; blank becomes "unset".
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Trim every entry of a submitted list and drop the blanks a form leaves
/// behind. Order is preserved and duplicates are kept: both are meaningful, and
/// the validator reports a real duplicate far better than a silent drop would.
fn clean_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Drop unnamed invocation flags. A flag's VALUE may legitimately be empty (a
/// valueless switch), so only the key is required.
fn clean_flags(flags: BTreeMap<String, String>) -> BTreeMap<String, String> {
    flags
        .into_iter()
        .map(|(key, value)| (key.trim().to_string(), value))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// The config file the section being reported lives in: the project's own
/// override file when the scope is Custom, else the global config.
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
    team: bool,
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
            team: p.team.is_some(),
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
// GET/POST /api/team/config
// ---------------------------------------------------------------------------

fn team_json(team: &TeamConfig, repo: Option<&Path>, custom: bool) -> serde_json::Value {
    serde_json::json!({
        "enabled": team.enabled,
        "roster": team.roster,
        "manager_provider": team.manager_provider,
        "manager_model": team.manager_model,
        "manager_base_url": team.manager_base_url,
        "leader_order": team.leader_order,
        "members": team.members.iter().map(TeamMemberView::from_member).collect::<Vec<_>>(),
        // The CONFIGURED ladder, which may be empty — that is not the same as
        // "no ladder", and the UI must be able to tell the two apart to know
        // whether saving would pin the shipped default into the file.
        "tiers": team.tiers.iter().map(|(tier, members)| TierView {
            tier: tier.to_string(),
            members: members.to_vec(),
        }).collect::<Vec<_>>(),
        "policy": TeamPolicyView::from_policy(&team.policy),
        "effective": effective_view(team),
        "path": config_path_string(repo, custom),
    })
}

pub(crate) async fn get_team_config(
    axum::Extension(state): axum::Extension<AppState>,
    axum::extract::Query(_q): axum::extract::Query<ProjectQuery>,
) -> axum::response::Response {
    let repo = Some(state.project.memory_root().to_path_buf());
    let custom = project_overrides(repo.as_deref()).team;
    let cfg = match rtrt_core::Config::load_effective(repo.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    Json(with_scope(
        team_json(&cfg.team, repo.as_deref(), custom),
        repo.as_deref(),
        custom,
    ))
    .into_response()
}

/// Full-replace write: the body carries the whole desired roster, so removing a
/// lane or a rung is just omitting it.
///
/// `manager_provider` / `manager_model` fall back to the stored value when the
/// sender omits them, so a partial client can never blank an identity field
/// into a validation error it did not cause.
#[derive(Debug, Deserialize)]
pub(crate) struct SetTeamRequest {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    roster: Option<RosterPreset>,
    #[serde(default)]
    manager_provider: Option<String>,
    #[serde(default)]
    manager_model: Option<String>,
    #[serde(default)]
    manager_base_url: Option<String>,
    #[serde(default)]
    leader_order: Option<Vec<String>>,
    #[serde(default)]
    members: Option<Vec<TeamMemberView>>,
    #[serde(default)]
    tiers: Option<Vec<TierView>>,
    #[serde(default)]
    policy: Option<TeamPolicyPatch>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TeamPolicyPatch {
    max_retries: Option<u32>,
    redo_on_fallback: Option<bool>,
    prefer_sibling_on_quota: Option<bool>,
    record_provenance: Option<bool>,
    #[serde(default)]
    max_fallback_depth: crate::util::JsonPatch<usize>,
    #[serde(default)]
    default_tier: crate::util::JsonPatch<String>,
    #[serde(default)]
    explore_tier: crate::util::JsonPatch<String>,
    #[serde(default)]
    review_tier: crate::util::JsonPatch<String>,
    balance: Option<Balance>,
    worker_summary_max_lines: Option<u8>,
    isolate_conflicting: Option<bool>,
    #[serde(default)]
    design_only_tiers: crate::util::JsonPatch<Vec<String>>,
}

fn patch_policy(mut policy: TeamPolicy, patch: TeamPolicyPatch) -> Result<TeamPolicy, String> {
    if let Some(v) = patch.max_retries {
        policy.max_retries = v
    }
    if let Some(v) = patch.redo_on_fallback {
        policy.redo_on_fallback = v
    }
    if let Some(v) = patch.prefer_sibling_on_quota {
        policy.prefer_sibling_on_quota = v
    }
    if let Some(v) = patch.record_provenance {
        policy.record_provenance = v
    }
    policy.max_fallback_depth = patch_nullable_usize(
        patch.max_fallback_depth,
        policy.max_fallback_depth,
        "max_fallback_depth",
    )?;
    policy.default_tier =
        patch_nullable_string(patch.default_tier, policy.default_tier, "default_tier")?;
    policy.explore_tier =
        patch_nullable_string(patch.explore_tier, policy.explore_tier, "explore_tier")?;
    policy.review_tier =
        patch_nullable_string(patch.review_tier, policy.review_tier, "review_tier")?;
    if let Some(v) = patch.balance {
        policy.balance = v
    }
    if let Some(v) = patch.worker_summary_max_lines {
        policy.worker_summary_max_lines = v
    }
    if let Some(v) = patch.isolate_conflicting {
        policy.isolate_conflicting = v
    }
    policy.design_only_tiers = patch_nullable_list(
        patch.design_only_tiers,
        policy.design_only_tiers,
        "design_only_tiers",
    )?;
    Ok(policy)
}

fn patch_nullable_usize(
    value: crate::util::JsonPatch<usize>,
    existing: Option<usize>,
    _field: &str,
) -> Result<Option<usize>, String> {
    match value {
        crate::util::JsonPatch::Missing => Ok(existing),
        crate::util::JsonPatch::Null => Ok(None),
        crate::util::JsonPatch::Value(value) => Ok(Some(value)),
    }
}

fn patch_nullable_string(
    value: crate::util::JsonPatch<String>,
    existing: Option<String>,
    _field: &str,
) -> Result<Option<String>, String> {
    match value {
        crate::util::JsonPatch::Missing => Ok(existing),
        crate::util::JsonPatch::Null => Ok(None),
        crate::util::JsonPatch::Value(value) => Ok(non_empty(Some(value))),
    }
}

fn patch_nullable_list(
    value: crate::util::JsonPatch<Vec<String>>,
    existing: Option<Vec<String>>,
    _field: &str,
) -> Result<Option<Vec<String>>, String> {
    match value {
        crate::util::JsonPatch::Missing => Ok(existing),
        crate::util::JsonPatch::Null => Ok(None),
        crate::util::JsonPatch::Value(value) => Ok(Some(clean_list(value))),
    }
}

pub(crate) async fn post_team_config(
    axum::Extension(state): axum::Extension<AppState>,
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
    // `?scope=global` carries no body (the "Follow global" path), so a missing
    // payload must be tolerated exactly as the other scoped endpoints do.
    body: Option<Json<SetTeamRequest>>,
) -> axum::response::Response {
    let repo = Some(state.project.memory_root().to_path_buf());
    let follow_global = q
        .scope
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("global"));

    // "Follow global": CLEAR this project's `[team]` override only. Everything
    // else in the same `<repo>/.rtrt/config.toml` (statusline / output_level /
    // compression / providers / agents / failover) is preserved — the file is
    // rewritten from the whole `ProjectConfig`, and removed only once nothing
    // is overridden at all.
    if follow_global && repo.is_some() {
        let path = repo.as_deref().expect("repo is some");
        let mut project = match rtrt_core::Config::load_project(path) {
            Ok(p) => p,
            Err(e) => return clear_field_error(e),
        };
        project.team = None;
        if let Err(e) = crate::util::write_project_config(path, &project) {
            return clear_field_error(e);
        }
        let cfg = match rtrt_core::Config::load_effective(Some(path)) {
            Ok(cfg) => cfg,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        return Json(with_scope(
            team_json(&cfg.team, Some(path), false),
            Some(path),
            false,
        ))
        .into_response();
    }

    // The layer the form was populated from, used only to fill in identity
    // fields a partial client omitted. Falling back to the global config keeps
    // a project whose stored override is unreadable repairable from the UI.
    let current = rtrt_core::Config::load_effective(repo.as_deref())
        .or_else(|_| rtrt_core::Config::load())
        .unwrap_or_default()
        .team;

    // `?scope=global` with no project selected has nothing to clear; fall
    // through to the plain global write, as the other scoped endpoints do.
    if follow_global {
        let cfg = match rtrt_core::Config::load() {
            Ok(cfg) => cfg,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        return Json(with_scope(team_json(&cfg.team, None, false), None, false)).into_response();
    }

    let Some(Json(req)) = body else {
        return error_response(StatusCode::BAD_REQUEST, "missing body");
    };

    let policy = match req.policy {
        Some(patch) => match patch_policy(current.policy.clone(), patch) {
            Ok(policy) => policy,
            Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
        },
        None => current.policy.clone(),
    };
    let team = TeamConfig {
        enabled: req.enabled.unwrap_or(current.enabled),
        roster: req.roster.unwrap_or(current.roster),
        manager_provider: non_empty(req.manager_provider).unwrap_or(current.manager_provider),
        manager_model: non_empty(req.manager_model).unwrap_or(current.manager_model),
        manager_base_url: req
            .manager_base_url
            .map(|v| non_empty(Some(v)))
            .unwrap_or(current.manager_base_url),
        leader_order: req
            .leader_order
            .map(clean_list)
            .unwrap_or(current.leader_order),
        members: req
            .members
            .map(|members| {
                members
                    .into_iter()
                    .map(TeamMemberView::into_member)
                    .collect()
            })
            .unwrap_or(current.members),
        tiers: req
            .tiers
            .map(|tiers| {
                TierMap::from_pairs(
                    tiers
                        .into_iter()
                        .map(|rung| (rung.tier.trim().to_string(), clean_list(rung.members)))
                        .filter(|(tier, _)| !tier.is_empty()),
                )
            })
            .unwrap_or(current.tiers),
        policy,
    };

    // Validate BEFORE writing: an invalid roster must never reach the file.
    // Because a project override REPLACES the global `[team]`, this roster is
    // exactly the one that becomes effective for the scope being written — so
    // validating it here validates the effective config, for both scopes.
    if let Err(e) = team.validate() {
        return error_response(StatusCode::BAD_REQUEST, e.to_string());
    }

    // Per-project write.
    if let Some(path) = repo.as_deref() {
        let mut project = match rtrt_core::Config::load_project(path) {
            Ok(p) => p,
            Err(e) => return clear_field_error(e),
        };
        project.team = Some(team.clone());
        if let Err(e) = crate::util::write_project_config(path, &project) {
            return clear_field_error(e);
        }
        return Json(with_scope(
            team_json(&team, Some(path), true),
            Some(path),
            true,
        ))
        .into_response();
    }

    // Global write.
    let (cfg, _) = match crate::util::update_config_file(|cfg| {
        cfg.team = team;
        Ok(())
    }) {
        Ok(result) => result,
        Err((status, msg)) => return error_response(status, msg),
    };
    Json(with_scope(team_json(&cfg.team, None, false), None, false)).into_response()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lane_round_trips_through_the_view() {
        let team = TeamConfig::default();
        let member = &team.members[0];
        let view = TeamMemberView::from_member(member);
        assert_eq!(&view.into_member(), member);
        assert!(
            serde_json::to_value(TeamMemberView::from_member(member))
                .expect("serialize lane")
                .get("permissions")
                .is_some_and(serde_json::Value::is_null)
        );
    }

    #[test]
    fn blank_optionals_become_unset_rather_than_empty_strings() {
        let view = TeamMemberView {
            name: "  lane  ".to_string(),
            target: "cli".to_string(),
            model: Some("   ".to_string()),
            mode: TeamMode::Cli,
            roles: vec!["  review  ".to_string(), "  ".to_string()],
            delegation: Delegation::Native,
            host_agent: Some("  ".to_string()),
            logical: Some(String::new()),
            sibling: None,
            tier: Some("  ".to_string()),
            fallback: vec![String::new()],
            allow_impl: false,
            flags: BTreeMap::from([
                ("  ".to_string(), "dropped".to_string()),
                // A valueless switch is legal, so an empty VALUE is kept.
                ("verbose".to_string(), String::new()),
            ]),
            permissions: None,
        };
        let member = view.into_member();
        assert_eq!(member.name, "lane");
        assert_eq!(member.model, None);
        assert_eq!(member.host_agent, None);
        assert_eq!(member.logical, None);
        assert_eq!(member.tier, None);
        assert_eq!(member.roles, vec!["review".to_string()]);
        assert!(member.fallback.is_empty());
        assert_eq!(member.flags.len(), 1);
        assert_eq!(member.flags.get("verbose").map(String::as_str), Some(""));
    }

    #[test]
    fn new_team_wire_fields_round_trip() {
        let mut team = TeamConfig::preset(RosterPreset::OpencodeLead);
        let member = team.members.first().expect("preset has members").clone();
        let member_view: TeamMemberView = serde_json::from_value(
            serde_json::to_value(TeamMemberView::from_member(&member)).expect("serialize member"),
        )
        .expect("deserialize member");
        assert_eq!(member_view.into_member(), member);

        team.policy.explore_tier = Some("explore".to_string());
        team.policy.review_tier = Some("review".to_string());
        team.policy.balance = Balance::Room;
        team.policy.worker_summary_max_lines = 7;
        team.policy.isolate_conflicting = false;
        let policy = team.policy.clone();
        let policy_view: TeamPolicyView = serde_json::from_value(
            serde_json::to_value(TeamPolicyView::from_policy(&policy)).expect("serialize policy"),
        )
        .expect("deserialize policy");
        assert_eq!(policy_view.into_policy(), policy);

        assert_eq!(team_json(&team, None, false)["roster"], "opencode-lead");
        let request: SetTeamRequest = serde_json::from_value(serde_json::json!({
            "roster": "opencode-lead"
        }))
        .expect("deserialize request");
        assert_eq!(request.roster, Some(RosterPreset::OpencodeLead));
    }

    #[test]
    fn native_permissions_round_trip_preserves_edit_bash_order_and_none() {
        use rtrt_core::config::{PermissionAction, PermissionMap};

        let mut member = TeamConfig::default().members[0].clone();
        member.permissions = Some(NativePermissions {
            edit: Some(PermissionAction::Ask),
            bash: PermissionMap::from_pairs([
                ("git status", PermissionAction::Allow),
                ("cargo test *", PermissionAction::Ask),
                ("cargo build", PermissionAction::Deny),
            ]),
        });

        let view = TeamMemberView::from_member(&member);
        assert_eq!(view.permissions, member.permissions);
        assert_eq!(view.into_member(), member);
        assert_eq!(
            member
                .permissions
                .as_ref()
                .expect("native permissions")
                .bash
                .iter()
                .collect::<Vec<_>>(),
            [
                ("git status", PermissionAction::Allow),
                ("cargo test *", PermissionAction::Ask),
                ("cargo build", PermissionAction::Deny),
            ]
        );

        let without_permissions = TeamMemberView::from_member(&TeamConfig::default().members[0]);
        assert_eq!(without_permissions.permissions, None);
    }

    #[test]
    fn old_team_wire_json_uses_new_field_defaults() {
        let member: TeamMemberView = serde_json::from_value(serde_json::json!({
            "name": "worker",
            "target": "opencode",
            "model": null,
            "logical": null,
            "sibling": null,
            "tier": null
        }))
        .expect("deserialize old member");
        assert_eq!(member.delegation, Delegation::Native);
        assert!(member.host_agent.is_none());
        assert!(member.permissions.is_none());

        let permissions: NativePermissions = serde_json::from_value(serde_json::json!({
            "edit": "ask",
            "bash": {"git status": "allow"}
        }))
        .expect("deserialize typed worker permissions");
        use rtrt_core::config::PermissionAction;
        assert_eq!(permissions.edit, Some(PermissionAction::Ask));
        assert_eq!(
            permissions.bash.get("git status"),
            Some(PermissionAction::Allow)
        );

        let policy = serde_json::from_value::<TeamPolicyView>(serde_json::json!({
            "max_retries": 2,
            "redo_on_fallback": true,
            "prefer_sibling_on_quota": true,
            "record_provenance": true,
            "max_fallback_depth": null,
            "default_tier": null,
            "design_only_tiers": null
        }))
        .expect("deserialize old policy")
        .into_policy();
        assert_eq!(policy.explore_tier, None);
        assert_eq!(policy.review_tier, None);
        assert_eq!(policy.balance, Balance::Order);
        assert_eq!(policy.worker_summary_max_lines, 3);
        assert!(policy.isolate_conflicting);

        let request: SetTeamRequest =
            serde_json::from_value(serde_json::json!({})).expect("deserialize old request");
        assert_eq!(request.roster, None);
    }

    #[test]
    fn the_effective_view_resolves_the_shipped_ladder() {
        let team = TeamConfig::default();
        let view = effective_view(&team);
        // Nothing is asserted about WHICH tiers ship — only that the resolved
        // ladder is non-empty, that the default rung is its first, and that the
        // design-only set is derived rather than invented.
        assert!(!view.tiers.is_empty());
        assert_eq!(
            view.default_tier.as_deref(),
            Some(view.tiers[0].tier.as_str())
        );
        assert_eq!(view.max_fallback_depth, team.members.len());
        for tier in &view.design_only_tiers {
            assert!(team.is_design_only_tier(tier));
        }
        assert_eq!(view.chains.len(), team.members.len());
    }

    #[test]
    fn tier_order_survives_the_wire_view() {
        let mut team = TeamConfig::default();
        let first = team.members[0].name.clone();
        let second = team.members[1].name.clone();
        team.tiers = TierMap::from_pairs([
            ("z-last", vec![first.clone()]),
            ("a-first", vec![second.clone()]),
        ]);
        let json = team_json(&team, None, false);
        let tiers = json["tiers"].as_array().expect("tiers is a list");
        // A JSON object would leave rung ordering to the consumer; the list
        // form keeps the declared difficulty order.
        assert_eq!(tiers[0]["tier"], "z-last");
        assert_eq!(tiers[1]["tier"], "a-first");
    }

    #[test]
    fn the_scope_triple_matches_the_other_config_endpoints() {
        let repo = Path::new("/nonexistent/repo");
        let global = scope_fields(None, false);
        assert_eq!(global["scope"], "global");
        assert_eq!(global["custom"], false);
        // Nothing to inherit from in the global scope.
        assert_eq!(global["inherited"], false);

        // A selected project that pins no `[team]` INHERITS the global roster…
        let inherited = scope_fields(Some(repo), false);
        assert_eq!(inherited["scope"], "global");
        assert_eq!(inherited["custom"], false);
        assert_eq!(inherited["inherited"], true);

        // …and one that pins its own is Custom, never both.
        let custom = scope_fields(Some(repo), true);
        assert_eq!(custom["scope"], "custom");
        assert_eq!(custom["custom"], true);
        assert_eq!(custom["inherited"], false);
    }

    #[test]
    fn the_reported_path_follows_the_scope() {
        let repo = Path::new("/nonexistent/repo");
        // Custom → the project's own override file, so the UI hint names the
        // file the save actually lands in.
        let custom = config_path_string(Some(repo), true);
        assert_eq!(
            custom,
            rtrt_core::Config::project_config_path(repo)
                .to_string_lossy()
                .into_owned()
        );
        // Following global → the global config, same as the global scope.
        assert_eq!(
            config_path_string(Some(repo), false),
            config_path_string(None, false)
        );
    }
}
