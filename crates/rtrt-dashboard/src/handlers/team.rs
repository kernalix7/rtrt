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
//! "Follow global / Custom (this project)" helper drives them unchanged.
//! Orchestration currently has NO per-project layer — `ProjectConfig` carries
//! no `[team]` / `[failover]` override — so a project scope always resolves to
//! the global value. `project_overridable: false` says so explicitly instead of
//! letting the UI guess, and the day a project layer lands the endpoint shape
//! does not have to change.
//!
//! Every write runs [`TeamConfig::validate`] BEFORE touching the config file:
//! an invalid roster (fallback cycle, unknown lane name, mismatched sibling,
//! design-only lane in an implementing tier…) comes back as a 400 carrying the
//! validator's own message and nothing is persisted.
#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::path::Path;

use axum::{Json, http::StatusCode, response::IntoResponse};
use rtrt_core::config::{FailoverConfig, TeamConfig, TeamMember, TeamMode, TeamPolicy, TierMap};
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
}

fn default_team_mode() -> TeamMode {
    TeamMode::Cli
}

fn default_allow_impl() -> bool {
    true
}

impl TeamMemberView {
    fn from_member(member: &TeamMember) -> Self {
        Self {
            name: member.name.clone(),
            target: member.target.clone(),
            model: member.model.clone(),
            mode: member.mode,
            roles: member.roles.clone(),
            logical: member.logical.clone(),
            sibling: member.sibling.clone(),
            tier: member.tier.clone(),
            fallback: member.fallback.clone(),
            allow_impl: member.allow_impl,
            flags: member.flags.clone(),
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
            logical: non_empty(self.logical),
            sibling: non_empty(self.sibling),
            tier: non_empty(self.tier),
            fallback: clean_list(self.fallback),
            allow_impl: self.allow_impl,
            flags: clean_flags(self.flags),
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
            design_only_tiers: policy.design_only_tiers.clone(),
        }
    }

