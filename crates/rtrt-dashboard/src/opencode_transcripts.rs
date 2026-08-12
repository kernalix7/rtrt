//! Background capture for OpenCode's SQLite-backed session store.
//!
//! OpenCode's MCP configuration only makes memory tools callable; it does not
//! emit session turns to them. This watcher reads completed visible text from
//! OpenCode's own database and saves it into the same RTRT store as the Claude
//! transcript watcher.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rtrt_memory::{MemoryStore, is_synthetic_prompt};
use rusqlite::{Connection, OpenFlags, params};
use tokio::sync::Mutex;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
const BATCH_SIZE: i64 = 512;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_METADATA_BYTES: usize = 4096;

#[derive(Clone, Copy, Eq, PartialEq)]
struct DatabaseIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl DatabaseIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                created: metadata.created().ok(),
                modified: metadata.modified().ok(),
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Cursor {
    updated_ms: i64,
    part_rowid: i64,
}

#[derive(Debug)]
struct Candidate {
    cursor: Cursor,
    turn: Option<Turn>,
}

#[derive(Debug)]
struct Turn {
    text: String,
    session_id: String,
    parent_session: Option<String>,
    message_id: String,
    database: PathBuf,
    kind: &'static str,
    source_kind: &'static str,
}

pub fn spawn_transcript_watcher(
    memory: Option<Arc<Mutex<MemoryStore>>>,
    identity: Arc<rtrt_core::ProjectIdentity>,
) {
    let Some(memory) = memory else {
        tracing::info!("OpenCode transcript watcher disabled (memory store not available)");
        return;
    };
    let Some(database) = database_path() else {
        tracing::info!("OpenCode transcript watcher disabled ($HOME unset)");
        return;
    };
    if database.exists() {
        tracing::info!("OpenCode transcript watcher on: {}", database.display());
    } else {
        tracing::info!(
            "OpenCode transcript watcher waiting for: {}",
            database.display()
        );
    }

    tokio::spawn(async move {
        let mut cursor = Cursor::default();
        let mut tick = tokio::time::interval(DEFAULT_INTERVAL);
        loop {
            tick.tick().await;
            if !database.exists() {
                continue;
            }
            let candidates = match read_candidates(&database, &cursor, &identity) {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::warn!("OpenCode transcript sweep failed: {error}");
                    continue;
                }
            };
            for candidate in candidates {
                if let Some(turn) = candidate.turn
                    && let Err(error) = save_turn(&memory, &turn, identity.slug()).await
                {
                    tracing::warn!("OpenCode transcript save: {error}");
                    break;
                }
                cursor = candidate.cursor;
            }
        }
    });
}

fn database_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RTRT_OPENCODE_DB") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let root = PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("opencode");
    let channel = std::env::var("RTRT_OPENCODE_CHANNEL").unwrap_or_else(|_| "default".into());
    if channel == "default" {
        Some(root.join("opencode.db"))
    } else if !channel.is_empty()
        && channel.len() <= 64
        && channel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Some(root.join(channel).join("opencode.db"))
    } else {
        None
    }
}

