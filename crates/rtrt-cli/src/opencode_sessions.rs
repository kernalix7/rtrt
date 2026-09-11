//! Lossless, content-oblivious migration of OpenCode's global SQLite graph.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use serde_json::json;

use rtrt_core::ProjectIdentity;

const GRAPH_TABLES: &[&str] = &[
    "project",
    "project_directory",
    "workspace",
    "session",
    "message",
    "part",
    "todo",
    "session_share",
    "session_message",
    "session_input",
    "session_context_epoch",
    "event_sequence",
    "event",
    // Older resumable graph names remain supported for existing databases.
    "share",
    "session_projection",
    "projection",
    "session_event",
    "session_diff",
    "snapshot",
];
const SCHEMA_TABLES: &[&str] = &[
    "__drizzle_migrations",
    "_prisma_migrations",
    "migration",
    "data_migration",
];
const SENSITIVE_TABLES: &[&str] = &[
    "account",
    "account_state",
    "credential",
    "permission",
    "approval",
    "auth",
    "authentication",
    "authorization",
    "control_account",
];
const MAX_TABLES: usize = 128;
const MAX_COLUMNS: usize = 128;
const MAX_SCHEMA_OBJECTS: usize = MAX_TABLES * 4;
const MAX_SCHEMA_SQL_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROJECT_CACHE_BYTES: u64 = 128;
const RUNTIME_CHECKPOINT_VERSION: u64 = 1;
const MAX_RUNTIME_CHECKPOINT_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationMode {
    Status,
    DryRun,
    Apply,
    /// Incremental launcher policy: preserve private conflicts and do not wait
    /// behind another launcher. Not exposed as a CLI action.
    CatchUp,
}

#[derive(Clone, Debug)]
pub struct MigrationOptions {
    pub home: PathBuf,
    pub source: Option<PathBuf>,
    pub mode: MigrationMode,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MigrationReport {
    pub source: Option<PathBuf>,
    pub sessions: usize,
    pub projects: usize,
    pub archived_sessions: usize,
    pub skipped_malformed_sessions: usize,
    pub rows: usize,
    pub changed_rows: usize,
    pub conflicts: usize,
    pub private_preserved_conflicts: usize,
    pub archived_event_forks: usize,
    pub up_to_date: bool,
    pub skipped_locked: bool,
}

/// Result of validating and repairing one already-prepared private OpenCode DB.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeRepair {
    NeedsProbe,
    Complete(RuntimeRepairReport),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeRepairReport {
    pub changed_rows: usize,
    pub quarantined_sessions: usize,
    /// Category-only relative archive path; never contains session data.
    pub archive: Option<PathBuf>,
    pub valid_root_sessions: usize,
    pub checkpoint_hit: bool,
}

#[derive(Clone, Debug)]
struct Table {
    name: String,
    sql: String,
    columns: Vec<String>,
    primary_key: Vec<usize>,
    column_specs: Vec<ColumnSpec>,
    foreign_keys: Vec<ForeignKeySpec>,
    without_rowid: bool,
    strict: bool,
    sql_signature: String,
    create_schema: bool,
    copy_rows: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ColumnSpec {
    name: String,
    ty: String,
    not_null: bool,
    default: Option<String>,
    primary_key_order: i64,
    hidden: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ForeignKeySpec {
    id: i64,
    sequence: i64,
    table: String,
    from: String,
    to: Option<String>,
    on_update: String,
    on_delete: String,
    match_clause: String,
}

#[derive(Clone, Debug)]
struct Index {
    name: String,
    table: String,
    sql: String,
    unique: bool,
    columns: Vec<IndexColumnSpec>,
    partial: bool,
    sql_signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IndexColumnSpec {
    cid: i64,
    name: Option<String>,
    descending: bool,
    collation: String,
    key: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Row {
    values: Vec<Value>,
}

type RowsByTable = BTreeMap<String, Vec<Row>>;

struct PreparedSource {
    tables: Vec<Table>,
    indexes: Vec<Index>,
    plans: Vec<(String, RowsByTable)>,
    report: MigrationReport,
    generation: SourceGeneration,
}

struct OperationLock {
    _connection: Connection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceGeneration(String);

/// Discover the sole supported global source without consulting cwd or
/// OPENCODE_DB (which may point at an RTRT-private database).
pub fn discover_global_source(home: &Path) -> Result<Option<PathBuf>> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let source = data.join("opencode/opencode.db");
    match fs::symlink_metadata(&source) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            bail!(
                "global OpenCode database is not a regular non-symlink file: {}",
                source.display()
            )
        }
        Ok(_) => Ok(Some(source)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", source.display())),
    }
}

pub fn migrate(options: &MigrationOptions) -> Result<MigrationReport> {
    let source = match &options.source {
        Some(path) => {
            validate_source(path)?;
            Some(path.clone())
        }
        None => discover_global_source(&options.home)?,
    };
    let Some(source) = source else {
        return Ok(MigrationReport::default());
    };
    validate_source(&source)?;
    let root = options.home.join(".rtrt");
    if source.starts_with(root.join("projects")) {
        bail!("refusing to treat an RTRT-private OpenCode database as global source")
    }

    let writes = matches!(options.mode, MigrationMode::Apply | MigrationMode::CatchUp);
    let lock = if writes {
        super::ensure_private_directory(&root)?;
        match acquire_lock(
            &root.join("opencode-session-migration.lock.sqlite"),
            options.mode,
        )? {
            Some(lock) => Some(lock),
            None => {
                return Ok(MigrationReport {
                    source: Some(source),
                    skipped_locked: true,
                    ..MigrationReport::default()
                });
            }
        }
    } else {
        None
    };

    // Check only after taking the operation lock, then take a fresh bounded
    // DB+WAL stamp. This avoids an expensive SQLite/schema scan on every
    // launcher while ensuring the manifest and observed generation belong to
    // one serialized migration operation.
    if options.mode == MigrationMode::CatchUp {
        let generation = source_generation(&source)?;
        if manifest_generation(&root)?.as_ref() == Some(&generation) {
            return Ok(MigrationReport {
                source: Some(source),
                up_to_date: true,
                ..MigrationReport::default()
            });
        }
    }

    let PreparedSource {
        tables,
        indexes,
        plans,
        mut report,
        generation,
    } = prepare_source(&source, options.mode)?;
    if !writes {
        return Ok(report);
    }
    for (destination, _) in &plans {
        validate_destination_before_merge(
            &destination_db(&options.home, destination),
            &tables,
            &indexes,
        )?;
    }
    for (destination, selected) in plans {
        let db = destination_db(&options.home, &destination);
        let outcome = merge_destination(&db, &tables, &indexes, &selected)?;
        report.changed_rows += outcome.changed;
        report.conflicts += outcome.conflicts;
        report.private_preserved_conflicts += outcome.private_preserved_conflicts;
        report.archived_event_forks += outcome.archived_event_forks;
    }
    report.up_to_date = false;
    write_manifest(&root, &source, &generation, &report)?;
    drop(lock);
    Ok(report)
}

/// Read OpenCode's own project selector from an already sandbox-validated
/// common Git directory. Missing cache is meaningful; malformed or exchanged
/// files fail closed.
pub fn read_runtime_project_cache(common_git_dir: &Path) -> Result<Option<String>> {
    let path = common_git_dir.join("opencode");
    let before = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if before.file_type().is_symlink() || !before.is_file() {
        bail!("OpenCode project cache is not a regular non-symlink file")
    }
    if before.len() > MAX_PROJECT_CACHE_BYTES {
        bail!("OpenCode project cache exceeds bounded size")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.uid() != super::unsafe_geteuid() {
            bail!("OpenCode project cache has unsafe owner")
        }
    }
    let file = fs::File::open(&path)?;
    let opened = file.metadata()?;
    let current = fs::symlink_metadata(&path)?;
    if current.file_type().is_symlink()
        || !current.is_file()
        || opened.len() > MAX_PROJECT_CACHE_BYTES
    {
        bail!("OpenCode project cache changed during validation")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != before.dev()
            || opened.ino() != before.ino()
            || current.dev() != opened.dev()
            || current.ino() != opened.ino()
            || opened.uid() != super::unsafe_geteuid()
        {
            bail!("OpenCode project cache changed during validation")
        }
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.len())?);
    file.take(MAX_PROJECT_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROJECT_CACHE_BYTES {
        bail!("OpenCode project cache exceeds bounded size")
    }
    let raw = std::str::from_utf8(&bytes).context("OpenCode project cache is not UTF-8")?;
    let id = raw.trim();
    if !valid_runtime_project_id(id) || raw.split_whitespace().count() != 1 {
        bail!("OpenCode project cache contains an invalid project ID")
    }
    Ok(Some(id.to_string()))
}

fn valid_runtime_project_id(id: &str) -> bool {
    id.len() == 40
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Repair runtime attribution and quarantine undecodable sessions in exactly
/// one prepared private DB. `probe_completed` may only be set after launching
/// trusted OpenCode against this same DB and authoritative project selector.
pub fn repair_runtime_attribution(
    db: &Path,
    authoritative: &str,
    probe_completed: bool,
) -> Result<RuntimeRepair> {
    if authoritative != "global" && !valid_runtime_project_id(authoritative) {
        bail!("invalid authoritative OpenCode runtime project ID")
    }
    super::ensure_private_file(db)?;
    let generation = source_generation(db)?;
    if let Some(roots) = read_runtime_checkpoint(db, authoritative, &generation)? {
        if source_generation(db)? == generation {
            return Ok(RuntimeRepair::Complete(RuntimeRepairReport {
                valid_root_sessions: roots,
                checkpoint_hit: true,
                ..RuntimeRepairReport::default()
            }));
        }
    }
    // Absence of OpenCode's cache is authoritative for global only after a
    // successful probe. A stale/missing checkpoint must never revive that
    // inference from copied project_directory rows.
    if authoritative == "global" && !probe_completed {
        return Ok(RuntimeRepair::NeedsProbe);
    }
    #[cfg(test)]
    FULL_REPAIR_SCANS.with(|scans| scans.set(scans.get() + 1));
    let mut conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("private OpenCode database integrity check failed before runtime repair")
    }
    let violations: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        bail!("private OpenCode database foreign-key check failed before runtime repair")
    }
    let (tables, _) = match probe_schema(&conn, true) {
        Ok(schema) => schema,
        Err(_) if !probe_completed => return Ok(RuntimeRepair::NeedsProbe),
        Err(error) => {
            return Err(error).context("OpenCode probe did not create a supported runtime schema");
        }
    };
    validate_event_schema(&conn, &tables, false)?;
    let rows = read_rows(&conn, &tables)?;
    validate_event_graph(&tables, &rows, "private OpenCode database")?;
    let session_table = tables
        .iter()
        .find(|table| table.name == "session")
        .context("session schema")?;
    let malformed = malformed_session_closure(session_table, &rows["session"])?;
    let project = tables
        .iter()
        .find(|table| table.name == "project")
        .context("supported OpenCode runtime schema lacks project table")?;
    let project_exists = rows["project"]
        .iter()
        .any(|row| text_field(project, row, "id") == Some(authoritative));
    if !project_exists {
        if !probe_completed {
            return Ok(RuntimeRepair::NeedsProbe);
        } else {
            bail!(
                "authoritative OpenCode project row is absent after probe; refusing to fabricate it"
            )
        }
    }

    let archive = if malformed.is_empty() {
        None
    } else {
        Some(archive_private_database(&conn, db)?)
    };
    conn.pragma_update(None, "foreign_keys", false)?;
    let transaction = conn.transaction()?;
    let result = (|| -> Result<RuntimeRepairReport> {
        let mut changed = remove_malformed_graph(&transaction, &tables, &malformed)?;
        changed += normalize_runtime_project(&transaction, &tables, authoritative)?;
        let current_rows = read_rows(&transaction, &tables)?;
        validate_event_graph(&tables, &current_rows, "repaired private OpenCode database")?;
        let violations: i64 =
            transaction.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            bail!("runtime repair would leave foreign-key violations")
        }
        let integrity: String =
            transaction.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("runtime repair integrity check failed")
        }
        let roots: i64 = transaction.query_row(
            r"SELECT count(*) FROM session WHERE id LIKE 'ses\_%' ESCAPE '\' AND parent_id IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(RuntimeRepairReport {
            changed_rows: changed,
            quarantined_sessions: malformed.len(),
            archive,
            valid_root_sessions: usize::try_from(roots)?,
            checkpoint_hit: false,
        })
    })();
    match result {
        Ok(report) => {
            transaction.commit()?;
            let roots = refresh_runtime_checkpoint(db, authoritative)?;
            Ok(RuntimeRepair::Complete(RuntimeRepairReport {
                valid_root_sessions: roots,
                ..report
            }))
        }
        Err(error) => {
            drop(transaction);
            Err(error)
        }
    }
}

#[cfg(test)]
thread_local! {
    static FULL_REPAIR_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn runtime_checkpoint_path(db: &Path) -> Result<PathBuf> {
    Ok(db
        .parent()
        .context("private OpenCode DB has no parent")?
        .join("rtrt-runtime-repair.json"))
}

fn read_runtime_checkpoint(
    db: &Path,
    authoritative: &str,
    generation: &SourceGeneration,
) -> Result<Option<usize>> {
    let path = runtime_checkpoint_path(db)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => super::ensure_private_file(&path)?,
    }
    let file = fs::File::open(&path)?;
    if file.metadata()?.len() > MAX_RUNTIME_CHECKPOINT_BYTES {
        bail!("OpenCode runtime-repair checkpoint exceeds bounded size")
    }
    let value: serde_json::Value =
        serde_json::from_reader(file).context("malformed OpenCode runtime-repair checkpoint")?;
    let object = value
        .as_object()
        .context("malformed OpenCode runtime-repair checkpoint")?;
    let expected = [
        "version",
        "schema_version",
        "authority",
        "generation",
        "valid_root_sessions",
    ];
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        bail!("malformed OpenCode runtime-repair checkpoint")
    }
    let version = object.get("version").and_then(serde_json::Value::as_u64);
    let schema_version = object
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if version != Some(RUNTIME_CHECKPOINT_VERSION)
        || schema_version != Some(RUNTIME_CHECKPOINT_VERSION)
    {
        return Ok(None);
    }
    let stored_authority = object
        .get("authority")
        .and_then(serde_json::Value::as_str)
        .context("malformed OpenCode runtime-repair checkpoint")?;
    if stored_authority != "global" && !valid_runtime_project_id(stored_authority) {
        bail!("malformed OpenCode runtime-repair checkpoint")
    }
    let stored_generation = object
        .get("generation")
        .and_then(serde_json::Value::as_str)
        .context("malformed OpenCode runtime-repair checkpoint")?;
    let roots = object
        .get("valid_root_sessions")
        .and_then(serde_json::Value::as_u64)
        .context("malformed OpenCode runtime-repair checkpoint")?;
    if stored_authority != authoritative || stored_generation != generation.0 {
        return Ok(None);
    }
    Ok(Some(usize::try_from(roots)?))
}

fn write_runtime_checkpoint(
    db: &Path,
    authoritative: &str,
    generation: &SourceGeneration,
    roots: usize,
) -> Result<()> {
    let path = runtime_checkpoint_path(db)?;
    let parent = path.parent().context("runtime checkpoint has no parent")?;
    super::ensure_private_directory(parent)?;
    if fs::symlink_metadata(&path).is_ok() {
        super::ensure_private_file(&path)?;
    }
    let staging = parent.join(format!(".rtrt-runtime-repair-{}.json", std::process::id()));
    if fs::symlink_metadata(&staging).is_ok() {
        super::ensure_private_file(&staging)?;
        fs::remove_file(&staging)?;
    }
    let payload = serde_json::to_vec(&json!({
        "version": RUNTIME_CHECKPOINT_VERSION,
        "schema_version": RUNTIME_CHECKPOINT_VERSION,
        "authority": authoritative,
        "generation": generation.0,
        "valid_root_sessions": roots,
    }))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&staging)?;
    file.write_all(&payload)?;
    file.sync_all()?;
    fs::rename(&staging, &path)?;
    sync_directory(parent)?;
    Ok(())
}