    fn into_policy(self) -> TeamPolicy {
        TeamPolicy {
            max_retries: self.max_retries,
            redo_on_fallback: self.redo_on_fallback,
            prefer_sibling_on_quota: self.prefer_sibling_on_quota,
            record_provenance: self.record_provenance,
            max_fallback_depth: self.max_fallback_depth,
            default_tier: non_empty(self.default_tier),
            design_only_tiers: self.design_only_tiers.map(clean_list),
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

/// The config file these sections are written to.
fn config_path_string() -> String {
    rtrt_core::Config::default_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The shared scope triple.
///
/// `custom` is false unconditionally: orchestration has no per-project layer in
/// `ProjectConfig`, so a selected project always reads the global roster. The
/// shape still matches the other config endpoints so the UI helper is reusable
/// and a future project layer is a value change, not a contract change.
fn scope_fields(repo: Option<&Path>) -> serde_json::Value {
    let custom = false;
    serde_json::json!({
        "scope": if custom { "custom" } else { "global" },
        "custom": custom,
        "inherited": repo.is_some(),
        "project_overridable": false,
    })
}

/// Merge the scope triple into a response object.
fn with_scope(mut value: serde_json::Value, repo: Option<&Path>) -> serde_json::Value {
    if let (Some(target), Some(scope)) = (value.as_object_mut(), scope_fields(repo).as_object()) {
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

fn team_json(team: &TeamConfig) -> serde_json::Value {
    serde_json::json!({
        "enabled": team.enabled,
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
        "path": config_path_string(),
    })
}

pub(crate) async fn get_team_config(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
) -> axum::response::Response {
    let repo = resolve_project_repo(q.project.as_deref());
    let cfg = match rtrt_core::Config::load_effective(repo.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    Json(with_scope(team_json(&cfg.team), repo.as_deref())).into_response()
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
    enabled: bool,
    #[serde(default)]
    manager_provider: Option<String>,
    #[serde(default)]
    manager_model: Option<String>,
    #[serde(default)]
    manager_base_url: Option<String>,
    #[serde(default)]
    leader_order: Vec<String>,
    #[serde(default)]
    members: Vec<TeamMemberView>,
    #[serde(default)]
    tiers: Vec<TierView>,
    #[serde(default)]
    policy: Option<TeamPolicyView>,
}

pub(crate) async fn post_team_config(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
    // `?scope=global` carries no body (the "Follow global" path), so a missing
    // payload must be tolerated exactly as the other scoped endpoints do.
    body: Option<Json<SetTeamRequest>>,
) -> axum::response::Response {
    let repo = resolve_project_repo(q.project.as_deref());
    let follow_global = q
        .scope
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("global"));

    let mut cfg = match rtrt_core::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    // "Follow global": there is no project-level `[team]` to clear, so this
    // just re-reads the global roster. Kept so the UI's shared scope handler
    // can call it without special-casing this page.
    if follow_global {
        return Json(with_scope(team_json(&cfg.team), repo.as_deref())).into_response();
    }

    let Some(Json(req)) = body else {
        return error_response(StatusCode::BAD_REQUEST, "missing body");
    };

    let current = cfg.team.clone();
    let team = TeamConfig {
        enabled: req.enabled,
        manager_provider: non_empty(req.manager_provider).unwrap_or(current.manager_provider),
        manager_model: non_empty(req.manager_model).unwrap_or(current.manager_model),
        manager_base_url: non_empty(req.manager_base_url),
        leader_order: clean_list(req.leader_order),
        members: req
            .members
            .into_iter()
            .map(TeamMemberView::into_member)
            .collect(),
        tiers: TierMap::from_pairs(
            req.tiers
                .into_iter()
                .map(|rung| (rung.tier.trim().to_string(), clean_list(rung.members)))
                .filter(|(tier, _)| !tier.is_empty()),
        ),
        policy: req
            .policy
            .map(TeamPolicyView::into_policy)
            .unwrap_or(current.policy),
    };

    // Validate BEFORE writing: an invalid roster must never reach the file.
    if let Err(e) = team.validate() {
        return error_response(StatusCode::BAD_REQUEST, e.to_string());
    }

    cfg.team = team;
    if let Err((status, msg)) = write_config_file(&cfg) {
        return error_response(status, msg);
    }
    Json(with_scope(team_json(&cfg.team), repo.as_deref())).into_response()
}

// ---------------------------------------------------------------------------
// GET/POST /api/failover/config
// ---------------------------------------------------------------------------

fn failover_json(failover: &FailoverConfig) -> serde_json::Value {
    serde_json::json!({
        "fatal": failover.fatal,
        "quota": failover.quota,
        "transient": failover.transient,
        "transient_retries": failover.transient_retries,
        "backoff_divisor": failover.backoff_divisor,
        "backoff_ms": failover.backoff_ms,
        "path": config_path_string(),
    })
}

pub(crate) async fn get_failover_config(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
) -> axum::response::Response {
    let repo = resolve_project_repo(q.project.as_deref());
    let cfg = match rtrt_core::Config::load_effective(repo.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    Json(with_scope(failover_json(&cfg.failover), repo.as_deref())).into_response()
}

/// Full-replace write. An omitted marker list clears that class; the numeric
/// knobs are optional and `null` restores the built-in behaviour.
#[derive(Debug, Deserialize)]
pub(crate) struct SetFailoverRequest {
    #[serde(default)]
    fatal: Vec<String>,
    #[serde(default)]
    quota: Vec<String>,
    #[serde(default)]
    transient: Vec<String>,
    #[serde(default)]
    transient_retries: Option<u32>,
    #[serde(default)]
    backoff_divisor: Option<u32>,
    #[serde(default)]
    backoff_ms: Option<u64>,
}

pub(crate) async fn post_failover_config(
    axum::extract::Query(q): axum::extract::Query<ProjectQuery>,
    body: Option<Json<SetFailoverRequest>>,
) -> axum::response::Response {
    let repo = resolve_project_repo(q.project.as_deref());
    let follow_global = q
        .scope
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("global"));

    let mut cfg = match rtrt_core::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if follow_global {
        return Json(with_scope(failover_json(&cfg.failover), repo.as_deref())).into_response();
    }

    let Some(Json(req)) = body else {
        return error_response(StatusCode::BAD_REQUEST, "missing body");
    };

    // A zero divisor would divide the per-call timeout by zero when deriving
    // the backoff; reject it here rather than shipping a config that panics or
    // silently falls back at invoke time.
    if req.backoff_divisor == Some(0) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "failover.backoff_divisor must be greater than 0",
        );
    }

    cfg.failover = FailoverConfig {
        fatal: clean_list(req.fatal),
        quota: clean_list(req.quota),
        transient: clean_list(req.transient),
        transient_retries: req.transient_retries,
        backoff_divisor: req.backoff_divisor,
        backoff_ms: req.backoff_ms,
    };
    if let Err((status, msg)) = write_config_file(&cfg) {
        return error_response(status, msg);
    }
    Json(with_scope(failover_json(&cfg.failover), repo.as_deref())).into_response()
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
    }

    #[test]
    fn blank_optionals_become_unset_rather_than_empty_strings() {
        let view = TeamMemberView {
            name: "  lane  ".to_string(),
            target: "cli".to_string(),
            model: Some("   ".to_string()),
            mode: TeamMode::Cli,
            roles: vec!["  review  ".to_string(), "  ".to_string()],
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
        };
        let member = view.into_member();
        assert_eq!(member.name, "lane");
        assert_eq!(member.model, None);
        assert_eq!(member.logical, None);
        assert_eq!(member.tier, None);
        assert_eq!(member.roles, vec!["review".to_string()]);
        assert!(member.fallback.is_empty());
        assert_eq!(member.flags.len(), 1);
        assert_eq!(member.flags.get("verbose").map(String::as_str), Some(""));
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
        let json = team_json(&team);
        let tiers = json["tiers"].as_array().expect("tiers is a list");
        // A JSON object would leave rung ordering to the consumer; the list
        // form keeps the declared difficulty order.
        assert_eq!(tiers[0]["tier"], "z-last");
        assert_eq!(tiers[1]["tier"], "a-first");
    }

    #[test]
    fn the_scope_triple_matches_the_other_config_endpoints() {
        let global = scope_fields(None);
        assert_eq!(global["scope"], "global");
        assert_eq!(global["custom"], false);
        assert_eq!(global["inherited"], false);
        let project = scope_fields(Some(Path::new("/tmp/repo")));
        assert_eq!(project["scope"], "global");
        assert_eq!(project["custom"], false);
        // A selected project inherits the global roster — there is no
        // per-project `[team]` layer to override it with.
        assert_eq!(project["inherited"], true);
        assert_eq!(project["project_overridable"], false);
    }
}
