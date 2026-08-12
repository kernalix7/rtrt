use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(u32);

impl NodeId {
    pub const ROOT: Self = Self(0);
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Running,
    Cancelling,
    Cancelled,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaGrant {
    pub nodes: u32,
    pub tokens: u64,
    pub deadline_ms: u64,
    pub payload_bytes: u32,
}

/// Runtime projection of `rtrt_core::config::RecursionPolicy`. Kept as a
/// transport DTO so orchestration core does not create a dependency cycle with
/// config/provider integration planned for Wave 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecursionLimits {
    pub max_depth: u8,
    pub max_fan_out: u16,
    pub max_total_nodes: u32,
    pub max_tokens: u64,
    pub deadline_secs: u64,
    pub max_payload_bytes: u32,
    pub subleader_tiers: Vec<String>,
}

impl RecursionLimits {
    pub fn may_sublead(&self, tier: &str) -> bool {
        self.subleader_tiers.iter().any(|allowed| allowed == tier)
    }
}

impl QuotaGrant {
    pub fn subgrant(&mut self, requested: Self, now_ms: u64) -> Result<Self, AdmissionError> {
        if requested.tokens == 0 || requested.payload_bytes == 0 {
            return Err(AdmissionError::ZeroGrant);
        }
        let node_cost = requested
            .nodes
            .checked_add(1)
            .ok_or(AdmissionError::QuotaExceeded)?;
        if node_cost > self.nodes
            || requested.tokens > self.tokens
            || requested.payload_bytes > self.payload_bytes
            || requested.deadline_ms > self.deadline_ms
            || requested.deadline_ms <= now_ms
        {
            return Err(AdmissionError::QuotaExceeded);
        }
        self.nodes -= node_cost;
        self.tokens -= requested.tokens;
        self.payload_bytes -= requested.payload_bytes;
        Ok(requested)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub depth: u8,
    pub tier: String,
    pub state: NodeState,
    pub grant: QuotaGrant,
    pub children: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancellationStep {
    pub node_id: NodeId,
    pub forced_lease_release: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("recursive orchestration is disabled")]
    Disabled,
    #[error("parent node does not exist")]
    UnknownParent,
    #[error("parent node is not running")]
    ParentNotRunning,
    #[error("tier is not permitted to lead a recursive team")]
    TierNotPermitted,
    #[error("maximum recursion depth exceeded")]
    DepthExceeded,
    #[error("maximum fan-out exceeded")]
    FanOutExceeded,
    #[error("maximum total node count exceeded")]
    TotalNodesExceeded,
    #[error("grant values must be non-zero")]
    ZeroGrant,
    #[error("subgrant exceeds parent quota or deadline")]
    QuotaExceeded,
    #[error("payload exceeds granted payload limit")]
    PayloadExceeded,
}

#[derive(Debug)]
pub struct TeamTree {
    policy: RecursionLimits,
    nodes: BTreeMap<NodeId, TeamNode>,
    next_id: u32,
}

impl TeamTree {
    pub fn new(policy: RecursionLimits, now_ms: u64) -> Result<Self, AdmissionError> {
        if policy.max_depth == 0 {
            return Err(AdmissionError::Disabled);
        }
        let deadline_ms = now_ms.saturating_add(policy.deadline_secs.saturating_mul(1000));
        let root = TeamNode {
            id: NodeId::ROOT,
            parent: None,
            depth: 0,
            tier: "root".into(),
            state: NodeState::Running,
            grant: QuotaGrant {
                nodes: policy.max_total_nodes.saturating_sub(1),
                tokens: policy.max_tokens,
                deadline_ms,
                payload_bytes: policy.max_payload_bytes,
            },
            children: Vec::new(),
        };
        Ok(Self {
            policy,
            nodes: BTreeMap::from([(NodeId::ROOT, root)]),
            next_id: 1,
        })
    }

    pub fn node(&self, id: NodeId) -> Option<&TeamNode> {
        self.nodes.get(&id)
    }
    pub fn nodes(&self) -> impl Iterator<Item = &TeamNode> {
        self.nodes.values()
    }
    pub fn is_direct_child(&self, parent: NodeId, child: NodeId) -> bool {
        self.nodes
            .get(&child)
            .is_some_and(|node| node.parent == Some(parent))
    }

    pub fn admit(
        &mut self,
        parent: NodeId,
        tier: impl Into<String>,
        grant: QuotaGrant,
        payload_len: usize,
        now_ms: u64,
    ) -> Result<NodeId, AdmissionError> {
        let tier = tier.into();
        let parent_snapshot = self
            .nodes
            .get(&parent)
            .ok_or(AdmissionError::UnknownParent)?;
        if parent_snapshot.state != NodeState::Running {
            return Err(AdmissionError::ParentNotRunning);
        }
        if parent != NodeId::ROOT && !self.policy.may_sublead(&parent_snapshot.tier) {
            return Err(AdmissionError::TierNotPermitted);
        }
        if parent_snapshot.depth >= self.policy.max_depth {
            return Err(AdmissionError::DepthExceeded);
        }
        if parent_snapshot.children.len() >= usize::from(self.policy.max_fan_out) {
            return Err(AdmissionError::FanOutExceeded);
        }
        if self.nodes.len() >= self.policy.max_total_nodes as usize {
            return Err(AdmissionError::TotalNodesExceeded);
        }
        if payload_len > grant.payload_bytes as usize {
            return Err(AdmissionError::PayloadExceeded);
        }
        let depth = parent_snapshot.depth + 1;
        let child_grant = self
            .nodes
            .get_mut(&parent)
            .expect("checked parent")
            .grant
            .subgrant(grant, now_ms)?;
        let id = NodeId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(AdmissionError::TotalNodesExceeded)?;
        self.nodes
            .get_mut(&parent)
            .expect("checked parent")
            .children
            .push(id);
        self.nodes.insert(
            id,
            TeamNode {
                id,
                parent: Some(parent),
                depth,
                tier,
                state: NodeState::Running,
                grant: child_grant,
                children: Vec::new(),
            },
        );
        Ok(id)
    }

