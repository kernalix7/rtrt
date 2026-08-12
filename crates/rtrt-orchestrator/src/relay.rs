use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{IdempotencyKey, MessageId, NodeId, TeamTree};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayMessage {
    pub message_id: MessageId,
    pub idempotency_key: IdempotencyKey,
    pub from: NodeId,
    pub to: NodeId,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayOutcome {
    Delivered,
    DuplicateMessage,
    IdempotencyReplay(RelayMessage),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RelayError {
    #[error("sender or recipient is not in the team tree")]
    UnknownNode,
    #[error("relay permits only self, parent, or direct-child routing")]
    Unroutable,
    #[error("relay payload exceeds configured bound")]
    PayloadTooLarge,
    #[error("recipient mailbox is full")]
    MailboxFull,
    #[error("message ID was reused with different content")]
    MessageIdConflict,
}

#[derive(Debug, Clone)]
pub struct Mailbox {
    capacity: usize,
    messages: VecDeque<RelayMessage>,
}

impl Mailbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            messages: VecDeque::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.messages.len()
    }
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
    pub fn pop(&mut self) -> Option<RelayMessage> {
        self.messages.pop_front()
    }
    fn push(&mut self, message: RelayMessage) -> Result<(), RelayError> {
        if self.messages.len() >= self.capacity {
            return Err(RelayError::MailboxFull);
        }
        self.messages.push_back(message);
        Ok(())
    }
}

#[derive(Debug)]
pub struct Relay {
    mailbox_capacity: usize,
    max_payload_bytes: usize,
    mailboxes: BTreeMap<NodeId, Mailbox>,
    messages: BTreeMap<MessageId, RelayMessage>,
    idempotency: BTreeMap<IdempotencyKey, RelayMessage>,
}

impl Relay {
    pub fn new(mailbox_capacity: usize, max_payload_bytes: usize) -> Self {
        Self {
            mailbox_capacity,
            max_payload_bytes,
            mailboxes: BTreeMap::new(),
            messages: BTreeMap::new(),
            idempotency: BTreeMap::new(),
        }
    }

    pub fn route(
        &mut self,
        tree: &TeamTree,
        message: RelayMessage,
    ) -> Result<RelayOutcome, RelayError> {
        if tree.node(message.from).is_none() || tree.node(message.to).is_none() {
            return Err(RelayError::UnknownNode);
        }
        let related = message.from == message.to
            || tree.is_direct_child(message.from, message.to)
            || tree.is_direct_child(message.to, message.from);
        if !related {
            return Err(RelayError::Unroutable);
        }
        if message.payload.len() > self.max_payload_bytes {
            return Err(RelayError::PayloadTooLarge);
        }
        if let Some(previous) = self.messages.get(&message.message_id) {
            return if previous == &message {
                Ok(RelayOutcome::DuplicateMessage)
            } else {
                Err(RelayError::MessageIdConflict)
            };
        }
        if let Some(previous) = self.idempotency.get(&message.idempotency_key) {
            return Ok(RelayOutcome::IdempotencyReplay(previous.clone()));
        }
        self.mailboxes
            .entry(message.to)
            .or_insert_with(|| Mailbox::new(self.mailbox_capacity))
            .push(message.clone())?;
        self.messages
            .insert(message.message_id.clone(), message.clone());
        self.idempotency
            .insert(message.idempotency_key.clone(), message);
        Ok(RelayOutcome::Delivered)
    }

    pub fn mailbox(&mut self, node: NodeId) -> &mut Mailbox {
        self.mailboxes
            .entry(node)
            .or_insert_with(|| Mailbox::new(self.mailbox_capacity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QuotaGrant, RecursionLimits};
    fn message(from: NodeId, to: NodeId, id: &str, key: &str) -> RelayMessage {
        RelayMessage {
            message_id: MessageId::new(id).unwrap(),
            idempotency_key: IdempotencyKey::new(key).unwrap(),
            from,
            to,
            payload: "ok".into(),
        }
    }
    #[test]
    fn rejects_siblings_and_deduplicates_delivery() {
        let policy = RecursionLimits {
            max_depth: 2,
            max_fan_out: 4,
            max_total_nodes: 32,
            max_tokens: 1_000_000,
            deadline_secs: 3600,
            max_payload_bytes: 65_536,
            subleader_tiers: Vec::new(),
        };
        let mut tree = TeamTree::new(policy, 0).unwrap();
        let grant = || QuotaGrant {
            nodes: 1,
            tokens: 10,
            deadline_ms: 1000,
            payload_bytes: 10,
        };
        let a = tree.admit(NodeId::ROOT, "worker", grant(), 2, 1).unwrap();
        let b = tree.admit(NodeId::ROOT, "worker", grant(), 2, 1).unwrap();
        let mut relay = Relay::new(2, 10);
        assert_eq!(
            relay.route(&tree, message(a, b, "m1", "k1")),
            Err(RelayError::Unroutable)
        );
        let sent = message(a, NodeId::ROOT, "m2", "k2");
        assert_eq!(
            relay.route(&tree, sent.clone()).unwrap(),
            RelayOutcome::Delivered
        );
        assert_eq!(
            relay.route(&tree, sent).unwrap(),
            RelayOutcome::DuplicateMessage
        );
        assert!(matches!(
            relay
                .route(&tree, message(a, NodeId::ROOT, "m3", "k2"))
                .unwrap(),
            RelayOutcome::IdempotencyReplay(_)
        ));
        assert_eq!(relay.mailbox(NodeId::ROOT).len(), 1);
    }
}
