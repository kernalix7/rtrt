use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{CompressionLevel, Error, Result, pool::PoolKey};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
static CONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Only permission-prompt bridge accepted for delegated Claude CLI lanes.
pub const CLAUDE_PERMISSION_PROMPT_TOOL: &str = "mcp__rtrt__permission_prompt";
pub const DEFAULT_CLAUDE_CONTINUITY_MAX_RESUMED_TURNS: u8 = 4;
pub const DEFAULT_CLAUDE_CONTINUITY_TTL_SECS: u64 = 900;
pub const MAX_CLAUDE_CONTINUITY_RESUMED_TURNS: u8 = 16;
pub const MIN_CLAUDE_CONTINUITY_TTL_SECS: u64 = 30;
pub const MAX_CLAUDE_CONTINUITY_TTL_SECS: u64 = 86_400;
pub const MAX_RECURSION_DEPTH: u8 = 8;
pub const MAX_RECURSION_FAN_OUT: u16 = 32;
pub const MAX_RECURSION_TOTAL_NODES: u32 = 1024;
pub const MAX_RECURSION_TOKEN_BUDGET: u64 = 100_000_000;
pub const MAX_RECURSION_DEADLINE_SECS: u64 = 86_400;
pub const MAX_RECURSION_PAYLOAD_BYTES: u32 = 1024 * 1024;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub auto_compress: AutoCompressConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default, skip_serializing_if = "TeamConfig::is_default")]
    pub team: TeamConfig,
    #[serde(default, skip_serializing_if = "FailoverConfig::is_default")]
    pub failover: FailoverConfig,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TeamMode {
    Cli,
    Api,
    Auto,
}

/// How the host reaches a team member.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delegation {
    /// Use the current host's native Task / agent mechanism.
    #[default]
    Native,
    /// Spawn an external CLI via `claude -p`. Restricted to `claude` by
    /// [`TeamConfig::validate`].
    #[serde(rename = "cli", alias = "shell")]
    Shell,
}

/// Action applied by a native host to one configurable worker capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

/// Insertion-ordered command-pattern permissions for a native worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionMap(Vec<(String, PermissionAction)>);

impl PermissionMap {
    pub fn from_pairs<K>(pairs: impl IntoIterator<Item = (K, PermissionAction)>) -> Self
    where
        K: Into<String>,
    {
        let mut permissions = Self::default();
        for (pattern, action) in pairs {
            permissions.insert(pattern, action);
        }
        permissions
    }

    pub fn insert(&mut self, pattern: impl Into<String>, action: PermissionAction) {
        let pattern = pattern.into();
        if let Some((_, existing)) = self.0.iter_mut().find(|(existing, _)| existing == &pattern) {
            *existing = action;
        } else {
            self.0.push((pattern, action));
        }
    }

    pub fn get(&self, pattern: &str) -> Option<PermissionAction> {
        self.0
            .iter()
            .find(|(configured, _)| configured == pattern)
            .map(|(_, action)| *action)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, PermissionAction)> {
        self.0
            .iter()
            .map(|(pattern, action)| (pattern.as_str(), *action))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for PermissionMap {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(pattern, action)| (pattern, action)))
    }
}

impl<'de> Deserialize<'de> for PermissionMap {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct PermissionMapVisitor;

        impl<'de> serde::de::Visitor<'de> for PermissionMapVisitor {
            type Value = PermissionMap;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an ordered table of command patterns to permission actions")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut access: M,
            ) -> std::result::Result<PermissionMap, M::Error> {
                let mut permissions = PermissionMap::default();
                while let Some((pattern, action)) =
                    access.next_entry::<String, PermissionAction>()?
                {
                    if permissions.get(&pattern).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate bash permission pattern: {pattern}"
                        )));
                    }
                    permissions.0.push((pattern, action));
                }
                Ok(permissions)
            }
        }

        deserializer.deserialize_map(PermissionMapVisitor)
    }
}

/// Capabilities whose policy may be selected by the config owner for a native
/// worker. Host boundaries such as Task, external directories, and agent/team
/// bridges deliberately are not represented here and remain host-denied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePermissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<PermissionAction>,
    #[serde(default, skip_serializing_if = "PermissionMap::is_empty")]
    pub bash: PermissionMap,
}

/// Explicit, bounded opt-in to resuming a Claude CLI lane.
///
/// Omission and `enabled = false` both retain one-shot fresh sessions. Runtime
/// state is intentionally not serializable as part of configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeContinuity {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_claude_continuity_max_resumed_turns")]
    pub max_resumed_turns: u8,
    #[serde(default = "default_claude_continuity_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for ClaudeContinuity {
    fn default() -> Self {
        Self {
            enabled: false,
            max_resumed_turns: default_claude_continuity_max_resumed_turns(),
            ttl_secs: default_claude_continuity_ttl_secs(),
        }
    }
}

impl Delegation {
    fn is_native(&self) -> bool {
        *self == Self::Native
    }
}

/// How equally suitable lanes in one tier are ordered.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Balance {
    /// Preserve the configured lane order.
    #[default]
    Order,
    /// Prefer the lane whose backing pool has more room.
    Room,
}

impl Balance {
    fn is_order(&self) -> bool {
        *self == Self::Order
    }
}

/// A shipped roster shape. Selecting one is explicit; [`TeamConfig::default`]
/// always remains the classic roster.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RosterPreset {
    #[default]
    Classic,
    OpencodeLead,
}

impl RosterPreset {
    fn is_classic(&self) -> bool {
        *self == Self::Classic
    }
}

/// One lane of the team: a concrete `(target, model, mode)` the leader can
/// delegate to, plus the routing policy that decides *when* it is used.
///
/// Everything after `roles` is optional and defaults to "unset", so a `[team]`
/// section written before lanes existed parses and re-serializes byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamMember {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub mode: TeamMode,
    pub roles: Vec<String>,
    /// Native delegation stays inside the current agent host. CLI delegation
    /// runs via `claude -p`; invoking another OpenCode instance is rejected
    /// during validation.
    #[serde(default, skip_serializing_if = "Delegation::is_native")]
    pub delegation: Delegation,
    /// Config-owner policy for host-native edit and bounded command access.
    /// CLI delegation continues to use its invocation flags instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<NativePermissions>,
    /// Host-native agent name used for Task / `@mention` delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_agent: Option<String>,
    /// The logical model behind this lane (e.g. `glm-5.2`). Two members sharing
    /// a `logical` are the *same* model reached through different pools — that
    /// is what makes quota crossover between them safe, and it is the only
    /// thing [`TeamMember::sibling`] is allowed to pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical: Option<String>,
    /// Name of the sibling lane: the same [`TeamMember::logical`] model served
    /// by another pool. Consulted before the fallback chain when this lane's
    /// pool runs out of quota, so a quota wall costs a pool switch instead of a
    /// model downgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sibling: Option<String>,
    /// The difficulty tier this lane serves. Purely a self-declaration: it adds
    /// the lane to that tier's roster in [`TeamConfig::effective_tiers`], which
    /// lets a roster be expressed member-by-member without a `[team.tiers]`
    /// table at all. `[team.tiers]` still decides ordering within a tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Ordered replacement lanes, tried left to right when this one fails past
    /// its retries. Names must resolve to other members and must not form a
    /// cycle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    /// Whether this lane may implement (write code). A design-only lane —
    /// typically an expensive or tightly rationed one — sets `false` and may
    /// then only appear in tiers listed under
    /// [`TeamPolicy::design_only_tiers`].
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub allow_impl: bool,
    /// Free-form per-lane invocation flags, passed through verbatim by whoever
    /// invokes the lane (e.g. `permission-mode` / `output-format` for a
    /// `claude -p` lane). Safe future flags remain pass-through, while
    /// [`TeamConfig::validate`] rejects permission-bypass flags.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub flags: BTreeMap<String, String>,
}

impl TeamMember {
    /// A lane with only its identity set; every routing field takes its default
    /// so callers opt into exactly the policy they mean.
    pub fn new(name: impl Into<String>, target: impl Into<String>, mode: TeamMode) -> Self {
        Self {
            name: name.into(),
            target: target.into(),
            model: None,
            mode,
            roles: Vec::new(),
            delegation: Delegation::Native,
            permissions: None,
            host_agent: None,
            logical: None,
            sibling: None,
            tier: None,
            fallback: Vec::new(),
            allow_impl: true,
            flags: BTreeMap::new(),
        }
    }

    /// One invocation flag by key.
    pub fn flag(&self, key: &str) -> Option<&str> {
        self.flags.get(key).map(String::as_str)
    }

    /// Bounded continuity selected by canonical Claude CLI flags. Absence is a
    /// disabled config, preserving legacy one-shot behavior.
    pub fn claude_continuity(&self) -> ClaudeContinuity {
        ClaudeContinuity {
            enabled: self.flag("continuity-enabled") == Some("true"),
            max_resumed_turns: self
                .flag("continuity-max-resumed-turns")
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_CLAUDE_CONTINUITY_MAX_RESUMED_TURNS),
            ttl_secs: self
                .flag("continuity-ttl-secs")
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_CLAUDE_CONTINUITY_TTL_SECS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "RosterPreset::is_classic")]
    pub roster: RosterPreset,
    #[serde(default = "default_team_manager_provider")]
    pub manager_provider: String,
    #[serde(default = "default_team_manager_model")]
    pub manager_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_base_url: Option<String>,
    #[serde(default = "default_team_leader_order")]
    pub leader_order: Vec<String>,
    #[serde(default = "default_team_members")]
    pub members: Vec<TeamMember>,
    /// Difficulty ladder: tier name -> the lanes that serve it, most preferred
    /// first. Empty means "use the shipped default ladder" (see
    /// [`TeamConfig::effective_tiers`]); a non-empty table *replaces* the
    /// default outright rather than merging with it, so a user roster is never
    /// polluted by lanes they did not ask for.
    #[serde(default, skip_serializing_if = "TierMap::is_empty")]
    pub tiers: TierMap,
    /// How the leader walks the ladder: retries, sibling crossover, fallback
    /// depth, provenance.
    #[serde(default, skip_serializing_if = "TeamPolicy::is_default")]
    pub policy: TeamPolicy,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            roster: RosterPreset::Classic,
            manager_provider: default_team_manager_provider(),
            manager_model: default_team_manager_model(),
            manager_base_url: None,
            leader_order: default_team_leader_order(),
            members: default_team_members(),
            // Empty, not the default ladder: an unset `[team.tiers]` must not
            // be written back into anyone's config file. `effective_tiers`
            // supplies the default at read time instead.
            tiers: TierMap::default(),
            policy: TeamPolicy::default(),
        }
    }
}