    /// Marks subtree cancelling, then returns deterministic child-before-parent
    /// cancellation actions. Calling again only returns unfinished nodes.
    pub fn cancel_depth_first(
        &mut self,
        root: NodeId,
        grace_expired: bool,
    ) -> Result<Vec<CancellationStep>, AdmissionError> {
        self.request_cancellation(root)?;
        self.finish_cancellation(root, grace_expired)
    }

    /// Starts a grace period without releasing leases. Returned order is the
    /// required child-before-parent signal order.
    pub fn request_cancellation(&mut self, root: NodeId) -> Result<Vec<NodeId>, AdmissionError> {
        if !self.nodes.contains_key(&root) {
            return Err(AdmissionError::UnknownParent);
        }
        let mut order = Vec::new();
        self.postorder(root, &mut order);
        for id in &order {
            if let Some(node) = self.nodes.get_mut(id)
                && !matches!(
                    node.state,
                    NodeState::Cancelled | NodeState::Succeeded | NodeState::Failed
                )
            {
                node.state = NodeState::Cancelling;
            }
        }
        Ok(order)
    }

    /// Completes cancellation after graceful acknowledgement or grace expiry.
    /// `forced_lease_release` is true only for expiry-driven completion.
    pub fn finish_cancellation(
        &mut self,
        root: NodeId,
        grace_expired: bool,
    ) -> Result<Vec<CancellationStep>, AdmissionError> {
        if !self.nodes.contains_key(&root) {
            return Err(AdmissionError::UnknownParent);
        }
        let mut order = Vec::new();
        self.postorder(root, &mut order);
        let mut steps = Vec::new();
        for id in order {
            if let Some(node) = self.nodes.get_mut(&id)
                && node.state == NodeState::Cancelling
            {
                node.state = NodeState::Cancelled;
                steps.push(CancellationStep {
                    node_id: id,
                    forced_lease_release: grace_expired,
                });
            }
        }
        Ok(steps)
    }

    fn postorder(&self, id: NodeId, output: &mut Vec<NodeId>) {
        if let Some(node) = self.nodes.get(&id) {
            for child in &node.children {
                self.postorder(*child, output);
            }
            output.push(id);
        }
    }

    pub fn descendants(&self, id: NodeId) -> BTreeSet<NodeId> {
        let mut out = Vec::new();
        self.postorder(id, &mut out);
        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> RecursionLimits {
        RecursionLimits {
            max_depth: 3,
            max_fan_out: 4,
            max_total_nodes: 32,
            max_tokens: 1_000_000,
            deadline_secs: 3600,
            max_payload_bytes: 65_536,
            subleader_tiers: vec!["lead".into()],
        }
    }
    fn grant(nodes: u32, tokens: u64) -> QuotaGrant {
        QuotaGrant {
            nodes,
            tokens,
            deadline_ms: 10_000,
            payload_bytes: 100,
        }
    }

    #[test]
    fn quota_subgrants_are_conservative() {
        let mut parent = grant(4, 100);
        assert_eq!(parent.subgrant(grant(2, 40), 1).unwrap().tokens, 40);
        assert_eq!((parent.nodes, parent.tokens), (1, 60));
        assert_eq!(
            parent.subgrant(grant(3, 1), 1),
            Err(AdmissionError::QuotaExceeded)
        );
    }

    #[test]
    fn admission_and_cancellation_are_bounded_and_depth_first() {
        let mut tree = TeamTree::new(policy(), 0).unwrap();
        let a = tree
            .admit(NodeId::ROOT, "lead", grant(3, 100), 10, 1)
            .unwrap();
        let b = tree.admit(a, "worker", grant(1, 20), 10, 1).unwrap();
        assert_eq!(
            tree.request_cancellation(NodeId::ROOT).unwrap(),
            [b, a, NodeId::ROOT]
        );
        assert_eq!(tree.node(a).unwrap().state, NodeState::Cancelling);
        let order = tree.finish_cancellation(NodeId::ROOT, true).unwrap();
        assert_eq!(
            order.iter().map(|step| step.node_id).collect::<Vec<_>>(),
            [b, a, NodeId::ROOT]
        );
        assert!(order.iter().all(|step| step.forced_lease_release));
        assert!(
            tree.cancel_depth_first(NodeId::ROOT, true)
                .unwrap()
                .is_empty()
        );
    }
}
