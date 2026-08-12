//! Foundation types for isolated task orchestration.
//!
//! This crate deliberately does not execute task-provided commands. It exposes
//! a strict host protocol and a Git worktree manager whose process invocations
//! are fixed, validated argument vectors.

pub mod contract;
pub mod events;
pub mod leases;
pub mod protocol;
pub mod relay;
pub mod tree;
pub mod worktree;

pub use contract::{
    ContractError, DEFAULT_MAX_SUMMARY_LINES, ExecutionWave, FallbackProvenance, IsolationPlan,
    PlannedTask, ResultProvenance, TaskIsolation, TaskWriteSet, TestResult, TestStatus,
    WorkerResult, WorkerReturn, plan_isolation, write_sets_overlap,
};
pub use events::{Event, EventError, EventKind, EventLog, Replay};
pub use leases::{LeaseError, LeaseId, LeaseManager, WriteLease};
pub use protocol::{
    Cancel, Complete, Dispatch, Envelope, Failed, Hello, IdempotencyKey, IdentifierError, Message,
    MessageId, PeerRole, PinnedSha, PinnedShaError, ProtocolError, ProtocolVersion, Reconcile,
    Revision, RunCompletionStatus, RunId, RunStatus, Start, Started, TaskId, TaskOutcome,
    TaskResult, TaskSnapshot, TaskStatus, decode_json_line, encode_json_line,
};
pub use relay::{Mailbox, Relay, RelayError, RelayMessage, RelayOutcome};
pub use tree::{
    AdmissionError, CancellationStep, NodeId, NodeState, QuotaGrant, RecursionLimits, TeamNode,
    TeamTree,
};
pub use worktree::{
    CleanupReport, ManagedWorktree, PreservationReason, PreservedWorktree, WorktreeError,
    WorktreeKey, WorktreeManager, WorktreeStatus,
};