impl TeamConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Build one shipped roster explicitly. The process-wide default is never
    /// inferred from environment or host, so existing configurations remain on
    /// [`RosterPreset::Classic`].
    pub fn preset(roster: RosterPreset) -> Self {
        match roster {
            RosterPreset::Classic => Self::default(),
            RosterPreset::OpencodeLead => opencode_lead_team(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_team_value("manager_provider", &self.manager_provider)?;
        validate_team_value("manager_model", &self.manager_model)?;
        if let Some(base_url) = &self.manager_base_url {
            validate_team_value("manager_base_url", base_url)?;
        }
        // This safety boundary applies even to disabled rosters: persisting an
        // OpenCode CLI lane must never become valid merely by toggling
        // `enabled` later.
        for (index, member) in self.members.iter().enumerate() {
            if member.delegation == Delegation::Shell && member.target != "claude" {
                return Err(Error::Config(format!(
                    "team.members[{index}].delegation = cli requires target = \"claude\"; \
                     refusing to execute CLI delegation for target {}",
                    member.target
                )));
            }
            if member.delegation == Delegation::Shell && member.permissions.is_some() {
                return Err(Error::Config(format!(
                    "team.members[{index}].permissions applies only to native delegation; \
                     Claude CLI permissions must use flags"
                )));
            }
            if member.delegation == Delegation::Shell {
                if !matches!(member.model.as_deref(), Some("opus" | "sonnet")) {
                    return Err(Error::Config(format!(
                        "team.members[{index}] Claude CLI model must be exactly opus or sonnet"
                    )));
                }
                validate_claude_cli_flags(index, &member.flags)?;
            }
            if let Some(permissions) = &member.permissions {
                for (pattern, _) in permissions.bash.iter() {
                    validate_bash_permission_pattern(index, pattern)?;
                }
            }
        }
        self.policy.recursion.validate()?;
        if !self.enabled {
            return Ok(());
        }
        if let Some((provider, _)) = self.manager_model.trim().split_once('/') {
            if !is_valid_opencode_model_id(self.manager_model.trim()) {
                return Err(Error::Config(format!(
                    "team.manager_model must be a valid nonempty provider/model ID: {}",
                    self.manager_model
                )));
            }
            if normalize_provider_id(provider) != normalize_provider_id(&self.manager_provider) {
                return Err(Error::Config(format!(
                    "team.manager_model provider prefix {provider} does not match \
                     team.manager_provider {}",
                    self.manager_provider
                )));
            }
        }
        if self.leader_order.is_empty() {
            return Err(Error::Config(
                "team.leader_order must not be empty when team is enabled".to_string(),
            ));
        }
        if self.members.is_empty() {
            return Err(Error::Config(
                "team.members must not be empty when team is enabled".to_string(),
            ));
        }

        let mut member_names = std::collections::BTreeSet::new();
        let mut member_targets = std::collections::BTreeSet::new();
        let mut native_host_agents = std::collections::BTreeSet::new();
        for (index, member) in self.members.iter().enumerate() {
            validate_team_value(&format!("members[{index}].name"), &member.name)?;
            validate_team_value(&format!("members[{index}].target"), &member.target)?;
            if let Some(model) = &member.model {
                validate_team_value(&format!("members[{index}].model"), model)?;
            }
            if let Some(host_agent) = &member.host_agent {
                validate_team_value(&format!("members[{index}].host_agent"), host_agent)?;
            }
            if member.delegation == Delegation::Native {
                if let Some(host_agent) = &member.host_agent {
                    if !is_valid_native_host_agent(host_agent) {
                        return Err(Error::Config(format!(
                            "team.members[{index}].host_agent must contain only ASCII letters, \
                             digits, '-' or '_': {host_agent}"
                        )));
                    }
                    if host_agent == "rtrt-manager" {
                        return Err(Error::Config(format!(
                            "team.members[{index}].host_agent uses reserved OpenCode agent name: \
                             {host_agent}"
                        )));
                    }
                    if matches!(host_agent.as_str(), "build" | "plan") {
                        return Err(Error::Config(format!(
                            "team.members[{index}].host_agent names primary-only OpenCode agent: \
                             {host_agent}"
                        )));
                    }
                    if matches!(host_agent.as_str(), "explore" | "general" | "scout")
                        && member.model.is_some()
                    {
                        return Err(Error::Config(format!(
                            "team.members[{index}].host_agent names built-in OpenCode subagent \
                             {host_agent} and must not set model"
                        )));
                    }
                    if !native_host_agents.insert(host_agent.as_str()) {
                        return Err(Error::Config(format!(
                            "duplicate native team host_agent at index {index}: {host_agent}"
                        )));
                    }
                }
                if member.target == "opencode"
                    && let Some(model) = &member.model
                    && !is_valid_opencode_model_id(model)
                {
                    return Err(Error::Config(format!(
                        "team.members[{index}].model must be a valid nonempty provider/model ID \
                         for native target opencode: {model}"
                    )));
                }
            }
            if member.roles.is_empty() {
                return Err(Error::Config(format!(
                    "team.members[{index}].roles must not be empty"
                )));
            }
            for (role_index, role) in member.roles.iter().enumerate() {
                validate_team_value(&format!("members[{index}].roles[{role_index}]"), role)?;
            }
            if !member_names.insert(member.name.as_str()) {
                return Err(Error::Config(format!(
                    "duplicate team member name at index {index}: {}",
                    member.name
                )));
            }
            if !member_targets.insert((
                member.target.as_str(),
                member.model.as_deref(),
                member.mode,
            )) {
                return Err(Error::Config(format!(
                    "duplicate team member at index {index}: target/model/mode must be unique"
                )));
            }
        }

        let mut leaders = std::collections::BTreeSet::new();
        for (index, leader) in self.leader_order.iter().enumerate() {
            validate_team_value(&format!("leader_order[{index}]"), leader)?;
            if !leaders.insert(leader.as_str()) {
                return Err(Error::Config(format!(
                    "duplicate team leader at index {index}: {leader}"
                )));
            }
            if !member_names.contains(leader.as_str()) {
                return Err(Error::Config(format!(
                    "team.leader_order[{index}] references unknown member: {leader}"
                )));
            }
        }

        self.validate_lane_links(&member_names)?;
        self.validate_tiers(&member_names)?;
        Ok(())
    }

    /// Cross-references between lanes: siblings must be the same logical model,
    /// fallbacks must resolve and must not loop.
    fn validate_lane_links(&self, member_names: &BTreeSet<&str>) -> Result<()> {
        for (index, member) in self.members.iter().enumerate() {
            if let Some(logical) = &member.logical {
                validate_team_value(&format!("members[{index}].logical"), logical)?;
            }
            if let Some(tier) = &member.tier {
                validate_team_value(&format!("members[{index}].tier"), tier)?;
            }
            for (key, value) in &member.flags {
                validate_team_value(&format!("members[{index}].flags key"), key)?;
                validate_team_text(&format!("members[{index}].flags.{key}"), value)?;
            }

            if let Some(sibling) = &member.sibling {
                validate_team_value(&format!("members[{index}].sibling"), sibling)?;
                if sibling == &member.name {
                    return Err(Error::Config(format!(
                        "team.members[{index}].sibling must not reference itself: {sibling}"
                    )));
                }
                let Some(other) = self.member(sibling) else {
                    return Err(Error::Config(format!(
                        "team.members[{index}].sibling references unknown member: {sibling}"
                    )));
                };
                if other.sibling.as_deref() != Some(member.name.as_str()) {
                    return Err(Error::Config(format!(
                        "team.members[{index}].sibling {sibling} must reciprocally reference {}",
                        member.name
                    )));
                }
                match (member.logical.as_deref(), other.logical.as_deref()) {
                    (Some(mine), Some(theirs)) if mine == theirs => {}
                    (Some(mine), Some(theirs)) => {
                        return Err(Error::Config(format!(
                            "team.members[{index}].sibling {sibling} serves logical model \
                             {theirs}, not {mine}: siblings must be the same model on \
                             different pools"
                        )));
                    }
                    _ => {
                        return Err(Error::Config(format!(
                            "team.members[{index}].sibling {sibling} requires both members to \
                             declare `logical`: a sibling pair is one model on two pools"
                        )));
                    }
                }
                let pool = PoolKey::from_target_model(&member.target, member.model.as_deref());
                let sibling_pool =
                    PoolKey::from_target_model(&other.target, other.model.as_deref());
                if pool == sibling_pool {
                    return Err(Error::Config(format!(
                        "team.members[{index}].sibling {sibling} resolves to the same backing pool \
                         {pool}: siblings must use distinct pools"
                    )));
                }
            }

            let mut seen_fallback = BTreeSet::new();
            for (position, name) in member.fallback.iter().enumerate() {
                validate_team_value(&format!("members[{index}].fallback[{position}]"), name)?;
                if name == &member.name {
                    return Err(Error::Config(format!(
                        "team.members[{index}].fallback[{position}] must not reference itself: \
                         {name}"
                    )));
                }
                if !member_names.contains(name.as_str()) {
                    return Err(Error::Config(format!(
                        "team.members[{index}].fallback[{position}] references unknown member: \
                         {name}"
                    )));
                }
                if !seen_fallback.insert(name.as_str()) {
                    return Err(Error::Config(format!(
                        "team.members[{index}].fallback lists {name} twice"
                    )));
                }
            }
        }

        if let Some(cycle) = fallback_cycle(&self.members) {
            return Err(Error::Config(format!(
                "team fallback chain forms a cycle: {}",
                cycle.join(" -> ")
            )));
        }
        Ok(())
    }

    /// The difficulty ladder: every explicitly configured tier must be usable,
    /// and no design-only lane may sit in a tier that implements.
    fn validate_tiers(&self, member_names: &BTreeSet<&str>) -> Result<()> {
        for (tier, lanes) in self.tiers.iter() {
            validate_team_value(&format!("tiers.{tier}"), tier)?;
            if lanes.is_empty() {
                return Err(Error::Config(format!(
                    "team.tiers.{tier} must list at least one member"
                )));
            }
            let mut seen = BTreeSet::new();
            for name in lanes {
                validate_team_value(&format!("tiers.{tier}"), name)?;
                if !member_names.contains(name.as_str()) {
                    return Err(Error::Config(format!(
                        "team.tiers.{tier} references unknown member: {name}"
                    )));
                }
                if !seen.insert(name.as_str()) {
                    return Err(Error::Config(format!(
                        "team.tiers.{tier} lists {name} twice"
                    )));
                }
            }
        }

        let effective = self.effective_tiers();
        if let Some(configured) = &self.policy.design_only_tiers {
            for tier in configured {
                validate_team_value("policy.design_only_tiers", tier)?;
                if !effective.contains(tier) {
                    return Err(Error::Config(format!(
                        "team.policy.design_only_tiers references unknown tier: {tier}"
                    )));
                }
            }
        }
        for (field, configured) in [
            ("default_tier", &self.policy.default_tier),
            ("explore_tier", &self.policy.explore_tier),
            ("review_tier", &self.policy.review_tier),
        ] {
            if let Some(tier) = configured {
                validate_team_value(&format!("policy.{field}"), tier)?;
                if !effective.contains(tier) {
                    return Err(Error::Config(format!(
                        "team.policy.{field} references unknown tier: {tier}"
                    )));
                }
            }
        }
        if self.policy.recursion.enabled {
            for tier in &self.policy.recursion.subleader_tiers {
                validate_team_value("policy.recursion.subleader_tiers", tier)?;
                if !effective.contains(tier) {
                    return Err(Error::Config(format!(
                        "team.policy.recursion.subleader_tiers references unknown tier: {tier}"
                    )));
                }
            }
        }

        let design_only = self.design_only_tier_names(&effective);
        for (tier, lanes) in effective.iter() {
            if design_only.contains(tier) {
                continue;
            }
            for name in lanes {
                let implements = self.member(name).is_none_or(|member| member.allow_impl);
                if !implements {
                    return Err(Error::Config(format!(
                        "team.tiers.{tier} places design-only member {name} in an implementation \
                         tier: set allow_impl = true or list {tier} under \
                         team.policy.design_only_tiers"
                    )));
                }
            }
        }
        Ok(())
    }

    /// One lane by name.
    pub fn member(&self, name: &str) -> Option<&TeamMember> {
        self.members.iter().find(|member| member.name == name)
    }

    /// The sibling lane of `name`, when it declares one that resolves.
    pub fn sibling_of(&self, name: &str) -> Option<&TeamMember> {
        let sibling = self.member(name)?.sibling.as_deref()?;
        self.member(sibling)
    }

    /// The difficulty ladder actually in force.
    ///
    /// A configured `[team.tiers]` replaces the shipped ladder outright. With
    /// none configured, the shipped ladder is used but filtered to lanes that
    /// exist in *this* roster, so a fully custom roster never inherits lane
    /// names it does not have. Either way, lanes that declare a
    /// [`TeamMember::tier`] are appended to that tier, creating it when the
    /// table does not.
    pub fn effective_tiers(&self) -> TierMap {
        let names: BTreeSet<&str> = self
            .members
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        let mut tiers = if self.tiers.is_empty() {
            let mut shipped = default_team_tiers();
            shipped.retain_members(|name| names.contains(name));
            shipped
        } else {
            self.tiers.clone()
        };
        for member in &self.members {
            if let Some(tier) = &member.tier {
                tiers.push_member(tier, &member.name);
            }
        }
        tiers
    }

    /// Tiers whose output is a plan, not an edit. Configured names win; with
    /// none configured the shipped name is used, but only if such a tier
    /// actually exists — a default must never invalidate a config.
    fn design_only_tier_names(&self, effective: &TierMap) -> BTreeSet<String> {
        match &self.policy.design_only_tiers {
            Some(configured) => configured.iter().cloned().collect(),
            None => default_design_only_tiers()
                .into_iter()
                .filter(|tier| effective.contains(tier))
                .collect(),
        }
    }

    /// Whether a tier is design-only (its lanes plan, they do not implement).
    pub fn is_design_only_tier(&self, tier: &str) -> bool {
        self.design_only_tier_names(&self.effective_tiers())
            .contains(tier)
    }

    /// The tier to start from when a task's difficulty is unclear: the
    /// configured one, else the first rung of the ladder.
    pub fn effective_default_tier(&self) -> Option<String> {
        if let Some(tier) = &self.policy.default_tier {
            return Some(tier.clone());
        }
        self.effective_tiers().first_name().map(str::to_string)
    }

    /// How many lanes deep a fallback walk may go. Derived from the roster —
    /// a walk can visit each lane at most once — unless pinned by the config.
    pub fn effective_max_fallback_depth(&self) -> usize {
        self.policy.max_fallback_depth.unwrap_or(self.members.len())
    }

    /// The fallback chain starting at `name`: every replacement it declares, in
    /// order, then their replacements, deduplicated and cut at
    /// [`TeamConfig::effective_max_fallback_depth`]. `name` itself is never in
    /// the result.
    ///
    /// Breadth-first on purpose — a lane's own preferences outrank the
    /// preferences of its replacement.
    pub fn fallback_chain(&self, name: &str) -> Vec<String> {
        let depth = self.effective_max_fallback_depth();
        let mut chain: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::from([name.to_string()]);
        let mut current = name.to_string();
        let mut cursor = 0usize;
        loop {
            if let Some(member) = self.member(&current) {
                for next in &member.fallback {
                    if chain.len() >= depth {
                        return chain;
                    }
                    if seen.insert(next.clone()) {
                        chain.push(next.clone());
                    }
                }
            }
            let Some(next) = chain.get(cursor) else {
                return chain;
            };
            current = next.clone();
            cursor += 1;
        }
    }
}

/// The first fallback cycle in the roster, as the looping path, or `None` when
/// the graph is acyclic. Iterative three-colour DFS: the roster is small, but a
/// cycle must never blow the stack of whoever loads a config.
fn fallback_cycle(members: &[TeamMember]) -> Option<Vec<String>> {
    const WHITE: u8 = 0;
    const GREY: u8 = 1;
    const BLACK: u8 = 2;

    let index: BTreeMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(position, member)| (member.name.as_str(), position))
        .collect();
    let mut colour = vec![WHITE; members.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for start in 0..members.len() {
        if colour[start] != WHITE {
            continue;
        }
        colour[start] = GREY;
        stack.push((start, 0));
        while let Some(&(node, cursor)) = stack.last() {
            let Some(next_name) = members[node].fallback.get(cursor) else {
                colour[node] = BLACK;
                stack.pop();
                continue;
            };
            if let Some(top) = stack.last_mut() {
                top.1 += 1;
            }
            // Unresolvable names are reported separately; skip them here so the
            // cycle report never blames a typo.
            let Some(&next) = index.get(next_name.as_str()) else {
                continue;
            };
            match colour[next] {
                GREY => {
                    let entry = stack
                        .iter()
                        .position(|(node, _)| *node == next)
                        .unwrap_or_default();
                    let mut cycle: Vec<String> = stack[entry..]
                        .iter()
                        .map(|(node, _)| members[*node].name.clone())
                        .collect();
                    cycle.push(members[next].name.clone());
                    return Some(cycle);
                }
                WHITE => {
                    colour[next] = GREY;
                    stack.push((next, 0));
                }
                _ => {}
            }
        }
    }
    None
}

/// An insertion-ordered map of tier name -> the lanes serving it.
///
/// Order is meaningful — it is the difficulty ladder the leader climbs — so
/// this preserves the order the config declares instead of sorting names the
/// way a `BTreeMap` would. Serializes as a plain TOML table, so
/// `[team.tiers]\nmechanical = ["glm-go"]` is all a user writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierMap(Vec<(String, Vec<String>)>);

impl TierMap {
    /// Build a ladder from ordered `(tier, lanes)` pairs. A repeated tier name
    /// extends the first occurrence rather than shadowing it.
    pub fn from_pairs<N, M>(pairs: impl IntoIterator<Item = (N, M)>) -> Self
    where
        N: Into<String>,
        M: IntoIterator,
        M::Item: Into<String>,
    {
        let mut map = Self::default();
        for (tier, lanes) in pairs {
            let tier = tier.into();
            for lane in lanes {
                map.push_member(&tier, &lane.into());
            }
        }
        map
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Tiers in ladder order, each with its lanes in preference order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.0
            .iter()
            .map(|(tier, lanes)| (tier.as_str(), lanes.as_slice()))
    }

    /// Tier names in ladder order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(tier, _)| tier.as_str())
    }

    /// The lanes serving one tier, most preferred first.
    pub fn get(&self, tier: &str) -> Option<&[String]> {
        self.0
            .iter()
            .find(|(name, _)| name == tier)
            .map(|(_, lanes)| lanes.as_slice())
    }

    pub fn contains(&self, tier: &str) -> bool {
        self.0.iter().any(|(name, _)| name == tier)
    }

    /// The first rung of the ladder.
    pub fn first_name(&self) -> Option<&str> {
        self.0.first().map(|(tier, _)| tier.as_str())
    }

    /// Append a lane to a tier, creating the tier at the end of the ladder when
    /// it is new. Re-adding a lane it already holds is a no-op, so ordering
    /// stays with the first declaration.
    pub fn push_member(&mut self, tier: &str, member: &str) {
        match self.0.iter_mut().find(|(name, _)| name == tier) {
            Some((_, lanes)) => {
                if !lanes.iter().any(|lane| lane == member) {
                    lanes.push(member.to_string());
                }
            }
            None => self.0.push((tier.to_string(), vec![member.to_string()])),
        }
    }

    /// Drop lanes that fail `keep`, then drop tiers left with none. Used to fit
    /// the shipped ladder to a roster that renamed or removed lanes.
    pub fn retain_members(&mut self, keep: impl Fn(&str) -> bool) {
        for (_, lanes) in &mut self.0 {
            lanes.retain(|lane| keep(lane));
        }
        self.0.retain(|(_, lanes)| !lanes.is_empty());
    }
}

impl Serialize for TierMap {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(tier, lanes)| (tier, lanes)))
    }
}

impl<'de> Deserialize<'de> for TierMap {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct TierMapVisitor;

        impl<'de> serde::de::Visitor<'de> for TierMapVisitor {
            type Value = TierMap;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a table of tier name to member names")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut access: M,
            ) -> std::result::Result<TierMap, M::Error> {
                let mut pairs: Vec<(String, Vec<String>)> = Vec::new();
                while let Some((tier, lanes)) = access.next_entry::<String, Vec<String>>()? {
                    if pairs.iter().any(|(existing, _)| *existing == tier) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate team tier: {tier}"
                        )));
                    }
                    pairs.push((tier, lanes));
                }
                Ok(TierMap(pairs))
            }
        }

        deserializer.deserialize_map(TierMapVisitor)
    }
}

/// How the leader climbs the ladder and recovers from failures.
///
/// The whole table is omitted from the serialized config while it equals the
/// defaults, so adding it never rewrites an existing `~/.rtrt/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamPolicy {
    /// Same-lane attempts a *transient* failure earns before the lane is
    /// abandoned for its sibling or fallback chain. `0` falls over on the first
    /// failure.
    #[serde(default = "default_team_max_retries")]
    pub max_retries: u32,
    /// Redo the delegated work from scratch on the replacement lane instead of
    /// resuming from whatever the failed lane produced. On by default because a
    /// lane that failed mid-task usually left partial edits.
    #[serde(default = "default_true")]
    pub redo_on_fallback: bool,
    /// On a *quota* failure, cross over to the lane's sibling pool before
    /// walking the fallback chain — a pool switch keeps the same model, a
    /// fallback usually does not.
    #[serde(default = "default_true")]
    pub prefer_sibling_on_quota: bool,
    /// Report which lane produced each delegated result.
    #[serde(default = "default_true")]
    pub record_provenance: bool,
    /// Hard cap on how many lanes deep a fallback walk may go. `None` derives
    /// it from the roster (a walk visits each lane at most once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fallback_depth: Option<usize>,
    /// Rung to start from when a task's difficulty is unclear. `None` uses the
    /// first tier of the effective ladder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tier: Option<String>,
    /// Tier reserved for codebase discovery through a dedicated host agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explore_tier: Option<String>,
    /// Tier used to review implementation produced by non-Claude lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_tier: Option<String>,
    /// Preserve configured order or choose equally suitable lanes by pool room.
    #[serde(default, skip_serializing_if = "Balance::is_order")]
    pub balance: Balance,
    /// Maximum number of summary lines returned by a worker to its lead.
    #[serde(
        default = "default_worker_summary_max_lines",
        skip_serializing_if = "is_default_worker_summary_max_lines"
    )]
    pub worker_summary_max_lines: u8,
    /// Put tasks with conflicting write sets in separate worktrees.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub isolate_conflicting: bool,
    /// Tiers whose lanes plan rather than implement; the only tiers a member
    /// with `allow_impl = false` may appear in. `None` uses the shipped name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_only_tiers: Option<Vec<String>>,
    /// Explicit opt-in and hard resource limits for recursive team leaders.
    #[serde(default, skip_serializing_if = "RecursionPolicy::is_default")]
    pub recursion: RecursionPolicy,
}

impl Default for TeamPolicy {
    fn default() -> Self {
        Self {
            max_retries: default_team_max_retries(),
            redo_on_fallback: true,
            prefer_sibling_on_quota: true,
            record_provenance: true,
            max_fallback_depth: None,
            default_tier: None,
            explore_tier: None,
            review_tier: None,
            balance: Balance::Order,
            worker_summary_max_lines: default_worker_summary_max_lines(),
            isolate_conflicting: true,
            design_only_tiers: None,
            recursion: RecursionPolicy::default(),
        }
    }
}

/// Bounded recursive delegation policy. Defaults preserve flat orchestration:
/// disabled with an effective depth of exactly zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecursionPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub max_depth: u8,
    #[serde(default = "default_recursion_fan_out")]
    pub max_fan_out: u16,
    #[serde(default = "default_recursion_total_nodes")]
    pub max_total_nodes: u32,
    #[serde(default = "default_recursion_token_budget")]
    pub max_tokens: u64,
    #[serde(default = "default_recursion_deadline_secs")]
    pub deadline_secs: u64,
    #[serde(default = "default_recursion_payload_bytes")]
    pub max_payload_bytes: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subleader_tiers: Vec<String>,
}

impl Default for RecursionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_depth: 0,
            max_fan_out: default_recursion_fan_out(),
            max_total_nodes: default_recursion_total_nodes(),
            max_tokens: default_recursion_token_budget(),
            deadline_secs: default_recursion_deadline_secs(),
            max_payload_bytes: default_recursion_payload_bytes(),
            subleader_tiers: Vec::new(),
        }
    }
}

impl RecursionPolicy {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub const fn effective_max_depth(&self) -> u8 {
        if self.enabled { self.max_depth } else { 0 }
    }

    pub const fn effective_max_fan_out(&self) -> u16 {
        if self.enabled { self.max_fan_out } else { 0 }
    }

    pub const fn effective_max_total_nodes(&self) -> u32 {
        if self.enabled {
            self.max_total_nodes
        } else {
            0
        }
    }

    pub const fn effective_max_tokens(&self) -> u64 {
        if self.enabled { self.max_tokens } else { 0 }
    }

    pub const fn effective_deadline_secs(&self) -> u64 {
        if self.enabled { self.deadline_secs } else { 0 }
    }

    pub const fn effective_max_payload_bytes(&self) -> u32 {
        if self.enabled {
            self.max_payload_bytes
        } else {
            0
        }
    }

    pub fn may_sublead(&self, tier: &str) -> bool {
        self.enabled && self.subleader_tiers.iter().any(|allowed| allowed == tier)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.enabled && self.max_depth != 0 {
            return Err(Error::Config(
                "team.policy.recursion.max_depth must be 0 when recursion is disabled".into(),
            ));
        }
        if self.enabled && !(1..=MAX_RECURSION_DEPTH).contains(&self.max_depth) {
            return Err(Error::Config(format!(
                "team.policy.recursion.max_depth must be between 1 and {MAX_RECURSION_DEPTH}"
            )));
        }
        bounded_recursion_value(
            "max_fan_out",
            self.max_fan_out as u64,
            MAX_RECURSION_FAN_OUT as u64,
        )?;
        bounded_recursion_value(
            "max_total_nodes",
            self.max_total_nodes as u64,
            MAX_RECURSION_TOTAL_NODES as u64,
        )?;
        bounded_recursion_value("max_tokens", self.max_tokens, MAX_RECURSION_TOKEN_BUDGET)?;
        bounded_recursion_value(
            "deadline_secs",
            self.deadline_secs,
            MAX_RECURSION_DEADLINE_SECS,
        )?;
        bounded_recursion_value(
            "max_payload_bytes",
            self.max_payload_bytes as u64,
            MAX_RECURSION_PAYLOAD_BYTES as u64,
        )?;
        if self.max_total_nodes < u32::from(self.max_fan_out).saturating_add(1) {
            return Err(Error::Config(
                "team.policy.recursion.max_total_nodes must accommodate root plus max_fan_out"
                    .into(),
            ));
        }
        let mut tiers = BTreeSet::new();
        for tier in &self.subleader_tiers {
            if !tiers.insert(tier) {
                return Err(Error::Config(format!(
                    "team.policy.recursion.subleader_tiers lists {tier} twice"
                )));
            }
        }
        Ok(())
    }
}

fn bounded_recursion_value(name: &str, value: u64, max: u64) -> Result<()> {
    if value == 0 || value > max {
        return Err(Error::Config(format!(
            "team.policy.recursion.{name} must be between 1 and {max}"
        )));
    }
    Ok(())
}

const fn default_recursion_fan_out() -> u16 {
    4
}
const fn default_recursion_total_nodes() -> u32 {
    32
}
const fn default_recursion_token_budget() -> u64 {
    1_000_000
}
const fn default_recursion_deadline_secs() -> u64 {
    3_600
}
const fn default_recursion_payload_bytes() -> u32 {
    65_536
}

impl TeamPolicy {
    /// True while nothing is customised, i.e. the policy is exactly the shipped
    /// one. Keeps an untouched `[team.policy]` out of the serialized config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn validate_team_value(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::Config(format!("team.{name} must not be empty")));
    }
    validate_team_text(name, value)
}