fn read_candidates(
    database: &Path,
    cursor: &Cursor,
    identity: &rtrt_core::ProjectIdentity,
) -> anyhow::Result<Vec<Candidate>> {
    let before = std::fs::symlink_metadata(database)?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "OpenCode database is not a regular file"
    );
    let expected_identity = DatabaseIdentity::from_metadata(&before);
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "query_only", true)?;

    let mut statement = connection.prepare(
        "SELECT CASE WHEN length(CAST(s.directory AS BLOB)) <= ?4 THEN s.directory END,
                CASE WHEN length(CAST(s.id AS BLOB)) <= ?4 THEN s.id END,
                CASE WHEN length(CAST(s.parent_id AS BLOB)) <= ?4 THEN s.parent_id END,
                (s.parent_id IS NULL OR length(CAST(s.parent_id AS BLOB)) <= ?4),
                CASE WHEN length(CAST(m.id AS BLOB)) <= ?4 THEN m.id END,
                CASE WHEN length(CAST(p.id AS BLOB)) <= ?4 THEN p.id END,
                p.rowid,
                CASE
                    WHEN length(CAST(json_extract(m.data, '$.role') AS BLOB)) <= ?4
                    THEN json_extract(m.data, '$.role')
                    ELSE NULL
                END,
                CASE
                    WHEN length(CAST(json_extract(p.data, '$.text') AS BLOB)) <= ?5
                    THEN json_extract(p.data, '$.text')
                    ELSE NULL
                END,
                max(m.time_updated, p.time_updated) AS updated_ms
         FROM part p
         JOIN message m ON m.id = p.message_id
         JOIN session s ON s.id = m.session_id
         WHERE json_extract(p.data, '$.type') = 'text'
           AND (
                json_extract(m.data, '$.role') = 'user'
                OR (
                    json_extract(m.data, '$.role') = 'assistant'
                    AND json_extract(m.data, '$.time.completed') IS NOT NULL
                )
           )
           AND (
                max(m.time_updated, p.time_updated) > ?1
                OR (
                    max(m.time_updated, p.time_updated) = ?1
                    AND p.rowid > ?2
                )
           )
         ORDER BY updated_ms, p.rowid
         LIMIT ?3",
    )?;

    let rows = statement.query_map(
        params![
            cursor.updated_ms,
            cursor.part_rowid,
            BATCH_SIZE,
            MAX_METADATA_BYTES as i64,
            MAX_TEXT_BYTES as i64
        ],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
            ))
        },
    )?;

    let mut candidates = Vec::new();
    for row in rows {
        let (
            directory,
            session_id,
            parent_session,
            parent_session_valid,
            message_id,
            part_id,
            part_rowid,
            role,
            text,
            updated_ms,
        ) = row?;
        let text = text.as_deref().map(str::trim);
        let matches_project = directory
            .as_deref()
            .and_then(|directory| rtrt_core::ProjectIdentity::derive(directory).ok())
            .is_some_and(|candidate| candidate.fingerprint() == identity.fingerprint());
        let turn = if let (
            true,
            true,
            Some(session_id),
            Some(message_id),
            Some(_part_id),
            Some(role),
            Some(text),
        ) = (
            matches_project,
            parent_session_valid,
            session_id,
            message_id,
            part_id,
            role,
            text,
        ) && !text.is_empty()
            && (role != "user" || !is_synthetic_prompt(text))
        {
            let is_subagent = parent_session.is_some();
            Some(Turn {
                text: text.to_string(),
                session_id,
                parent_session,
                message_id,
                database: database.to_path_buf(),
                kind: match (role.as_str(), is_subagent) {
                    ("assistant", true) => "teammate-message",
                    ("assistant", false) => "assistant-turn",
                    ("user", _) => "user-prompt-submit",
                    _ => continue,
                },
                source_kind: if is_subagent { "subagent" } else { "main" },
            })
        } else {
            None
        };
        candidates.push(Candidate {
            cursor: Cursor {
                updated_ms,
                part_rowid,
            },
            turn,
        });
    }
    let after = std::fs::symlink_metadata(database)?;
    anyhow::ensure!(
        after.file_type().is_file() && DatabaseIdentity::from_metadata(&after) == expected_identity,
        "OpenCode database changed during sweep"
    );
    Ok(candidates)
}

