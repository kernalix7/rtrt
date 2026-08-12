use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{IsolationPlan, NodeId, TaskWriteSet, plan_isolation, write_sets_overlap};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(String);

impl LeaseId {
    pub fn generate() -> Self {
        Self(format!("lease_{}", Uuid::new_v4().simple()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteLease {
    pub id: LeaseId,
    pub owner: NodeId,
    pub writes: Vec<PathBuf>,
    pub expires_at_ms: u64,
    pub preserve: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LeaseError {
    #[error("write path must be a canonical relative path")]
    UnsafePath,
    #[error("write lease overlaps an active lease")]
    Overlap,
    #[error("lease does not exist")]
    UnknownLease,
    #[error("only lease owner may renew it")]
    WrongOwner,
    #[error("lease duration must be non-zero")]
    InvalidDuration,
}

#[derive(Debug, Default)]
pub struct LeaseManager {
    active: BTreeMap<LeaseId, WriteLease>,
    preserved: Vec<WriteLease>,
}

impl LeaseManager {
    pub fn active(&self) -> impl Iterator<Item = &WriteLease> {
        self.active.values()
    }
    pub fn preserved(&self) -> &[WriteLease] {
        &self.preserved
    }

    pub fn acquire(
        &mut self,
        owner: NodeId,
        writes: Vec<PathBuf>,
        now_ms: u64,
        ttl_ms: u64,
        preserve: bool,
    ) -> Result<WriteLease, LeaseError> {
        if ttl_ms == 0 {
            return Err(LeaseError::InvalidDuration);
        }
        self.expire(now_ms);
        let writes = canonical_write_set(writes)?;
        if self
            .active
            .values()
            .any(|lease| write_sets_overlap(&lease.writes, &writes))
        {
            return Err(LeaseError::Overlap);
        }
        let lease = WriteLease {
            id: LeaseId::generate(),
            owner,
            writes,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
            preserve,
        };
        self.active.insert(lease.id.clone(), lease.clone());
        Ok(lease)
    }

    pub fn renew(
        &mut self,
        id: &LeaseId,
        owner: NodeId,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<u64, LeaseError> {
        if ttl_ms == 0 {
            return Err(LeaseError::InvalidDuration);
        }
        self.expire(now_ms);
        let lease = self.active.get_mut(id).ok_or(LeaseError::UnknownLease)?;
        if lease.owner != owner {
            return Err(LeaseError::WrongOwner);
        }
        lease.expires_at_ms = now_ms.saturating_add(ttl_ms);
        Ok(lease.expires_at_ms)
    }

    pub fn revoke(&mut self, id: &LeaseId) -> Result<WriteLease, LeaseError> {
        let lease = self.active.remove(id).ok_or(LeaseError::UnknownLease)?;
        if lease.preserve {
            self.preserved.push(lease.clone());
        }
        Ok(lease)
    }

    pub fn revoke_owner(&mut self, owner: NodeId) -> Vec<WriteLease> {
        let ids = self
            .active
            .values()
            .filter(|lease| lease.owner == owner)
            .map(|lease| lease.id.clone())
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| self.revoke(&id).ok())
            .collect()
    }

    pub fn expire(&mut self, now_ms: u64) -> Vec<WriteLease> {
        let ids = self
            .active
            .values()
            .filter(|lease| lease.expires_at_ms <= now_ms)
            .map(|lease| lease.id.clone())
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| self.revoke(&id).ok())
            .collect()
    }

    pub fn plan(tasks: Vec<TaskWriteSet>) -> Result<IsolationPlan, LeaseError> {
        let tasks = tasks
            .into_iter()
            .map(|mut task| {
                task.writes = canonical_write_set(task.writes)?;
                Ok(task)
            })
            .collect::<Result<Vec<_>, LeaseError>>()?;
        Ok(plan_isolation(tasks))
    }
}

pub fn canonical_relative_path(path: &Path) -> Result<PathBuf, LeaseError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(LeaseError::UnsafePath);
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => clean.push(value),
            _ => return Err(LeaseError::UnsafePath),
        }
    }
    if clean.as_os_str().is_empty() {
        Err(LeaseError::UnsafePath)
    } else {
        Ok(clean)
    }
}

fn canonical_write_set(writes: Vec<PathBuf>) -> Result<Vec<PathBuf>, LeaseError> {
    let mut result = writes
        .into_iter()
        .map(|path| canonical_relative_path(&path))
        .collect::<Result<Vec<_>, _>>()?;
    result.sort();
    result.dedup();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskId;
    #[test]
    fn overlap_expiry_renewal_and_preservation() {
        let mut leases = LeaseManager::default();
        let first = leases
            .acquire(NodeId::ROOT, vec!["src".into()], 0, 10, true)
            .unwrap();
        assert_eq!(
            leases.acquire(NodeId::new(1), vec!["src/lib.rs".into()], 0, 10, false),
            Err(LeaseError::Overlap)
        );
        leases.renew(&first.id, NodeId::ROOT, 5, 20).unwrap();
        assert!(leases.expire(24).is_empty());
        assert_eq!(leases.expire(25).len(), 1);
        assert_eq!(leases.preserved().len(), 1);
    }
    #[test]
    fn paths_and_waves_are_canonical_and_serialized() {
        for unsafe_path in ["", "/tmp/x", "../x", "a/../b", "./x"] {
            assert_eq!(
                canonical_relative_path(Path::new(unsafe_path)),
                Err(LeaseError::UnsafePath)
            );
        }
        let task =
            |id, path| TaskWriteSet::new(TaskId::new(id).unwrap(), vec![PathBuf::from(path)]);
        let plan = LeaseManager::plan(vec![task("a", "src/a"), task("b", "src/a")]).unwrap();
        assert_eq!(plan.waves.len(), 2);
    }
}