/// Bless state produced by a successful trusted OpenCode child without reading
/// prompt graphs. Generation checks bracket only bounded project/root queries;
/// any concurrent DB/WAL write makes the checkpoint stale or aborts refresh.
pub fn refresh_runtime_checkpoint(db: &Path, authoritative: &str) -> Result<usize> {
    if authoritative != "global" && !valid_runtime_project_id(authoritative) {
        bail!("invalid authoritative OpenCode runtime project ID")
    }
    super::ensure_private_file(db)?;
    for _ in 0..3 {
        let before = source_generation(db)?;
        let conn = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "query_only", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let project: i64 = conn.query_row(
            "SELECT count(*) FROM project WHERE id=?1",
            [authoritative],
            |row| row.get(0),
        )?;
        if project != 1 {
            bail!("authoritative OpenCode project row is absent during checkpoint refresh")
        }
        let roots: i64 = conn.query_row(
            r"SELECT count(*) FROM session WHERE id LIKE 'ses\_%' ESCAPE '\' AND parent_id IS NULL AND project_id=?1",
            [authoritative],
            |row| row.get(0),
        )?;
        drop(conn);
        let after = source_generation(db)?;
        if before == after {
            let roots = usize::try_from(roots)?;
            write_runtime_checkpoint(db, authoritative, &after, roots)?;
            return Ok(roots);
        }
    }
    bail!("private OpenCode database changed during checkpoint refresh")
}

fn malformed_session_closure(table: &Table, rows: &[Row]) -> Result<BTreeSet<String>> {
    let mut malformed = BTreeSet::new();
    for row in rows {
        let id = text_field(table, row, "id").context("session row lacks text id")?;
        if !id.starts_with("ses_")
            || text_field(table, row, "parent_id").is_some_and(|parent| !parent.starts_with("ses_"))
        {
            malformed.insert(id.to_string());
        }
    }
    for _ in 0..rows.len() {
        let before = malformed.len();
        for row in rows {
            if let (Some(id), Some(parent)) = (
                text_field(table, row, "id"),
                text_field(table, row, "parent_id"),
            ) && malformed.contains(parent)
            {
                malformed.insert(id.to_string());
            }
        }
        if malformed.len() == before {
            break;
        }
    }
    Ok(malformed)
}

fn archive_private_database(conn: &Connection, db: &Path) -> Result<PathBuf> {
    let parent = db.parent().context("private OpenCode DB has no parent")?;
    let directory = parent.join("rtrt-repair-archives");
    super::ensure_private_directory(&directory)?;
    let generation = source_generation(db)?.0;
    let name = format!("malformed-session-{generation}.sqlite");
    let path = directory.join(&name);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            super::ensure_private_file(&path)?;
            let archive = open_source(&path)?;
            let integrity: String =
                archive.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                bail!("existing malformed-session archive failed integrity check")
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let staging = directory.join(format!(".{name}.{}", std::process::id()));
            if fs::symlink_metadata(&staging).is_ok() {
                super::ensure_private_file(&staging)?;
                fs::remove_file(&staging)?;
            }
            conn.execute("VACUUM INTO ?1", [staging.to_string_lossy().as_ref()])?;
            set_private_file(&staging)?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staging)?
                .sync_all()?;
            match fs::hard_link(&staging, &path) {
                Ok(()) => fs::remove_file(&staging)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    fs::remove_file(&staging)?;
                    super::ensure_private_file(&path)?;
                }
                Err(error) => return Err(error.into()),
            }
            sync_directory(&directory)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(PathBuf::from("rtrt-repair-archives").join(name))
}

