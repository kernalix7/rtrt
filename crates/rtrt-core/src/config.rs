use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{CompressionLevel, Error, Result};

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
    /// invokes the lane (e.g. `permission-mode` / `allowed-tools` for a
    /// `claude -p` lane). rtrt stores and renders them; it does not interpret
    /// them, so a new upstream flag needs no rtrt release.
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamConfig {
    #[serde(default)]
    pub enabled: bool,
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

    pub fn validate(&self) -> Result<()> {
        validate_team_value("manager_provider", &self.manager_provider)?;
        validate_team_value("manager_model", &self.manager_model)?;
        if let Some(base_url) = &self.manager_base_url {
            validate_team_value("manager_base_url", base_url)?;
        }
        if !self.enabled {
            return Ok(());
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
        for (index, member) in self.members.iter().enumerate() {
            validate_team_value(&format!("members[{index}].name"), &member.name)?;
            validate_team_value(&format!("members[{index}].target"), &member.target)?;
            if let Some(model) = &member.model {
                validate_team_value(&format!("members[{index}].model"), model)?;
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
        if let Some(tier) = &self.policy.default_tier {
            validate_team_value("policy.default_tier", tier)?;
            if !effective.contains(tier) {
                return Err(Error::Config(format!(
                    "team.policy.default_tier references unknown tier: {tier}"
                )));
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
    /// Tiers whose lanes plan rather than implement; the only tiers a member
    /// with `allow_impl = false` may appear in. `None` uses the shipped name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_only_tiers: Option<Vec<String>>,
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
            design_only_tiers: None,
        }
    }
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

/// NUL check only — for values that may legitimately be empty, such as a
/// valueless invocation flag.
fn validate_team_text(name: &str, value: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(Error::Config(format!("team.{name} must not contain NUL")));
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
}

impl ProjectConfig {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(format!("project config TOML: {e}")))
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
            interval_sec: default_compress_interval(),
            age_sec: default_compress_age(),
            min_chars: default_compress_min_chars(),
            batch: default_compress_batch(),
            max_tokens: default_compress_max_tokens(),
        }
    }
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

/// Canonical default memory store: `~/.rtrt/memory.sqlite`.
///
/// Every surface (CLI, MCP server, dashboard, hooks, services) must resolve
/// the store through this function when no explicit `--store` /
/// `RTRT_MEMORY_PATH` override is given, so a fresh install reads and writes
/// one SQLite file instead of scattering cwd-relative stores per directory.
pub fn default_memory_store_path() -> PathBuf {
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
        self.enabled.get(name).copied()
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(name.to_string(), enabled);
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
        self.targets.get(name)
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
                let raw = std::fs::read_to_string(&p)
                    .map_err(|e| Error::Config(format!("read {}: {e}", p.display())))?;
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
        match std::fs::read_to_string(Self::project_config_path(repo)) {
            Ok(raw) => ProjectConfig::from_toml_str(&raw),
            Err(_) => Ok(ProjectConfig::default()),
        }
    }

    /// Load the global config and overlay a project's customization overrides.
    /// The base kernel is never overlaid — only the customization layer
    /// (output level, compression, enabled agents/providers).
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
    }

    /// Write a project override file, creating `.rtrt/` as needed. When the
    /// override is empty the file is removed so the repo stays clean.
    pub fn save_project(repo: &Path, over: &ProjectConfig) -> Result<()> {
        let path = Self::project_config_path(repo);
        if over.is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        let body = toml::to_string_pretty(over)
            .map_err(|e| Error::Config(format!("serialize project config: {e}")))?;
        std::fs::write(&path, body)
            .map_err(|e| Error::Config(format!("write {}: {e}", path.display())))?;
        Ok(())
    }
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
            model = "first"
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
            allowed-tools = "Read,Edit"
        "#;

        let team = Config::from_toml_str(source).unwrap().team;
        assert!(!team.members[0].allow_impl);
        assert!(team.members[1].allow_impl);
        assert_eq!(team.members[1].flag("permission-mode"), Some("acceptEdits"));
        assert_eq!(team.members[1].flag("allowed-tools"), Some("Read,Edit"));
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
        // unset field keeps its default
        assert_eq!(c.auto_compress.age_sec, 3600);
        // unrelated section still defaults
        assert!(c.capture.enabled);
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