fn validate_claude_cli_flags(index: usize, flags: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in flags {
        let normalized = key.to_ascii_lowercase();
        match normalized.as_str() {
            "output-format" if value == "json" => {}
            "permission-mode" if matches!(value.as_str(), "plan" | "acceptEdits") => {}
            "permission-prompt-tool" if value == CLAUDE_PERMISSION_PROMPT_TOOL => {}
            "continuity-enabled" if matches!(value.as_str(), "true" | "false") => {}
            "continuity-max-resumed-turns" => {
                let parsed = value.parse::<u8>().map_err(|_| {
                    Error::Config(format!(
                        "team.members[{index}] --continuity-max-resumed-turns must be an integer"
                    ))
                })?;
                validate_claude_continuity(
                    index,
                    &ClaudeContinuity {
                        max_resumed_turns: parsed,
                        ..ClaudeContinuity::default()
                    },
                )?;
            }
            "continuity-ttl-secs" => {
                let parsed = value.parse::<u64>().map_err(|_| {
                    Error::Config(format!(
                        "team.members[{index}] --continuity-ttl-secs must be an integer"
                    ))
                })?;
                validate_claude_continuity(
                    index,
                    &ClaudeContinuity {
                        ttl_secs: parsed,
                        ..ClaudeContinuity::default()
                    },
                )?;
            }
            _ => {
                return Err(Error::Config(format!(
                    "team.members[{index}] Claude CLI delegation may only set canonical \
                     output-format=json, permission-mode=plan|acceptEdits, and \
                     permission-prompt-tool={CLAUDE_PERMISSION_PROMPT_TOOL}, plus bounded \
                     continuity fields; rejected --{key}"
                )));
            }
        }
    }
    for required in ["output-format", "permission-mode", "permission-prompt-tool"] {
        if !flags.contains_key(required) {
            return Err(Error::Config(format!(
                "team.members[{index}] Claude CLI delegation requires canonical --{required}"
            )));
        }
    }
    Ok(())
}

fn validate_claude_continuity(index: usize, value: &ClaudeContinuity) -> Result<()> {
    if !(1..=MAX_CLAUDE_CONTINUITY_RESUMED_TURNS).contains(&value.max_resumed_turns) {
        return Err(Error::Config(format!(
            "team.members[{index}].continuity.max_resumed_turns must be between 1 and \
             {MAX_CLAUDE_CONTINUITY_RESUMED_TURNS}"
        )));
    }
    if !(MIN_CLAUDE_CONTINUITY_TTL_SECS..=MAX_CLAUDE_CONTINUITY_TTL_SECS).contains(&value.ttl_secs)
    {
        return Err(Error::Config(format!(
            "team.members[{index}].continuity.ttl_secs must be between \
             {MIN_CLAUDE_CONTINUITY_TTL_SECS} and {MAX_CLAUDE_CONTINUITY_TTL_SECS}"
        )));
    }
    Ok(())
}

const fn default_claude_continuity_max_resumed_turns() -> u8 {
    DEFAULT_CLAUDE_CONTINUITY_MAX_RESUMED_TURNS
}

const fn default_claude_continuity_ttl_secs() -> u64 {
    DEFAULT_CLAUDE_CONTINUITY_TTL_SECS
}

fn is_valid_native_host_agent(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_valid_opencode_model_id(value: &str) -> bool {
    value.contains('/')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'-' | b'_' | b'.' | b':' | b'@' | b'+')
        })
        && value.split('/').all(|segment| {
            segment
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
        })
}

/// NUL check only — for values that may legitimately be empty, such as a
/// valueless invocation flag.
fn validate_team_text(name: &str, value: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(Error::Config(format!("team.{name} must not contain NUL")));
    }
    Ok(())
}

fn validate_bash_permission_pattern(member_index: usize, pattern: &str) -> Result<()> {
    let field = format!("team.members[{member_index}].permissions.bash pattern");
    if pattern.trim().is_empty() {
        return Err(Error::Config(format!("{field} must not be empty")));
    }
    if pattern.contains('\0') {
        return Err(Error::Config(format!("{field} must not contain NUL")));
    }
    if pattern.trim() == "*" {
        return Err(Error::Config(format!(
            "{field} must not be the catch-all `*`"
        )));
    }
    if pattern
        .chars()
        .any(|character| matches!(character, '\n' | '\r'))
    {
        return Err(Error::Config(format!("{field} must not contain newlines")));
    }
    if pattern.contains("$(") || pattern.contains('`') {
        return Err(Error::Config(format!(
            "{field} must not contain command substitution"
        )));
    }
    if pattern
        .chars()
        .any(|character| matches!(character, '|' | '&' | ';' | '<' | '>' | '(' | ')'))
    {
        return Err(Error::Config(format!(
            "{field} must not contain shell operators or redirections"
        )));
    }
    Ok(())
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_team_manager_provider() -> String {
    "ollama".to_string()
}

fn default_team_manager_model() -> String {
    "granite4:350m".to_string()
}

fn default_team_leader_order() -> Vec<String> {
    ["opus", "gpt-sol", "glm-go", "sonnet", "kimi-cloud"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn default_team_members() -> Vec<TeamMember> {
    vec![
        TeamMember {
            // Plans and integrates; the ladder keeps it out of every tier that
            // writes code.
            allow_impl: false,
            fallback: team_names(&["gpt-sol"]),
            ..team_member(
                "opus",
                "claude",
                "opus",
                "opus",
                &["lead", "architecture", "integration"],
            )
        },
        TeamMember {
            fallback: team_names(&["sonnet"]),
            ..team_member(
                "gpt-sol",
                "opencode",
                "openai/gpt-5.6-sol",
                "gpt-5.6-sol",
                &["deputy", "hard-implementation", "debugging"],
            )
        },
        TeamMember {
            // Same model as glm-cloud on a different pool: when this pool is
            // spent the work crosses over instead of dropping a model tier.
            sibling: Some("glm-cloud".to_string()),
            fallback: team_names(&["kimi-cloud"]),
            ..team_member(
                "glm-go",
                "opencode",
                "opencode-go/glm-5.2",
                "glm-5.2",
                &["routine", "boilerplate", "bulk-edit"],
            )
        },
        TeamMember {
            sibling: Some("glm-go".to_string()),
            fallback: team_names(&["kimi-cloud"]),
            ..team_member(
                "glm-cloud",
                "opencode",
                "ollama/glm-5.2:cloud",
                "glm-5.2",
                &["routine", "overflow", "bulk-edit"],
            )
        },
        TeamMember {
            fallback: team_names(&["kimi-cloud"]),
            ..team_member(
                "sonnet",
                "claude",
                "sonnet",
                "sonnet",
                &["general-implementation", "tests", "review"],
            )
        },
        // Last rung: the widest-quota lane, so its chain terminates here.
        team_member(
            "kimi-cloud",
            "opencode",
            "ollama/kimi-k2.7-code:cloud",
            "kimi-k2.7-code",
            &["parallel-implementation", "research", "tests"],
        ),
    ]
}

/// Opt-in roster for a live OpenCode lead. OpenCode-backed lanes stay native;
/// the two Claude lanes are the only members allowed to use CLI delegation.
fn opencode_lead_team() -> TeamConfig {
    let members = vec![
        TeamMember {
            delegation: Delegation::Shell,
            host_agent: Some("claude-opus".to_string()),
            allow_impl: false,
            flags: team_flags(&[
                ("permission-mode", "plan"),
                ("output-format", "json"),
                ("permission-prompt-tool", CLAUDE_PERMISSION_PROMPT_TOOL),
            ]),
            ..team_member(
                "opus",
                "claude",
                "opus",
                "opus",
                &[
                    "plan",
                    "architecture",
                    "task-breakdown",
                    "architecture-review",
                ],
            )
        },
        TeamMember {
            host_agent: Some("kimi-k3".to_string()),
            sibling: Some("kimi-k3-cloud".to_string()),
            fallback: team_names(&["codex-sol"]),
            ..team_member(
                "kimi-k3",
                "opencode",
                "opencode-go/kimi-k3",
                "kimi-k3",
                &[
                    "hard-implementation",
                    "multifile",
                    "refactoring",
                    "frontend",
                ],
            )
        },
        TeamMember {
            delegation: Delegation::Native,
            host_agent: Some("kimi-k3-cloud".to_string()),
            sibling: Some("kimi-k3".to_string()),
            fallback: team_names(&["codex-sol"]),
            ..team_member(
                "kimi-k3-cloud",
                "opencode",
                "ollama/kimi-k3:cloud",
                "kimi-k3",
                &[
                    "hard-implementation",
                    "multifile",
                    "refactoring",
                    "frontend",
                ],
            )
        },
        TeamMember {
            host_agent: Some("codex-sol-worker".to_string()),
            fallback: team_names(&["sonnet"]),
            ..team_member(
                "codex-sol",
                "opencode",
                "openai/gpt-5.6-sol",
                "gpt-5.6-sol",
                &[
                    "hard-implementation",
                    "debugging",
                    "systems",
                    "architecture-aware",
                ],
            )
        },
        TeamMember {
            host_agent: Some("codex-luna".to_string()),
            fallback: team_names(&["kimi-k3", "codex-sol"]),
            ..team_member(
                "codex-luna",
                "opencode",
                "openai/gpt-5.6-luna",
                "gpt-5.6-luna",
                &["routine", "tests", "docs"],
            )
        },
        TeamMember {
            host_agent: Some("glm".to_string()),
            sibling: Some("glm-cloud".to_string()),
            fallback: team_names(&["kimi"]),
            ..team_member(
                "glm",
                "opencode",
                "opencode-go/glm-5.2",
                "glm-5.2",
                &["simple", "mechanical", "boilerplate", "bulk-edit"],
            )
        },
        TeamMember {
            delegation: Delegation::Native,
            host_agent: Some("glm-cloud".to_string()),
            sibling: Some("glm".to_string()),
            fallback: team_names(&["kimi-cloud"]),
            ..team_member(
                "glm-cloud",
                "opencode",
                "ollama/glm-5.2:cloud",
                "glm-5.2",
                &["simple", "mechanical", "boilerplate", "bulk-edit"],
            )
        },
        TeamMember {
            host_agent: Some("kimi".to_string()),
            sibling: Some("kimi-cloud".to_string()),
            fallback: team_names(&["codex-luna"]),
            ..team_member(
                "kimi",
                "opencode",
                "opencode-go/kimi-k2.7-code",
                "kimi-k2.7-code",
                &["simple", "mechanical", "boilerplate", "single-file"],
            )
        },
        TeamMember {
            delegation: Delegation::Native,
            host_agent: Some("kimi-cloud".to_string()),
            sibling: Some("kimi".to_string()),
            fallback: team_names(&["codex-luna"]),
            ..team_member(
                "kimi-cloud",
                "opencode",
                "ollama/kimi-k2.7-code:cloud",
                "kimi-k2.7-code",
                &["simple", "mechanical", "boilerplate", "single-file"],
            )
        },
        TeamMember {
            host_agent: Some("explore".to_string()),
            roles: team_names(&["discovery"]),
            ..TeamMember::new("explore", "opencode", TeamMode::Cli)
        },
        TeamMember {
            delegation: Delegation::Shell,
            host_agent: Some("claude-sonnet".to_string()),
            flags: team_flags(&[
                ("permission-mode", "acceptEdits"),
                ("output-format", "json"),
                ("permission-prompt-tool", CLAUDE_PERMISSION_PROMPT_TOOL),
            ]),
            ..team_member(
                "sonnet",
                "claude",
                "sonnet",
                "sonnet",
                &["review", "consistency"],
            )
        },
    ];

    TeamConfig {
        enabled: true,
        roster: RosterPreset::OpencodeLead,
        manager_provider: "openai".to_string(),
        manager_model: "gpt-5.6-sol".to_string(),
        leader_order: team_names(&["codex-sol", "sonnet", "kimi-k3-cloud"]),
        members,
        tiers: TierMap::from_pairs([
            ("simple", vec!["glm", "glm-cloud", "kimi", "kimi-cloud"]),
            ("routine", vec!["codex-luna"]),
            ("hard", vec!["codex-sol", "kimi-k3-cloud", "kimi-k3"]),
            ("plan", vec!["opus"]),
            ("review", vec!["sonnet"]),
            ("explore", vec!["explore"]),
        ]),
        policy: TeamPolicy {
            default_tier: Some("hard".to_string()),
            explore_tier: Some("explore".to_string()),
            review_tier: Some("review".to_string()),
            balance: Balance::Room,
            design_only_tiers: Some(team_names(&["plan"])),
            ..TeamPolicy::default()
        },
        ..TeamConfig::default()
    }
}

/// The shipped difficulty ladder, expressed over [`default_team_members`].
///
/// Only a default: a `[team.tiers]` table replaces it wholesale, and a roster
/// that renames these lanes drops the ones it no longer has (see
/// [`TeamConfig::effective_tiers`]).
fn default_team_tiers() -> TierMap {
    TierMap::from_pairs([
        (TIER_MECHANICAL, vec!["glm-go", "glm-cloud"]),
        (TIER_ROUTINE, vec!["kimi-cloud", "glm-cloud"]),
        (TIER_MULTIFILE, vec!["gpt-sol", "kimi-cloud"]),
        (TIER_DESIGN, vec!["opus", "gpt-sol"]),
        (TIER_REVIEW, vec!["sonnet", "gpt-sol"]),
    ])
}

/// Mechanical edits: renames, moves, formatting — cheapest lanes first.
const TIER_MECHANICAL: &str = "mechanical";
/// Routine single-file work with a clear spec.
const TIER_ROUTINE: &str = "routine";
/// Changes spanning several files that have to stay consistent.
const TIER_MULTIFILE: &str = "multifile";
/// Architecture and API shape: a plan, not an edit.
const TIER_DESIGN: &str = "design";
/// Reading someone else's diff for defects.
const TIER_REVIEW: &str = "review";

fn default_design_only_tiers() -> Vec<String> {
    vec![TIER_DESIGN.to_string()]
}

/// Same-lane attempts a transient failure earns before the leader gives up on
/// the lane. Overridable via `[team.policy] max_retries`.
pub const DEFAULT_TEAM_MAX_RETRIES: u32 = 2;

fn default_team_max_retries() -> u32 {
    DEFAULT_TEAM_MAX_RETRIES
}

fn default_worker_summary_max_lines() -> u8 {
    3
}

fn is_default_worker_summary_max_lines(value: &u8) -> bool {
    *value == default_worker_summary_max_lines()
}

fn team_member(name: &str, target: &str, model: &str, logical: &str, roles: &[&str]) -> TeamMember {
    TeamMember {
        model: Some(model.to_string()),
        logical: Some(logical.to_string()),
        roles: team_names(roles),
        ..TeamMember::new(name, target, TeamMode::Cli)
    }
}

fn team_names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn team_flags(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// Global security defaults applied before any per-project binding. A project
/// without its own `security_profile` (and any ad-hoc scan) falls back to
/// `default_profile`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Profile name used when a project has no bound profile. Defaults to
    /// `ai-default`.
    #[serde(default = "default_security_profile")]
    pub default_profile: String,
}

fn default_security_profile() -> String {
    "ai-default".to_string()
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            default_profile: default_security_profile(),
        }
    }
}

/// A registered project. Either a real repo on disk (`path` set) or a
/// memory-only project (`path = None`). `security_profile` binds the project
/// to a named profile; `None` means fall back to `ai-default`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    /// Absolute repo path; `None` = memory-only project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Bound profile name; `None` = use ai-default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
    /// Per-project embedding override: `Some(true)`/`Some(false)` forces the
    /// semantic (vector) memory map on/off for this project; `None` inherits the
    /// global `[embeddings] enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_enabled: Option<bool>,
}

/// Per-project customization overrides, layered on top of the global config.
/// Stored at `<repo>/.rtrt/config.toml`. Only the customization layer is
/// overridable here — the base kernel (hooks / MCP / statusLine command
/// binding) stays global and immutable except via `rtrt setup`. Every field is
/// optional: an absent field inherits the global default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Terse output level override: `off` | `lite` | `full` | `ultra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_level: Option<String>,
    /// Output-compression override (level + enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionConfig>,
    /// Per-project agent enable/disable overlay (merged over global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsConfig>,
    /// Per-project provider enable/disable + active overlay (merged over global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<ProvidersConfig>,
    /// Opaque statusline override; shape owned by the dashboard schema so the
    /// core does not need to know it. Stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<toml::Value>,
    /// Per-project orchestration roster (`[team]`).
    ///
    /// **Granularity: whole-section replacement, never a field-level merge.**
    /// When this is `Some`, it *replaces* the global `[team]` outright; keys the
    /// project omits fall back to the schema default, not to the global value.
    ///
    /// The reason is [`TeamConfig::validate`]. A roster is a web of
    /// cross-references — `leader_order` and `tiers.*` name lanes, lanes name a
    /// `sibling` and a `fallback` chain, `policy` names tiers — so merging two
    /// rosters field by field can synthesise a config neither side wrote: a
    /// global tier naming a lane the project removed, a global leader that no
    /// longer exists, a sibling pair split across layers. Replacement keeps the
    /// validator meaningful, because the effective roster is then *exactly* the
    /// section that was validated (see [`ProjectConfig::validate`]) — there is
    /// no third, unvalidated combination to reason about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<TeamConfig>,
    /// Per-project invocation failure policy (`[failover]`). Whole-section
    /// replacement, for the same reason as `team`: the three marker classes are
    /// consulted in priority order, so appending a project list to a global one
    /// would silently reclassify markers the project never mentioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover: Option<FailoverConfig>,
}

impl ProjectConfig {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let over: Self =
            toml::from_str(s).map_err(|e| Error::Config(format!("project config TOML: {e}")))?;
        over.validate()?;
        Ok(over)
    }

    /// Reject an override that could not be a valid effective config.
    ///
    /// Mirrors [`Config::from_toml_str`], which validates the global `[team]`
    /// at parse time. Because a project `[team]` *replaces* the global section
    /// rather than merging into it, the effective roster is byte-for-byte this
    /// override — so validating it here validates the effective config, and no
    /// caller of [`Config::load_effective`] can be handed a roster that
    /// [`TeamConfig::validate`] would reject. Errors carry the validator's own
    /// message.
    pub fn validate(&self) -> Result<()> {
        if let Some(team) = &self.team {
            team.validate()?;
        }
        Ok(())
    }

    /// The per-project statusline override serialized as a `[statusline]` TOML
    /// section, if the project set one (the "Custom" mode). `None` means the
    /// project follows the global statusline (the default). Returned as text so
    /// callers can reuse their existing `[statusline]` parser without depending
    /// on the `toml` crate.
    pub fn statusline_section_toml(&self) -> Option<String> {
        let value = self.statusline.as_ref()?;
        let body = toml::to_string(value).ok()?;
        Some(format!("[statusline]\n{body}"))
    }

    /// True when no override is set — used to delete the file and keep the repo
    /// clean rather than leave an empty `.rtrt/config.toml`.
    pub fn is_empty(&self) -> bool {
        self.output_level.is_none()
            && self.compression.is_none()
            && self.agents.as_ref().is_none_or(|a| a.enabled.is_empty())
            && self.providers.as_ref().is_none_or(|p| {
                p.enabled.is_empty() && p.active.is_none() && p.api_max_tokens.is_none()
            })
            && self.statusline.is_none()
            // `team` / `failover` are REPLACEMENT overrides, so — unlike the
            // `agents` / `providers` overlays above — an "all default" section
            // is not a no-op: it pins the shipped roster against a divergent
            // global. Only an absent section counts as no override, exactly as
            // for `compression` and `statusline`.
            && self.team.is_none()
            && self.failover.is_none()
    }
}

/// Dense-embedding knobs. When `enabled = true`, the dashboard and CLI route
/// `/api/memory/recall mode=hybrid` through a real OllamaEmbedder instead of
/// the graph-blend BM25 path. The embedder uses `model` served at `base_url`.
///
/// Resolution order (highest priority first):
///   `RTRT_EMBED_ENABLED` / `RTRT_EMBED_MODEL` / `RTRT_EMBED_BASE_URL`
///   → `[embeddings]` in `~/.rtrt/config.toml`
///   → built-in defaults (disabled, bge-m3, 127.0.0.1:11434)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    /// Enable dense-vector paths. Off by default so the binary builds and runs
    /// without an Ollama instance.
    #[serde(default)]
    pub enabled: bool,
    /// Ollama model to use for embeddings (default: `bge-m3`, 1024-dim).
    #[serde(default = "default_embed_model_ollama")]
    pub model: String,
    /// Ollama base URL. `None` falls back to `auto_compress.base_url`, then
    /// `http://127.0.0.1:11434`. A trailing `/v1` is stripped so the same URL
    /// can serve both the OpenAI-compat chat path and the embeddings path.
    #[serde(default)]
    pub base_url: Option<String>,
    /// When embeddings are enabled, also run the background auto-embed daemon
    /// that incrementally embeds newly captured rows (so the automatic recall
    /// loop can go hybrid without a manual backfill). Defaults to `true`; set
    /// `false` to keep embeddings enabled for manual/on-demand paths only.
    #[serde(default = "default_true")]
    pub auto: bool,
    /// Seconds between auto-embed daemon sweeps. Defaults to 120.
    #[serde(default = "default_auto_embed_interval")]
    pub auto_interval_sec: u64,
    /// Max rows embedded per auto-embed sweep (one batched, capped pass per
    /// cycle so capture is never blocked). Defaults to 64.
    #[serde(default = "default_auto_embed_batch")]
    pub auto_batch: usize,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_embed_model_ollama(),
            base_url: None,
            auto: true,
            auto_interval_sec: default_auto_embed_interval(),
            auto_batch: default_auto_embed_batch(),
        }
    }
}

