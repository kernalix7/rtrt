use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use super::WireEvent;

pub(super) struct Pending {
    pub event: WireEvent,
    pub attempts: u32,
}

fn now_millis() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn private_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("spool path needs parent directory")?;
    let mut current = std::path::PathBuf::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("spool path contains symlink")
            }
            Ok(metadata) if !metadata.is_dir() => bail!("spool path parent is not a directory"),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder
                    .create(&current)
                    .with_context(|| format!("create {}", current.display()))?;
            }
            Err(error) => return Err(error).context("inspect spool directory"),
        }
    }
    let metadata = fs::symlink_metadata(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o7777 != 0o700
            || metadata.uid() != crate::unsafe_geteuid()
        {
            bail!("spool directory must be operator-owned with mode 0700");
        }
    }
    Ok(())
}

pub(super) fn open_spool(path: &Path) -> Result<Connection> {
    private_parent(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("spool must be a regular file, not a symlink");
        }
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if metadata.permissions().mode() & 0o7777 != 0o600
                    || metadata.uid() != crate::unsafe_geteuid()
                {
                    bail!("spool file must be operator-owned with mode 0600");
                }
            }
            #[cfg(not(unix))]
            let _ = metadata;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).context("create private spool")?;
        }
        Err(error) => return Err(error).context("inspect spool"),
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS forward_spool (
           event_id TEXT PRIMARY KEY, payload TEXT NOT NULL,
           attempts INTEGER NOT NULL DEFAULT 0, next_attempt_ms INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    Ok(conn)
}

pub(super) fn enqueue(conn: &Connection, event: &WireEvent) -> Result<()> {
    let incoming = serde_json::to_value(event)?;
    let stored: Option<String> = conn
        .query_row(
            "SELECT payload FROM forward_spool WHERE event_id = ?1",
            params![event.event_id],
            |row| row.get(0),
        )
        .optional()
        .context("inspect queued forward event")?;
    if let Some(payload) = stored {
        let stored: serde_json::Value =
            serde_json::from_str(&payload).context("invalid stored forward event")?;
        if stored != incoming {
            bail!(
                "event id {} is already queued with a different payload",
                event.event_id
            );
        }
        return Ok(());
    }
    conn.execute(
        "INSERT INTO forward_spool (event_id, payload) VALUES (?1, ?2)",
        params![event.event_id, serde_json::to_string(event)?],
    )
    .context("enqueue forward event")?;
    Ok(())
}

pub(super) fn pending(conn: &Connection, limit: usize, force: bool) -> Result<Vec<Pending>> {
    let mut stmt = conn.prepare(
        "SELECT payload, attempts FROM forward_spool
         WHERE next_attempt_ms <= ?1 OR ?2 = 1
         ORDER BY CASE WHEN ?2 = 1 THEN rowid END DESC, rowid ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![now_millis()?, i32::from(force), i64::try_from(limit)?],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
    )?;
    rows.map(|row| {
        let (payload, attempts) = row?;
        Ok(Pending {
            event: serde_json::from_str(&payload).context("invalid stored forward event")?,
            attempts,
        })
    })
    .collect()
}

pub(super) fn delivered(conn: &Connection, event_id: &str) -> Result<()> {
    conn.execute("DELETE FROM forward_spool WHERE event_id = ?1", [event_id])?;
    Ok(())
}

pub(super) fn retry(conn: &Connection, event_id: &str, attempts: u32) -> Result<()> {
    let next =
        now_millis()?.saturating_add(i64::try_from(super::retry_delay(attempts).as_millis())?);
    conn.execute(
        "UPDATE forward_spool SET attempts = ?2, next_attempt_ms = ?3 WHERE event_id = ?1",
        params![event_id, attempts, next],
    )?;
    Ok(())
}