fn id_placeholders(count: usize) -> String {
    (1..=count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn remove_malformed_graph(
    conn: &Connection,
    tables: &[Table],
    ids: &BTreeSet<String>,
) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let values = ids.iter().cloned().map(Value::Text).collect::<Vec<_>>();
    let placeholders = id_placeholders(values.len());
    let mut changed = 0;
    // Message-linked rows must precede messages; direct session-linked rows
    // then precede sessions. Schema probing rejects unknown graph tables.
    for table in tables.iter().rev() {
        if matches!(
            table.name.as_str(),
            "session" | "message" | "event" | "event_sequence"
        ) {
            continue;
        }
        let predicate = if table.columns.iter().any(|column| column == "message_id") {
            format!("message_id IN (SELECT id FROM message WHERE session_id IN ({placeholders}))")
        } else if table.columns.iter().any(|column| column == "session_id") {
            format!("session_id IN ({placeholders})")
        } else {
            continue;
        };
        changed += conn.execute(
            &format!("DELETE FROM {} WHERE {predicate}", quote(&table.name)),
            params_from_iter(values.iter()),
        )?;
    }
    if tables.iter().any(|table| table.name == "event") {
        changed += conn.execute(
            &format!("DELETE FROM event WHERE aggregate_id IN ({placeholders})"),
            params_from_iter(values.iter()),
        )?;
        changed += conn.execute(
            &format!("DELETE FROM event_sequence WHERE aggregate_id IN ({placeholders})"),
            params_from_iter(values.iter()),
        )?;
    }
    changed += conn.execute(
        &format!("DELETE FROM message WHERE session_id IN ({placeholders})"),
        params_from_iter(values.iter()),
    )?;
    changed += conn.execute(
        &format!("DELETE FROM session WHERE id IN ({placeholders})"),
        params_from_iter(values.iter()),
    )?;
    Ok(changed)
}

fn normalize_runtime_project(
    conn: &Connection,
    tables: &[Table],
    project_id: &str,
) -> Result<usize> {
    let mut changed = 0;
    if tables.iter().any(|table| table.name == "project_directory") {
        changed += conn.execute(
            "INSERT OR IGNORE INTO project_directory(project_id,directory) SELECT ?1,directory FROM project_directory",
            [project_id],
        )?;
        changed += conn.execute(
            "DELETE FROM project_directory WHERE project_id<>?1",
            [project_id],
        )?;
    }
    if tables.iter().any(|table| table.name == "workspace") {
        changed += conn.execute(
            "UPDATE workspace SET project_id=?1 WHERE project_id IS NOT ?1",
            [project_id],
        )?;
    }
    changed += conn.execute(
        r"UPDATE session SET project_id=?1 WHERE id LIKE 'ses\_%' ESCAPE '\' AND project_id IS NOT ?1",
        [project_id],
    )?;
    Ok(changed)
}

fn validate_source(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!(
            "global OpenCode database is not a regular non-symlink file: {}",
            path.display()
        )
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != super::unsafe_geteuid() {
            bail!(
                "global OpenCode database has unsafe owner: {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn open_source(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags).with_context(|| {
        format!(
            "open global OpenCode database read-only: {}",
            path.display()
        )
    })?;
    conn.pragma_update(None, "query_only", true)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

fn prepare_source(source: &Path, mode: MigrationMode) -> Result<PreparedSource> {
    for _ in 0..3 {
        let before = source_generation(source)?;
        let mut conn = open_source(source)?;
        let transaction = conn.transaction()?;
        let integrity: String =
            transaction.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("global OpenCode database integrity check failed")
        }
        let (tables, indexes) = probe_schema(&transaction, true)?;
        let rows = read_rows(&transaction, &tables)?;
        validate_event_schema(&transaction, &tables, true)?;
        validate_event_graph(&tables, &rows, "global OpenCode database")?;
        let routing = route_sessions(&tables, &rows, mode == MigrationMode::CatchUp)?;
        let plans = routing
            .destinations
            .iter()
            .map(|destination| {
                Ok((
                    destination.clone(),
                    select_rows(destination, &tables, &rows, &routing)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let report = MigrationReport {
            source: Some(source.to_path_buf()),
            sessions: routing.sessions.len(),
            projects: routing
                .destinations
                .iter()
                .filter(|destination| destination.as_str() != "legacy-global")
                .count(),
            archived_sessions: routing
                .sessions
                .values()
                .filter(|destination| destination.as_str() == "legacy-global")
                .count(),
            skipped_malformed_sessions: routing.malformed_sessions.len(),
            rows: rows.values().map(Vec::len).sum(),
            ..MigrationReport::default()
        };
        transaction.rollback()?;
        drop(conn);
        let after = source_generation(source)?;
        if before == after {
            return Ok(PreparedSource {
                tables,
                indexes,
                plans,
                report,
                generation: before,
            });
        }
    }
    bail!("global OpenCode database changed during migration read")
}

fn source_generation(source: &Path) -> Result<SourceGeneration> {
    let wal = source.with_file_name(format!(
        "{}-wal",
        source
            .file_name()
            .context("global database has no filename")?
            .to_string_lossy()
    ));
    let mut stamp = Vec::with_capacity(256);
    append_file_generation(&mut stamp, source, false)?;
    append_file_generation(&mut stamp, &wal, true)?;
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in stamp {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // Domain-separate routing policy from older manifests/checkpoints. This
    // forces exactly one catch-up after canonical session-directory routing
    // shipped, without repeatedly invalidating unchanged sources.
    Ok(SourceGeneration(format!(
        "v2-session-directory-{hash:016x}"
    )))
}

fn append_file_generation(stamp: &mut Vec<u8>, path: &Path, optional: bool) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
            stamp.push(0);
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("unsafe OpenCode database sidecar: {}", path.display())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != super::unsafe_geteuid() {
            bail!("unsafe OpenCode database sidecar owner: {}", path.display())
        }
        stamp.extend_from_slice(&metadata.dev().to_le_bytes());
        stamp.extend_from_slice(&metadata.ino().to_le_bytes());
    }
    stamp.push(1);
    stamp.extend_from_slice(&metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified() {
        let duration = modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        stamp.extend_from_slice(&duration.as_secs().to_le_bytes());
        stamp.extend_from_slice(&duration.subsec_nanos().to_le_bytes());
    }
    // WAL frame headers contain page number, commit marker, salts, and
    // checksums—not row payload. Including bounded WAL and final-frame headers
    // avoids missing a same-length generation without inspecting prompts.
    if optional && metadata.len() > 0 {
        let mut file = fs::File::open(path)?;
        let mut header = [0_u8; 32];
        let read = file.read(&mut header)?;
        stamp.extend_from_slice(&header[..read]);
        if read == header.len() {
            let encoded_page_size = u32::from_be_bytes(header[8..12].try_into()?);
            let page_size = if encoded_page_size == 0 {
                65_536_u64
            } else {
                u64::from(encoded_page_size)
            };
            let frame_size = 24_u64 + page_size;
            let body_len = metadata.len().saturating_sub(32);
            let frame_count = body_len / frame_size;
            if frame_count > 0 {
                let final_header = 32 + (frame_count - 1) * frame_size;
                file.seek(SeekFrom::Start(final_header))?;
                let mut frame_header = [0_u8; 24];
                file.read_exact(&mut frame_header)?;
                stamp.extend_from_slice(&frame_header);
            }
        }
    }
    Ok(())
}

fn probe_schema(conn: &Connection, require_runtime: bool) -> Result<(Vec<Table>, Vec<Index>)> {
    let unsupported: Option<(String, String)> = conn
        .query_row(
            "SELECT type, name FROM sqlite_schema WHERE type IN ('trigger','view') ORDER BY type, name LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, _name)) = unsupported {
        bail!("unsupported OpenCode schema object type: {kind}")
    }
    let mut statement = conn.prepare(
        "SELECT name, CASE WHEN length(CAST(sql AS BLOB)) <= ?1 THEN sql END
         FROM sqlite_schema
         WHERE type='table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name LIMIT ?2",
    )?;
    let entries = statement
        .query_map(
            [
                i64::try_from(MAX_SCHEMA_SQL_BYTES)?,
                i64::try_from(MAX_TABLES + 1)?,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(name, sql)| {
            Ok((
                name,
                sql.context("OpenCode schema object exceeds bounded SQL limit")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    if entries.len() > MAX_TABLES {
        bail!("OpenCode schema exceeds bounded table limit")
    }
    let mut schema_bytes = entries
        .iter()
        .try_fold(0_usize, |total, (_, sql)| total.checked_add(sql.len()))
        .context("OpenCode schema size overflow")?;
    if schema_bytes > MAX_SCHEMA_TOTAL_BYTES {
        bail!("OpenCode schema exceeds bounded SQL limit")
    }
    let mut tables = Vec::new();
    for (name, sql) in entries {
        let columns = table_columns(conn, &name)?;
        let dependent = columns.iter().any(|column| {
            matches!(
                column.as_str(),
                "project_id"
                    | "workspace_id"
                    | "session_id"
                    | "message_id"
                    | "parent_id"
                    | "aggregate_id"
            )
        }) || has_graph_foreign_key(conn, &name)?;
        let known_sensitive = SENSITIVE_TABLES.contains(&name.as_str());
        let known_graph = GRAPH_TABLES.contains(&name.as_str());
        let known_schema = SCHEMA_TABLES.contains(&name.as_str());
        if dependent && !known_graph && !known_sensitive {
            bail!("unknown OpenCode graph-dependent table: {name}")
        }
        let primary_key = table_primary_key(conn, &name, &columns)?;
        let column_specs = table_column_specs(conn, &name)?;
        let foreign_keys = table_foreign_keys(conn, &name)?;
        let (without_rowid, strict) = table_options(conn, &name)?;
        let sql_signature = schema_sql_signature(&sql)?;
        tables.push(Table {
            name,
            sql,
            columns,
            primary_key,
            column_specs,
            foreign_keys,
            without_rowid,
            strict,
            sql_signature,
            create_schema: true,
            copy_rows: known_graph || known_schema,
        });
    }
    if require_runtime {
        for required in ["session", "message", "part"] {
            if !tables.iter().any(|table| table.name == required) {
                bail!("unsupported OpenCode schema: missing {required} table")
            }
        }
    }
    let schema_tables = tables
        .iter()
        .map(|table| table.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut statement = conn.prepare(
        "SELECT name, tbl_name,
                CASE WHEN length(CAST(sql AS BLOB)) <= ?1 THEN sql END
         FROM sqlite_schema WHERE type='index' AND sql IS NOT NULL
         ORDER BY name LIMIT ?2",
    )?;
    let index_entries = statement
        .query_map(
            [
                i64::try_from(MAX_SCHEMA_SQL_BYTES)?,
                i64::try_from(MAX_SCHEMA_OBJECTS + 1)?,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, table, _)| schema_tables.contains(table.as_str()))
        .map(|(name, table, sql)| {
            Ok((
                name,
                table,
                sql.context("OpenCode schema object exceeds bounded SQL limit")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    if index_entries.len() > MAX_SCHEMA_OBJECTS {
        bail!("OpenCode schema exceeds bounded object limit")
    }
    schema_bytes = index_entries
        .iter()
        .try_fold(schema_bytes, |total, (_, _, sql)| {
            total.checked_add(sql.len())
        })
        .context("OpenCode schema size overflow")?;
    if schema_bytes > MAX_SCHEMA_TOTAL_BYTES {
        bail!("OpenCode schema exceeds bounded SQL limit")
    }
    let indexes = index_entries
        .into_iter()
        .map(|(name, table, sql)| {
            let (unique, columns, partial) = index_signature(conn, &name, &table)?;
            let sql_signature = schema_sql_signature(&sql)?;
            Ok(Index {
                name,
                table,
                sql,
                unique,
                columns,
                partial,
                sql_signature,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if tables.len() + indexes.len() > MAX_SCHEMA_OBJECTS {
        bail!("OpenCode schema exceeds bounded object limit")
    }
    Ok((tables, indexes))
}

fn table_column_specs(conn: &Connection, table: &str) -> Result<Vec<ColumnSpec>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_xinfo({})", quote(table)))?;
    let mut columns = statement
        .query_map([], |row| {
            Ok(ColumnSpec {
                name: row.get(1)?,
                ty: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default: row.get(4)?,
                primary_key_order: row.get(5)?,
                hidden: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for column in &mut columns {
        column.name.make_ascii_lowercase();
        column.ty = schema_sql_signature(&column.ty)?;
        column.default = column
            .default
            .as_deref()
            .map(schema_sql_signature)
            .transpose()?;
    }
    Ok(columns)
}

fn table_options(conn: &Connection, table: &str) -> Result<(bool, bool)> {
    conn.query_row(
        "SELECT wr, strict FROM pragma_table_list WHERE schema='main' AND name=?1",
        [table],
        |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
    )
    .with_context(|| format!("OpenCode table {table} lacks table metadata"))
}

fn table_foreign_keys(conn: &Connection, table: &str) -> Result<Vec<ForeignKeySpec>> {
    let mut statement = conn.prepare(&format!("PRAGMA foreign_key_list({})", quote(table)))?;
    let mut keys = statement
        .query_map([], |row| {
            Ok(ForeignKeySpec {
                id: row.get(0)?,
                sequence: row.get(1)?,
                table: row.get(2)?,
                from: row.get(3)?,
                to: row.get(4)?,
                on_update: row.get(5)?,
                on_delete: row.get(6)?,
                match_clause: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for key in &mut keys {
        key.table.make_ascii_lowercase();
        key.from.make_ascii_lowercase();
        if let Some(to) = &mut key.to {
            to.make_ascii_lowercase();
        }
        key.on_update.make_ascii_uppercase();
        key.on_delete.make_ascii_uppercase();
        key.match_clause.make_ascii_uppercase();
    }
    keys.sort();
    Ok(keys)
}

fn index_signature(
    conn: &Connection,
    index: &str,
    table: &str,
) -> Result<(bool, Vec<IndexColumnSpec>, bool)> {
    let mut statement = conn.prepare(&format!("PRAGMA index_list({})", quote(table)))?;
    let entries = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (_, unique, partial) = entries
        .into_iter()
        .find(|(name, _, _)| name == index)
        .with_context(|| format!("OpenCode index {index} lacks metadata"))?;
    let mut columns_statement = conn.prepare(&format!("PRAGMA index_xinfo({})", quote(index)))?;
    let mut columns = columns_statement
        .query_map([], |row| {
            Ok(IndexColumnSpec {
                cid: row.get(1)?,
                name: row.get(2)?,
                descending: row.get::<_, i64>(3)? != 0,
                collation: row.get(4)?,
                key: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for column in &mut columns {
        if let Some(name) = &mut column.name {
            name.make_ascii_lowercase();
        }
        column.collation.make_ascii_lowercase();
    }
    Ok((unique, columns, partial))
}

fn schema_sql_signature(sql: &str) -> Result<String> {
    if sql.len() > MAX_SCHEMA_SQL_BYTES {
        bail!("OpenCode schema object exceeds bounded SQL limit")
    }
    let mut signature = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_whitespace() {
            continue;
        }
        if character == '-' && characters.peek() == Some(&'-') {
            characters.next();
            for comment in characters.by_ref() {
                if matches!(comment, '\n' | '\r') {
                    break;
                }
            }
            continue;
        }
        if character == '/' && characters.peek() == Some(&'*') {
            characters.next();
            let mut terminated = false;
            while let Some(comment) = characters.next() {
                if comment == '*' && characters.peek() == Some(&'/') {
                    characters.next();
                    terminated = true;
                    break;
                }
            }
            if !terminated {
                bail!("malformed OpenCode schema SQL comment")
            }
            continue;
        }
        if matches!(character, '\'' | '"' | '`' | '[') {
            let end = if character == '[' { ']' } else { character };
            let mut quoted = String::new();
            let mut terminated = false;
            while let Some(value) = characters.next() {
                if value == end {
                    if character != '[' && characters.peek() == Some(&end) {
                        characters.next();
                        quoted.push(end);
                    } else {
                        terminated = true;
                        break;
                    }
                } else {
                    quoted.push(value);
                }
            }
            if !terminated {
                bail!("malformed OpenCode schema SQL quote")
            }
            let kind = if character == '\'' {
                's'
            } else {
                quoted.make_ascii_lowercase();
                'q'
            };
            append_schema_token(&mut signature, kind, &quoted);
            continue;
        }
        let word = character.is_alphanumeric() || matches!(character, '_' | '$');
        let mut token = String::from(character.to_ascii_lowercase());
        if word {
            while characters
                .peek()
                .is_some_and(|next| next.is_alphanumeric() || matches!(next, '_' | '$'))
            {
                token.push(
                    characters
                        .next()
                        .expect("peeked schema token")
                        .to_ascii_lowercase(),
                );
            }
        }
        append_schema_token(&mut signature, 't', &token);
    }
    Ok(signature)
}

fn append_schema_token(signature: &mut String, kind: char, token: &str) {
    use std::fmt::Write as _;

    write!(signature, "{kind}{}:", token.len()).expect("write to String");
    signature.push_str(token);
}

fn schema_object_sql(conn: &Connection, kind: &str, name: &str) -> Result<String> {
    let sql = conn.query_row(
        "SELECT CASE WHEN length(CAST(sql AS BLOB)) <= ?3 THEN sql END
         FROM sqlite_schema WHERE type=?1 AND name=?2",
        rusqlite::params![kind, name, i64::try_from(MAX_SCHEMA_SQL_BYTES)?],
        |row| row.get::<_, Option<String>>(0),
    )?;
    sql.context("OpenCode schema object exceeds bounded SQL limit")
}

fn has_graph_foreign_key(conn: &Connection, table: &str) -> Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA foreign_key_list({})", quote(table)))?;
    let references = statement
        .query_map([], |row| row.get::<_, String>(2))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(references.iter().any(|target| {
        matches!(
            target.as_str(),
            "project" | "workspace" | "session" | "message" | "part" | "event_sequence"
        )
    }))
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({})", quote(table)))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if columns.len() > MAX_COLUMNS || columns.is_empty() {
        bail!("OpenCode table {table} has invalid column count")
    }
    Ok(columns)
}

fn table_primary_key(conn: &Connection, table: &str, columns: &[String]) -> Result<Vec<usize>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({})", quote(table)))?;
    let mut keyed = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, order)| *order > 0)
        .collect::<Vec<_>>();
    keyed.sort_by_key(|(_, order)| *order);
    keyed
        .into_iter()
        .map(|(name, _)| {
            columns
                .iter()
                .position(|column| column == &name)
                .context("primary-key column missing")
        })
        .collect()
}

fn read_rows(conn: &Connection, tables: &[Table]) -> Result<BTreeMap<String, Vec<Row>>> {
    let mut all = BTreeMap::new();
    for table in tables {
        if !table.copy_rows {
            all.insert(table.name.clone(), Vec::new());
            continue;
        }
        let sql = format!("SELECT * FROM {}", quote(&table.name));
        let mut statement = conn.prepare(&sql)?;
        let column_count = table.columns.len();
        let values = statement
            .query_map([], |row| {
                let mut values = Vec::with_capacity(column_count);
                for index in 0..column_count {
                    values.push(value_owned(row.get_ref(index)?));
                }
                Ok(Row { values })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        all.insert(table.name.clone(), values);
    }
    Ok(all)
}

fn validate_event_schema(conn: &Connection, tables: &[Table], require_indexes: bool) -> Result<()> {
    let sequence = tables.iter().find(|table| table.name == "event_sequence");
    let event = tables.iter().find(|table| table.name == "event");
    if sequence.is_none() && event.is_none() {
        return Ok(());
    }
    let (Some(sequence), Some(event)) = (sequence, event) else {
        bail!("unsupported OpenCode event schema: event tables must be paired")
    };
    if sequence.columns != ["aggregate_id", "seq", "owner_id"]
        || sequence.primary_key != [0]
        || event.columns != ["id", "aggregate_id", "seq", "type", "data"]
        || event.primary_key != [0]
    {
        bail!("unsupported OpenCode event schema")
    }
    let column_specs = |table: &str| -> Result<Vec<(String, String, i64)>> {
        let mut statement = conn.prepare(&format!("PRAGMA table_info({})", quote(table)))?;
        Ok(statement
            .query_map([], |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    };
    let sequence_specs = column_specs("event_sequence")?;
    let event_specs = column_specs("event")?;
    if sequence_specs
        .iter()
        .map(|(_, ty, _)| ty.as_str())
        .collect::<Vec<_>>()
        != ["TEXT", "INTEGER", "TEXT"]
        || sequence_specs[1].2 != 1
        || event_specs
            .iter()
            .map(|(_, ty, _)| ty.as_str())
            .collect::<Vec<_>>()
            != ["TEXT", "TEXT", "INTEGER", "TEXT", "TEXT"]
        || event_specs[1..]
            .iter()
            .any(|(_, _, not_null)| *not_null != 1)
    {
        bail!("unsupported OpenCode event column definitions")
    }
    let mut foreign_key_statement = conn.prepare("PRAGMA foreign_key_list(event)")?;
    let foreign_keys = foreign_key_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if foreign_keys
        != [(
            String::from("event_sequence"),
            String::from("aggregate_id"),
            String::from("aggregate_id"),
            String::from("CASCADE"),
        )]
    {
        bail!("unsupported OpenCode event foreign key")
    }
    let mut has_position_unique = false;
    let mut has_lookup = false;
    let mut statement = conn.prepare("PRAGMA index_list(event)")?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, unique) in indexes {
        let mut index_statement = conn.prepare(&format!("PRAGMA index_info({})", quote(&name)))?;
        let columns = index_statement
            .query_map([], |row| row.get::<_, String>(2))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        has_position_unique |= unique == 1 && columns == ["aggregate_id", "seq"];
        has_lookup |= unique == 0 && columns == ["aggregate_id", "type", "seq"];
    }
    if require_indexes && (!has_position_unique || !has_lookup) {
        bail!("unsupported OpenCode event indexes")
    }
    Ok(())
}

fn validate_event_graph(tables: &[Table], rows: &RowsByTable, label: &str) -> Result<()> {
    let Some(sequence_table) = tables.iter().find(|table| table.name == "event_sequence") else {
        return Ok(());
    };
    let event_table = tables
        .iter()
        .find(|table| table.name == "event")
        .context("event table missing")?;
    let mut high_waters = BTreeMap::new();
    for row in &rows["event_sequence"] {
        let aggregate = required_text(sequence_table, row, "aggregate_id", label)?;
        let seq = required_integer(sequence_table, row, "seq", label)?;
        if seq < 0 || high_waters.insert(aggregate.to_string(), seq).is_some() {
            bail!("malformed event history in {label}")
        }
    }
    let mut ids = BTreeSet::new();
    let mut positions = BTreeSet::new();
    for row in &rows["event"] {
        let id = required_text(event_table, row, "id", label)?;
        let aggregate = required_text(event_table, row, "aggregate_id", label)?;
        let seq = required_integer(event_table, row, "seq", label)?;
        required_text(event_table, row, "type", label)?;
        required_text(event_table, row, "data", label)?;
        if !ids.insert(id.to_string())
            || !positions.insert((aggregate.to_string(), seq))
            || high_waters
                .get(aggregate)
                .is_none_or(|high_water| seq < 0 || seq > *high_water)
        {
            bail!("malformed event history in {label}")
        }
    }
    for (aggregate, high_water) in high_waters {
        if (0..=high_water).any(|seq| !positions.contains(&(aggregate.clone(), seq))) {
            bail!("malformed event history in {label}")
        }
    }
    Ok(())
}

fn field_index(table: &Table, name: &str) -> Result<usize> {
    table
        .columns
        .iter()
        .position(|column| column == name)
        .with_context(|| format!("OpenCode table {} lacks {name}", table.name))
}

fn required_text<'a>(table: &Table, row: &'a Row, name: &str, label: &str) -> Result<&'a str> {
    match &row.values[field_index(table, name)?] {
        Value::Text(value) => Ok(value),
        _ => bail!("malformed {name} in {label}"),
    }
}

fn required_integer(table: &Table, row: &Row, name: &str, label: &str) -> Result<i64> {
    match &row.values[field_index(table, name)?] {
        Value::Integer(value) => Ok(*value),
        _ => bail!("malformed {name} in {label}"),
    }
}

fn value_owned(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => Value::Integer(v),
        ValueRef::Real(v) => Value::Real(v),
        ValueRef::Text(v) => Value::Text(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(v) => Value::Blob(v.to_vec()),
    }
}

struct Routing {
    sessions: HashMap<String, String>,
    messages: HashMap<String, String>,
    projects: HashMap<String, String>,
    workspaces: HashMap<String, String>,
    destinations: BTreeSet<String>,
    malformed_sessions: BTreeSet<String>,
    malformed_messages: BTreeSet<String>,
    destination_projects: HashMap<String, BTreeSet<String>>,
    destination_workspaces: HashMap<String, BTreeSet<String>>,
}

fn route_sessions(
    tables: &[Table],
    rows: &BTreeMap<String, Vec<Row>>,
    quarantine_malformed: bool,
) -> Result<Routing> {
    let project_table = tables.iter().find(|table| table.name == "project");
    let mut projects = HashMap::new();
    if let Some(table) = project_table {
        for row in &rows["project"] {
            if let (Some(id), Some(identity)) = (
                text_field(table, row, "id"),
                identity_from_metadata(table, row),
            ) {
                projects.insert(id.to_string(), identity.slug().to_string());
            }
        }
    }
    if let Some(table) = tables
        .iter()
        .find(|table| table.name == "project_directory")
    {
        for row in &rows["project_directory"] {
            if let (Some(project_id), Some(destination)) = (
                text_field(table, row, "project_id"),
                text_field(table, row, "directory")
                    .and_then(identity_from_path)
                    .map(|identity| identity.slug().to_string()),
            ) {
                insert_consistent_route(&mut projects, project_id, destination);
            }
        }
    }
    if let Some(table) = project_table {
        for row in &rows["project"] {
            if let Some(id) = text_field(table, row, "id") {
                projects
                    .entry(id.to_string())
                    .or_insert_with(|| "legacy-global".to_string());
            }
        }
    }
    let mut workspaces = HashMap::new();
    if let Some(table) = tables.iter().find(|table| table.name == "workspace") {
        for row in &rows["workspace"] {
            let Some(id) = text_field(table, row, "id") else {
                continue;
            };
            let destination = text_field(table, row, "project_id")
                .and_then(|project_id| projects.get(project_id).cloned())
                .or_else(|| {
                    identity_from_metadata(table, row).map(|identity| identity.slug().to_string())
                });
            if let Some(destination) = destination {
                insert_consistent_route(&mut workspaces, id, destination);
            }
        }
    }
    if let Some(table) = tables.iter().find(|table| table.name == "workspace") {
        for row in &rows["workspace"] {
            if let Some(id) = text_field(table, row, "id") {
                workspaces
                    .entry(id.to_string())
                    .or_insert_with(|| "legacy-global".to_string());
            }
        }
    }
    let table = tables
        .iter()
        .find(|table| table.name == "session")
        .context("session schema")?;
    let mut sessions = HashMap::new();
    let mut parents = HashMap::new();
    for row in &rows["session"] {
        let id = text_field(table, row, "id").context("session row lacks text id")?;
        let direct = text_field(table, row, "directory")
            .and_then(identity_from_path)
            .map(|identity| identity.slug().to_string())
            .or_else(|| {
                text_field(table, row, "workspace_id").and_then(|id| workspaces.get(id).cloned())
            })
            .or_else(|| {
                text_field(table, row, "project_id").and_then(|id| projects.get(id).cloned())
            });
        if let Some(destination) = direct {
            sessions.insert(id.to_string(), destination);
        }
        if let Some(parent) = text_field(table, row, "parent_id") {
            parents.insert(id.to_string(), parent.to_string());
        }
    }
    let mut malformed_sessions = BTreeSet::new();
    if quarantine_malformed {
        malformed_sessions = sessions
            .keys()
            .filter(|id| !id.starts_with("ses_"))
            .cloned()
            .collect::<BTreeSet<_>>();
        for row in &rows["session"] {
            let id = text_field(table, row, "id").context("session row lacks text id")?;
            if text_field(table, row, "parent_id").is_some_and(|parent| !parent.starts_with("ses_"))
            {
                malformed_sessions.insert(id.to_string());
            }
        }
        for _ in 0..rows["session"].len() {
            let before = malformed_sessions.len();
            for row in &rows["session"] {
                if let (Some(id), Some(parent)) = (
                    text_field(table, row, "id"),
                    text_field(table, row, "parent_id"),
                ) && malformed_sessions.contains(parent)
                {
                    malformed_sessions.insert(id.to_string());
                }
            }
            if malformed_sessions.len() == before {
                break;
            }
        }
        for id in &malformed_sessions {
            sessions.insert(id.clone(), "legacy-global".to_string());
        }
    }
    for _ in 0..rows["session"].len() {
        let mut changed = false;
        for (child, parent) in &parents {
            if !sessions.contains_key(child) {
                if let Some(destination) = sessions.get(parent).cloned() {
                    sessions.insert(child.clone(), destination);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    for row in &rows["session"] {
        let id = text_field(table, row, "id").context("session row lacks text id")?;
        sessions
            .entry(id.to_string())
            .or_insert_with(|| "legacy-global".to_string());
    }
    let message_table = tables
        .iter()
        .find(|table| table.name == "message")
        .context("message schema")?;
    let mut messages = HashMap::new();
    for row in &rows["message"] {
        if let (Some(id), Some(session)) = (
            text_field(message_table, row, "id"),
            text_field(message_table, row, "session_id"),
        ) {
            if let Some(destination) = sessions.get(session) {
                messages.insert(id.to_string(), destination.clone());
            }
        }
    }
    let destinations = sessions.values().cloned().collect::<BTreeSet<_>>();
    let malformed_messages = rows["message"]
        .iter()
        .filter_map(|row| {
            let id = text_field(message_table, row, "id")?;
            let session = text_field(message_table, row, "session_id")?;
            malformed_sessions.contains(session).then(|| id.to_string())
        })
        .collect();
    let workspace_table = tables
        .iter()
        .find(|candidate| candidate.name == "workspace");
    let mut workspace_projects = HashMap::new();
    if let Some(workspace_table) = workspace_table {
        for row in &rows["workspace"] {
            if let (Some(id), Some(project_id)) = (
                text_field(workspace_table, row, "id"),
                text_field(workspace_table, row, "project_id"),
            ) {
                workspace_projects.insert(id.to_string(), project_id.to_string());
            }
        }
    }
    let mut destination_projects = HashMap::<String, BTreeSet<String>>::new();
    let mut destination_workspaces = HashMap::<String, BTreeSet<String>>::new();
    for row in &rows["session"] {
        let Some(id) = text_field(table, row, "id") else {
            continue;
        };
        if malformed_sessions.contains(id) {
            continue;
        }
        let Some(destination) = sessions.get(id) else {
            continue;
        };
        if let Some(project_id) = text_field(table, row, "project_id") {
            destination_projects
                .entry(destination.clone())
                .or_default()
                .insert(project_id.to_string());
        }
        if let Some(workspace_id) = text_field(table, row, "workspace_id") {
            destination_workspaces
                .entry(destination.clone())
                .or_default()
                .insert(workspace_id.to_string());
            if let Some(project_id) = workspace_projects.get(workspace_id) {
                destination_projects
                    .entry(destination.clone())
                    .or_default()
                    .insert(project_id.clone());
            }
        }
    }
    if let Some(directory_table) = tables
        .iter()
        .find(|candidate| candidate.name == "project_directory")
    {
        for row in &rows["project_directory"] {
            if let (Some(project_id), Some(destination)) = (
                text_field(directory_table, row, "project_id"),
                text_field(directory_table, row, "directory")
                    .and_then(identity_from_path)
                    .map(|identity| identity.slug().to_string()),
            ) && destinations.contains(&destination)
            {
                destination_projects
                    .entry(destination)
                    .or_default()
                    .insert(project_id.to_string());
            }
        }
    }
    let mut routing = Routing {
        sessions,
        messages,
        projects,
        workspaces,
        destinations,
        malformed_sessions,
        malformed_messages,
        destination_projects,
        destination_workspaces,
    };
    let has_unattributed_graph = tables.iter().any(|table| {
        GRAPH_TABLES.contains(&table.name.as_str())
            && rows[&table.name]
                .iter()
                .any(|row| row_destination(table, row, &routing).is_none())
    });
    if routing.sessions.is_empty() || has_unattributed_graph {
        routing.destinations.insert("legacy-global".to_string());
    }
    Ok(routing)
}

fn insert_consistent_route(routes: &mut HashMap<String, String>, id: &str, destination: String) {
    match routes.get(id) {
        None => {
            routes.insert(id.to_string(), destination);
        }
        Some(existing) if existing == &destination || existing == "legacy-global" => {}
        Some(_) => {
            // Conflicting attribution must not route either graph into a project.
            routes.insert(id.to_string(), "legacy-global".to_string());
        }
    }
}

fn identity_from_metadata(table: &Table, row: &Row) -> Option<ProjectIdentity> {
    ["worktree", "directory", "path", "root"]
        .iter()
        .find_map(|column| text_field(table, row, column).and_then(identity_from_path))
}

fn identity_from_path(value: &str) -> Option<ProjectIdentity> {
    let path = Path::new(value);
    if !path.is_absolute() || fs::symlink_metadata(path).ok()?.file_type().is_symlink() {
        return None;
    }
    ProjectIdentity::derive(path).ok()
}

fn text_field<'a>(table: &Table, row: &'a Row, name: &str) -> Option<&'a str> {
    let index = table.columns.iter().position(|column| column == name)?;
    match &row.values[index] {
        Value::Text(value) => Some(value),
        _ => None,
    }
}

fn select_rows(
    destination: &str,
    tables: &[Table],
    rows: &BTreeMap<String, Vec<Row>>,
    routing: &Routing,
) -> Result<BTreeMap<String, Vec<Row>>> {
    let mut selected = BTreeMap::new();
    for table in tables {
        let mut output = Vec::new();
        if !table.copy_rows {
            selected.insert(table.name.clone(), output);
            continue;
        }
        for row in &rows[&table.name] {
            if SCHEMA_TABLES.contains(&table.name.as_str()) {
                output.push(row.clone());
                continue;
            }
            if table.name == "project" {
                if text_field(table, row, "id").is_some_and(|id| {
                    routing
                        .destination_projects
                        .get(destination)
                        .is_some_and(|ids| ids.contains(id))
                }) {
                    output.push(row.clone());
                }
                continue;
            }
            if table.name == "workspace" {
                if text_field(table, row, "id").is_some_and(|id| {
                    routing
                        .destination_workspaces
                        .get(destination)
                        .is_some_and(|ids| ids.contains(id))
                }) {
                    output.push(row.clone());
                }
                continue;
            }
            if table.name == "project_directory" {
                let directory_matches = text_field(table, row, "directory")
                    .and_then(identity_from_path)
                    .is_some_and(|identity| identity.slug() == destination);
                let required_project = text_field(table, row, "project_id").is_some_and(|id| {
                    routing
                        .destination_projects
                        .get(destination)
                        .is_some_and(|ids| ids.contains(id))
                });
                // A shared/global project ancestor is duplicated for FK
                // closure, but its unrelated directory rows are not. Keep a
                // directory row only when it independently identifies this
                // destination; required_project documents why its project
                // ancestor is available without broad cross-project leakage.
                if directory_matches && required_project {
                    output.push(row.clone());
                }
                continue;
            }
            let route = row_destination(table, row, routing);
            if destination == "legacy-global" && row_is_malformed_graph(table, row, routing) {
                continue;
            }
            if route.as_deref().unwrap_or("legacy-global") == destination {
                output.push(row.clone());
            }
        }
        selected.insert(table.name.clone(), output);
    }
    Ok(selected)
}

fn row_is_malformed_graph(table: &Table, row: &Row, routing: &Routing) -> bool {
    if table.name == "session" {
        return text_field(table, row, "id")
            .is_some_and(|id| routing.malformed_sessions.contains(id));
    }
    if let Some(id) = text_field(table, row, "session_id") {
        return routing.malformed_sessions.contains(id);
    }
    if let Some(message) = text_field(table, row, "message_id") {
        return routing.malformed_messages.contains(message);
    }
    matches!(table.name.as_str(), "event" | "event_sequence")
        && text_field(table, row, "aggregate_id")
            .is_some_and(|id| routing.malformed_sessions.contains(id))
}

fn row_destination(table: &Table, row: &Row, routing: &Routing) -> Option<String> {
    if table.name == "session" {
        return text_field(table, row, "id").and_then(|id| routing.sessions.get(id).cloned());
    }
    if let Some(id) = text_field(table, row, "session_id") {
        return routing.sessions.get(id).cloned();
    }
    if let Some(id) = text_field(table, row, "message_id") {
        return routing.messages.get(id).cloned();
    }
    if table.name == "message" {
        return text_field(table, row, "id").and_then(|id| routing.messages.get(id).cloned());
    }
    if matches!(table.name.as_str(), "event" | "event_sequence") {
        return text_field(table, row, "aggregate_id")
            .and_then(|id| routing.sessions.get(id).cloned());
    }
    if let Some(id) = text_field(table, row, "project_id") {
        return routing.projects.get(id).cloned();
    }
    if table.name == "project" {
        return text_field(table, row, "id").and_then(|id| routing.projects.get(id).cloned());
    }
    if table.name == "workspace" {
        return text_field(table, row, "id").and_then(|id| routing.workspaces.get(id).cloned());
    }
    None
}

fn destination_db(home: &Path, destination: &str) -> PathBuf {
    home.join(".rtrt/projects")
        .join(destination)
        .join("opencode/data/opencode/opencode.db")
}

#[derive(Default)]
struct MergeOutcome {
    changed: usize,
    conflicts: usize,
    private_preserved_conflicts: usize,
    archived_event_forks: usize,
}

fn validate_destination_before_merge(
    path: &Path,
    source_tables: &[Table],
    source_indexes: &[Index],
) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => super::ensure_private_file(path)?,
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("destination OpenCode integrity check failed")
    }
    let violations: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        bail!("destination OpenCode foreign-key check failed")
    }
    let (tables, _) = probe_schema(&conn, false)?;
    validate_event_schema(&conn, &tables, false)?;
    let rows = read_rows(&conn, &tables)?;
    validate_event_graph(&tables, &rows, "private OpenCode database")?;
    for source in source_tables {
        if let Some(existing) = tables.iter().find(|table| table.name == source.name)
            && (existing.column_specs != source.column_specs
                || existing.foreign_keys != source.foreign_keys
                || existing.without_rowid != source.without_rowid
                || existing.strict != source.strict
                || existing.sql_signature != source.sql_signature)
        {
            bail!("destination schema conflict for table {}", source.name)
        }
    }
    for source in source_indexes {
        let existing: Option<String> = conn
            .query_row(
                "SELECT tbl_name FROM sqlite_schema WHERE type='index' AND name=?1",
                [&source.name],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(table) = existing {
            let (unique, columns, partial) = index_signature(&conn, &source.name, &table)?;
            let sql = schema_object_sql(&conn, "index", &source.name)?;
            if table != source.table
                || unique != source.unique
                || columns != source.columns
                || partial != source.partial
                || schema_sql_signature(&sql)? != source.sql_signature
            {
                bail!("destination schema conflict for index {}", source.name)
            }
        }
    }
    Ok(())
}

fn merge_destination(
    path: &Path,
    tables: &[Table],
    indexes: &[Index],
    rows: &BTreeMap<String, Vec<Row>>,
) -> Result<MergeOutcome> {
    let parent = path
        .parent()
        .context("OpenCode destination has no parent")?;
    super::ensure_private_directory(parent)?;
    let existed = match fs::symlink_metadata(path) {
        Ok(_) => {
            super::ensure_private_file(path)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let staging = parent.join(format!(".opencode.db.migrate-{}", std::process::id()));
    let target = if existed { path } else { &staging };
    if !existed && staging.exists() {
        fs::remove_file(&staging)?;
    }
    if !existed {
        super::ensure_private_file(target)?;
    }
    let mut conn = Connection::open_with_flags(
        target,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    set_private_file(target)?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let transaction = conn.transaction()?;
    let result = (|| -> Result<MergeOutcome> {
        if existed {
            let integrity: String =
                transaction.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                bail!("destination OpenCode integrity check failed")
            }
            let violations: i64 = transaction.query_row(
                "SELECT count(*) FROM pragma_foreign_key_check",
                [],
                |row| row.get(0),
            )?;
            if violations != 0 {
                bail!("destination OpenCode foreign-key check failed")
            }
            let (destination_tables, _) = probe_schema(&transaction, false)?;
            validate_event_schema(&transaction, &destination_tables, false)?;
            let destination_rows = read_rows(&transaction, &destination_tables)?;
            validate_event_graph(
                &destination_tables,
                &destination_rows,
                "private OpenCode database",
            )?;
        }
        // Build the complete runtime schema first. Only a newly constructed
        // database may be seeded from source journals; existing destinations
        // own every existing row, including private/global state.
        for table in tables.iter().filter(|table| table.create_schema) {
            ensure_table(&transaction, table)?;
        }
        for index in indexes {
            ensure_index(&transaction, index)?;
        }
        let mut outcome = MergeOutcome::default();
        merge_event_graph(&transaction, parent, tables, rows, &mut outcome)?;
        for table in tables.iter().filter(|table| {
            table.copy_rows
                && (!SCHEMA_TABLES.contains(&table.name.as_str()) || !existed)
                && !matches!(table.name.as_str(), "event" | "event_sequence")
        }) {
            if table.primary_key.is_empty() {
                outcome.changed += merge_keyless_rows(&transaction, table, &rows[&table.name])?;
            } else {
                for row in &rows[&table.name] {
                    match insert_or_compare(&transaction, table, row)? {
                        RowMerge::Inserted => outcome.changed += 1,
                        RowMerge::Identical => {}
                        RowMerge::Conflict => {
                            outcome.conflicts += 1;
                            outcome.private_preserved_conflicts += 1;
                        }
                    }
                }
            }
        }
        validate_complete_destination(&transaction, tables, indexes)?;
        let integrity: String =
            transaction.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("destination OpenCode integrity check failed")
        }
        let violations: i64 =
            transaction.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            bail!("destination OpenCode foreign-key check failed")
        }
        Ok(outcome)
    })();
    match result {
        Ok(outcome) => {
            transaction.commit()?;
            drop(conn);
            if !existed {
                fs::rename(&staging, path)?;
                sync_directory(parent)?;
            }
            Ok(outcome)
        }
        Err(error) => {
            drop(transaction);
            drop(conn);
            if !existed {
                let _ = fs::remove_file(&staging);
            }
            Err(error)
        }
    }
}

fn merge_event_graph(
    conn: &Connection,
    parent: &Path,
    tables: &[Table],
    source_rows: &RowsByTable,
    outcome: &mut MergeOutcome,
) -> Result<()> {
    let Some(sequence_table) = tables.iter().find(|table| table.name == "event_sequence") else {
        return Ok(());
    };
    let event_table = tables
        .iter()
        .find(|table| table.name == "event")
        .context("event table missing")?;
    let destination_rows = read_rows(conn, &[sequence_table.clone(), event_table.clone()])?;
    validate_event_graph(
        &[sequence_table.clone(), event_table.clone()],
        &destination_rows,
        "private OpenCode database",
    )?;
    let source = event_histories(sequence_table, event_table, source_rows)?;
    let destination = event_histories(sequence_table, event_table, &destination_rows)?;
    let destination_ids = destination
        .values()
        .flat_map(|history| history.events.iter())
        .map(|row| required_text(event_table, row, "id", "private OpenCode database"))
        .collect::<Result<BTreeSet<_>>>()?;
    for (aggregate, source_history) in source {
        let Some(private_history) = destination.get(&aggregate) else {
            let reuses_id = source_history
                .events
                .iter()
                .try_fold(false, |reused, row| {
                    Ok::<_, anyhow::Error>(
                        reused
                            || destination_ids.contains(required_text(
                                event_table,
                                row,
                                "id",
                                "global OpenCode database",
                            )?),
                    )
                })?;
            if reuses_id {
                if archive_event_fork(parent, &source_history)? {
                    outcome.archived_event_forks += 1;
                }
                outcome.conflicts += 1;
                continue;
            }
            insert_row(conn, sequence_table, &source_history.sequence)?;
            outcome.changed += 1;
            for event in &source_history.events {
                insert_row(conn, event_table, event)?;
                outcome.changed += 1;
            }
            continue;
        };
        let common_len = source_history
            .events
            .len()
            .min(private_history.events.len());
        let common_is_identical =
            source_history.events[..common_len] == private_history.events[..common_len];
        let source_tail_reuses_id =
            source_history.events[common_len..]
                .iter()
                .try_fold(false, |reused, row| {
                    Ok::<_, anyhow::Error>(
                        reused
                            || destination_ids.contains(required_text(
                                event_table,
                                row,
                                "id",
                                "global OpenCode database",
                            )?),
                    )
                })?;
        if !common_is_identical || source_tail_reuses_id {
            if archive_event_fork(parent, &source_history)? {
                outcome.archived_event_forks += 1;
            }
            outcome.conflicts += 1;
            continue;
        }
        if source_history.events.len() <= private_history.events.len() {
            continue;
        }
        for event in &source_history.events[common_len..] {
            insert_row(conn, event_table, event)?;
            outcome.changed += 1;
        }
        let source_seq = required_integer(
            sequence_table,
            &source_history.sequence,
            "seq",
            "global OpenCode database",
        )?;
        conn.execute(
            "UPDATE event_sequence SET seq=?1 WHERE aggregate_id=?2",
            (&source_seq, &aggregate),
        )?;
        outcome.changed += 1;
    }
    Ok(())
}

struct EventHistory {
    sequence: Row,
    events: Vec<Row>,
}

fn event_histories(
    sequence_table: &Table,
    event_table: &Table,
    rows: &RowsByTable,
) -> Result<BTreeMap<String, EventHistory>> {
    let mut histories = BTreeMap::new();
    for sequence in &rows["event_sequence"] {
        let aggregate = required_text(
            sequence_table,
            sequence,
            "aggregate_id",
            "OpenCode event sequence",
        )?;
        histories.insert(
            aggregate.to_string(),
            EventHistory {
                sequence: sequence.clone(),
                events: Vec::new(),
            },
        );
    }
    for event in &rows["event"] {
        let aggregate = required_text(event_table, event, "aggregate_id", "OpenCode event")?;
        histories
            .get_mut(aggregate)
            .context("event lacks sequence row")?
            .events
            .push(event.clone());
    }
    let seq_index = field_index(event_table, "seq")?;
    for history in histories.values_mut() {
        history
            .events
            .sort_by_key(|row| match &row.values[seq_index] {
                Value::Integer(seq) => *seq,
                _ => i64::MIN,
            });
    }
    Ok(histories)
}

fn archive_event_fork(parent: &Path, history: &EventHistory) -> Result<bool> {
    let path = parent.join("rtrt-event-forks.sqlite");
    super::ensure_private_file(&path)?;
    let mut key = row_key(&history.sequence);
    for event in &history.events {
        key.extend_from_slice(&row_key(event));
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in key {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let fork_id = format!("v1-{hash:016x}");
    let mut archive = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    set_private_file(&path)?;
    archive.pragma_update(None, "foreign_keys", true)?;
    archive.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_fork(
             id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL, seq INTEGER NOT NULL, owner_id TEXT
         );
         CREATE TABLE IF NOT EXISTS event_fork_event(
             fork_id TEXT NOT NULL REFERENCES event_fork(id) ON DELETE CASCADE,
             id TEXT NOT NULL, aggregate_id TEXT NOT NULL, seq INTEGER NOT NULL,
             type TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY(fork_id,seq)
         );",
    )?;
    let unsupported: i64 = archive.query_row(
        "SELECT count(*) FROM sqlite_schema
         WHERE type IN ('trigger','view')
            OR (type='table' AND name NOT IN ('event_fork','event_fork_event') AND name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get(0),
    )?;
    if unsupported != 0
        || table_columns(&archive, "event_fork")? != ["id", "aggregate_id", "seq", "owner_id"]
        || table_columns(&archive, "event_fork_event")?
            != ["fork_id", "id", "aggregate_id", "seq", "type", "data"]
    {
        bail!("unsafe event fork archive schema")
    }
    let transaction = archive.transaction()?;
    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO event_fork(id,aggregate_id,seq,owner_id) VALUES(?1,?2,?3,?4)",
        (
            &fork_id,
            &history.sequence.values[0],
            &history.sequence.values[1],
            &history.sequence.values[2],
        ),
    )? == 1;
    if inserted {
        for event in &history.events {
            transaction.execute(
                "INSERT INTO event_fork_event(fork_id,id,aggregate_id,seq,type,data) VALUES(?1,?2,?3,?4,?5,?6)",
                params_from_iter(
                    std::iter::once(&Value::Text(fork_id.clone())).chain(event.values.iter()),
                ),
            )?;
        }
    } else {
        let stored_sequence = transaction.query_row(
            "SELECT aggregate_id,seq,owner_id FROM event_fork WHERE id=?1",
            [&fork_id],
            |row| {
                Ok(Row {
                    values: (0..3)
                        .map(|index| row.get_ref(index).map(value_owned))
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                })
            },
        )?;
        let mut statement = transaction.prepare(
            "SELECT id,aggregate_id,seq,type,data FROM event_fork_event WHERE fork_id=?1 ORDER BY seq",
        )?;
        let stored_events = statement
            .query_map([&fork_id], |row| {
                Ok(Row {
                    values: (0..5)
                        .map(|index| row.get_ref(index).map(value_owned))
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if stored_sequence != history.sequence || stored_events != history.events {
            bail!("event fork archive hash collision or corruption")
        }
    }
    let integrity: String =
        transaction.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("event fork archive integrity check failed")
    }
    let violations: i64 =
        transaction.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        bail!("event fork archive foreign-key check failed")
    }
    transaction.commit()?;
    drop(archive);
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?
        .sync_all()?;
    sync_directory(parent)?;
    Ok(inserted)
}

fn ensure_table(conn: &Connection, table: &Table) -> Result<()> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name=?1",
            [&table.name],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        conn.execute_batch(&table.sql)?;
        return Ok(());
    }
    let actual_columns = table_column_specs(conn, &table.name)?;
    let actual_foreign_keys = table_foreign_keys(conn, &table.name)?;
    let (actual_without_rowid, actual_strict) = table_options(conn, &table.name)?;
    let actual_sql = schema_object_sql(conn, "table", &table.name)?;
    if actual_columns != table.column_specs
        || actual_foreign_keys != table.foreign_keys
        || actual_without_rowid != table.without_rowid
        || actual_strict != table.strict
        || schema_sql_signature(&actual_sql)? != table.sql_signature
    {
        bail!("destination schema conflict for table {}", table.name)
    }
    Ok(())
}

fn ensure_index(conn: &Connection, index: &Index) -> Result<()> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_schema WHERE type='index' AND name=?1",
            [&index.name],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(table) = existing {
        let (unique, columns, partial) = index_signature(conn, &index.name, &table)?;
        let actual_sql = schema_object_sql(conn, "index", &index.name)?;
        if table != index.table
            || unique != index.unique
            || columns != index.columns
            || partial != index.partial
            || schema_sql_signature(&actual_sql)? != index.sql_signature
        {
            bail!("destination schema conflict for index {}", index.name)
        }
        return Ok(());
    }
    conn.execute_batch(&index.sql)?;
    Ok(())
}

fn validate_complete_destination(
    conn: &Connection,
    source_tables: &[Table],
    source_indexes: &[Index],
) -> Result<()> {
    let (destination_tables, _) = probe_schema(conn, false)?;
    for source in source_tables {
        let destination = destination_tables
            .iter()
            .find(|table| table.name == source.name)
            .with_context(|| format!("destination schema lacks table {}", source.name))?;
        if destination.column_specs != source.column_specs
            || destination.foreign_keys != source.foreign_keys
            || destination.without_rowid != source.without_rowid
            || destination.strict != source.strict
            || destination.sql_signature != source.sql_signature
        {
            bail!("destination schema conflict for table {}", source.name)
        }
    }
    for source in source_indexes {
        let table: String = conn
            .query_row(
                "SELECT tbl_name FROM sqlite_schema WHERE type='index' AND name=?1",
                [&source.name],
                |row| row.get(0),
            )
            .with_context(|| format!("destination schema lacks index {}", source.name))?;
        let (unique, columns, partial) = index_signature(conn, &source.name, &table)?;
        let sql = schema_object_sql(conn, "index", &source.name)?;
        if table != source.table
            || unique != source.unique
            || columns != source.columns
            || partial != source.partial
            || schema_sql_signature(&sql)? != source.sql_signature
        {
            bail!("destination schema conflict for index {}", source.name)
        }
    }
    Ok(())
}

fn merge_keyless_rows(conn: &Connection, table: &Table, rows: &[Row]) -> Result<usize> {
    let mut source_counts = BTreeMap::<Vec<u8>, (&Row, usize)>::new();
    for row in rows {
        source_counts
            .entry(row_key(row))
            .and_modify(|(_, count)| *count += 1)
            .or_insert((row, 1));
    }
    let predicate = table
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let value_parameter = index * 2 + 2;
            let type_parameter = value_parameter - 1;
            format!(
                "(typeof({}) = ?{type_parameter} AND {} IS ?{value_parameter})",
                quote(column),
                quote(column)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let count_sql = format!(
        "SELECT count(*) FROM {} WHERE {predicate}",
        quote(&table.name)
    );
    let mut changed = 0;
    for (_key, (row, source_count)) in source_counts {
        let mut comparison = Vec::with_capacity(row.values.len() * 2);
        for value in &row.values {
            comparison.push(Value::Text(sqlite_value_type(value).to_string()));
            comparison.push(value.clone());
        }
        let destination_count: i64 =
            conn.query_row(&count_sql, params_from_iter(comparison.iter()), |found| {
                found.get(0)
            })?;
        let missing = source_count.saturating_sub(usize::try_from(destination_count)?);
        for _ in 0..missing {
            insert_row(conn, table, row)?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn sqlite_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Integer(_) => "integer",
        Value::Real(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
    }
}

fn row_key(row: &Row) -> Vec<u8> {
    let mut key = Vec::new();
    for value in &row.values {
        match value {
            Value::Null => key.push(0),
            Value::Integer(value) => {
                key.push(1);
                key.extend_from_slice(&value.to_le_bytes());
            }
            Value::Real(value) => {
                key.push(2);
                key.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            Value::Text(value) => append_key_bytes(&mut key, 3, value.as_bytes()),
            Value::Blob(value) => append_key_bytes(&mut key, 4, value),
        }
    }
    key
}

fn append_key_bytes(key: &mut Vec<u8>, tag: u8, value: &[u8]) {
    key.push(tag);
    key.extend_from_slice(&(value.len() as u64).to_le_bytes());
    key.extend_from_slice(value);
}

fn insert_row(conn: &Connection, table: &Table, row: &Row) -> Result<()> {
    let columns = table
        .columns
        .iter()
        .map(|column| quote(column))
        .collect::<Vec<_>>()
        .join(",");
    let placeholders = (1..=row.values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO {} ({columns}) VALUES ({placeholders})",
        quote(&table.name)
    );
    conn.execute(&sql, params_from_iter(row.values.iter()))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowMerge {
    Inserted,
    Identical,
    Conflict,
}

fn insert_or_compare(conn: &Connection, table: &Table, row: &Row) -> Result<RowMerge> {
    let columns = table
        .columns
        .iter()
        .map(|v| quote(v))
        .collect::<Vec<_>>()
        .join(",");
    if !table.primary_key.is_empty() {
        let predicate = table
            .primary_key
            .iter()
            .enumerate()
            .map(|(parameter, index)| {
                format!("{} IS ?{}", quote(&table.columns[*index]), parameter + 1)
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let keys = table.primary_key.iter().map(|index| &row.values[*index]);
        let sql = format!(
            "SELECT {columns} FROM {} WHERE {predicate}",
            quote(&table.name)
        );
        let existing = conn
            .query_row(&sql, params_from_iter(keys), |found| {
                let mut values = Vec::with_capacity(row.values.len());
                for index in 0..row.values.len() {
                    values.push(value_owned(found.get_ref(index)?));
                }
                Ok(values)
            })
            .optional()?;
        if let Some(existing) = existing {
            if existing != row.values {
                return Ok(RowMerge::Conflict);
            }
            return Ok(RowMerge::Identical);
        }
    }
    insert_row(conn, table, row)?;
    Ok(RowMerge::Inserted)
}

fn acquire_lock(path: &Path, mode: MigrationMode) -> Result<Option<OperationLock>> {
    super::ensure_private_file(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    set_private_file(path)?;
    let timeout = if mode == MigrationMode::CatchUp {
        std::time::Duration::from_millis(200)
    } else {
        std::time::Duration::from_secs(5)
    };
    connection.busy_timeout(timeout)?;
    if let Err(error) = connection.execute_batch("BEGIN EXCLUSIVE") {
        if mode == MigrationMode::CatchUp && sqlite_is_busy(&error) {
            return Ok(None);
        }
        return Err(error).context("another OpenCode session migration is active");
    }
    Ok(Some(OperationLock {
        _connection: connection,
    }))
}

fn sqlite_is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn manifest_generation(root: &Path) -> Result<Option<SourceGeneration>> {
    let path = root.join("opencode-session-migration.json");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => super::ensure_private_file(&path)?,
    }
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > 64 * 1024 {
        bail!("OpenCode migration manifest exceeds 64 KiB")
    }
    let value: serde_json::Value = serde_json::from_reader(file)?;
    Ok(value
        .get("source_generation")
        .and_then(serde_json::Value::as_str)
        .map(|value| SourceGeneration(value.to_string())))
}

fn write_manifest(
    root: &Path,
    source: &Path,
    generation: &SourceGeneration,
    report: &MigrationReport,
) -> Result<()> {
    let path = root.join("opencode-session-migration.json");
    let staging = root.join(format!(
        ".opencode-session-migration-{}.json",
        std::process::id()
    ));
    let payload = serde_json::to_vec(&json!({
        "version": 3, "source": source, "source_generation": &generation.0,
        "sessions": report.sessions,
        "projects": report.projects, "archived_sessions": report.archived_sessions,
        "skipped_malformed_sessions": report.skipped_malformed_sessions,
        "rows": report.rows, "conflicts": report.conflicts,
        "private_preserved_conflicts": report.private_preserved_conflicts,
        "archived_event_forks": report.archived_event_forks
    }))?;
    if staging.exists() {
        super::ensure_private_file(&staging)?;
        fs::remove_file(&staging)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&staging)?;
    file.write_all(&payload)?;
    file.sync_all()?;
    fs::rename(staging, path)?;
    sync_directory(root)?;
    Ok(())
}

fn sync_directory(#[cfg_attr(not(unix), allow(unused_variables))] path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn set_private_file(#[cfg_attr(not(unix), allow(unused_variables))] path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical only where it matters. macOS reaches the temp dir through
    /// `/var -> /private/var`, which breaks comparisons against canonical paths.
    /// Windows canonicalization instead yields a `\\?\` verbatim path, which the
    /// production code rejects, so the plain temp path is the correct fixture there.
    fn canonical_for_tests(path: &std::path::Path) -> std::path::PathBuf {
        #[cfg(unix)]
        {
            std::fs::canonicalize(path).expect("canonicalize temp path")
        }
        #[cfg(not(unix))]
        {
            path.to_path_buf()
        }
    }

    /// macOS resolves the system temp dir through `/var -> /private/var`, and the
    /// migration deliberately refuses to open a database reached through a
    /// symlink. Tests therefore need a canonical root, not the raw handle path.
    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let handle = tempfile::tempdir().unwrap();
        let root = canonical_for_tests(handle.path());
        (handle, root)
    }

    fn fixture(temp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let first = temp.join("a/same");
        let second = temp.join("b/same");
        fs::create_dir_all(first.join(".git")).unwrap();
        fs::create_dir_all(second.join(".git")).unwrap();
        let db = temp.join("global.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE project(id TEXT PRIMARY KEY, worktree TEXT);
             CREATE TABLE project_directory(project_id TEXT NOT NULL REFERENCES project(id), directory TEXT NOT NULL, PRIMARY KEY(project_id,directory));
             CREATE TABLE workspace(id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id), directory TEXT);
             CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT REFERENCES project(id), workspace_id TEXT REFERENCES workspace(id), parent_id TEXT REFERENCES session(id), directory TEXT);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id), session_id TEXT, data BLOB);
             CREATE TABLE todo(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), content TEXT);
             CREATE TABLE session_share(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
             CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
             CREATE TABLE session_input(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
             CREATE TABLE session_context_epoch(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
             CREATE TABLE event_sequence(aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
             CREATE TABLE event(id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL, seq INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL, FOREIGN KEY (aggregate_id) REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE);
             CREATE TABLE migration(id TEXT PRIMARY KEY, hash TEXT);
             CREATE TABLE data_migration(id TEXT PRIMARY KEY, hash TEXT);
             CREATE TABLE __drizzle_migrations(id INTEGER PRIMARY KEY, hash TEXT);
             CREATE TABLE session_projection(id TEXT PRIMARY KEY, message_id TEXT REFERENCES message(id), data TEXT);
             CREATE TABLE session_event(id TEXT PRIMARY KEY, session_id TEXT REFERENCES session(id), data TEXT);
             CREATE TABLE account(id TEXT PRIMARY KEY, credential TEXT);
             CREATE TABLE account_state(id TEXT PRIMARY KEY, project_id TEXT, secret TEXT);
             CREATE TABLE control_account(id TEXT PRIMARY KEY, secret TEXT);
             CREATE TABLE credential(id TEXT PRIMARY KEY, secret TEXT);
             CREATE TABLE permission(id TEXT PRIMARY KEY, session_id TEXT, approval TEXT);
             CREATE TABLE approval(id TEXT PRIMARY KEY, session_id TEXT, secret TEXT);
             CREATE TABLE auth(id TEXT PRIMARY KEY, secret TEXT);
             CREATE TABLE authentication(id TEXT PRIMARY KEY, secret TEXT);
             CREATE TABLE authorization(id TEXT PRIMARY KEY, session_id TEXT, secret TEXT);
             CREATE TABLE runtime_setting(key TEXT PRIMARY KEY, value TEXT NOT NULL DEFAULT 'enabled');
             CREATE UNIQUE INDEX part_message_unique ON part(message_id);
             CREATE INDEX todo_session_idx ON todo(session_id);
             CREATE UNIQUE INDEX event_aggregate_seq ON event(aggregate_id,seq);
             CREATE INDEX event_aggregate_type_seq ON event(aggregate_id,type,seq);
             CREATE UNIQUE INDEX credential_secret_unique ON credential(secret);
             CREATE INDEX permission_session_idx ON permission(session_id);
             CREATE INDEX runtime_setting_value_idx ON runtime_setting(value);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project VALUES('p1',?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project VALUES('p2',?1)",
            [&second.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_directory VALUES('p1',?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_directory VALUES('p2',?1)",
            [&second.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspace VALUES('w1','p1',?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspace VALUES('w2','p2',?1)",
            [&second.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES('s1','p1','w1',NULL,?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES('child','p1','w1','s1','/deleted')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES('s2','p2','w2',NULL,?1)",
            [&second.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES('lost',NULL,NULL,NULL,'/deleted')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO message VALUES('m1','s1','opaque prompt')", [])
            .unwrap();
        conn.execute("INSERT INTO message VALUES('m2','s2','opaque second')", [])
            .unwrap();
        conn.execute("INSERT INTO message VALUES('ml','lost','opaque lost')", [])
            .unwrap();
        conn.execute("INSERT INTO part VALUES('x','m1','s1',x'0102')", [])
            .unwrap();
        conn.execute("INSERT INTO part VALUES('x2','m2','s2',x'0304')", [])
            .unwrap();
        conn.execute("INSERT INTO part VALUES('xl','ml','lost',x'0506')", [])
            .unwrap();
        conn.execute("INSERT INTO todo VALUES('t1','child','todo')", [])
            .unwrap();
        conn.execute("INSERT INTO todo VALUES('t2','child','todo')", [])
            .unwrap();
        conn.execute("INSERT INTO todo VALUES('t-second','s2','second')", [])
            .unwrap();
        conn.execute("INSERT INTO todo VALUES('t-lost','lost','lost')", [])
            .unwrap();
        conn.execute("INSERT INTO session_share VALUES('sh1','s1','opaque')", [])
            .unwrap();
        conn.execute("INSERT INTO session_share VALUES('sh2','s2','opaque')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO session_share VALUES('shl','lost','opaque')",
            [],
        )
        .unwrap();
        for table in ["session_message", "session_input", "session_context_epoch"] {
            for (suffix, session) in [("1", "s1"), ("2", "s2"), ("l", "lost")] {
                conn.execute(
                    &format!("INSERT INTO {table} VALUES(?1,?2,?3)"),
                    [
                        format!("{table}-{suffix}"),
                        session.to_string(),
                        "opaque".to_string(),
                    ],
                )
                .unwrap();
            }
        }
        conn.execute_batch(
            "INSERT INTO event_sequence VALUES('s1',0,'global-owner');
             INSERT INTO event_sequence VALUES('s2',0,'global-owner');
             INSERT INTO event_sequence VALUES('unknown-aggregate',0,NULL);
             INSERT INTO event VALUES('e1','s1',0,'message','opaque-event-one');
             INSERT INTO event VALUES('e2','s2',0,'message','opaque-event-two');
             INSERT INTO event VALUES('eu','unknown-aggregate',0,'message','opaque-event-unknown');
             INSERT INTO migration VALUES('migration-opaque','hash-a');
             INSERT INTO data_migration VALUES('data-migration-opaque','hash-b');
             INSERT INTO __drizzle_migrations VALUES(17,'hash-c');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_projection VALUES('pr1','m1','opaque')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO session_event VALUES('ev1','s1','opaque')", [])
            .unwrap();
        conn.execute("INSERT INTO account VALUES('acct','secret')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO account_state VALUES('state','p1','secret')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO control_account VALUES('control','secret')", [])
            .unwrap();
        conn.execute("INSERT INTO credential VALUES('credential','secret')", [])
            .unwrap();
        conn.execute("INSERT INTO permission VALUES('perm','s1','always')", [])
            .unwrap();
        conn.execute("INSERT INTO approval VALUES('approval','s1','secret')", [])
            .unwrap();
        conn.execute("INSERT INTO auth VALUES('auth','secret')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO authentication VALUES('authentication','secret')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO authorization VALUES('authorization','s1','secret')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO runtime_setting VALUES('mode','private')", [])
            .unwrap();
        drop(conn);
        (db, first, second)
    }

    fn probed_table(connection: &Connection, name: &str) -> Table {
        probe_schema(connection, false)
            .unwrap()
            .0
            .into_iter()
            .find(|table| table.name == name)
            .unwrap()
    }

    fn probed_index(connection: &Connection, name: &str) -> Index {
        probe_schema(connection, false)
            .unwrap()
            .1
            .into_iter()
            .find(|index| index.name == name)
            .unwrap()
    }

    #[test]
    fn schema_compatibility_accepts_harmless_sql_formatting() {
        let source = Connection::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE sample(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0),
                    normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED,
                    parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL
                        DEFERRABLE INITIALLY DEFERRED
                 ) STRICT;
                 CREATE UNIQUE INDEX sample_idx
                    ON sample(lower(name) COLLATE NOCASE DESC, id ASC)
                    WHERE name <> '';",
            )
            .unwrap();
        let table = probed_table(&source, "sample");
        let index = probed_index(&source, "sample_idx");

        let destination = Connection::open_in_memory().unwrap();
        destination
            .execute_batch(
                "create /* formatting */ table sample (
                    id integer primary key autoincrement,
                    name text collate nocase not null on conflict fail
                        check ( length ( name ) > 0 ),
                    normalized text generated always as( lower ( name ) ) stored,
                    parent integer references sample ( id )
                        on update cascade on delete set null deferrable initially deferred
                 ) strict;
                 create unique index sample_idx on sample (
                    lower ( name ) collate nocase desc, id asc
                 ) where name<>'';",
            )
            .unwrap();

        ensure_table(&destination, &table).unwrap();
        ensure_index(&destination, &index).unwrap();
    }

    #[test]
    fn table_schema_semantic_mismatches_fail_closed() {
        let source = Connection::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE sample(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0),
                    normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED,
                    parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL
                        DEFERRABLE INITIALLY DEFERRED
                 ) STRICT;",
            )
            .unwrap();
        let table = probed_table(&source, "sample");
        let mismatches = [
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) >= 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE RTRIM NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT IGNORE CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (upper(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL NOT DEFERRABLE) STRICT",
            "CREATE TABLE sample(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT COLLATE NOCASE NOT NULL ON CONFLICT FAIL CHECK(length(name) > 0), normalized TEXT GENERATED ALWAYS AS (lower(name)) STORED, parent INTEGER REFERENCES sample(id) ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED)",
        ];
        for sql in mismatches {
            let destination = Connection::open_in_memory().unwrap();
            destination.execute_batch(sql).unwrap();
            let error = ensure_table(&destination, &table).unwrap_err();
            assert!(error.to_string().contains("schema conflict"), "{sql}");
        }

        let rowid_source = Connection::open_in_memory().unwrap();
        rowid_source
            .execute_batch("CREATE TABLE keyed(a TEXT, b TEXT, PRIMARY KEY(a,b)) WITHOUT ROWID")
            .unwrap();
        let rowid_table = probed_table(&rowid_source, "keyed");
        let destination = Connection::open_in_memory().unwrap();
        destination
            .execute_batch("CREATE TABLE keyed(a TEXT, b TEXT, PRIMARY KEY(a,b))")
            .unwrap();
        assert!(ensure_table(&destination, &rowid_table).is_err());
    }

    #[test]
    fn index_schema_semantic_mismatches_fail_closed() {
        let source = Connection::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE sample(id INTEGER PRIMARY KEY, name TEXT);
                 CREATE UNIQUE INDEX sample_idx
                    ON sample(lower(name) COLLATE NOCASE DESC, id ASC)
                    WHERE name <> '';",
            )
            .unwrap();
        let index = probed_index(&source, "sample_idx");
        let mismatches = [
            "CREATE UNIQUE INDEX sample_idx ON sample(upper(name) COLLATE NOCASE DESC,id ASC) WHERE name <> ''",
            "CREATE UNIQUE INDEX sample_idx ON sample(lower(name) COLLATE BINARY DESC,id ASC) WHERE name <> ''",
            "CREATE UNIQUE INDEX sample_idx ON sample(lower(name) COLLATE NOCASE ASC,id ASC) WHERE name <> ''",
            "CREATE UNIQUE INDEX sample_idx ON sample(lower(name) COLLATE NOCASE DESC,id ASC) WHERE name = ''",
            "CREATE INDEX sample_idx ON sample(lower(name) COLLATE NOCASE DESC,id ASC) WHERE name <> ''",
        ];
        for sql in mismatches {
            let destination = Connection::open_in_memory().unwrap();
            destination
                .execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, name TEXT)")
                .unwrap();
            destination.execute_batch(sql).unwrap();
            let error = ensure_index(&destination, &index).unwrap_err();
            assert!(error.to_string().contains("schema conflict"), "{sql}");
        }
    }

    #[test]
    fn opencode_1_18_current_schema_routes_complete_resumable_graph() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, second) = fixture(&temp);
        let before = fs::read(&source).unwrap();
        let options = MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        };
        let report = migrate(&options).unwrap();
        assert_eq!(
            (report.sessions, report.projects, report.archived_sessions),
            (4, 2, 1)
        );
        assert_eq!(fs::read(&source).unwrap(), before);
        let one = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        let two = destination_db(&home, ProjectIdentity::derive(second).unwrap().slug());
        assert_ne!(one, two);
        let conn = Connection::open(&one).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT group_concat(name, ',') FROM pragma_table_info('event_sequence')",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "aggregate_id,seq,owner_id"
        );
        assert_eq!(
            conn.query_row(
                "SELECT group_concat(name, ',') FROM pragma_table_info('event')",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "id,aggregate_id,seq,type,data"
        );
        assert_eq!(
            conn.query_row(
                "SELECT on_delete FROM pragma_foreign_key_list('event') WHERE \"table\"='event_sequence' AND \"from\"='aggregate_id' AND \"to\"='aggregate_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "CASCADE"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM session", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT parent_id FROM session WHERE id='child'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "s1"
        );
        assert_eq!(
            conn.query_row("SELECT workspace_id FROM session WHERE id='s1'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "w1"
        );
        assert_eq!(
            conn.query_row("SELECT data FROM message WHERE id='m1'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "opaque prompt"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM message", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM part", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT hex(data) FROM part WHERE id='x'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "0102"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM todo WHERE content='todo'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM todo", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        for index in [
            "part_message_unique",
            "todo_session_idx",
            "event_aggregate_seq",
            "event_aggregate_type_seq",
            "credential_secret_unique",
            "permission_session_idx",
            "runtime_setting_value_idx",
        ] {
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name=?1 AND sql IS NOT NULL",
                    [index],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                1,
                "{index}"
            );
        }
        for table in [
            "account",
            "account_state",
            "control_account",
            "credential",
            "permission",
            "approval",
            "auth",
            "authentication",
            "authorization",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "sensitive table {table}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runtime_setting", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        // Representative OpenCode startup preparation: every runtime table can
        // be resolved after journals have declared all migrations applied.
        for table in [
            "project",
            "project_directory",
            "workspace",
            "session",
            "message",
            "part",
            "todo",
            "session_share",
            "session_message",
            "session_input",
            "session_context_epoch",
            "event_sequence",
            "event",
            "session_projection",
            "session_event",
            "account",
            "account_state",
            "control_account",
            "credential",
            "permission",
            "approval",
            "auth",
            "authentication",
            "authorization",
            "runtime_setting",
            "migration",
            "data_migration",
            "__drizzle_migrations",
        ] {
            conn.prepare(&format!("SELECT * FROM {table} LIMIT 0"))
                .unwrap_or_else(|error| panic!("startup query for {table}: {error}"));
        }
        for table in [
            "project",
            "project_directory",
            "workspace",
            "session_share",
            "session_projection",
            "session_event",
            "session_message",
            "session_input",
            "session_context_epoch",
            "event_sequence",
            "event",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
        for table in ["migration", "data_migration", "__drizzle_migrations"] {
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1,
                "metadata table {table}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
        drop(conn);
        let conn = Connection::open(&two).unwrap();
        for (table, predicate) in [
            ("project", "id='p2'"),
            ("project_directory", "project_id='p2'"),
            ("workspace", "id='w2'"),
            ("session", "id='s2'"),
            ("message", "id='m2'"),
            ("part", "id='x2'"),
            ("todo", "id='t-second'"),
            ("session_share", "id='sh2'"),
            ("session_message", "id='session_message-2'"),
            ("session_input", "id='session_input-2'"),
            ("session_context_epoch", "id='session_context_epoch-2'"),
            ("event_sequence", "aggregate_id='s2'"),
            ("event", "id='e2'"),
        ] {
            let count: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE {predicate}"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "second project {table}");
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
                1,
                "second project isolation {table}"
            );
        }
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM session WHERE id IN ('s1','child','lost')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(conn);
        let archive = Connection::open(destination_db(&home, "legacy-global")).unwrap();
        assert_eq!(
            archive
                .query_row("SELECT count(*) FROM session", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        for (table, predicate) in [
            ("message", "id='ml'"),
            ("part", "id='xl'"),
            ("todo", "id='t-lost'"),
            ("session_share", "id='shl'"),
            ("session_message", "id='session_message-l'"),
            ("session_input", "id='session_input-l'"),
            ("session_context_epoch", "id='session_context_epoch-l'"),
            ("event_sequence", "aggregate_id='unknown-aggregate'"),
            ("event", "id='eu'"),
        ] {
            assert_eq!(
                archive
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE {predicate}"),
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                1,
                "archive {table}"
            );
            assert_eq!(
                archive
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap(),
                1,
                "archive isolation {table}"
            );
        }
        assert_eq!(
            archive
                .query_row("SELECT count(*) FROM project", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(archive);
        assert_eq!(migrate(&options).unwrap().changed_rows, 0);

        Connection::open(&one)
            .unwrap()
            .execute(
                "INSERT INTO todo VALUES('private','s1','private-extra')",
                [],
            )
            .unwrap();
        Connection::open(&source)
            .unwrap()
            .execute("INSERT INTO todo VALUES('t3','child','todo')", [])
            .unwrap();
        assert_eq!(migrate(&options).unwrap().changed_rows, 1);
        let conn = Connection::open(one).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM todo WHERE content='todo'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM todo", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            4
        );
    }

    #[test]
    fn dry_run_writes_nothing_and_unknown_dependency_fails_closed() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, _, _) = fixture(&temp);
        let dry = MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::DryRun,
        };
        assert_eq!(migrate(&dry).unwrap().sessions, 4);
        assert!(!home.join(".rtrt").exists());
        Connection::open(&source)
            .unwrap()
            .execute_batch("CREATE TABLE session_tokens(session_id TEXT, token TEXT)")
            .unwrap();
        assert!(
            migrate(&dry)
                .unwrap_err()
                .to_string()
                .contains("unknown OpenCode graph-dependent table")
        );
    }

    #[test]
    fn explicit_migration_rejects_triggers_and_views() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, _, _) = fixture(&temp);
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER future_trigger AFTER INSERT ON message BEGIN SELECT 1; END;",
            )
            .unwrap();
        let options = MigrationOptions {
            home,
            source: Some(source),
            mode: MigrationMode::Apply,
        };
        assert!(
            migrate(&options)
                .unwrap_err()
                .to_string()
                .contains("unsupported OpenCode schema object type: trigger")
        );
        connection
            .execute_batch(
                "DROP TRIGGER future_trigger; CREATE VIEW future_view AS SELECT id FROM session;",
            )
            .unwrap();
        assert!(
            migrate(&options)
                .unwrap_err()
                .to_string()
                .contains("unsupported OpenCode schema object type: view")
        );
    }

    #[test]
    fn conflict_preserves_existing_private_rows_and_inserts_safe_rows() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, second) = fixture(&temp);
        let destination = destination_db(&home, ProjectIdentity::derive(&first).unwrap().slug());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let conn = Connection::open(&destination).unwrap();
        conn.execute_batch(
            "CREATE TABLE project(id TEXT PRIMARY KEY, worktree TEXT);
             CREATE TABLE workspace(id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id), directory TEXT);
             CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT REFERENCES project(id), workspace_id TEXT REFERENCES workspace(id), parent_id TEXT REFERENCES session(id), directory TEXT);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id), session_id TEXT, data BLOB);
             CREATE TABLE todo(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), content TEXT);
             CREATE TABLE migration(id TEXT PRIMARY KEY, hash TEXT);
             INSERT INTO migration VALUES('private-journal','private-hash');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project VALUES('p1',?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspace VALUES('w1','p1',?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES('s1','p1','w1',NULL,?1)",
            [&first.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES('private',NULL,NULL,NULL,'private')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO message VALUES('m1','s1','different')", [])
            .unwrap();
        drop(conn);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let options = MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        };
        let applied = migrate(&options).unwrap();
        assert_eq!(applied.private_preserved_conflicts, 1);
        let conn = Connection::open(&destination).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM session WHERE id='private'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM session WHERE id='s1'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT group_concat(id, ',') FROM migration", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "private-journal"
        );
        for journal in ["data_migration", "__drizzle_migrations"] {
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {journal}"), [], |row| row
                    .get::<_, i64>(0),)
                    .unwrap(),
                0
            );
        }
        for table in [
            "account",
            "account_state",
            "control_account",
            "credential",
            "permission",
            "runtime_setting",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "repaired schema {table}"
            );
        }
        drop(conn);
        let second_destination =
            destination_db(&home, ProjectIdentity::derive(second).unwrap().slug());
        assert_eq!(
            Connection::open(second_destination)
                .unwrap()
                .query_row("SELECT count(*) FROM session WHERE id='s2'", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );

        let private = Connection::open(&destination).unwrap();
        private
            .execute("INSERT INTO account VALUES('stale','must-be-cleared')", [])
            .unwrap();
        private
            .execute("INSERT INTO runtime_setting VALUES('stale','global')", [])
            .unwrap();
        private
            .execute(
                "INSERT INTO account_state VALUES('private-state','p1','private-secret')",
                [],
            )
            .unwrap();
        private
            .execute(
                "INSERT INTO credential VALUES('private-credential','private-secret')",
                [],
            )
            .unwrap();
        private
            .execute(
                "INSERT INTO permission VALUES('private-permission','s1','private')",
                [],
            )
            .unwrap();
        drop(private);
        let catch_up = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(catch_up.up_to_date);
        assert_eq!(catch_up.changed_rows, 0);
        let conn = Connection::open(&destination).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM session WHERE id='private'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM session WHERE id='s1'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
        for (table, predicate) in [
            ("account", "id='stale' AND credential='must-be-cleared'"),
            (
                "account_state",
                "id='private-state' AND secret='private-secret'",
            ),
            (
                "credential",
                "id='private-credential' AND secret='private-secret'",
            ),
            (
                "permission",
                "id='private-permission' AND approval='private'",
            ),
            ("runtime_setting", "key='stale' AND value='global'"),
        ] {
            assert_eq!(
                conn.query_row(
                    &format!("SELECT count(*) FROM {table} WHERE {predicate}"),
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                1,
                "preserved destination row in {table}"
            );
        }
        drop(conn);
        assert!(
            migrate(&MigrationOptions {
                home: home.clone(),
                source: options.source,
                mode: MigrationMode::CatchUp,
            })
            .unwrap()
            .up_to_date
        );
        assert!(!home.join(".rtrt/tmp").exists());
    }

    #[test]
    fn existing_database_missing_core_tables_is_repaired_to_complete_schema() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, _) = fixture(&temp);
        let destination = destination_db(&home, ProjectIdentity::derive(&first).unwrap().slug());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        Connection::open(&destination)
            .unwrap()
            .execute_batch(
                "CREATE TABLE migration(id TEXT PRIMARY KEY, hash TEXT);
                 INSERT INTO migration VALUES('private-journal','private-hash');",
            )
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
        }

        migrate(&MigrationOptions {
            home,
            source: Some(source),
            mode: MigrationMode::Apply,
        })
        .unwrap();

        let connection = Connection::open(destination).unwrap();
        for table in [
            "session",
            "message",
            "part",
            "account",
            "runtime_setting",
            "data_migration",
            "__drizzle_migrations",
        ] {
            connection
                .prepare(&format!("SELECT * FROM {table} LIMIT 0"))
                .unwrap_or_else(|error| panic!("repaired table {table}: {error}"));
        }
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM account", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM migration", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        for journal in ["data_migration", "__drizzle_migrations"] {
            assert_eq!(
                connection
                    .query_row(&format!("SELECT count(*) FROM {journal}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM session", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn event_history_owner_prefix_append_and_fork_are_safe_and_idempotent() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, _) = fixture(&temp);
        let options = MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        };
        migrate(&options).unwrap();
        let destination = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        let private = Connection::open(&destination).unwrap();
        private
            .execute(
                "UPDATE event_sequence SET owner_id='private-owner' WHERE aggregate_id='s1'",
                [],
            )
            .unwrap();
        assert_eq!(migrate(&options).unwrap().changed_rows, 0);
        assert_eq!(
            private
                .query_row(
                    "SELECT owner_id FROM event_sequence WHERE aggregate_id='s1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "private-owner"
        );

        let global = Connection::open(&source).unwrap();
        global
            .execute_batch(
                "INSERT INTO event VALUES('e1-tail','s1',1,'message','global-tail');
                 UPDATE event_sequence SET seq=1 WHERE aggregate_id='s1';",
            )
            .unwrap();
        let appended = migrate(&options).unwrap();
        assert_eq!(appended.changed_rows, 2);
        assert_eq!(
            private
                .query_row(
                    "SELECT seq FROM event_sequence WHERE aggregate_id='s1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(migrate(&options).unwrap().changed_rows, 0);

        private
            .execute(
                "UPDATE event SET data='private-fork' WHERE aggregate_id='s1' AND seq=1",
                [],
            )
            .unwrap();
        let forked = migrate(&options).unwrap();
        assert_eq!(forked.archived_event_forks, 1);
        assert_eq!(forked.private_preserved_conflicts, 0);
        assert_eq!(
            private
                .query_row(
                    "SELECT data FROM event WHERE aggregate_id='s1' AND seq=1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "private-fork"
        );
        let repeated = migrate(&options).unwrap();
        assert_eq!(repeated.changed_rows, 0);
        assert_eq!(repeated.archived_event_forks, 0);
        let archive = Connection::open(
            destination
                .parent()
                .unwrap()
                .join("rtrt-event-forks.sqlite"),
        )
        .unwrap();
        assert_eq!(
            archive
                .query_row("SELECT count(*) FROM event_fork", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            archive
                .query_row("SELECT count(*) FROM event_fork_event", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(
                    destination
                        .parent()
                        .unwrap()
                        .join("rtrt-event-forks.sqlite")
                )
                .unwrap()
                .permissions()
                .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn private_event_history_ahead_wins_unchanged() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, _) = fixture(&temp);
        let options = MigrationOptions {
            home: home.clone(),
            source: Some(source),
            mode: MigrationMode::Apply,
        };
        migrate(&options).unwrap();
        let destination = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        let private = Connection::open(destination).unwrap();
        private
            .execute_batch(
                "INSERT INTO event VALUES('private-tail','s1',1,'message','private');
                 UPDATE event_sequence SET seq=1,owner_id='private-owner' WHERE aggregate_id='s1';",
            )
            .unwrap();
        let report = migrate(&options).unwrap();
        assert_eq!(report.changed_rows, 0);
        assert_eq!(report.archived_event_forks, 0);
        assert_eq!(
            private
                .query_row(
                    "SELECT count(*) FROM event WHERE aggregate_id='s1'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn malformed_event_history_aborts_before_any_destination_mutation() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, _, _) = fixture(&temp);
        Connection::open(&source)
            .unwrap()
            .execute(
                "UPDATE event_sequence SET seq=2 WHERE aggregate_id='s1'",
                [],
            )
            .unwrap();
        let error = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source),
            mode: MigrationMode::Apply,
        })
        .unwrap_err();
        assert!(error.to_string().contains("malformed event history"));
        assert!(!home.join(".rtrt/projects").exists());
    }

    #[test]
    fn malformed_private_history_aborts_before_other_project_mutation() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, second) = fixture(&temp);
        let options = MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        };
        migrate(&options).unwrap();
        let first_db = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        let second_db = destination_db(&home, ProjectIdentity::derive(second).unwrap().slug());
        Connection::open(first_db)
            .unwrap()
            .execute(
                "UPDATE event_sequence SET seq=2 WHERE aggregate_id='s1'",
                [],
            )
            .unwrap();
        Connection::open(source)
            .unwrap()
            .execute("INSERT INTO todo VALUES('later-second','s2','later')", [])
            .unwrap();
        assert!(migrate(&options).is_err());
        assert_eq!(
            Connection::open(second_db)
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM todo WHERE id='later-second'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn retry_after_partial_project_and_new_event_tail_completes_all_projects() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, second) = fixture(&temp);
        let options = MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        };
        migrate(&options).unwrap();
        let first_db = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        let second_db = destination_db(&home, ProjectIdentity::derive(second).unwrap().slug());
        fs::remove_file(&second_db).unwrap();
        Connection::open(&source)
            .unwrap()
            .execute_batch(
                "INSERT INTO event VALUES('later-tail','s1',1,'message','later');
                 UPDATE event_sequence SET seq=1 WHERE aggregate_id='s1';",
            )
            .unwrap();
        let report = migrate(&options).unwrap();
        assert!(report.changed_rows > 0);
        assert_eq!(
            Connection::open(first_db)
                .unwrap()
                .query_row(
                    "SELECT seq FROM event_sequence WHERE aggregate_id='s1'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
        assert_eq!(
            Connection::open(second_db)
                .unwrap()
                .query_row("SELECT count(*) FROM session WHERE id='s2'", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(migrate(&options).unwrap().changed_rows, 0);
    }

    #[test]
    fn linked_worktree_uses_main_identity() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let main = temp.join("main");
        let linked = temp.join("linked");
        fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        fs::create_dir(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        fs::write(
            main.join(".git/worktrees/wt/gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            ProjectIdentity::derive(&main).unwrap().slug(),
            ProjectIdentity::derive(&linked).unwrap().slug()
        );
        let source = temp.join("linked.db");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session(id TEXT PRIMARY KEY, directory TEXT);
                 CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
                 CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, data TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES('main',?1)",
                [&main.to_string_lossy()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES('linked',?1)",
                [&linked.to_string_lossy()],
            )
            .unwrap();
        drop(connection);
        let report = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source),
            mode: MigrationMode::Apply,
        })
        .unwrap();
        assert_eq!(report.projects, 1);
        let destination = destination_db(&home, ProjectIdentity::derive(&main).unwrap().slug());
        assert_eq!(
            Connection::open(destination)
                .unwrap()
                .query_row("SELECT count(*) FROM session", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn wal_safe_read_leaves_source_sidecars_unchanged() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let source = temp.join("wal.db");
        let connection = Connection::open(&source).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        connection.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, directory TEXT); CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, data TEXT); INSERT INTO session VALUES('lost','/deleted');").unwrap();
        let wal = source.with_extension("db-wal");
        let shm = source.with_extension("db-shm");
        let before_db = fs::read(&source).unwrap();
        let before_wal = fs::read(&wal).unwrap();
        let before_shm = fs::metadata(&shm).unwrap();
        migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::DryRun,
        })
        .unwrap();
        assert_eq!(
            before_db,
            fs::read(&source).unwrap(),
            "source database changed"
        );
        assert_eq!(before_wal, fs::read(&wal).unwrap(), "source WAL changed");
        let after_shm = fs::metadata(&shm).unwrap();
        assert_eq!(before_shm.len(), after_shm.len(), "source SHM size changed");
        assert_eq!(
            before_shm.permissions(),
            after_shm.permissions(),
            "source SHM permissions changed"
        );
        assert!(!home.join(".rtrt").exists());

        let applied = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        })
        .unwrap();
        assert_eq!(applied.sessions, 1);
        let unchanged = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(unchanged.up_to_date);
        connection
            .execute("INSERT INTO session VALUES('later','/deleted')", [])
            .unwrap();
        let changed = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(!changed.up_to_date);
        assert_eq!(changed.sessions, 2);
        assert!(!home.join(".rtrt/tmp").exists());
        drop(connection);
    }

    #[test]
    fn launcher_catch_up_skips_busy_migration_lock() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, _, _) = fixture(&temp);
        let root = home.join(".rtrt");
        super::super::ensure_private_directory(&root).unwrap();
        let _lock = acquire_lock(
            &root.join("opencode-session-migration.lock.sqlite"),
            MigrationMode::Apply,
        )
        .unwrap()
        .unwrap();
        let report = migrate(&MigrationOptions {
            home,
            source: Some(source),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(report.skipped_locked);
    }

    fn runtime_fixture(path: &Path, checkout: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE project(id TEXT PRIMARY KEY, worktree TEXT);
                 CREATE TABLE project_directory(project_id TEXT NOT NULL REFERENCES project(id), directory TEXT NOT NULL, PRIMARY KEY(project_id,directory));
                 CREATE TABLE workspace(id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id), directory TEXT);
                 CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT REFERENCES project(id), workspace_id TEXT REFERENCES workspace(id), parent_id TEXT REFERENCES session(id), directory TEXT);
                 CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
                 CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id), session_id TEXT, data BLOB);
                 CREATE TABLE todo(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), content TEXT);
                 CREATE TABLE session_input(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), data TEXT);
                 CREATE TABLE event_sequence(aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
                 CREATE TABLE event(id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL, seq INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL, FOREIGN KEY(aggregate_id) REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE);
                 CREATE UNIQUE INDEX event_aggregate_seq ON event(aggregate_id,seq);
                 CREATE INDEX event_aggregate_type_seq ON event(aggregate_id,type,seq);
                 INSERT INTO project VALUES('stale',NULL);
                 INSERT INTO project VALUES('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',NULL);
                 INSERT INTO project VALUES('global',NULL);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO project_directory VALUES('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1)",
                [checkout.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);
        set_private_file(path).unwrap();
    }

    #[test]
    fn runtime_repair_normalizes_stale_and_runtime_directory_rows_idempotently() {
        FULL_REPAIR_SCANS.with(|scans| scans.set(0));
        let (_temp_guard, temp) = canonical_tempdir();
        let checkout = temp.join("checkout");
        fs::create_dir(&checkout).unwrap();
        let db = temp.join("private.sqlite");
        runtime_fixture(&db, &checkout);
        let connection = Connection::open(&db).unwrap();
        connection
            .execute(
                "INSERT INTO project_directory VALUES('stale',?1)",
                [checkout.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute_batch(
                "INSERT INTO project_directory VALUES('stale','/linked');
                 INSERT INTO workspace VALUES('w','stale','/linked');
                 INSERT INTO session VALUES('ses_root','stale','w',NULL,'/linked');",
            )
            .unwrap();
        drop(connection);

        let RuntimeRepair::Complete(first) =
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap()
        else {
            panic!("caller-supplied runtime authority must be used")
        };
        assert!(first.changed_rows > 0);
        assert!(!first.checkpoint_hit);
        assert_eq!(FULL_REPAIR_SCANS.with(std::cell::Cell::get), 1);
        assert_eq!(first.valid_root_sessions, 1);
        let connection = Connection::open(&db).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT project_id FROM session", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            connection
                .query_row("SELECT directory FROM session", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "/linked"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM project_directory WHERE project_id<>'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        drop(connection);
        let RuntimeRepair::Complete(second) =
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap()
        else {
            panic!("second repair")
        };
        assert_eq!(second.changed_rows, 0);
        assert!(second.checkpoint_hit);
        assert_eq!(FULL_REPAIR_SCANS.with(std::cell::Cell::get), 1);

        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO session VALUES('ses_new','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',NULL,NULL,'/new')",
                [],
            )
            .unwrap();
        let RuntimeRepair::Complete(changed) =
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap()
        else {
            panic!("changed DB must run repair")
        };
        assert!(!changed.checkpoint_hit);
        assert_eq!(changed.valid_root_sessions, 2);
        assert_eq!(FULL_REPAIR_SCANS.with(std::cell::Cell::get), 2);

        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO session VALUES('ses_child_run','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',NULL,NULL,'/child')",
                [],
            )
            .unwrap();
        assert_eq!(
            refresh_runtime_checkpoint(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
            3
        );
        let RuntimeRepair::Complete(refreshed) =
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap()
        else {
            panic!("child-style refresh must fast-path")
        };
        assert!(refreshed.checkpoint_hit);
        assert_eq!(refreshed.valid_root_sessions, 3);
        assert_eq!(FULL_REPAIR_SCANS.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn malformed_and_symlink_runtime_checkpoints_fail_closed() {
        let (_temp_guard, temp) = canonical_tempdir();
        let checkout = temp.join("checkout");
        fs::create_dir(&checkout).unwrap();
        let db = temp.join("private.sqlite");
        runtime_fixture(&db, &checkout);
        let checkpoint = runtime_checkpoint_path(&db).unwrap();
        fs::write(&checkpoint, b"not-json").unwrap();
        set_private_file(&checkpoint).unwrap();
        assert!(
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .is_err()
        );
        fs::remove_file(&checkpoint).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&db, &checkpoint).unwrap();
            assert!(
                repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false,)
                    .is_err()
            );
        }
    }

    #[test]
    fn db_wal_generation_change_invalidates_runtime_checkpoint() {
        let (_temp_guard, temp) = canonical_tempdir();
        let checkout = temp.join("checkout");
        fs::create_dir(&checkout).unwrap();
        let db = temp.join("private.sqlite");
        runtime_fixture(&db, &checkout);
        let authority = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let generation = source_generation(&db).unwrap();
        write_runtime_checkpoint(&db, authority, &generation, 0).unwrap();
        assert_eq!(
            read_runtime_checkpoint(&db, authority, &generation).unwrap(),
            Some(0)
        );
        let wal = db.with_file_name("private.sqlite-wal");
        fs::write(&wal, [0_u8; 32]).unwrap();
        let changed = source_generation(&db).unwrap();
        assert_ne!(changed, generation);
        assert_eq!(
            read_runtime_checkpoint(&db, authority, &changed).unwrap(),
            None
        );
    }

    #[test]
    fn unique_stale_directory_row_does_not_replace_missing_runtime_project_row() {
        let (_temp_guard, temp) = canonical_tempdir();
        let checkout = temp.join("checkout");
        fs::create_dir(&checkout).unwrap();
        let db = temp.join("private.sqlite");
        runtime_fixture(&db, &checkout);
        let connection = Connection::open(&db).unwrap();
        connection
            .execute(
                "DELETE FROM project_directory WHERE project_id='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO project_directory VALUES('stale',?1)",
                [checkout.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM project WHERE id='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES('ses_root','stale',NULL,NULL,?1)",
                [checkout.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);
        let before = fs::read(&db).unwrap();
        assert_eq!(
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap(),
            RuntimeRepair::NeedsProbe
        );
        assert_eq!(before, fs::read(&db).unwrap());
        assert!(
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", true)
                .unwrap_err()
                .to_string()
                .contains("absent after probe")
        );
    }

    #[test]
    fn runtime_global_overrides_unique_stale_directory_attribution() {
        let (_temp_guard, temp) = canonical_tempdir();
        let checkout = temp.join("checkout");
        fs::create_dir(&checkout).unwrap();
        let db = temp.join("private.sqlite");
        runtime_fixture(&db, &checkout);
        let connection = Connection::open(&db).unwrap();
        connection
            .execute("DELETE FROM project_directory", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO project_directory VALUES('stale',?1)",
                [checkout.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES('ses_root','stale',NULL,NULL,?1)",
                [checkout.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);
        let RuntimeRepair::Complete(report) =
            repair_runtime_attribution(&db, "global", true).unwrap()
        else {
            panic!("probe-established global authority")
        };
        assert!(report.changed_rows > 0);
        let connection = Connection::open(&db).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT project_id FROM session", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "global"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM project_directory WHERE project_id='global'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(connection);
        let RuntimeRepair::Complete(checkpointed) =
            repair_runtime_attribution(&db, "global", false).unwrap()
        else {
            panic!("current global checkpoint must avoid a new probe")
        };
        assert!(checkpointed.checkpoint_hit);
        assert_eq!(checkpointed.valid_root_sessions, 1);
    }

    #[test]
    fn malformed_session_graph_is_archived_then_removed() {
        let (_temp_guard, temp) = canonical_tempdir();
        let checkout = temp.join("checkout");
        fs::create_dir(&checkout).unwrap();
        let db = temp.join("private.sqlite");
        runtime_fixture(&db, &checkout);
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "INSERT INTO session VALUES('dummy','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',NULL,NULL,'/private');
                 INSERT INTO session VALUES('ses_child','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',NULL,'dummy','/private');
                 INSERT INTO message VALUES('msg','dummy','secret');
                 INSERT INTO part VALUES('part','msg','dummy',x'01');
                 INSERT INTO todo VALUES('todo','ses_child','secret');
                 INSERT INTO session_input VALUES('input','dummy','secret');
                 INSERT INTO event_sequence VALUES('dummy',0,'owner');
                 INSERT INTO event VALUES('event','dummy',0,'message','secret');",
            )
            .unwrap();
        let RuntimeRepair::Complete(report) =
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap()
        else {
            panic!("runtime evidence exists")
        };
        assert_eq!(report.quarantined_sessions, 2);
        assert_eq!(report.valid_root_sessions, 0);
        let archive = db.parent().unwrap().join(report.archive.unwrap());
        assert_eq!(
            Connection::open(archive)
                .unwrap()
                .query_row("SELECT count(*) FROM session", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        let active = Connection::open(&db).unwrap();
        for table in [
            "session",
            "message",
            "part",
            "todo",
            "session_input",
            "event_sequence",
            "event",
        ] {
            assert_eq!(
                active
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0,
                "{table}"
            );
        }
        drop(active);
        let RuntimeRepair::Complete(second) =
            repair_runtime_attribution(&db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)
                .unwrap()
        else {
            panic!("idempotent repaired database")
        };
        assert_eq!(second.changed_rows, 0);
        assert_eq!(second.quarantined_sessions, 0);
        assert!(second.archive.is_none());
    }

    #[test]
    fn catch_up_current_manifest_does_not_open_destination() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, _) = fixture(&temp);
        migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        })
        .unwrap();
        let destination = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        let locked = Connection::open(destination).unwrap();
        locked.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let report = migrate(&MigrationOptions {
            home,
            source: Some(source),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(report.up_to_date);
        assert_eq!(report.sessions, 0, "fast path skipped source row scan");
    }

    #[test]
    fn catch_up_routes_new_malformed_source_graph_only_to_legacy_archive() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, _) = fixture(&temp);
        migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        })
        .unwrap();
        Connection::open(&source)
            .unwrap()
            .execute_batch(
                "INSERT INTO session VALUES('dummy','p1','w1',NULL,'/private');
                 INSERT INTO message VALUES('dummy-message','dummy','secret');",
            )
            .unwrap();
        let report = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert_eq!(report.archived_sessions, 5);
        let active = destination_db(&home, ProjectIdentity::derive(first).unwrap().slug());
        assert_eq!(
            Connection::open(active)
                .unwrap()
                .query_row("SELECT count(*) FROM session WHERE id='dummy'", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            Connection::open(destination_db(&home, "legacy-global"))
                .unwrap()
                .query_row("SELECT count(*) FROM session WHERE id='dummy'", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn session_directory_overrides_global_project_and_closes_shared_fk_ancestors() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, first, second) = fixture(&temp);
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "INSERT INTO project VALUES('global','/');
                 INSERT INTO project_directory VALUES('global','/');
                 INSERT INTO workspace VALUES('shared-global','global','/');",
            )
            .unwrap();
        for (session, message, part, todo, event, directory) in [
            (
                "ses_global_a",
                "msg-global-a",
                "part-global-a",
                "todo-global-a",
                "event-global-a",
                &first,
            ),
            (
                "ses_global_b",
                "msg-global-b",
                "part-global-b",
                "todo-global-b",
                "event-global-b",
                &second,
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO session VALUES(?1,'global','shared-global',NULL,?2)",
                    (session, directory.to_string_lossy().as_ref()),
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO message VALUES(?1,?2,'opaque')",
                    (message, session),
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO part VALUES(?1,?2,?3,x'01')",
                    (part, message, session),
                )
                .unwrap();
            connection
                .execute("INSERT INTO todo VALUES(?1,?2,'opaque')", (todo, session))
                .unwrap();
            connection
                .execute(
                    "INSERT INTO event_sequence VALUES(?1,0,'global')",
                    [session],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO event VALUES(?1,?2,0,'message','opaque')",
                    (event, session),
                )
                .unwrap();
        }
        drop(connection);
        let before = fs::read(&source).unwrap();

        let generic = destination_db(
            &home,
            ProjectIdentity::derive(Path::new("/")).unwrap().slug(),
        );
        fs::create_dir_all(generic.parent().unwrap()).unwrap();
        Connection::open(&generic)
            .unwrap()
            .execute_batch(
                "CREATE TABLE preserved(value TEXT); INSERT INTO preserved VALUES('wrong-store');",
            )
            .unwrap();
        set_private_file(&generic).unwrap();

        migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        })
        .unwrap();
        assert_eq!(fs::read(source).unwrap(), before);
        for (directory, session) in [(&first, "ses_global_a"), (&second, "ses_global_b")] {
            let db = destination_db(&home, ProjectIdentity::derive(directory).unwrap().slug());
            let private = Connection::open(db).unwrap();
            assert_eq!(
                private
                    .query_row(
                        "SELECT count(*) FROM session WHERE id=?1",
                        [session],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
            for table in ["message", "part", "todo", "event_sequence", "event"] {
                assert!(
                    private
                        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                            .get::<_, i64>(0))
                        .unwrap()
                        > 0,
                    "{table}"
                );
            }
            assert_eq!(
                private
                    .query_row(
                        "SELECT count(*) FROM project WHERE id='global'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                private
                    .query_row(
                        "SELECT count(*) FROM workspace WHERE id='shared-global'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                private
                    .query_row(
                        "SELECT count(*) FROM project_directory WHERE directory='/'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            assert_eq!(
                private
                    .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            Connection::open(generic)
                .unwrap()
                .query_row("SELECT value FROM preserved", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "wrong-store"
        );
        let first_db = destination_db(&home, ProjectIdentity::derive(&first).unwrap().slug());
        Connection::open(&first_db)
            .unwrap()
            .execute(
                "INSERT INTO project VALUES('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',NULL)",
                [],
            )
            .unwrap();
        let RuntimeRepair::Complete(repaired) =
            repair_runtime_attribution(&first_db, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", true)
                .unwrap()
        else {
            panic!("runtime project row exists")
        };
        assert_eq!(repaired.valid_root_sessions, 1);
        assert!(repaired.quarantined_sessions > 0);
    }

    #[test]
    fn routing_policy_domain_invalidates_old_manifest_once() {
        let (_temp_guard, temp) = canonical_tempdir();
        let home = temp.join("home");
        fs::create_dir(&home).unwrap();
        let (source, _, _) = fixture(&temp);
        migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::Apply,
        })
        .unwrap();
        let manifest = home.join(".rtrt/opencode-session-migration.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value["source_generation"] = serde_json::Value::String("v1-old-routing-policy".to_string());
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        set_private_file(&manifest).unwrap();
        let upgraded = migrate(&MigrationOptions {
            home: home.clone(),
            source: Some(source.clone()),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(!upgraded.up_to_date);
        let fast = migrate(&MigrationOptions {
            home,
            source: Some(source),
            mode: MigrationMode::CatchUp,
        })
        .unwrap();
        assert!(fast.up_to_date);
        assert_eq!(fast.sessions, 0);
    }
}