fn default_embed_model_ollama() -> String {
    "bge-m3".to_string()
}

fn default_auto_embed_interval() -> u64 {
    120
}

fn default_auto_embed_batch() -> usize {
    64
}

impl EmbeddingsConfig {
    /// Resolve the effective base URL. Priority: `RTRT_EMBED_BASE_URL` env var
    /// → `self.base_url` → `compress_base_url` fallback → Ollama default.
    pub fn resolved_base_url(&self, compress_base_url: Option<&str>) -> String {
        if let Ok(url) = std::env::var("RTRT_EMBED_BASE_URL") {
            if !url.is_empty() {
                return url;
            }
        }
        if let Some(url) = &self.base_url {
            if !url.is_empty() {
                return url.clone();
            }
        }
        if let Some(url) = compress_base_url {
            if !url.is_empty() {
                return url.to_string();
            }
        }
        "http://127.0.0.1:11434".to_string()
    }

    /// Whether embeddings are enabled, honouring the `RTRT_EMBED_ENABLED` env
    /// var first.
    pub fn is_enabled(&self) -> bool {
        match std::env::var("RTRT_EMBED_ENABLED").as_deref() {
            Ok("0") | Ok("false") | Ok("no") => false,
            Ok(v) if !v.is_empty() => true,
            _ => self.enabled,
        }
    }

    /// Effective model name, honouring `RTRT_EMBED_MODEL` env var first.
    pub fn effective_model(&self) -> String {
        std::env::var("RTRT_EMBED_MODEL").unwrap_or_else(|_| self.model.clone())
    }
}

/// Auto-capture pipeline knobs. Mirror the `RTRT_AUTO_*` env vars; env
/// always wins over the file so a one-off `RTRT_AUTO_CAPTURE=0 rtrt …`
/// still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub redact: bool,
    #[serde(default = "default_dedup_window")]
    pub dedup_window_sec: i64,
    #[serde(default)]
    pub project: Option<String>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            redact: true,
            dedup_window_sec: default_dedup_window(),
            project: None,
        }
    }
}

fn default_dedup_window() -> i64 {
    300
}

/// LLM auto-compress knobs (SessionEnd hook + dashboard daemon). Mirror the
/// `RTRT_AUTO_COMPRESS_*` env vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoCompressConfig {
    /// Off by default; set true (or `RTRT_AUTO_COMPRESS_LLM=1`) to enable.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_compress_model")]
    pub model: String,
    /// OpenAI-compatible base URL (e.g. a local Ollama endpoint).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Runtime/provider identity behind `base_url`. This is deliberately
    /// separate from its OpenAI-compatible wire protocol.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default = "default_compress_interval")]
    pub interval_sec: u64,
    #[serde(default = "default_compress_age")]
    pub age_sec: i64,
    #[serde(default = "default_compress_min_chars")]
    pub min_chars: usize,
    #[serde(default = "default_compress_batch")]
    pub batch: usize,
    #[serde(default = "default_compress_max_tokens")]
    pub max_tokens: u32,
}

impl Default for AutoCompressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_compress_model(),
            base_url: None,
            provider: None,
            interval_sec: default_compress_interval(),
            age_sec: default_compress_age(),
            min_chars: default_compress_min_chars(),
            batch: default_compress_batch(),
            max_tokens: default_compress_max_tokens(),
        }
    }
}

impl AutoCompressConfig {
    /// Identity for an OpenAI-compatible endpoint. Explicit env/config values
    /// win. Only Ollama's shipped loopback URL is recognised implicitly;
    /// arbitrary compatible endpoints retain the neutral identity.
    pub fn effective_provider(&self, base_url: &str) -> String {
        let provider = std::env::var("RTRT_OPENAI_COMPAT_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.provider
                    .clone()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| {
                if is_default_ollama_compatible_url(base_url) {
                    "ollama".to_string()
                } else {
                    "openai-compat".to_string()
                }
            });
        normalize_provider_id(&provider)
    }
}

/// Canonical provider/runtime identity used at configuration and routing
/// boundaries. Provider names are case-insensitive. These aliases are limited
/// to explicit spellings of the same runtime; transport names never become a
/// provider and collapse to the neutral compatible-endpoint identity.
pub fn normalize_provider_id(provider: &str) -> String {
    let normalized = provider.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "openai-compatible" | "openai_compatible" | "openai-compat" => "openai-compat".to_string(),
        "lmstudio" | "lm_studio" | "lms" | "lm-studio" => "lm-studio".to_string(),
        "llama" | "llamacpp" | "llama_cpp" | "llama-cpp" | "llama.cpp" => "llama.cpp".to_string(),
        _ => normalized,
    }
}

/// Whether `url` is the known default Ollama loopback endpoint. Do not extend
/// this into runtime fingerprinting: compatible servers must identify
/// themselves explicitly.
pub fn is_default_ollama_compatible_url(url: &str) -> bool {
    matches!(
        url.trim().trim_end_matches('/'),
        "http://127.0.0.1:11434"
            | "http://127.0.0.1:11434/v1"
            | "http://localhost:11434"
            | "http://localhost:11434/v1"
            | "http://[::1]:11434"
            | "http://[::1]:11434/v1"
    )
}

fn default_compress_model() -> String {
    "claude-haiku-4-5".to_string()
}
fn default_compress_interval() -> u64 {
    1800
}
fn default_compress_age() -> i64 {
    3600
}
fn default_compress_min_chars() -> usize {
    // Default to "compress everything": every row is attempted once. The
    // no-shrink guard tags rows the model can't shrink with
    // `compressed_skip=no-shrink` (and `compressed_at`), so they are
    // excluded from future sweeps — each row costs at most one LLM call
    // over its lifetime. Raise this if you want to spend calls only on
    // longer rows (the bench shows ~1000+ chars is where big savings are).
    1
}
fn default_compress_batch() -> usize {
    20
}
fn default_compress_max_tokens() -> u32 {
    512
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    #[serde(default)]
    pub level: CompressionLevel,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            level: CompressionLevel::default(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_path")]
    pub path: PathBuf,
    #[serde(default = "default_embed_model")]
    pub embed_model: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            path: default_memory_path(),
            embed_model: default_embed_model(),
        }
    }
}

fn default_memory_path() -> PathBuf {
    default_memory_store_path()
}

/// Explicit legacy/admin memory store: `~/.rtrt/memory.sqlite`.
///
/// Retained for backward-compatible configuration and explicit migration
/// workflows. New project-scoped callers must not use this default.
pub fn default_memory_store_path() -> PathBuf {
    legacy_memory_store_path()
}

/// Explicit legacy/admin path. Strict project callers must instead use
/// [`crate::project_memory_db_path`].
pub fn legacy_memory_store_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rtrt")
        .join("memory.sqlite")
}

fn default_embed_model() -> String {
    "all-MiniLM-L6-v2".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    #[serde(default = "default_dashboard_addr")]
    pub bind: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            bind: default_dashboard_addr(),
        }
    }
}

fn default_dashboard_addr() -> String {
    "127.0.0.1:7311".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(flatten)]
    pub enabled: BTreeMap<String, bool>,
}

impl AgentsConfig {
    pub fn enabled_override(&self, name: &str) -> Option<bool> {
        self.enabled.get(name).copied()
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(name.to_string(), enabled);
    }
}

/// Default output-token ceiling for routed API-mode invocations when neither
/// `[providers] api_max_tokens` nor `RTRT_API_MAX_TOKENS` is set.
pub const DEFAULT_API_MAX_TOKENS: u32 = 4096;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub active: Option<String>,
    /// Max output tokens for routed API-mode invocations (`rtrt route` /
    /// `rtrt call --mode api`, MCP `agent_call` / `agent_route`). `None`
    /// falls back to [`DEFAULT_API_MAX_TOKENS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_max_tokens: Option<u32>,
    #[serde(flatten)]
    pub enabled: BTreeMap<String, bool>,
}

impl ProvidersConfig {
    pub fn enabled_override(&self, name: &str) -> Option<bool> {
        let normalized = normalize_provider_id(name);
        self.enabled
            .iter()
            .find(|(configured, _)| normalize_provider_id(configured) == normalized)
            .map(|(_, enabled)| *enabled)
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(normalize_provider_id(name), enabled);
    }

    /// Effective output-token ceiling for API-mode invocations. Resolution
    /// order mirrors the other provider knobs: `RTRT_API_MAX_TOKENS` env var
    /// → `[providers] api_max_tokens` → [`DEFAULT_API_MAX_TOKENS`]. Zero and
    /// unparseable values are ignored so a typo never truncates answers to 0.
    pub fn effective_api_max_tokens(&self) -> u32 {
        if let Ok(raw) = std::env::var("RTRT_API_MAX_TOKENS")
            && let Ok(value) = raw.trim().parse::<u32>()
            && value > 0
        {
            return value;
        }
        self.api_max_tokens
            .filter(|&value| value > 0)
            .unwrap_or(DEFAULT_API_MAX_TOKENS)
    }
}

/// Optional daily usage ceilings by provider or target name.
///
/// Example `~/.rtrt/config.toml`:
///
/// ```toml
/// [limits.openai]
/// daily_tokens = 1_000_000
/// daily_requests = 2_000
///
/// [limits.ollama]
/// daily_tokens = 250_000
/// ```
///
/// One target often fronts several upstream quotas (see [`crate::pool`]). Those
/// pools can be capped individually, nested under the target they belong to:
///
/// ```toml
/// [limits.opencode]
/// daily_tokens = 2_000_000        # still the target-wide cap
///
/// [limits.opencode.pools.opencode-go]
/// daily_tokens = 1_200_000
///
/// [limits.opencode.pools.ollama]
/// daily_requests = 500
/// ```
///
/// Pool caps are strictly optional: a target with none behaves exactly as it
/// always has, and its pools share the target-wide cap rather than each being
/// given a synthesised slice of it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(flatten)]
    pub targets: BTreeMap<String, TargetLimit>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_requests: Option<u64>,
    /// Per-pool ceilings inside this target (`[limits.<target>.pools.<pool>]`).
    /// Absent for every config written before pool identity existed, and
    /// skipped on serialize when empty, so those configs round-trip byte for
    /// byte.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pools: BTreeMap<String, PoolLimit>,
}

/// Daily ceilings for one upstream pool inside a target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_requests: Option<u64>,
}

impl TargetLimit {
    /// The cap configured for one pool inside this target, if any. Matched
    /// exactly first, then case-insensitively, because pool names derived from
    /// model strings are lowercased.
    pub fn pool(&self, name: &str) -> Option<&PoolLimit> {
        if let Some(limit) = self.pools.get(name) {
            return Some(limit);
        }
        self.pools
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, limit)| limit)
    }

    /// True when at least one pool inside this target carries its own cap.
    pub fn has_pool_limits(&self) -> bool {
        self.pools.values().any(PoolLimit::is_set)
    }
}

impl PoolLimit {
    /// True when this pool pins at least one axis.
    pub fn is_set(&self) -> bool {
        self.daily_tokens.is_some() || self.daily_requests.is_some()
    }
}

impl LimitsConfig {
    pub fn target(&self, name: &str) -> Option<&TargetLimit> {
        self.targets.get(name).or_else(|| {
            self.targets
                .iter()
                .find(|(target, _)| target.eq_ignore_ascii_case(name))
                .map(|(_, limit)| limit)
        })
    }

    /// The cap for one pool inside a target, if the config pins one.
    pub fn pool(&self, target: &str, pool: &str) -> Option<&PoolLimit> {
        self.target(target)?.pool(pool)
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(s).map_err(|e| Error::Config(format!("config TOML: {e}")))?;
        config.team.validate()?;
        Ok(config)
    }

    /// Resolve the config file path: `$RTRT_CONFIG` if set, else
    /// `~/.rtrt/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("RTRT_CONFIG") {
            return Some(PathBuf::from(p));
        }
        dirs::home_dir().map(|h| h.join(".rtrt").join("config.toml"))
    }

    /// Load from the default path. Returns `Config::default()` when the file
    /// is absent; surfaces an error only on a malformed file so a typo
    /// doesn't silently fall back to defaults.
    pub fn load() -> Result<Self> {
        match Self::default_path() {
            Some(p) if p.exists() => {
                let raw = read_bounded_config(&p, false)?;
                Self::from_toml_str(&raw)
            }
            _ => Ok(Self::default()),
        }
    }

    /// Look up a registered project by name.
    pub fn project(&self, name: &str) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Insert or replace a project entry, matching on `name`.
    pub fn upsert_project(&mut self, entry: ProjectEntry) {
        if let Some(existing) = self.projects.iter_mut().find(|p| p.name == entry.name) {
            *existing = entry;
        } else {
            self.projects.push(entry);
        }
    }

    pub fn set_agent_enabled(&mut self, name: &str, enabled: bool) {
        self.agents.set_enabled(name, enabled);
    }

    pub fn set_provider_enabled(&mut self, name: &str, enabled: bool) {
        self.providers.set_enabled(name, enabled);
    }

    pub fn set_tool_enabled(&mut self, name: &str, enabled: bool) {
        if self.providers.enabled.contains_key(name) {
            self.set_provider_enabled(name, enabled);
        } else {
            self.set_agent_enabled(name, enabled);
        }
    }

    /// Per-project override file: `<repo>/.rtrt/config.toml`.
    pub fn project_config_path(repo: &Path) -> PathBuf {
        repo.join(".rtrt").join("config.toml")
    }

    /// Load a project's override file if present (empty default otherwise).
    pub fn load_project(repo: &Path) -> Result<ProjectConfig> {
        let root = canonical_repo_root(repo)?;
        let Some(config_dir) = existing_project_config_dir(&root)? else {
            return Ok(ProjectConfig::default());
        };
        let path = config_dir.join("config.toml");
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                validate_project_target(&root, &path, &metadata)?;
                let raw = read_bounded_config(&path, true)?;
                ProjectConfig::from_toml_str(&raw)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProjectConfig::default())
            }
            Err(error) => Err(config_error("inspect", &path, error)),
        }
    }

    /// Load the global config and overlay a project's customization overrides.
    /// The base kernel is never overlaid — only the customization layer
    /// (output level, compression, enabled agents/providers, orchestration).
    ///
    /// The result always satisfies [`TeamConfig::validate`]: the global roster
    /// is validated by [`Config::from_toml_str`], a project roster by
    /// [`ProjectConfig::from_toml_str`], and the overlay picks one of the two
    /// whole rather than blending them.
    pub fn load_effective(repo: Option<&Path>) -> Result<Self> {
        let mut base = Self::load()?;
        if let Some(repo) = repo {
            let over = Self::load_project(repo)?;
            base.apply_project_overrides(&over);
        }
        Ok(base)
    }

    /// The config effective for the current working directory: the global
    /// config overlaid with the enclosing repo's `.rtrt/config.toml` when the
    /// cwd is inside a repo, else the plain global config. Errors fall back to
    /// the default config so a malformed per-project file never breaks the
    /// caller (routing, MCP tool dispatch, hooks).
    pub fn load_effective_for_cwd() -> Self {
        let repo = std::env::current_dir()
            .ok()
            .and_then(|cwd| repo_root_from(&cwd));
        Self::load_effective(repo.as_deref()).unwrap_or_default()
    }

    /// Overlay one project's customization overrides onto this config.
    pub fn apply_project_overrides(&mut self, over: &ProjectConfig) {
        if let Some(compression) = &over.compression {
            self.compression = compression.clone();
        }
        if let Some(agents) = &over.agents {
            for (name, enabled) in &agents.enabled {
                self.agents.enabled.insert(name.clone(), *enabled);
            }
        }
        if let Some(providers) = &over.providers {
            for (name, enabled) in &providers.enabled {
                self.providers.enabled.insert(name.clone(), *enabled);
            }
            if providers.active.is_some() {
                self.providers.active = providers.active.clone();
            }
            if providers.api_max_tokens.is_some() {
                self.providers.api_max_tokens = providers.api_max_tokens;
            }
        }
        // Whole-section replacement — see `ProjectConfig::team`. The overlay
        // takes one validated roster or the other, never a blend of both.
        if let Some(team) = &over.team {
            self.team = team.clone();
        }
        if let Some(failover) = &over.failover {
            self.failover = failover.clone();
        }
    }

    /// Write a project override file, creating `.rtrt/` as needed. When the
    /// override is empty the file is removed so the repo stays clean.
    ///
    /// An override that would make the effective config invalid is rejected
    /// here — before anything is written — with the validator's own message, so
    /// no writer (dashboard, CLI, future callers) can persist a broken roster.
    pub fn save_project(repo: &Path, over: &ProjectConfig) -> Result<()> {
        over.validate()?;
        let root = canonical_repo_root(repo)?;
        let config_dir = prepare_project_config_dir(&root, !over.is_empty())?;
        let Some(config_dir) = config_dir else {
            return Ok(());
        };
        let path = config_dir.join("config.toml");
        let target = match fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(config_error("inspect", &path, error)),
        };
        if let Some(metadata) = &target {
            validate_project_target(&root, &path, metadata)?;
        }
        if over.is_empty() {
            if target.is_some() {
                fs::remove_file(&path).map_err(|error| config_error("remove", &path, error))?;
            }
            return Ok(());
        }
        let body = toml::to_string_pretty(over)
            .map_err(|e| Error::Config(format!("serialize project config: {e}")))?;
        if body.len() as u64 > MAX_CONFIG_BYTES {
            return Err(Error::Config(format!(
                "project config exceeds {MAX_CONFIG_BYTES} bytes"
            )));
        }
        atomic_write_project_config(&root, &config_dir, &path, body.as_bytes())
    }
}

fn config_error(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error::Config(format!("{action} {}: {error}", path.display()))
}

fn canonical_repo_root(repo: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(repo).map_err(|error| config_error("canonicalize", repo, error))?;
    let metadata = fs::metadata(&root).map_err(|error| config_error("inspect", &root, error))?;
    if !metadata.is_dir() {
        return Err(Error::Config(format!(
            "project root is not a directory: {}",
            root.display()
        )));
    }
    Ok(root)
}

fn existing_project_config_dir(root: &Path) -> Result<Option<PathBuf>> {
    let path = root.join(".rtrt");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(config_error("inspect", &path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Config(format!(
            "project config directory must be a real directory: {}",
            path.display()
        )));
    }
    validate_same_owner(root, &path, &metadata)?;
    let canonical =
        fs::canonicalize(&path).map_err(|error| config_error("canonicalize", &path, error))?;
    if canonical.parent() != Some(root) {
        return Err(Error::Config(format!(
            "project config directory escapes repository: {}",
            path.display()
        )));
    }
    Ok(Some(canonical))
}

fn prepare_project_config_dir(root: &Path, create: bool) -> Result<Option<PathBuf>> {
    if let Some(path) = existing_project_config_dir(root)? {
        return Ok(Some(path));
    }
    if !create {
        return Ok(None);
    }
    let path = root.join(".rtrt");
    fs::create_dir(&path).map_err(|error| config_error("mkdir", &path, error))?;
    set_private_directory_mode(&path)?;
    existing_project_config_dir(root)
}