async fn save_turn(
    memory: &Arc<Mutex<MemoryStore>>,
    turn: &Turn,
    pinned_project: &str,
) -> anyhow::Result<()> {
    let sha = MemoryStore::body_sha(&turn.text);
    let guard = memory.lock().await;
    if guard.body_seen_at(pinned_project, &sha)?.is_some() {
        return Ok(());
    }

    let mut metadata = BTreeMap::new();
    metadata.insert("source".into(), "opencode".into());
    metadata.insert("source_kind".into(), turn.source_kind.into());
    metadata.insert("session_id".into(), turn.session_id.clone());
    metadata.insert("message_id".into(), turn.message_id.clone());
    metadata.insert(
        "opencode_database".into(),
        turn.database.to_string_lossy().into_owned(),
    );
    if let Some(parent) = &turn.parent_session {
        metadata.insert("parent_session".into(), parent.clone());
    }

    let id = guard.save_with_metadata(pinned_project, turn.kind, &turn.text, &metadata)?;
    guard.tag_row(id, Some(&turn.session_id), Some(&sha))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        database: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let database = temp.path().join("opencode.db");
            let project = temp.path().join("project-alpha");
            std::fs::create_dir_all(project.join(".git")).unwrap();
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE session (
                        id TEXT PRIMARY KEY,
                        parent_id TEXT,
                        directory TEXT NOT NULL
                    );
                    CREATE TABLE message (
                        id TEXT PRIMARY KEY,
                        session_id TEXT NOT NULL,
                        time_updated INTEGER NOT NULL,
                        data TEXT NOT NULL
                    );
                    CREATE TABLE part (
                        id TEXT PRIMARY KEY,
                        message_id TEXT NOT NULL,
                        time_updated INTEGER NOT NULL,
                        data TEXT NOT NULL
                    );",
                )
                .unwrap();
            drop(connection);
            Self {
                _temp: temp,
                database,
                project,
            }
        }

        fn insert_session(&self, id: &str, parent_id: Option<&str>, directory: &str) {
            let connection = Connection::open(&self.database).unwrap();
            connection
                .execute(
                    "INSERT INTO session (id, parent_id, directory) VALUES (?1, ?2, ?3)",
                    params![id, parent_id, directory],
                )
                .unwrap();
        }

        fn insert_message(
            &self,
            id: &str,
            session_id: &str,
            role: &str,
            text: &str,
            updated_ms: i64,
            completed: bool,
        ) {
            let connection = Connection::open(&self.database).unwrap();
            let part_id = id.replacen("msg", "prt", 1);
            let mut message = serde_json::json!({"role": role});
            if completed {
                message["time"] = serde_json::json!({"completed": updated_ms});
            }
            connection
                .execute(
                    "INSERT INTO message (id, session_id, time_updated, data)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id, session_id, updated_ms, message.to_string()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO part (id, message_id, time_updated, data)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        part_id,
                        id,
                        updated_ms,
                        serde_json::json!({"type": "text", "text": text}).to_string()
                    ],
                )
                .unwrap();
        }
    }

    #[test]
    fn reads_completed_main_and_subagent_turns() {
        let fixture = Fixture::new();
        let directory = fixture.project.to_string_lossy();
        fixture.insert_session("ses_main", None, &directory);
        fixture.insert_session("ses_child", Some("ses_main"), &directory);
        fixture.insert_message("msg_1", "ses_main", "user", "hello", 100, false);
        fixture.insert_message("msg_2", "ses_main", "assistant", "answer", 200, true);
        fixture.insert_message("msg_3", "ses_child", "assistant", "child answer", 300, true);
        fixture.insert_message(
            "msg_4",
            "ses_main",
            "assistant",
            "still streaming",
            400,
            false,
        );

        let identity = rtrt_core::ProjectIdentity::derive(&fixture.project).unwrap();
        let candidates = read_candidates(&fixture.database, &Cursor::default(), &identity).unwrap();
        assert_eq!(candidates.len(), 3);
        let turns: Vec<&Turn> = candidates.iter().filter_map(|c| c.turn.as_ref()).collect();
        assert_eq!(turns[0].kind, "user-prompt-submit");
        assert_eq!(turns[0].source_kind, "main");
        assert_eq!(turns[1].kind, "assistant-turn");
        assert_eq!(turns[2].kind, "teammate-message");
        assert_eq!(turns[2].source_kind, "subagent");
        assert_eq!(turns[2].parent_session.as_deref(), Some("ses_main"));
    }

    #[test]
    fn cursor_resumes_after_the_last_saved_part() {
        let fixture = Fixture::new();
        let directory = fixture.project.to_string_lossy();
        fixture.insert_session("ses_main", None, &directory);
        fixture.insert_message("msg_1", "ses_main", "user", "one", 100, false);
        fixture.insert_message("msg_2", "ses_main", "user", "two", 200, false);

        let identity = rtrt_core::ProjectIdentity::derive(&fixture.project).unwrap();
        let first = read_candidates(&fixture.database, &Cursor::default(), &identity).unwrap();
        let resumed = read_candidates(&fixture.database, &first[0].cursor, &identity).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].turn.as_ref().unwrap().text, "two");
    }

    #[test]
    fn synthetic_prompts_advance_the_cursor_without_being_saved() {
        let fixture = Fixture::new();
        let directory = fixture.project.to_string_lossy();
        fixture.insert_session("ses_main", None, &directory);
        fixture.insert_message(
            "msg_1",
            "ses_main",
            "user",
            "<task-notification><task-id>internal</task-id></task-notification>",
            100,
            false,
        );

        let identity = rtrt_core::ProjectIdentity::derive(&fixture.project).unwrap();
        let candidates = read_candidates(&fixture.database, &Cursor::default(), &identity).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].turn.is_none());
        assert_eq!(candidates[0].cursor.updated_ms, 100);
    }

    #[test]
    fn same_basename_foreign_session_is_dropped_by_fingerprint() {
        let fixture = Fixture::new();
        let foreign_parent = fixture._temp.path().join("foreign");
        let foreign = foreign_parent.join("project-alpha");
        std::fs::create_dir_all(foreign.join(".git")).unwrap();
        fixture.insert_session("ses_foreign", None, &foreign.to_string_lossy());
        fixture.insert_message("msg_1", "ses_foreign", "user", "foreign", 100, false);
        let identity = rtrt_core::ProjectIdentity::derive(&fixture.project).unwrap();
        let candidates = read_candidates(&fixture.database, &Cursor::default(), &identity).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].turn.is_none());
    }

    #[test]
    fn oversized_text_advances_cursor_without_allocating_a_turn() {
        let fixture = Fixture::new();
        let directory = fixture.project.to_string_lossy();
        fixture.insert_session("ses_main", None, &directory);
        fixture.insert_message(
            "msg_1",
            "ses_main",
            "user",
            &"x".repeat(MAX_TEXT_BYTES + 1),
            100,
            false,
        );
        fixture.insert_message("msg_2", "ses_main", "user", "bounded", 200, false);

        let identity = rtrt_core::ProjectIdentity::derive(&fixture.project).unwrap();
        let candidates = read_candidates(&fixture.database, &Cursor::default(), &identity).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].turn.is_none());
        assert_eq!(candidates[1].turn.as_ref().unwrap().text, "bounded");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_database_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let link = fixture._temp.path().join("linked.db");
        symlink(&fixture.database, &link).unwrap();
        let identity = rtrt_core::ProjectIdentity::derive(&fixture.project).unwrap();
        assert!(read_candidates(&link, &Cursor::default(), &identity).is_err());
    }
}
