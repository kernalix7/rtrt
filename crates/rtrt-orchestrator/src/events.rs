use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{LeaseId, MessageId, NodeId, NodeState, RunId};

const EVENT_VERSION: u16 = 1;
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_EVENT_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// Operational metadata only. Prompt text, secrets, tool arguments, and model
/// output have no representable field in persisted events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventKind {
    NodeState {
        node_id: NodeId,
        state: NodeState,
    },
    MessageRelayed {
        message_id: MessageId,
        from: NodeId,
        to: NodeId,
    },
    LeaseGranted {
        lease_id: LeaseId,
        owner: NodeId,
        path_count: u32,
    },
    LeaseReleased {
        lease_id: LeaseId,
        forced: bool,
        preserved: bool,
    },
    WaveBoundary {
        wave: u32,
    },
    CancellationRequested {
        node_id: NodeId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u16,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub kind: EventKind,
}

impl Event {
    pub fn new(sequence: u64, timestamp_ms: u64, kind: EventKind) -> Self {
        Self {
            version: EVENT_VERSION,
            sequence,
            timestamp_ms,
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub events: Vec<Event>,
    pub truncated_tail: bool,
}

#[derive(Debug, Error)]
pub enum EventError {
    #[error("failed to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("unsafe run event path: {0}")]
    UnsafePath(PathBuf),
    #[error("event exceeds {MAX_EVENT_BYTES} bytes")]
    EventTooLarge,
    #[error("event log exceeds {MAX_EVENT_LOG_BYTES} bytes")]
    LogTooLarge,
    #[error("corrupt event at line {line}: {message}")]
    Corrupt { line: usize, message: String },
    #[error("unsupported event version {0}")]
    Version(u16),
    #[error("event sequence is not contiguous")]
    Sequence,
}

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    file: File,
    next_sequence: u64,
}

impl EventLog {
    pub fn open(
        project_root: impl AsRef<Path>,
        run_id: &RunId,
    ) -> Result<(Self, Replay), EventError> {
        let root = project_root
            .as_ref()
            .canonicalize()
            .map_err(|source| EventError::Io {
                operation: "canonicalize project root",
                source,
            })?;
        let rtrt = secure_directory(&root.join(".rtrt"), &root)?;
        let runs = secure_directory(&rtrt.join("runs"), &rtrt)?;
        let run = secure_directory(&runs.join(run_id.as_str()), &runs)?;
        let path = run.join("events.jsonl");
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(EventError::UnsafePath(path));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|source| EventError::Io {
                operation: "open event log",
                source,
            })?;
        set_private_file(&path)?;
        let replay = replay_and_repair(&mut file)?;
        let next_sequence = replay.events.last().map_or(0, |event| event.sequence + 1);
        Ok((
            Self {
                path,
                file,
                next_sequence,
            },
            replay,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn append(&mut self, timestamp_ms: u64, kind: EventKind) -> Result<Event, EventError> {
        let event = Event::new(self.next_sequence, timestamp_ms, kind);
        let mut encoded = serde_json::to_vec(&event).map_err(|error| EventError::Corrupt {
            line: 0,
            message: error.to_string(),
        })?;
        if encoded.len() > MAX_EVENT_BYTES {
            return Err(EventError::EventTooLarge);
        }
        encoded.push(b'\n');
        let size = self
            .file
            .metadata()
            .map_err(|source| EventError::Io {
                operation: "inspect event log",
                source,
            })?
            .len();
        if size.saturating_add(encoded.len() as u64) > MAX_EVENT_LOG_BYTES {
            return Err(EventError::LogTooLarge);
        }
        self.file
            .write_all(&encoded)
            .map_err(|source| EventError::Io {
                operation: "append event",
                source,
            })?;
        if matches!(event.kind, EventKind::WaveBoundary { .. }) {
            self.sync_wave()?;
        }
        self.next_sequence += 1;
        Ok(event)
    }

    pub fn sync_wave(&mut self) -> Result<(), EventError> {
        self.file.sync_all().map_err(|source| EventError::Io {
            operation: "fsync event log",
            source,
        })
    }
}

fn replay_and_repair(file: &mut File) -> Result<Replay, EventError> {
    let metadata = file.metadata().map_err(|source| EventError::Io {
        operation: "inspect event log",
        source,
    })?;
    if metadata.len() > MAX_EVENT_LOG_BYTES {
        return Err(EventError::LogTooLarge);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|source| EventError::Io {
            operation: "seek event log",
            source,
        })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|source| EventError::Io {
            operation: "read event log",
            source,
        })?;
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let truncated_tail = complete_len != bytes.len();
    let mut events = Vec::new();
    for (index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if line.len() > MAX_EVENT_BYTES {
            return Err(EventError::Corrupt {
                line: index + 1,
                message: "event too large".into(),
            });
        }
        let event: Event = serde_json::from_slice(line).map_err(|error| EventError::Corrupt {
            line: index + 1,
            message: error.to_string(),
        })?;
        if event.version != EVENT_VERSION {
            return Err(EventError::Version(event.version));
        }
        if event.sequence != events.len() as u64 {
            return Err(EventError::Sequence);
        }
        events.push(event);
    }
    if truncated_tail {
        file.set_len(complete_len as u64)
            .map_err(|source| EventError::Io {
                operation: "truncate crashed event tail",
                source,
            })?;
        file.sync_all().map_err(|source| EventError::Io {
            operation: "sync repaired event log",
            source,
        })?;
    }
    file.seek(SeekFrom::End(0))
        .map_err(|source| EventError::Io {
            operation: "seek event log end",
            source,
        })?;
    Ok(Replay {
        events,
        truncated_tail,
    })
}

fn secure_directory(path: &Path, parent: &Path) -> Result<PathBuf, EventError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(EventError::UnsafePath(path.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| EventError::Io {
                operation: "create event directory",
                source,
            })?
        }
        Err(source) => {
            return Err(EventError::Io {
                operation: "inspect event directory",
                source,
            });
        }
    }
    set_private_directory(path)?;
    let canonical = path.canonicalize().map_err(|source| EventError::Io {
        operation: "canonicalize event directory",
        source,
    })?;
    if canonical.parent() != Some(parent) {
        return Err(EventError::UnsafePath(canonical));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), EventError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| EventError::Io {
        operation: "chmod event directory",
        source,
    })
}
#[cfg(not(unix))]
fn set_private_directory(_: &Path) -> Result<(), EventError> {
    Ok(())
}
#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<(), EventError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| EventError::Io {
        operation: "chmod event log",
        source,
    })
}
#[cfg(not(unix))]
fn set_private_file(_: &Path) -> Result<(), EventError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn replay_repairs_only_crash_tail_and_rejects_corruption() {
        let temp = tempdir().unwrap();
        let run = RunId::new("run1").unwrap();
        let (mut log, _) = EventLog::open(temp.path(), &run).unwrap();
        log.append(1, EventKind::WaveBoundary { wave: 0 }).unwrap();
        let path = log.path().to_owned();
        drop(log);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{crash")
            .unwrap();
        let (_, replay) = EventLog::open(temp.path(), &run).unwrap();
        assert!(replay.truncated_tail);
        assert_eq!(replay.events.len(), 1);
        fs::write(&path, b"not-json\n").unwrap();
        assert!(matches!(
            EventLog::open(temp.path(), &run),
            Err(EventError::Corrupt { .. })
        ));
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_control_directory() {
        use std::os::unix::fs::symlink;
        let temp = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), temp.path().join(".rtrt")).unwrap();
        assert!(matches!(
            EventLog::open(temp.path(), &RunId::new("run1").unwrap()),
            Err(EventError::UnsafePath(_))
        ));

        fs::remove_file(temp.path().join(".rtrt")).unwrap();
        fs::create_dir(temp.path().join(".rtrt")).unwrap();
        fs::create_dir(temp.path().join(".rtrt/runs")).unwrap();
        symlink(outside.path(), temp.path().join(".rtrt/runs/run1")).unwrap();
        assert!(matches!(
            EventLog::open(temp.path(), &RunId::new("run1").unwrap()),
            Err(EventError::UnsafePath(_))
        ));
    }
}