fn validate_project_target(root: &Path, path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Config(format!(
            "project config must be a regular non-symlink file: {}",
            path.display()
        )));
    }
    validate_same_owner(root, path, metadata)
}

#[cfg(unix)]
fn validate_same_owner(root: &Path, path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let root_metadata = fs::metadata(root).map_err(|error| config_error("inspect", root, error))?;
    if metadata.uid() != root_metadata.uid() {
        return Err(Error::Config(format!(
            "project config path has foreign ownership: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_same_owner(_root: &Path, _path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

fn read_bounded_config(path: &Path, reject_symlink: bool) -> Result<String> {
    let expected = if reject_symlink {
        let metadata =
            fs::symlink_metadata(path).map_err(|error| config_error("inspect", path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::Config(format!(
                "config must be a regular non-symlink file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(Error::Config(format!(
                "config exceeds {MAX_CONFIG_BYTES} bytes: {}",
                path.display()
            )));
        }
        Some(metadata)
    } else {
        None
    };
    let file = File::open(path).map_err(|error| config_error("read", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| config_error("inspect", path, error))?;
    if expected
        .as_ref()
        .is_some_and(|expected| !metadata_same_file(expected, &metadata))
    {
        return Err(Error::Config(format!(
            "config changed while opening: {}",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(Error::Config(format!(
            "config is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(Error::Config(format!(
            "config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| config_error("read", path, error))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(Error::Config(format!(
            "config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| Error::Config(format!("read {}: {error}", path.display())))
}

#[cfg(unix)]
fn metadata_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn metadata_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn metadata_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.file_type() == right.file_type() && left.len() == right.len()
}

struct TempConfig {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn atomic_write_project_config(
    root: &Path,
    config_dir: &Path,
    destination: &Path,
    body: &[u8],
) -> Result<()> {
    let mut last_error = None;
    for _ in 0..32 {
        let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = config_dir.join(format!(
            ".config.toml.tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(mut file) => {
                let mut temp = TempConfig {
                    path: temp_path,
                    armed: true,
                };
                file.write_all(body)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| config_error("write", &temp.path, error))?;
                let written_metadata = file
                    .metadata()
                    .map_err(|error| config_error("inspect", &temp.path, error))?;
                drop(file);
                let checked_dir = existing_project_config_dir(root)?.ok_or_else(|| {
                    Error::Config("project config directory disappeared during write".to_string())
                })?;
                if checked_dir != config_dir {
                    return Err(Error::Config(
                        "project config directory changed during write".to_string(),
                    ));
                }
                if let Ok(metadata) = fs::symlink_metadata(destination) {
                    validate_project_target(root, destination, &metadata)?;
                }
                let temp_metadata = fs::symlink_metadata(&temp.path)
                    .map_err(|error| config_error("inspect", &temp.path, error))?;
                if temp_metadata.file_type().is_symlink()
                    || !temp_metadata.is_file()
                    || !metadata_same_file(&written_metadata, &temp_metadata)
                {
                    return Err(Error::Config(
                        "temporary project config changed during write".to_string(),
                    ));
                }
                fs::rename(&temp.path, destination)
                    .map_err(|error| config_error("replace", destination, error))?;
                temp.armed = false;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(config_error("create", &temp_path, error)),
        }
    }
    Err(config_error(
        "create temporary config",
        config_dir,
        last_error.unwrap_or_else(|| std::io::Error::other("name collision")),
    ))
}

#[cfg(unix)]
fn set_private_directory_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| config_error("chmod", path, error))
}

#[cfg(not(unix))]
fn set_private_directory_mode(_path: &Path) -> Result<()> {
    Ok(())
}

/// Walk up from `start` to the enclosing repo root — the first ancestor with a
/// `.git` or `.rtrt` entry. Returns `None` when `start` is not inside a repo,
/// so callers fall back to the plain global config.
pub fn repo_root_from(start: &Path) -> Option<PathBuf> {
    repo_root_in(std::iter::successors(Some(start), |dir| dir.parent()))
}

/// Core walk, parameterised over the ancestor sequence to examine.
///
/// Production always calls this with the *full*, unbounded ancestor chain of
/// `start` (a real `.git`/`.rtrt` anywhere above `start` legitimately wins).
/// Tests call it with a bounded, fixture-scoped ancestor list so their
/// assertions don't depend on what markers happen to exist above the system
/// temp dir on the machine running them.
fn repo_root_in<'a>(ancestors: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    for dir in ancestors {
        if dir.join(".git").exists() || dir.join(".rtrt").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// User overrides for the invocation failure policy (`rtrt-providers`
/// `invoke_with_policy`): which error messages count as fatal / quota /
/// transient, and how a transient failure is retried.
///
/// The marker tables shipped in `rtrt-providers` remain the source of truth;
/// this section is purely additive. An absent `[failover]` section classifies
/// exactly like the built-ins, so the default behaviour is unchanged.
///
/// Precedence, highest first:
///   1. user `fatal`, then user `quota`, then user `transient`;
///   2. built-in fatal, then built-in quota, then built-in transient
///      (including the 5xx heuristic);
///   3. anything still unmatched is fatal.
///
/// Because the user layer is consulted first, listing a built-in marker under a
/// different class *reclassifies* it — e.g. putting `"timed out"` under `quota`
/// stops timeouts from earning a same-target retry.
///
/// Example `~/.rtrt/config.toml`:
///
/// ```toml
/// [failover]
/// quota = ["seat limit reached"]
/// fatal = ["contract expired"]
/// transient_retries = 1
/// backoff_divisor = 60
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverConfig {
    /// Extra markers that halt the walk: no retry, no failover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fatal: Vec<String>,
    /// Extra markers that fall over immediately, without retrying the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<String>,
    /// Extra markers that earn a backed-off retry on the same target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transient: Vec<String>,
    /// Same-target retries granted to a transient failure; `None` keeps the
    /// built-in single retry, `0` disables retrying entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient_retries: Option<u32>,
    /// Divisor applied to the per-call timeout to derive the retry backoff;
    /// `None` keeps the built-in divisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_divisor: Option<u32>,
    /// Fixed retry backoff in milliseconds. Set only to pin the backoff; it
    /// overrides the timeout-derived value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
}

impl FailoverConfig {
    /// True when nothing is customised, i.e. the policy is exactly the
    /// built-in one. Used to keep an untouched `[failover]` section out of the
    /// serialized config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_all_defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert!(c.capture.enabled);
        assert_eq!(c.capture.dedup_window_sec, 300);
        assert!(!c.auto_compress.enabled);
        assert_eq!(c.auto_compress.model, "claude-haiku-4-5");
        assert_eq!(c.auto_compress.min_chars, 1);
    }

    #[test]
    fn team_defaults_are_backward_compatible_and_ordered() {
        let team = Config::from_toml_str("").unwrap().team;
        assert!(!team.enabled);
        assert_eq!(team.roster, RosterPreset::Classic);
        assert_eq!(team.manager_provider, "ollama");
        assert_eq!(team.manager_model, "granite4:350m");
        assert_eq!(
            team.leader_order,
            ["opus", "gpt-sol", "glm-go", "sonnet", "kimi-cloud"]
        );
        assert_eq!(
            team.members
                .iter()
                .map(|member| (
                    member.name.as_str(),
                    member.target.as_str(),
                    member.model.as_deref(),
                    member.mode,
                    member.roles.iter().map(String::as_str).collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "opus",
                    "claude",
                    Some("opus"),
                    TeamMode::Cli,
                    vec!["lead", "architecture", "integration"]
                ),
                (
                    "gpt-sol",
                    "opencode",
                    Some("openai/gpt-5.6-sol"),
                    TeamMode::Cli,
                    vec!["deputy", "hard-implementation", "debugging"]
                ),
                (
                    "glm-go",
                    "opencode",
                    Some("opencode-go/glm-5.2"),
                    TeamMode::Cli,
                    vec!["routine", "boilerplate", "bulk-edit"]
                ),
                (
                    "glm-cloud",
                    "opencode",
                    Some("ollama/glm-5.2:cloud"),
                    TeamMode::Cli,
                    vec!["routine", "overflow", "bulk-edit"]
                ),
                (
                    "sonnet",
                    "claude",
                    Some("sonnet"),
                    TeamMode::Cli,
                    vec!["general-implementation", "tests", "review"]
                ),
                (
                    "kimi-cloud",
                    "opencode",
                    Some("ollama/kimi-k2.7-code:cloud"),
                    TeamMode::Cli,
                    vec!["parallel-implementation", "research", "tests"]
                ),
            ]
        );
        assert!(
            team.members
                .iter()
                .all(|member| member.delegation == Delegation::Native)
        );
        assert!(
            team.members
                .iter()
                .all(|member| member.host_agent.is_none())
        );
    }

    #[test]
    fn partial_team_config_keeps_field_defaults() {
        let team = Config::from_toml_str(
            r#"
            [team]
            enabled = true
            manager_model = "qwen3:8b"
            "#,
        )
        .unwrap()
        .team;

        assert!(team.enabled);
        assert_eq!(team.manager_provider, "ollama");
        assert_eq!(team.manager_model, "qwen3:8b");
        assert_eq!(team.leader_order, default_team_leader_order());
        assert_eq!(team.members, default_team_members());
    }

    #[test]
    fn team_manager_base_url_roundtrips() {
        let config =
            Config::from_toml_str("[team]\nmanager_base_url = \"https://manager.example/v1\"\n")
                .unwrap();

        assert_eq!(
            config.team.manager_base_url.as_deref(),
            Some("https://manager.example/v1")
        );
        let roundtripped = Config::from_toml_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(
            roundtripped.team.manager_base_url,
            config.team.manager_base_url
        );
    }

    #[test]
    fn customized_team_roundtrip_preserves_member_and_leader_order() {
        let config = Config::from_toml_str(
            r#"
            [team]
            enabled = true
            manager_provider = "openai"
            manager_model = "gpt-5"
            leader_order = ["second", "first"]

            [[team.members]]
            name = "first"
            target = "opencode"
            model = "provider/first"
            mode = "api"
            roles = ["worker"]

            [[team.members]]
            name = "second"
            target = "claude"
            mode = "auto"
            roles = ["lead", "review"]
            "#,
        )
        .unwrap();

        let serialized = toml::to_string(&config).unwrap();
        let roundtripped = Config::from_toml_str(&serialized).unwrap();
        assert_eq!(roundtripped.team, config.team);
        assert_eq!(roundtripped.team.members[0].name, "first");
        assert_eq!(roundtripped.team.members[1].name, "second");
        assert_eq!(roundtripped.team.leader_order, ["second", "first"]);
    }

    #[test]
    fn workers_remain_available_outside_leader_order() {
        let team = Config::from_toml_str(
            r#"
            [team]
            enabled = true
            leader_order = ["opus"]
            "#,
        )
        .unwrap()
        .team;

        assert_eq!(team.leader_order, ["opus"]);
        assert!(team.members.iter().any(|member| member.name == "glm-cloud"));
        assert!(!team.leader_order.iter().any(|name| name == "glm-cloud"));
    }

    #[test]
    fn invalid_team_values_and_duplicates_are_rejected() {
        let enabled_team = || TeamConfig {
            enabled: true,
            ..TeamConfig::default()
        };
        for invalid in [
            "[team]\nmanager_provider = \" \"",
            "[team]\nmanager_model = \"\"",
            "[team]\nmembers = [{ name = \"x\", target = \"claude\", mode = \"shell\", roles = [\"lead\"] }]",
            "[team]\nmembers = [{ name = \"x\", target = \"claude\", mode = \"cli\", roles = [\"lead\"], command = \"rm\" }]",
        ] {
            assert!(
                Config::from_toml_str(invalid).is_err(),
                "accepted {invalid}"
            );
        }

        let empty_leaders = TeamConfig {
            enabled: true,
            leader_order: Vec::new(),
            ..TeamConfig::default()
        };
        assert!(empty_leaders.validate().is_err());

        let empty_members = TeamConfig {
            enabled: true,
            members: Vec::new(),
            ..TeamConfig::default()
        };
        assert!(empty_members.validate().is_err());

        let mut duplicate_name = enabled_team();
        let mut member = duplicate_name.members[0].clone();
        member.target = "other".to_string();
        duplicate_name.members.push(member);
        assert!(duplicate_name.validate().is_err());

        let mut duplicate_target = enabled_team();
        let mut member = duplicate_target.members[0].clone();
        member.name = "other".to_string();
        duplicate_target.members.push(member);
        assert!(duplicate_target.validate().is_err());

        let mut duplicate_leader = enabled_team();
        duplicate_leader.leader_order.push("opus".to_string());
        assert!(duplicate_leader.validate().is_err());

        let mut unknown_leader = enabled_team();
        unknown_leader.leader_order.push("missing".to_string());
        assert!(unknown_leader.validate().is_err());

        let mut empty_roles = enabled_team();
        empty_roles.members[0].roles.clear();
        assert!(empty_roles.validate().is_err());

        let mut blank_role = enabled_team();
        blank_role.members[0].roles[0] = " ".to_string();
        assert!(blank_role.validate().is_err());

        let mut blank_name = enabled_team();
        blank_name.members[0].name = " ".to_string();
        assert!(blank_name.validate().is_err());

        let mut blank_target = enabled_team();
        blank_target.members[0].target.clear();
        assert!(blank_target.validate().is_err());

        let mut blank_model = enabled_team();
        blank_model.members[0].model = Some(String::new());
        assert!(blank_model.validate().is_err());

        let mut nul = enabled_team();
        nul.members[0].target.push('\0');
        assert!(nul.validate().is_err());
    }

    #[test]
    fn enabled_native_host_and_opencode_model_policy_is_validated_from_toml() {
        let single_member = |host_agent: &str, model: Option<&str>| {
            let model = model
                .map(|model| format!("model = {model:?}\n"))
                .unwrap_or_default();
            format!(
                r#"
                [team]
                enabled = true
                leader_order = ["worker"]

                [[team.members]]
                name = "worker"
                target = "opencode"
                mode = "cli"
                roles = ["worker"]
                host_agent = {host_agent:?}
                {model}
                "#
            )
        };

        for host_agent in ["../worker", "worker.name", "worker name", "작업자"] {
            let error = Config::from_toml_str(&single_member(host_agent, Some("provider/model")))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("only ASCII letters"),
                "{host_agent}: {error}"
            );
        }
        for host_agent in ["build", "plan"] {
            let error = Config::from_toml_str(&single_member(host_agent, Some("provider/model")))
                .unwrap_err()
                .to_string();
            assert!(error.contains("primary-only OpenCode agent"), "{error}");
        }
        let error = Config::from_toml_str(&single_member("rtrt-manager", Some("provider/model")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved OpenCode agent name"), "{error}");

        for model in [
            "",
            "model",
            "/model",
            "provider/",
            "provider//model",
            "provider/model name",
            "provider/$model",
        ] {
            assert!(
                Config::from_toml_str(&single_member("worker", Some(model))).is_err(),
                "accepted invalid OpenCode model ID {model:?}"
            );
        }

        let duplicate = r#"
            [team]
            enabled = true
            leader_order = ["first"]

            [[team.members]]
            name = "first"
            target = "opencode"
            model = "one/model"
            mode = "cli"
            roles = ["worker"]
            host_agent = "shared"

            [[team.members]]
            name = "second"
            target = "opencode"
            model = "two/model"
            mode = "cli"
            roles = ["worker"]
            host_agent = "shared"
        "#;
        let error = Config::from_toml_str(duplicate).unwrap_err().to_string();
        assert!(
            error.contains("duplicate native team host_agent"),
            "{error}"
        );

        for host_agent in ["explore", "general", "scout"] {
            Config::from_toml_str(&single_member(host_agent, None)).unwrap();
        }
    }

    #[test]
    fn enabled_manager_model_provider_prefix_must_match() {
        for model in ["legacy-model", "ollama/qualified-model"] {
            Config::from_toml_str(&format!(
                "[team]\nenabled = true\nmanager_provider = \"ollama\"\n\
                 manager_model = {model:?}\n"
            ))
            .unwrap();
        }

        let error = Config::from_toml_str(
            "[team]\nenabled = true\nmanager_provider = \"ollama\"\n\
             manager_model = \"openai/model\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("provider prefix openai"), "{error}");
        assert!(error.contains("manager_provider ollama"), "{error}");

        for model in ["ollama/", "/model", "ollama/model name"] {
            let error = Config::from_toml_str(&format!(
                "[team]\nenabled = true\nmanager_provider = \"ollama\"\n\
                 manager_model = {model:?}\n"
            ))
            .unwrap_err()
            .to_string();
            assert!(error.contains("valid nonempty provider/model"), "{error}");
        }
    }

    #[test]
    fn delegation_and_host_policy_defaults_and_roundtrip() {
        let member = TeamMember::new("native", "opencode", TeamMode::Cli);
        assert_eq!(member.delegation, Delegation::Native);
        assert!(member.host_agent.is_none());

        let policy = TeamPolicy::default();
        assert!(policy.explore_tier.is_none());
        assert!(policy.review_tier.is_none());
        assert_eq!(policy.balance, Balance::Order);
        assert_eq!(policy.worker_summary_max_lines, 3);
        assert!(policy.isolate_conflicting);

        let source = r#"
            [team]
            enabled = true
            roster = "opencode-lead"
            leader_order = ["planner"]

            [[team.members]]
            name = "planner"
            target = "claude"
            model = "opus"
            mode = "cli"
            roles = ["plan"]
            delegation = "cli"
            host_agent = "claude-opus"
            allow_impl = false

            [team.members.flags]
            output-format = "json"
            permission-mode = "plan"
            permission-prompt-tool = "mcp__rtrt__permission_prompt"

            [team.tiers]
            discovery = ["planner"]
            review = ["planner"]
            plan = ["planner"]

            [team.policy]
            explore_tier = "discovery"
            review_tier = "review"
            balance = "room"
            worker_summary_max_lines = 2
            isolate_conflicting = false
            design_only_tiers = ["discovery", "review", "plan"]
        "#;
        let team = Config::from_toml_str(source).unwrap().team;
        assert_eq!(team.roster, RosterPreset::OpencodeLead);
        assert_eq!(team.members[0].delegation, Delegation::Shell);
        assert_eq!(team.members[0].host_agent.as_deref(), Some("claude-opus"));
        assert_eq!(team.policy.explore_tier.as_deref(), Some("discovery"));
        assert_eq!(team.policy.review_tier.as_deref(), Some("review"));
        assert_eq!(team.policy.balance, Balance::Room);
        assert_eq!(team.policy.worker_summary_max_lines, 2);
        assert!(!team.policy.isolate_conflicting);

        let serialized = toml::to_string(&team).unwrap();
        let reparsed: TeamConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, team);
        reparsed.validate().unwrap();
    }

    #[test]
    fn delegation_wire_names_support_cli_and_legacy_shell() {
        let member_toml = |delegation: &str| {
            format!(
                "name = \"worker\"\ntarget = \"claude\"\nmode = \"cli\"\nroles = [\"worker\"]\ndelegation = {delegation:?}\n"
            )
        };

        let cli: TeamMember = toml::from_str(&member_toml("cli")).unwrap();
        assert_eq!(cli.delegation, Delegation::Shell);
        assert!(
            toml::to_string(&cli)
                .unwrap()
                .contains("delegation = \"cli\"")
        );

        let legacy: TeamMember = toml::from_str(&member_toml("shell")).unwrap();
        assert_eq!(legacy.delegation, Delegation::Shell);
        assert!(
            toml::to_string(&legacy)
                .unwrap()
                .contains("delegation = \"cli\"")
        );

        assert_eq!(
            serde_json::to_string(&Delegation::Native).unwrap(),
            "\"native\""
        );
        assert_eq!(
            serde_json::from_str::<Delegation>("\"native\"").unwrap(),
            Delegation::Native
        );
    }

    #[test]
    fn native_permissions_toml_roundtrip_preserves_actions_and_bash_order() {
        let source = r#"
            [team]
            enabled = false

            [[team.members]]
            name = "worker"
            target = "opencode"
            mode = "cli"
            roles = ["arbitrary-role"]

            [team.members.permissions]
            edit = "ask"

            [team.members.permissions.bash]
            "git status" = "allow"
            "cargo test *" = "ask"
            "cargo build" = "deny"
        "#;
        let config = Config::from_toml_str(source).unwrap();
        let permissions = config.team.members[0].permissions.as_ref().unwrap();
        assert_eq!(permissions.edit, Some(PermissionAction::Ask));
        assert_eq!(
            permissions.bash.iter().collect::<Vec<_>>(),
            [
                ("git status", PermissionAction::Allow),
                ("cargo test *", PermissionAction::Ask),
                ("cargo build", PermissionAction::Deny),
            ]
        );

        let serialized = toml::to_string(&config).unwrap();
        let reparsed = Config::from_toml_str(&serialized).unwrap();
        assert_eq!(
            reparsed.team.members[0].permissions,
            Some(permissions.clone())
        );
        assert!(serialized.find("git status").unwrap() < serialized.find("cargo test *").unwrap());
        assert!(serialized.find("cargo test *").unwrap() < serialized.find("cargo build").unwrap());
    }

    #[test]
    fn omitted_native_permissions_are_backward_compatible_and_not_serialized() {
        let source = r#"
            name = "worker"
            target = "opencode"
            mode = "cli"
            roles = ["worker"]
        "#;
        let member: TeamMember = toml::from_str(source).unwrap();
        assert!(member.permissions.is_none());
        assert!(!toml::to_string(&member).unwrap().contains("permissions"));
        assert!(
            default_team_members()
                .iter()
                .all(|member| member.permissions.is_none())
        );
    }

    #[test]
    fn unsafe_native_bash_permission_patterns_and_cli_permissions_are_rejected() {
        for pattern in [
            "",
            "   ",
            "*",
            "  * ",
            "git status\0",
            "git status\ngit diff",
            "git status\r",
            "git status | cat",
            "git status && cargo test",
            "git status; cargo test",
            "cargo test > result",
            "$(whoami)",
            "`whoami`",
            "(git status)",
        ] {
            let mut member = TeamMember::new("worker", "opencode", TeamMode::Cli);
            member.roles = team_names(&["worker"]);
            member.permissions = Some(NativePermissions {
                edit: None,
                bash: PermissionMap::from_pairs([(pattern, PermissionAction::Allow)]),
            });
            let team = TeamConfig {
                members: vec![member],
                ..TeamConfig::default()
            };
            assert!(
                team.validate().is_err(),
                "accepted unsafe pattern {pattern:?}"
            );
        }

        let mut cli = TeamMember::new("worker", "claude", TeamMode::Cli);
        cli.roles = team_names(&["worker"]);
        cli.delegation = Delegation::Shell;
        cli.permissions = Some(NativePermissions {
            edit: Some(PermissionAction::Allow),
            bash: PermissionMap::default(),
        });
        let team = TeamConfig {
            members: vec![cli],
            ..TeamConfig::default()
        };
        let error = team.validate().unwrap_err().to_string();
        assert!(
            error.contains("applies only to native delegation"),
            "{error}"
        );
    }

    #[test]
    fn opencode_lead_preset_inherits_native_permissions() {
        let team = TeamConfig::preset(RosterPreset::OpencodeLead);
        assert!(
            team.members
                .iter()
                .filter(|member| member.delegation == Delegation::Native)
                .all(|member| member.permissions.is_none())
        );

        let opus = team.member("opus").unwrap();
        assert_eq!(opus.flag("permission-mode"), Some("plan"));
        assert_eq!(opus.flag("output-format"), Some("json"));
        assert_eq!(
            opus.flag("permission-prompt-tool"),
            Some(CLAUDE_PERMISSION_PROMPT_TOOL)
        );
        assert_eq!(opus.flag("allowed-tools"), None);

        let sonnet = team.member("sonnet").unwrap();
        assert_eq!(sonnet.flag("permission-mode"), Some("acceptEdits"));
        assert_eq!(sonnet.flag("output-format"), Some("json"));
        assert_eq!(
            sonnet.flag("permission-prompt-tool"),
            Some(CLAUDE_PERMISSION_PROMPT_TOOL)
        );
        assert_eq!(sonnet.flag("allowed-tools"), None);
    }

    #[test]
    fn claude_cli_delegation_rejects_permission_bypasses_even_when_disabled() {
        for (key, value, expected) in [
            ("ALLOWED-TOOLS", "Read", "--ALLOWED-TOOLS"),
            ("AllowedTools", "Read", "--AllowedTools"),
            (
                "DANGEROUSLY-SKIP-PERMISSIONS",
                "",
                "--DANGEROUSLY-SKIP-PERMISSIONS",
            ),
            (
                "Permission-Mode",
                "BYPASSPERMISSIONS",
                "rejected --Permission-Mode",
            ),
            (
                "Permission-Prompt-Tool",
                "foreign_tool",
                "rejected --Permission-Prompt-Tool",
            ),
        ] {
            let mut member = TeamMember::new("worker", "claude", TeamMode::Cli);
            member.roles = team_names(&["worker"]);
            member.delegation = Delegation::Shell;
            member.model = Some("sonnet".into());
            member.flags = team_flags(&[
                ("output-format", "json"),
                ("permission-mode", "acceptEdits"),
                ("permission-prompt-tool", CLAUDE_PERMISSION_PROMPT_TOOL),
            ]);
            member.flags.insert(key.to_string(), value.to_string());
            let team = TeamConfig {
                enabled: false,
                members: vec![member],
                ..TeamConfig::default()
            };
            let error = team.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{key}: {error}");
        }
    }

    #[test]
    fn claude_cli_delegation_rejects_unknown_future_flags() {
        let mut member = TeamMember::new("worker", "claude", TeamMode::Cli);
        member.roles = team_names(&["worker"]);
        member.delegation = Delegation::Shell;
        member.model = Some("sonnet".into());
        member.flags = team_flags(&[
            ("future-safe-flag", "future-value"),
            ("output-format", "json"),
            ("permission-mode", "acceptEdits"),
            ("Permission-Prompt-Tool", CLAUDE_PERMISSION_PROMPT_TOOL),
        ]);
        let team = TeamConfig {
            enabled: false,
            members: vec![member],
            ..TeamConfig::default()
        };
        assert!(team.validate().is_err());
    }

    #[test]
    fn cli_delegation_is_restricted_to_claude() {
        let cli_member = |target: &str| TeamMember {
            roles: team_names(&["worker"]),
            delegation: Delegation::Shell,
            ..TeamMember::new("worker", target, TeamMode::Cli)
        };

        for target in ["opencode", "opencode-go", "other"] {
            let team = TeamConfig {
                // The boundary applies before enablement so an invalid dormant
                // roster cannot be activated later.
                enabled: false,
                members: vec![cli_member(target)],
                ..TeamConfig::default()
            };
            let error = team.validate().unwrap_err().to_string();
            assert!(error.contains("requires target = \"claude\""), "{error}");
            assert!(error.contains("delegation = cli"), "{error}");
            assert!(error.contains(target), "{error}");
        }

        let mut claude_member = cli_member("claude");
        claude_member.model = Some("sonnet".into());
        claude_member.flags = team_flags(&[
            ("output-format", "json"),
            ("permission-mode", "acceptEdits"),
            ("permission-prompt-tool", CLAUDE_PERMISSION_PROMPT_TOOL),
        ]);
        let claude = TeamConfig {
            enabled: true,
            leader_order: team_names(&["worker"]),
            members: vec![claude_member],
            ..TeamConfig::default()
        };
        claude.validate().unwrap();
    }

    #[test]
    fn opencode_lead_preset_has_native_lanes_and_expected_fallbacks() {
        let team = TeamConfig::preset(RosterPreset::OpencodeLead);
        team.validate().unwrap();
        let serialized = toml::to_string(&Config {
            team: team.clone(),
            ..Config::default()
        })
        .unwrap();
        assert!(!serialized.contains("permissions"));
        assert!(!serialized.contains("allowed-tools"));
        assert_eq!(serialized.matches(CLAUDE_PERMISSION_PROMPT_TOOL).count(), 2);
        assert_eq!(Config::from_toml_str(&serialized).unwrap().team, team);

        assert!(team.enabled);
        assert_eq!(team.roster, RosterPreset::OpencodeLead);
        assert_eq!(team.manager_provider, "openai");
        assert_eq!(team.manager_model, "gpt-5.6-sol");
        assert_eq!(
            team.members
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>(),
            [
                "opus",
                "kimi-k3",
                "kimi-k3-cloud",
                "codex-sol",
                "codex-luna",
                "glm",
                "glm-cloud",
                "kimi",
                "kimi-cloud",
                "explore",
                "sonnet",
            ]
        );
        assert_eq!(team.leader_order, ["codex-sol", "sonnet", "kimi-k3-cloud"]);

        for (name, target, model, logical, sibling, host_agent, roles) in [
            (
                "opus",
                "claude",
                Some("opus"),
                Some("opus"),
                None,
                "claude-opus",
                &[
                    "plan",
                    "architecture",
                    "task-breakdown",
                    "architecture-review",
                ][..],
            ),
            (
                "kimi-k3",
                "opencode",
                Some("opencode-go/kimi-k3"),
                Some("kimi-k3"),
                Some("kimi-k3-cloud"),
                "kimi-k3",
                &[
                    "hard-implementation",
                    "multifile",
                    "refactoring",
                    "frontend",
                ],
            ),
            (
                "kimi-k3-cloud",
                "opencode",
                Some("ollama/kimi-k3:cloud"),
                Some("kimi-k3"),
                Some("kimi-k3"),
                "kimi-k3-cloud",
                &[
                    "hard-implementation",
                    "multifile",
                    "refactoring",
                    "frontend",
                ],
            ),
            (
                "codex-sol",
                "opencode",
                Some("openai/gpt-5.6-sol"),
                Some("gpt-5.6-sol"),
                None,
                "codex-sol-worker",
                &[
                    "hard-implementation",
                    "debugging",
                    "systems",
                    "architecture-aware",
                ],
            ),
            (
                "codex-luna",
                "opencode",
                Some("openai/gpt-5.6-luna"),
                Some("gpt-5.6-luna"),
                None,
                "codex-luna",
                &["routine", "tests", "docs"],
            ),
            (
                "glm",
                "opencode",
                Some("opencode-go/glm-5.2"),
                Some("glm-5.2"),
                Some("glm-cloud"),
                "glm",
                &["simple", "mechanical", "boilerplate", "bulk-edit"],
            ),
            (
                "glm-cloud",
                "opencode",
                Some("ollama/glm-5.2:cloud"),
                Some("glm-5.2"),
                Some("glm"),
                "glm-cloud",
                &["simple", "mechanical", "boilerplate", "bulk-edit"],
            ),
            (
                "kimi",
                "opencode",
                Some("opencode-go/kimi-k2.7-code"),
                Some("kimi-k2.7-code"),
                Some("kimi-cloud"),
                "kimi",
                &["simple", "mechanical", "boilerplate", "single-file"],
            ),
            (
                "kimi-cloud",
                "opencode",
                Some("ollama/kimi-k2.7-code:cloud"),
                Some("kimi-k2.7-code"),
                Some("kimi"),
                "kimi-cloud",
                &["simple", "mechanical", "boilerplate", "single-file"],
            ),
            (
                "explore",
                "opencode",
                None,
                None,
                None,
                "explore",
                &["discovery"],
            ),
            (
                "sonnet",
                "claude",
                Some("sonnet"),
                Some("sonnet"),
                None,
                "claude-sonnet",
                &["review", "consistency"],
            ),
        ] {
            let member = team.member(name).unwrap();
            assert_eq!(member.target, target, "{name}");
            assert_eq!(member.model.as_deref(), model, "{name}");
            assert_eq!(member.logical.as_deref(), logical, "{name}");
            assert_eq!(member.sibling.as_deref(), sibling, "{name}");
            assert_eq!(member.host_agent.as_deref(), Some(host_agent), "{name}");
            assert_eq!(
                member.roles.iter().map(String::as_str).collect::<Vec<_>>(),
                roles,
                "{name}"
            );
        }

        let opus = team.member("opus").unwrap();
        assert!(!opus.allow_impl);
        assert_eq!(opus.delegation, Delegation::Shell);
        assert_eq!(opus.host_agent.as_deref(), Some("claude-opus"));
        assert!(opus.roles.iter().any(|role| role == "plan"));

        let hard = team.effective_tiers();
        assert_eq!(
            hard.get("hard").unwrap(),
            ["codex-sol", "kimi-k3-cloud", "kimi-k3"]
        );
        assert_eq!(hard.get("routine").unwrap(), ["codex-luna"]);
        assert_eq!(
            hard.get("simple").unwrap(),
            ["glm", "glm-cloud", "kimi", "kimi-cloud"]
        );
        assert_eq!(hard.get("explore").unwrap(), ["explore"]);
        assert_eq!(hard.get("review").unwrap(), ["sonnet"]);
        assert_eq!(team.policy.balance, Balance::Room);
        assert_eq!(team.policy.explore_tier.as_deref(), Some("explore"));
        assert_eq!(team.policy.review_tier.as_deref(), Some("review"));
        assert!(team.is_design_only_tier("plan"));

        assert_eq!(
            team.members
                .iter()
                .map(|member| (
                    member.name.as_str(),
                    member
                        .fallback
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("opus", vec![]),
                ("kimi-k3", vec!["codex-sol"]),
                ("kimi-k3-cloud", vec!["codex-sol"]),
                ("codex-sol", vec!["sonnet"]),
                ("codex-luna", vec!["kimi-k3", "codex-sol"]),
                ("glm", vec!["kimi"]),
                ("glm-cloud", vec!["kimi-cloud"]),
                ("kimi", vec!["codex-luna"]),
                ("kimi-cloud", vec!["codex-luna"]),
                ("explore", vec![]),
                ("sonnet", vec![]),
            ]
        );
        assert_eq!(
            team.fallback_chain("glm"),
            ["kimi", "codex-luna", "kimi-k3", "codex-sol", "sonnet"]
        );
        assert_eq!(
            team.fallback_chain("glm-cloud"),
            ["kimi-cloud", "codex-luna", "kimi-k3", "codex-sol", "sonnet"]
        );
        assert_eq!(
            team.fallback_chain("kimi-k3-cloud"),
            ["codex-sol", "sonnet"]
        );

        let mut host_agents = BTreeSet::new();
        for member in &team.members {
            match member.target.as_str() {
                "opencode" => assert_eq!(member.delegation, Delegation::Native),
                "claude" => assert_eq!(member.delegation, Delegation::Shell),
                target => panic!("unexpected preset target: {target}"),
            }
            assert!(host_agents.insert(member.host_agent.as_deref().unwrap()));
        }
        assert_eq!(host_agents.len(), 11);

        assert_eq!(
            TeamConfig::preset(RosterPreset::Classic),
            TeamConfig::default()
        );
    }

    /// A `[team]` section exactly as it was written before lanes existed —
    /// the shape sitting in `~/.rtrt/config.toml` today.
    const LEGACY_TEAM_TOML: &str = r#"
        [team]
        enabled = true
        manager_provider = "ollama"
        manager_model = "granite4.1:3b"
        manager_base_url = "http://127.0.0.1:11434/v1"
        leader_order = ["opus", "gpt-sol", "glm-go", "sonnet", "kimi-cloud"]

        [[team.members]]
        name = "opus"
        target = "claude"
        model = "opus"
        mode = "cli"
        roles = ["lead", "architecture", "integration"]

        [[team.members]]
        name = "gpt-sol"
        target = "opencode"
        model = "openai/gpt-5.6-sol"
        mode = "cli"
        roles = ["deputy", "hard-implementation", "debugging"]

        [[team.members]]
        name = "glm-go"
        target = "opencode"
        model = "opencode-go/glm-5.2"
        mode = "cli"
        roles = ["routine", "boilerplate", "bulk-edit"]

        [[team.members]]
        name = "glm-cloud"
        target = "opencode"
        model = "ollama/glm-5.2:cloud"
        mode = "cli"
        roles = ["routine", "overflow", "bulk-edit"]

        [[team.members]]
        name = "sonnet"
        target = "claude"
        model = "sonnet"
        mode = "cli"
        roles = ["general-implementation", "tests", "review"]

        [[team.members]]
        name = "kimi-cloud"
        target = "opencode"
        model = "ollama/kimi-k2.7-code:cloud"
        mode = "cli"
        roles = ["parallel-implementation", "research", "tests"]
    "#;

    #[test]
    fn legacy_team_toml_round_trips_without_emitting_lane_keys() {
        let team = Config::from_toml_str(LEGACY_TEAM_TOML).unwrap().team;
        let serialized = toml::to_string(&team).unwrap();

        // Nothing a lane-less config never wrote may appear on the way out,
        // otherwise loading and saving would rewrite everyone's config file.
        for key in [
            "tiers",
            "policy",
            "roster",
            "delegation",
            "host_agent",
            "logical",
            "sibling",
            "fallback",
            "allow_impl",
            "flags",
        ] {
            assert!(
                !serialized.contains(key),
                "{key} leaked into a legacy [team] section:\n{serialized}"
            );
        }

        let reparsed: TeamConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, team);
        assert_eq!(toml::to_string(&reparsed).unwrap(), serialized);
        // The lane fields are present in memory, at their defaults.
        assert!(team.members.iter().all(|member| member.allow_impl));
        assert!(team.members.iter().all(|member| member.fallback.is_empty()));
        assert!(team.tiers.is_empty());
        assert!(team.policy.is_default());
        assert_eq!(team.roster, RosterPreset::Classic);
        team.validate().unwrap();
    }

    #[test]
    fn shipped_tier_ladder_is_ordered_and_validates() {
        let team = TeamConfig {
            enabled: true,
            ..TeamConfig::default()
        };
        team.validate().unwrap();

        let tiers = team.effective_tiers();
        assert_eq!(
            tiers.names().collect::<Vec<_>>(),
            ["mechanical", "routine", "multifile", "design", "review"]
        );
        assert_eq!(tiers.get("mechanical").unwrap(), ["glm-go", "glm-cloud"]);
        assert_eq!(tiers.get("design").unwrap(), ["opus", "gpt-sol"]);
        assert!(team.is_design_only_tier("design"));
        assert!(!team.is_design_only_tier("review"));
        assert_eq!(team.effective_default_tier().as_deref(), Some("mechanical"));
    }

    #[test]
    fn configured_tiers_replace_the_shipped_ladder_instead_of_merging() {
        let team = Config::from_toml_str(
            r#"
            [team]
            enabled = true
            leader_order = ["opus"]

            [team.tiers]
            quick = ["glm-go"]
            deep = ["opus", "gpt-sol"]

            [team.policy]
            design_only_tiers = ["deep"]
            "#,
        )
        .unwrap()
        .team;

        let tiers = team.effective_tiers();
        // Declaration order, not alphabetical, and none of the shipped rungs.
        assert_eq!(tiers.names().collect::<Vec<_>>(), ["quick", "deep"]);
        for shipped in ["mechanical", "routine", "multifile", "review"] {
            assert!(!tiers.contains(shipped), "{shipped} survived the override");
        }
        assert_eq!(team.effective_default_tier().as_deref(), Some("quick"));
        assert!(team.is_design_only_tier("deep"));
        assert!(!team.is_design_only_tier("quick"));
    }

    #[test]
    fn member_tier_declarations_build_a_ladder_without_a_tiers_table() {
        let team = Config::from_toml_str(
            r#"
            [team]
            enabled = true
            leader_order = ["lead"]

            [[team.members]]
            name = "lead"
            target = "claude"
            mode = "cli"
            roles = ["lead"]
            tier = "solo"

            [[team.members]]
            name = "helper"
            target = "opencode"
            mode = "cli"
            roles = ["helper"]
            tier = "solo"
            "#,
        )
        .unwrap()
        .team;

        // The shipped ladder names lanes this roster does not have, so it is
        // dropped rather than inherited; the members' own declarations stand.
        let tiers = team.effective_tiers();
        assert_eq!(tiers.names().collect::<Vec<_>>(), ["solo"]);
        assert_eq!(tiers.get("solo").unwrap(), ["lead", "helper"]);
        team.validate().unwrap();
    }

    #[test]
    fn unknown_and_looping_fallbacks_are_rejected() {
        let enabled_team = || TeamConfig {
            enabled: true,
            ..TeamConfig::default()
        };

        let mut unknown = enabled_team();
        unknown.members[0].fallback = vec!["missing".to_string()];
        assert_eq!(
            unknown.validate().unwrap_err().to_string(),
            "config error: team.members[0].fallback[0] references unknown member: missing"
        );

        let mut itself = enabled_team();
        itself.members[0].fallback = vec!["opus".to_string()];
        assert!(
            itself
                .validate()
                .unwrap_err()
                .to_string()
                .contains("fallback[0] must not reference itself: opus")
        );

        let mut repeated = enabled_team();
        repeated.members[0].fallback = vec!["sonnet".to_string(), "sonnet".to_string()];
        assert!(
            repeated
                .validate()
                .unwrap_err()
                .to_string()
                .contains("team.members[0].fallback lists sonnet twice")
        );

        // Shipped chain is opus -> gpt-sol -> sonnet -> kimi-cloud; close it.
        let mut cycle = enabled_team();
        let last = cycle.members.len() - 1;
        assert_eq!(cycle.members[last].name, "kimi-cloud");
        cycle.members[last].fallback = vec!["opus".to_string()];
        assert_eq!(
            cycle.validate().unwrap_err().to_string(),
            "config error: team fallback chain forms a cycle: \
             opus -> gpt-sol -> sonnet -> kimi-cloud -> opus"
        );
    }

    #[test]
    fn siblings_must_be_one_logical_model_on_two_pools() {
        let enabled_team = || TeamConfig {
            enabled: true,
            ..TeamConfig::default()
        };
        let glm_go = 2;
        assert_eq!(enabled_team().members[glm_go].name, "glm-go");

        let mut crossed = enabled_team();
        crossed.members[glm_go].logical = Some("kimi-k2.7-code".to_string());
        assert!(
            crossed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("sibling glm-cloud serves logical model glm-5.2, not kimi-k2.7-code"),
        );

        let mut undeclared = enabled_team();
        undeclared.members[glm_go].logical = None;
        assert!(
            undeclared
                .validate()
                .unwrap_err()
                .to_string()
                .contains("requires both members to declare `logical`")
        );

        let mut unknown = enabled_team();
        unknown.members[glm_go].sibling = Some("missing".to_string());
        assert!(
            unknown
                .validate()
                .unwrap_err()
                .to_string()
                .contains("sibling references unknown member: missing")
        );

        let mut itself = enabled_team();
        itself.members[glm_go].sibling = Some("glm-go".to_string());
        assert!(
            itself
                .validate()
                .unwrap_err()
                .to_string()
                .contains("sibling must not reference itself")
        );

        // The shipped pair resolves both ways.
        let team = enabled_team();
        assert_eq!(team.sibling_of("glm-go").unwrap().name, "glm-cloud");
        assert_eq!(team.sibling_of("glm-cloud").unwrap().name, "glm-go");
        assert!(team.sibling_of("sonnet").is_none());
    }

    #[test]
    fn sibling_toml_requires_reciprocal_distinct_pool_links() {
        let sibling_team = |first_model: &str,
                            first_logical: &str,
                            first_sibling: Option<&str>,
                            second_model: &str,
                            second_logical: &str,
                            second_sibling: Option<&str>| {
            let first_sibling = first_sibling
                .map(|name| format!("sibling = {name:?}"))
                .unwrap_or_default();
            let second_sibling = second_sibling
                .map(|name| format!("sibling = {name:?}"))
                .unwrap_or_default();
            format!(
                r#"
                    [team]
                    enabled = true
                    leader_order = ["first"]

                    [[team.members]]
                    name = "first"
                    target = "opencode"
                    model = {first_model:?}
                    mode = "cli"
                    roles = ["worker"]
                    logical = {first_logical:?}
                    {first_sibling}

                    [[team.members]]
                    name = "second"
                    target = "opencode"
                    model = {second_model:?}
                    mode = "cli"
                    roles = ["worker"]
                    logical = {second_logical:?}
                    {second_sibling}
                    "#
            )
        };

        let valid = sibling_team(
            "provider-one/model",
            "model",
            Some("second"),
            "provider-two/model",
            "model",
            Some("first"),
        );
        Config::from_toml_str(&valid).unwrap();

        let one_way = sibling_team(
            "provider-one/model",
            "model",
            Some("second"),
            "provider-two/model",
            "model",
            None,
        );
        let error = Config::from_toml_str(&one_way).unwrap_err().to_string();
        assert!(
            error.contains("must reciprocally reference first"),
            "{error}"
        );

        let crossed = sibling_team(
            "provider-one/model",
            "model-one",
            Some("second"),
            "provider-two/model",
            "model-two",
            Some("first"),
        );
        let error = Config::from_toml_str(&crossed).unwrap_err().to_string();
        assert!(error.contains("siblings must be the same model"), "{error}");

        let shared_pool = sibling_team(
            "provider/model-one",
            "model",
            Some("second"),
            "provider/model-two",
            "model",
            Some("first"),
        );
        let error = Config::from_toml_str(&shared_pool).unwrap_err().to_string();
        assert!(
            error.contains("same backing pool opencode#provider"),
            "{error}"
        );
    }

    #[test]
    fn tier_rosters_must_name_real_members_and_real_tiers() {
        let team = |body: &str| {
            Config::from_toml_str(&format!(
                "[team]\nenabled = true\nleader_order = [\"opus\"]\n{body}"
            ))
        };

        assert!(
            team("[team.tiers]\nquick = [\"nope\"]\n")
                .unwrap_err()
                .to_string()
                .contains("team.tiers.quick references unknown member: nope")
        );
        assert!(
            team("[team.tiers]\nquick = []\n")
                .unwrap_err()
                .to_string()
                .contains("team.tiers.quick must list at least one member")
        );
        assert!(
            team("[team.tiers]\nquick = [\"sonnet\", \"sonnet\"]\n")
                .unwrap_err()
                .to_string()
                .contains("team.tiers.quick lists sonnet twice")
        );
        assert!(
            team("[team.policy]\ndesign_only_tiers = [\"nope\"]\n")
                .unwrap_err()
                .to_string()
                .contains("team.policy.design_only_tiers references unknown tier: nope")
        );
        assert!(
            team("[team.policy]\ndefault_tier = \"nope\"\n")
                .unwrap_err()
                .to_string()
                .contains("team.policy.default_tier references unknown tier: nope")
        );
    }

    #[test]
    fn design_only_member_cannot_sit_in_an_implementation_tier() {
        let error = Config::from_toml_str(
            r#"
            [team]
            enabled = true
            leader_order = ["opus"]

            [team.tiers]
            deep = ["opus", "gpt-sol"]
            "#,
        )
        .unwrap_err()
        .to_string();

        // `opus` ships with allow_impl = false, and the override renamed the
        // design rung without saying the new one is design-only.
        assert!(
            error.contains(
                "team.tiers.deep places design-only member opus in an implementation tier"
            ),
            "unexpected error: {error}"
        );
        assert!(error.contains("team.policy.design_only_tiers"), "{error}");
    }

    #[test]
    fn policy_knobs_default_and_derive_from_the_roster() {
        let team = TeamConfig::default();
        assert_eq!(team.policy.max_retries, DEFAULT_TEAM_MAX_RETRIES);
        assert!(team.policy.redo_on_fallback);
        assert!(team.policy.prefer_sibling_on_quota);
        assert!(team.policy.record_provenance);
        assert!(team.policy.max_fallback_depth.is_none());
        assert!(team.policy.explore_tier.is_none());
        assert!(team.policy.review_tier.is_none());
        assert_eq!(team.policy.balance, Balance::Order);
        assert_eq!(team.policy.worker_summary_max_lines, 3);
        assert!(team.policy.isolate_conflicting);
        assert_eq!(team.policy.recursion, RecursionPolicy::default());
        assert!(!team.policy.recursion.enabled);
        assert_eq!(team.policy.recursion.effective_max_depth(), 0);
        assert_eq!(team.policy.recursion.effective_max_fan_out(), 0);
        assert_eq!(team.policy.recursion.effective_max_total_nodes(), 0);
        assert_eq!(team.policy.recursion.effective_max_tokens(), 0);
        assert_eq!(team.policy.recursion.effective_deadline_secs(), 0);
        assert_eq!(team.policy.recursion.effective_max_payload_bytes(), 0);
        // Derived from the roster, never a flat literal: a walk visits each
        // lane at most once.
        assert_eq!(team.effective_max_fallback_depth(), team.members.len());

        let pinned = Config::from_toml_str(
            r#"
            [team]
            enabled = true

            [team.policy]
            max_retries = 0
            redo_on_fallback = false
            prefer_sibling_on_quota = false
            max_fallback_depth = 1
            "#,
        )
        .unwrap()
        .team;
        assert_eq!(pinned.policy.max_retries, 0);
        assert!(!pinned.policy.redo_on_fallback);
        assert!(!pinned.policy.prefer_sibling_on_quota);
        assert!(pinned.policy.record_provenance);
        assert_eq!(pinned.effective_max_fallback_depth(), 1);
        assert!(!pinned.policy.is_default());
    }

    #[test]
    fn recursion_policy_is_backward_compatible_bounded_and_golden() {
        let legacy = Config::from_toml_str("[team.policy]\nmax_retries = 1\n").unwrap();
        assert_eq!(legacy.team.policy.recursion, RecursionPolicy::default());
        let serialized = toml::to_string(&legacy).unwrap();
        assert!(!serialized.contains("recursion"), "{serialized}");

        let source = r#"
            [team]
            enabled = true

            [team.policy.recursion]
            enabled = true
            max_depth = 2
            max_fan_out = 3
            max_total_nodes = 12
            max_tokens = 50000
            deadline_secs = 120
            max_payload_bytes = 4096
            subleader_tiers = ["design"]
        "#;
        let recursion = Config::from_toml_str(source).unwrap().team.policy.recursion;
        assert_eq!(recursion.effective_max_depth(), 2);
        assert_eq!(recursion.effective_max_fan_out(), 3);
        assert_eq!(recursion.effective_max_total_nodes(), 12);
        assert_eq!(recursion.effective_max_tokens(), 50_000);
        assert_eq!(recursion.effective_deadline_secs(), 120);
        assert_eq!(recursion.effective_max_payload_bytes(), 4096);
        assert!(recursion.may_sublead("design"));

        for invalid in [
            "[team.policy.recursion]\nmax_depth = 1",
            "[team.policy.recursion]\nenabled = true\nmax_depth = 0",
            "[team.policy.recursion]\nenabled = true\nmax_depth = 9",
            "[team.policy.recursion]\nenabled = true\nmax_depth = 1\nmax_fan_out = 0",
            "[team.policy.recursion]\nenabled = true\nmax_depth = 1\nmax_total_nodes = 2\nmax_fan_out = 4",
        ] {
            assert!(
                Config::from_toml_str(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn fallback_chain_is_breadth_first_and_bounded() {
        let team = TeamConfig::default();
        assert_eq!(
            team.fallback_chain("opus"),
            ["gpt-sol", "sonnet", "kimi-cloud"]
        );
        assert!(team.fallback_chain("kimi-cloud").is_empty());
        assert!(team.fallback_chain("missing").is_empty());

        let mut capped = TeamConfig::default();
        capped.policy.max_fallback_depth = Some(2);
        assert_eq!(capped.fallback_chain("opus"), ["gpt-sol", "sonnet"]);

        // Every direct replacement is offered before a replacement's own.
        let mut branching = TeamConfig::default();
        branching.members[0].fallback = vec!["glm-go".to_string(), "sonnet".to_string()];
        assert_eq!(
            branching.fallback_chain("opus"),
            ["glm-go", "sonnet", "kimi-cloud"]
        );
    }

    #[test]
    fn lane_fields_round_trip_through_toml() {
        let source = r#"
            [team]
            enabled = true
            leader_order = ["primary"]

            [team.tiers]
            mechanical = ["secondary"]
            deep = ["primary"]

            [team.policy]
            max_retries = 3
            design_only_tiers = ["deep"]

            [[team.members]]
            name = "primary"
            target = "claude"
            model = "opus"
            mode = "cli"
            roles = ["lead"]
            logical = "opus"
            allow_impl = false
            fallback = ["secondary"]

            [[team.members]]
            name = "secondary"
            target = "opencode"
            model = "opencode-go/glm-5.2"
            mode = "cli"
            roles = ["routine"]
            logical = "glm-5.2"

            [team.members.flags]
            permission-mode = "acceptEdits"
            future-safe-flag = "future-value"
        "#;

        let team = Config::from_toml_str(source).unwrap().team;
        assert!(!team.members[0].allow_impl);
        assert!(team.members[1].allow_impl);
        assert_eq!(team.members[1].flag("permission-mode"), Some("acceptEdits"));
        assert_eq!(
            team.members[1].flag("future-safe-flag"),
            Some("future-value")
        );
        assert_eq!(team.members[1].flag("nope"), None);
        assert_eq!(team.members[0].fallback, ["secondary"]);
        assert_eq!(
            team.effective_tiers().names().collect::<Vec<_>>(),
            ["mechanical", "deep"]
        );

        let serialized = toml::to_string(&team).unwrap();
        assert!(serialized.contains("allow_impl = false"));
        // The implementing lane keeps the default out of the file.
        assert_eq!(serialized.matches("allow_impl").count(), 1);
        for new_key in [
            "roster",
            "delegation",
            "host_agent",
            "explore_tier",
            "review_tier",
            "balance",
            "worker_summary_max_lines",
            "isolate_conflicting",
        ] {
            assert!(
                !serialized.contains(new_key),
                "defaulted {new_key} leaked into an old roster:\n{serialized}"
            );
        }
        let reparsed: TeamConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, team);
        assert_eq!(toml::to_string(&reparsed).unwrap(), serialized);
        reparsed.validate().unwrap();
    }

    #[test]
    fn unknown_lane_keys_are_still_rejected() {
        assert!(
            Config::from_toml_str(
                "[team]\nmembers = [{ name = \"x\", target = \"claude\", mode = \"cli\", \
                 roles = [\"lead\"], laneish = \"typo\" }]"
            )
            .is_err()
        );
        assert!(Config::from_toml_str("[team]\n[team.policy]\nmax_retry = 1\n").is_err());
    }

    #[test]
    fn disabled_team_allows_incomplete_topology() {
        let config =
            Config::from_toml_str("[team]\nenabled = false\nleader_order = []\nmembers = []\n")
                .unwrap();

        assert!(!config.team.enabled);
        assert!(config.team.leader_order.is_empty());
        assert!(config.team.members.is_empty());

        let legacy = Config::from_toml_str(
            r#"
            [team]
            enabled = false
            manager_provider = "ollama"
            manager_model = "other/model"
            leader_order = []

            [[team.members]]
            name = "legacy"
            target = "opencode"
            model = "legacy-unqualified-model"
            mode = "cli"
            roles = []
            host_agent = "../legacy-agent"
            "#,
        )
        .unwrap();
        assert!(!legacy.team.enabled);
    }

    #[test]
    fn default_team_is_omitted_from_serialization() {
        let serialized = toml::to_string(&Config::default()).unwrap();
        let value: toml::Value = toml::from_str(&serialized).unwrap();
        assert!(value.get("team").is_none());

        let mut config = Config::default();
        config.team.manager_model = "custom".to_string();
        let value: toml::Value = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert!(value.get("team").is_some());
    }

    #[test]
    fn embeddings_auto_defaults_and_old_configs_load() {
        // Empty config: auto-embed daemon knobs take their defaults.
        let c = Config::from_toml_str("").unwrap();
        assert!(!c.embeddings.enabled);
        assert!(c.embeddings.auto);
        assert_eq!(c.embeddings.auto_interval_sec, 120);
        assert_eq!(c.embeddings.auto_batch, 64);

        // An "old" [embeddings] block that predates the daemon knobs must still
        // load (serde default) and fill in the new fields.
        let c = Config::from_toml_str(
            r#"
            [embeddings]
            enabled = true
            model = "nomic-embed-text"
            "#,
        )
        .unwrap();
        assert!(c.embeddings.enabled);
        assert_eq!(c.embeddings.model, "nomic-embed-text");
        assert!(c.embeddings.auto);
        assert_eq!(c.embeddings.auto_interval_sec, 120);
        assert_eq!(c.embeddings.auto_batch, 64);

        // Explicit overrides win.
        let c = Config::from_toml_str(
            r#"
            [embeddings]
            enabled = true
            auto = false
            auto_interval_sec = 300
            auto_batch = 16
            "#,
        )
        .unwrap();
        assert!(!c.embeddings.auto);
        assert_eq!(c.embeddings.auto_interval_sec, 300);
        assert_eq!(c.embeddings.auto_batch, 16);
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let c = Config::from_toml_str(
            r#"
            [auto_compress]
            enabled = true
            model = "gemma3:4b"
            base_url = "http://127.0.0.1:11434/v1"
            provider = "ollama"
            min_chars = 256
            "#,
        )
        .unwrap();
        assert!(c.auto_compress.enabled);
        assert_eq!(c.auto_compress.model, "gemma3:4b");
        assert_eq!(
            c.auto_compress.base_url.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
        assert_eq!(c.auto_compress.min_chars, 256);
        assert_eq!(c.auto_compress.provider.as_deref(), Some("ollama"));
        assert_eq!(
            c.auto_compress
                .effective_provider("https://example.test/v1"),
            "ollama"
        );
        // unset field keeps its default
        assert_eq!(c.auto_compress.age_sec, 3600);
        // unrelated section still defaults
        assert!(c.capture.enabled);
    }

    #[test]
    fn compatible_provider_identity_is_only_inferred_for_default_ollama_url() {
        let config = AutoCompressConfig::default();
        for (url, expected) in [
            ("http://127.0.0.1:11434/v1", "ollama"),
            ("http://localhost:11434", "ollama"),
            ("http://[::1]:11434/v1/", "ollama"),
            ("http://192.168.1.2:11434/v1", "openai-compat"),
            ("http://127.0.0.1:8080/v1", "openai-compat"),
            ("https://azure.example/openai/v1", "openai-compat"),
        ] {
            assert_eq!(config.effective_provider(url), expected, "{url}");
        }
    }

    #[test]
    fn provider_names_normalize_case_and_explicit_aliases() {
        for (input, expected) in [
            (" OpenAI ", "openai"),
            ("OPENAI-COMPATIBLE", "openai-compat"),
            ("openai_compatible", "openai-compat"),
            ("LMStudio", "lm-studio"),
            ("lms", "lm-studio"),
            ("llama_cpp", "llama.cpp"),
            ("llama", "llama.cpp"),
            ("vLLM", "vllm"),
            ("Azure-West", "azure-west"),
        ] {
            assert_eq!(normalize_provider_id(input), expected, "{input}");
        }

        let mut providers = ProvidersConfig::default();
        providers.enabled.insert("OpenAI".to_string(), false);
        assert_eq!(providers.enabled_override("openai"), Some(false));
    }

    #[test]
    fn limits_targets_are_case_insensitive_without_becoming_provider_identity() {
        let mut limits = LimitsConfig::default();
        limits.targets.insert(
            "OpenCode".to_string(),
            TargetLimit {
                daily_tokens: Some(10),
                ..TargetLimit::default()
            },
        );
        assert_eq!(
            limits
                .target("opencode")
                .and_then(|limit| limit.daily_tokens),
            Some(10)
        );
    }

    #[test]
    fn malformed_toml_errors() {
        assert!(Config::from_toml_str("[auto_compress\nmodel =").is_err());
    }

    #[test]
    fn agent_and_provider_detect_overrides_load() {
        let c = Config::from_toml_str(
            r#"
            [agents]
            claude = true
            aider = false

            [providers]
            active = "openai"
            openrouter = false
            "#,
        )
        .unwrap();
        assert_eq!(c.agents.enabled_override("claude"), Some(true));
        assert_eq!(c.agents.enabled_override("aider"), Some(false));
        assert_eq!(c.agents.enabled_override("codex"), None);
        assert_eq!(c.providers.active.as_deref(), Some("openai"));
        assert_eq!(c.providers.enabled_override("openrouter"), Some(false));
    }

    #[test]
    fn limits_load_as_target_tables() {
        let c = Config::from_toml_str(
            r#"
            [limits.openai]
            daily_tokens = 1_000_000
            daily_requests = 2_000

            [limits.ollama]
            daily_tokens = 250_000
            "#,
        )
        .unwrap();

        let openai = c.limits.target("openai").unwrap();
        assert_eq!(openai.daily_tokens, Some(1_000_000));
        assert_eq!(openai.daily_requests, Some(2_000));
        let ollama = c.limits.target("ollama").unwrap();
        assert_eq!(ollama.daily_tokens, Some(250_000));
        assert_eq!(ollama.daily_requests, None);
        // Pool caps are opt-in: a legacy target table declares none.
        assert!(openai.pools.is_empty());
        assert!(!openai.has_pool_limits());
        assert_eq!(c.limits.pool("openai", "anything"), None);
    }

    #[test]
    fn legacy_limits_toml_round_trips_without_pool_tables() {
        let legacy = r#"
            [limits.openai]
            daily_tokens = 1000000
            daily_requests = 2000

            [limits.ollama]
            daily_tokens = 250000
        "#;
        let config = Config::from_toml_str(legacy).unwrap();
        let serialized = toml::to_string(&config.limits).unwrap();
        // The new `pools` field must not appear for a config that never set it,
        // otherwise every existing ~/.rtrt/config.toml would be rewritten.
        assert!(
            !serialized.contains("pools"),
            "empty pools must be skipped on serialize:\n{serialized}"
        );
        let reparsed: LimitsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.target("openai").unwrap().daily_tokens,
            Some(1_000_000)
        );
        assert_eq!(
            reparsed.target("openai").unwrap().daily_requests,
            Some(2_000)
        );
        assert_eq!(
            reparsed.target("ollama").unwrap().daily_tokens,
            Some(250_000)
        );
        assert_eq!(reparsed.target("ollama").unwrap().daily_requests, None);
        assert_eq!(toml::to_string(&reparsed).unwrap(), serialized);
    }

    #[test]
    fn pool_limits_nest_under_their_target() {
        let c = Config::from_toml_str(
            r#"
            [limits.opencode]
            daily_tokens = 2_000_000

            [limits.opencode.pools.opencode-go]
            daily_tokens = 1_200_000

            [limits.opencode.pools.ollama]
            daily_requests = 500
            "#,
        )
        .unwrap();

        let opencode = c.limits.target("opencode").unwrap();
        // The target-wide cap keeps working exactly as before.
        assert_eq!(opencode.daily_tokens, Some(2_000_000));
        assert!(opencode.has_pool_limits());

        let go = c.limits.pool("opencode", "opencode-go").unwrap();
        assert_eq!(go.daily_tokens, Some(1_200_000));
        assert_eq!(go.daily_requests, None);
        let ollama = c.limits.pool("opencode", "ollama").unwrap();
        assert_eq!(ollama.daily_tokens, None);
        assert_eq!(ollama.daily_requests, Some(500));
        // An unconfigured pool has no cap — never a slice of the target's.
        assert!(c.limits.pool("opencode", "unknown-pool").is_none());
        assert!(c.limits.pool("claude", "opencode-go").is_none());
    }

    #[test]
    fn pool_lookup_is_case_insensitive() {
        let c = Config::from_toml_str(
            r#"
            [limits.opencode.pools.OpenCode-Go]
            daily_tokens = 10
            "#,
        )
        .unwrap();
        // Pool names derived from model strings are lowercased, so a config key
        // written with capitals must still match.
        assert_eq!(
            c.limits
                .pool("opencode", "opencode-go")
                .unwrap()
                .daily_tokens,
            Some(10)
        );
    }

    #[test]
    fn pool_limits_round_trip_through_toml() {
        let source = r#"
            [limits.opencode]
            daily_requests = 100

            [limits.opencode.pools.ollama]
            daily_tokens = 42
        "#;
        let config = Config::from_toml_str(source).unwrap();
        let serialized = toml::to_string(&config.limits).unwrap();
        let reparsed: LimitsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.pool("opencode", "ollama").unwrap().daily_tokens,
            Some(42)
        );
        assert_eq!(
            reparsed.target("opencode").unwrap().daily_requests,
            Some(100)
        );
    }

    #[test]
    fn upsert_replaces_by_name_no_dup() {
        let mut c = Config::default();
        c.upsert_project(ProjectEntry {
            name: "alpha".to_string(),
            path: Some("/repo/alpha".to_string()),
            security_profile: None,
            embeddings_enabled: None,
        });
        c.upsert_project(ProjectEntry {
            name: "beta".to_string(),
            path: None,
            security_profile: Some("strict".to_string()),
            embeddings_enabled: None,
        });
        // replace alpha
        c.upsert_project(ProjectEntry {
            name: "alpha".to_string(),
            path: Some("/repo/alpha-2".to_string()),
            security_profile: Some("ai-default".to_string()),
            embeddings_enabled: None,
        });
        assert_eq!(c.projects.len(), 2);
        let alpha = c.project("alpha").unwrap();
        assert_eq!(alpha.path.as_deref(), Some("/repo/alpha-2"));
        assert_eq!(alpha.security_profile.as_deref(), Some("ai-default"));
    }

    #[test]
    fn api_max_tokens_loads_and_defaults() {
        // Absent → the safe default (no silent truncation to a tiny cap).
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.providers.api_max_tokens, None);
        assert_eq!(
            c.providers.effective_api_max_tokens(),
            DEFAULT_API_MAX_TOKENS
        );

        // Explicit value in [providers] wins; sibling flattened bool entries
        // (the enable map) must keep loading next to the typed field.
        let c = Config::from_toml_str(
            r#"
            [providers]
            active = "openai"
            api_max_tokens = 8192
            openrouter = false
            "#,
        )
        .unwrap();
        assert_eq!(c.providers.api_max_tokens, Some(8192));
        assert_eq!(c.providers.effective_api_max_tokens(), 8192);
        assert_eq!(c.providers.enabled_override("openrouter"), Some(false));

        // Zero is ignored — a typo must never truncate answers to nothing.
        let zeroed = ProvidersConfig {
            api_max_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(zeroed.effective_api_max_tokens(), DEFAULT_API_MAX_TOKENS);
    }

    #[test]
    fn project_override_carries_api_max_tokens() {
        let mut base = Config::from_toml_str(
            r#"
            [providers]
            api_max_tokens = 2048
            "#,
        )
        .unwrap();
        let over = ProjectConfig::from_toml_str(
            r#"
            [providers]
            api_max_tokens = 512
            "#,
        )
        .unwrap();
        assert!(!over.is_empty());
        base.apply_project_overrides(&over);
        assert_eq!(base.providers.api_max_tokens, Some(512));

        // An override without the field leaves the global value alone.
        let mut base = Config::from_toml_str(
            r#"
            [providers]
            api_max_tokens = 2048
            "#,
        )
        .unwrap();
        let over = ProjectConfig::from_toml_str(
            r#"
            [providers]
            active = "openai"
            "#,
        )
        .unwrap();
        base.apply_project_overrides(&over);
        assert_eq!(base.providers.api_max_tokens, Some(2048));
    }

    // -----------------------------------------------------------------------
    // Per-project orchestration overrides (`[team]` / `[failover]` in
    // `<repo>/.rtrt/config.toml`). These assert the LAYERING CONTRACT — whole
    // section replacement, validated before it can become effective — without
    // pinning any shipped lane, tier or model name.
    // -----------------------------------------------------------------------

    /// A global config whose roster shares no lane name with the project one
    /// below, so "replaced" and "merged" cannot be confused.
    fn global_with_roster() -> Config {
        Config::from_toml_str(
            r#"
            [team]
            enabled = true
            manager_provider = "global-manager"
            manager_model = "global-model"
            leader_order = ["global-lane"]

            [[team.members]]
            name = "global-lane"
            target = "global-target"
            mode = "cli"
            roles = ["lead"]

            [team.tiers]
            global-rung = ["global-lane"]

            [failover]
            quota = ["global marker"]
            transient_retries = 3
            "#,
        )
        .unwrap()
    }

    #[test]
    fn an_absent_orchestration_override_leaves_the_global_untouched() {
        let base = global_with_roster();
        let mut effective = base.clone();
        effective.apply_project_overrides(&ProjectConfig::default());
        assert_eq!(effective.team, base.team);
        assert_eq!(effective.failover, base.failover);
        // …and the empty override is never worth a file.
        assert!(ProjectConfig::default().is_empty());
    }

    #[test]
    fn a_project_roster_replaces_the_global_section_wholesale() {
        let mut base = global_with_roster();
        let over = ProjectConfig::from_toml_str(
            r#"
            [team]
            enabled = true
            manager_provider = "project-manager"
            manager_model = "project-model"
            leader_order = ["project-lane"]

            [[team.members]]
            name = "project-lane"
            target = "project-target"
            mode = "cli"
            roles = ["lead"]

            [failover]
            fatal = ["project marker"]
            "#,
        )
        .unwrap();
        assert!(!over.is_empty());
        base.apply_project_overrides(&over);

        // Replacement, not merge: nothing of the global roster survives — not
        // its lanes, not its leaders, not its ladder, not its manager.
        assert_eq!(base.team.manager_provider, "project-manager");
        assert_eq!(base.team.leader_order, ["project-lane"]);
        assert_eq!(
            base.team
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["project-lane"]
        );
        assert!(base.team.tiers.is_empty(), "the global ladder is dropped");
        // Same for the failure policy: the global `quota` marker and retry
        // count are gone rather than blended with the project's `fatal` list.
        assert_eq!(base.failover.fatal, ["project marker"]);
        assert!(base.failover.quota.is_empty());
        assert_eq!(base.failover.transient_retries, None);

        // The effective roster is exactly the validated override.
        base.team.validate().unwrap();
    }

    #[test]
    fn a_project_roster_that_cannot_be_valid_is_rejected_by_the_validator() {
        // A ladder rung naming a lane this roster does not define — the exact
        // shape a field-level merge could have synthesised silently.
        let err = ProjectConfig::from_toml_str(
            r#"
            [team]
            enabled = true
            leader_order = ["kept"]

            [[team.members]]
            name = "kept"
            target = "one"
            mode = "cli"
            roles = ["lead"]

            [team.tiers]
            rung = ["dropped"]
            "#,
        )
        .expect_err("a tier naming an unknown lane must not parse");
        let message = err.to_string();
        // The validator's own message, forwarded verbatim.
        assert!(
            message.contains("dropped") && message.contains("unknown member"),
            "expected the validator's message, got: {message}"
        );
    }

    /// A minimal usable lane: identity plus the one role the validator insists
    /// every lane declares.
    fn lane(name: &str) -> TeamMember {
        TeamMember {
            roles: vec!["work".to_string()],
            ..TeamMember::new(name, "some-target", TeamMode::Cli)
        }
    }

    #[test]
    fn save_project_rejects_an_invalid_roster_before_writing() {
        let repo = scratch_dir("rtrt-core-project-team");
        let over = ProjectConfig {
            team: Some(TeamConfig {
                enabled: true,
                leader_order: vec!["absent".to_string()],
                members: vec![lane("present")],
                ..TeamConfig::default()
            }),
            ..ProjectConfig::default()
        };
        let err = Config::save_project(&repo, &over).expect_err("invalid roster must not persist");
        assert!(
            err.to_string().contains("absent"),
            "expected the validator's message, got: {err}"
        );
        assert!(
            !Config::project_config_path(&repo).exists(),
            "a rejected override must not create the project config file"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn an_orchestration_override_round_trips_and_clears_without_touching_its_neighbours() {
        let repo = scratch_dir("rtrt-core-project-orch");

        // A project that already customises something else. Written first so
        // the later orchestration edits have a neighbour to preserve.
        let mut over = ProjectConfig {
            output_level: Some("lite".to_string()),
            ..ProjectConfig::default()
        };
        Config::save_project(&repo, &over).unwrap();
        let raw = std::fs::read_to_string(Config::project_config_path(&repo)).unwrap();
        assert!(
            !raw.contains("team") && !raw.contains("failover"),
            "an unset orchestration override must add no keys, got:\n{raw}"
        );
        // Round-trip: reading and rewriting an existing file is a no-op.
        let reread = Config::load_project(&repo).unwrap();
        assert!(reread.team.is_none() && reread.failover.is_none());
        Config::save_project(&repo, &reread).unwrap();
        assert_eq!(
            std::fs::read_to_string(Config::project_config_path(&repo)).unwrap(),
            raw
        );

        // Pin both orchestration sections for this project.
        over.team = Some(TeamConfig {
            enabled: true,
            leader_order: vec!["only".to_string()],
            members: vec![lane("only")],
            ..TeamConfig::default()
        });
        over.failover = Some(FailoverConfig {
            fatal: vec!["project marker".to_string()],
            ..FailoverConfig::default()
        });
        Config::save_project(&repo, &over).unwrap();
        let stored = Config::load_project(&repo).unwrap();
        assert_eq!(stored.team.as_ref().unwrap().leader_order, ["only"]);
        assert_eq!(
            stored.failover.as_ref().unwrap().fatal,
            ["project marker".to_string()]
        );
        assert_eq!(stored.output_level.as_deref(), Some("lite"));

        // Clearing `[team]` leaves `[failover]` and the unrelated override in
        // place — the file is rewritten from the whole ProjectConfig.
        let mut cleared = stored;
        cleared.team = None;
        Config::save_project(&repo, &cleared).unwrap();
        let after = Config::load_project(&repo).unwrap();
        assert!(after.team.is_none());
        assert!(after.failover.is_some());
        assert_eq!(after.output_level.as_deref(), Some("lite"));

        // Clearing the last override removes the file so the repo stays clean.
        let empty = ProjectConfig::default();
        assert!(empty.is_empty());
        Config::save_project(&repo, &empty).unwrap();
        assert!(!Config::project_config_path(&repo).exists());

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn project_and_global_config_reads_reject_oversize_before_parsing() {
        let repo = scratch_dir("rtrt-core-project-oversize");
        let config_dir = repo.join(".rtrt");
        std::fs::create_dir(&config_dir).unwrap();
        let project_path = config_dir.join("config.toml");
        let project_file = std::fs::File::create(&project_path).unwrap();
        project_file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        let error = Config::load_project(&repo).unwrap_err().to_string();
        assert!(error.contains("exceeds 1048576 bytes"), "{error}");

        let global_path = repo.join("global.toml");
        let global_file = std::fs::File::create(&global_path).unwrap();
        global_file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        let error = read_bounded_config(&global_path, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds 1048576 bytes"), "{error}");
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn oversized_save_preserves_existing_project_config_atomically() {
        let repo = scratch_dir("rtrt-core-project-atomic");
        let initial = ProjectConfig {
            output_level: Some("lite".to_string()),
            ..ProjectConfig::default()
        };
        Config::save_project(&repo, &initial).unwrap();
        let path = Config::project_config_path(&repo);
        let before = std::fs::read(&path).unwrap();

        let oversized = ProjectConfig {
            output_level: Some("x".repeat(MAX_CONFIG_BYTES as usize)),
            ..ProjectConfig::default()
        };
        let error = Config::save_project(&repo, &oversized)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds 1048576 bytes"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(repo.join(".rtrt")).unwrap().count(),
            1,
            "failed save left a sibling temporary file"
        );
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_config_rejects_symlink_traversal_and_symlink_targets() {
        use std::os::unix::fs::symlink;

        let repo = scratch_dir("rtrt-core-project-symlink");
        let outside = scratch_dir("rtrt-core-project-outside");
        symlink(&outside, repo.join(".rtrt")).unwrap();
        let over = ProjectConfig {
            output_level: Some("full".to_string()),
            ..ProjectConfig::default()
        };
        assert!(Config::save_project(&repo, &over).is_err());
        assert!(Config::load_project(&repo).is_err());
        assert!(!outside.join("config.toml").exists());
        std::fs::remove_file(repo.join(".rtrt")).unwrap();

        std::fs::create_dir(repo.join(".rtrt")).unwrap();
        let foreign = outside.join("foreign.toml");
        std::fs::write(&foreign, "output_level = \"lite\"\n").unwrap();
        let path = Config::project_config_path(&repo);
        symlink(&foreign, &path).unwrap();
        assert!(Config::load_project(&repo).is_err());
        assert!(Config::save_project(&repo, &over).is_err());
        assert!(Config::save_project(&repo, &ProjectConfig::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "output_level = \"lite\"\n"
        );
        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn project_config_rejects_non_directory_components_and_non_regular_targets() {
        let repo = scratch_dir("rtrt-core-project-components");
        std::fs::write(repo.join(".rtrt"), b"not a directory").unwrap();
        assert!(Config::load_project(&repo).is_err());
        assert!(Config::save_project(&repo, &ProjectConfig::default()).is_err());
        std::fs::remove_file(repo.join(".rtrt")).unwrap();

        std::fs::create_dir(repo.join(".rtrt")).unwrap();
        std::fs::create_dir(Config::project_config_path(&repo)).unwrap();
        assert!(Config::load_project(&repo).is_err());
        assert!(Config::save_project(&repo, &ProjectConfig::default()).is_err());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    /// A unique scratch directory for the file-touching tests above.
    fn scratch_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn repo_root_walks_up_to_rtrt_or_git_marker() {
        let root = std::env::temp_dir().join(format!(
            "rtrt-core-repo-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        // `repo_root_from` walks all the way to the filesystem root, so a
        // stray `.git`/`.rtrt` above the system temp dir (this machine has
        // one at `/tmp/.git`) would legitimately win and make the "no
        // marker yet" assertion depend on the environment. Bound the
        // ancestor walk to the fixture itself so the test only ever
        // inspects directories it created.
        let bounded_ancestors = || {
            let v: Vec<&Path> = std::iter::successors(Some(nested.as_path()), |d| d.parent())
                .take_while(|d| d.starts_with(&root))
                .collect();
            v
        };
        assert_eq!(repo_root_in(bounded_ancestors().into_iter()), None);
        std::fs::create_dir_all(root.join(".rtrt")).unwrap();
        assert_eq!(
            repo_root_in(bounded_ancestors().into_iter()),
            Some(root.clone())
        );

        // The unbounded production entry point still finds the marker we
        // just created (real markers above `root`, if any, are shadowed by
        // it since it's closer).
        assert_eq!(repo_root_from(&nested), Some(root.clone()));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn project_finds_and_none() {
        let mut c = Config::default();
        assert!(c.project("missing").is_none());
        c.upsert_project(ProjectEntry {
            name: "gamma".to_string(),
            path: None,
            security_profile: None,
            embeddings_enabled: None,
        });
        assert!(c.project("gamma").is_some());
        assert!(c.project("nope").is_none());
    }
}
