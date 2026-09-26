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

#[cfg(any(target_os = "linux", target_os = "android"))]
fn parse_effective_uid(status: &str) -> Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok())
        .context("cannot parse effective uid from /proc/self/status")
}

#[cfg(unix)]
fn effective_uid() -> Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let status = fs::read_to_string("/proc/self/status").context("read /proc/self/status")?;
        parse_effective_uid(&status)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let command = if Path::new("/usr/bin/id").is_file() {
            "/usr/bin/id"
        } else {
            "/bin/id"
        };
        let output = std::process::Command::new(command)
            .arg("-u")
            .output()
            .context("run id -u")?;
        if !output.status.success() {
            bail!("id -u failed");
        }
        let uid = std::str::from_utf8(&output.stdout).context("id -u output is not UTF-8")?;
        uid.trim()
            .parse::<u32>()
            .context("cannot parse uid from id -u")
    }
}

#[cfg(unix)]
fn owned_with_mode(metadata: &fs::Metadata, uid: u32, mode: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    metadata.permissions().mode() & 0o7777 == mode && metadata.uid() == uid
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
                #[cfg(unix)]
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                #[cfg(not(unix))]
                let builder = fs::DirBuilder::new();
                builder
                    .create(&current)
                    .with_context(|| format!("create {}", current.display()))?;
            }
            Err(error) => return Err(error).context("inspect spool directory"),
        }
    }
    #[cfg(unix)]
    {
        let metadata = fs::symlink_metadata(parent)?;
        if !owned_with_mode(&metadata, effective_uid()?, 0o700) {
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
                if !owned_with_mode(&metadata, effective_uid()?, 0o600) {
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
            let file = options.open(path).context("create private spool")?;
            #[cfg(unix)]
            {
                let metadata = file.metadata().context("inspect new spool")?;
                if !owned_with_mode(&metadata, effective_uid()?, 0o600) {
                    bail!("spool file must be operator-owned with mode 0600");
                }
            }
            drop(file);
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn effective_uid_parser_selects_effective_field_when_real_uid_differs() {
        // Given distinct real, effective, saved and filesystem UIDs.
        let status = "Name:\trtrt\nUid:\t1000\t1001\t1002\t1003\nGid:\t1000\t1000\t1000\t1000\n";
        // When the status is parsed.
        // Then ownership uses the effective UID, not the first or last field.
        assert_eq!(parse_effective_uid(status).unwrap(), 1001);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn effective_uid_parser_rejects_missing_or_malformed_effective_field() {
        // Given missing, incomplete and invalid Uid fields.
        for status in [
            "Name:\trtrt\nGid:\t1000\t1000\n",
            "Uid:\t1000\n",
            "Uid:\t1000\tbad\t1002\t1003\n",
            "Uid:\t1000\t-1\t1002\t1003\n",
            "Uid:\t1000\t4294967296\t1002\t1003\n",
        ] {
            // When the status is parsed.
            // Then there is no fallback to a guessed UID.
            assert!(parse_effective_uid(status).is_err(), "{status:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_mode_predicate_rejects_mismatched_uid_and_special_bits() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // Given an operator-owned file with mode 0600.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let uid = metadata.uid();

        // When checking owner and exact mode without changing ownership.
        // Then only a matching owner and mode are accepted.
        assert!(owned_with_mode(&metadata, uid, 0o600));
        assert!(!owned_with_mode(&metadata, uid.wrapping_add(1), 0o600));
        assert!(!owned_with_mode(&metadata, uid, 0o700));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4600)).unwrap();
        assert!(!owned_with_mode(
            &std::fs::metadata(&path).unwrap(),
            uid,
            0o600
        ));
    }
}
