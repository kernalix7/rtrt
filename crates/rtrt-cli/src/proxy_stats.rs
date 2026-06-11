use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

pub const DB_FILE_NAME: &str = "proxy-stats.sqlite";
pub const TABLE_SCHEMA: &str = "proxy_runs(id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, project TEXT NOT NULL, original_cmd TEXT NOT NULL, mode TEXT NOT NULL, input_chars INTEGER NOT NULL, output_chars INTEGER NOT NULL, saved_chars INTEGER NOT NULL, saved_pct REAL NOT NULL, exec_ms INTEGER NOT NULL)";

#[derive(Debug, Clone, Copy)]
pub enum Bucket {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone)]
pub struct ProxyRunRecord {
    pub project: String,
    pub original_cmd: String,
    pub mode: String,
    pub input_chars: u64,
    pub output_chars: u64,
    pub saved_chars: u64,
    pub saved_pct: f64,
    pub exec_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct GainSummary {
    pub path: PathBuf,
    pub unavailable: Option<String>,
    pub total_runs: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub saved_chars: u64,
    pub exec_ms: u64,
    pub top_commands: Vec<CommandSavings>,
    pub projects: Vec<ProjectSavings>,
    pub recent: Vec<RunRow>,
    pub buckets: Vec<BucketSavings>,
}

#[derive(Debug, Clone)]
pub struct CommandSavings {
    pub command: String,
    pub runs: u64,
    pub saved_chars: u64,
}

#[derive(Debug, Clone)]
pub struct ProjectSavings {
    pub project: String,
    pub runs: u64,
    pub saved_chars: u64,
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub ts: String,
    pub project: String,
    pub original_cmd: String,
    pub mode: String,
    pub input_chars: u64,
    pub output_chars: u64,
    pub saved_chars: u64,
    pub saved_pct: f64,
    pub exec_ms: u64,
}

#[derive(Debug, Clone)]
pub struct BucketSavings {
    pub bucket: String,
    pub runs: u64,
    pub saved_chars: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SavingsAverages {
    exact: BTreeMap<String, u64>,
    filter: BTreeMap<String, u64>,
    token: BTreeMap<String, u64>,
}

impl SavingsAverages {
    pub fn estimate_for(&self, command: &str, filter: Option<&str>, token: Option<&str>) -> u64 {
        self.exact
            .get(command)
            .copied()
            .or_else(|| filter.and_then(|key| self.filter.get(key).copied()))
            .or_else(|| token.and_then(|key| self.token.get(key).copied()))
            .unwrap_or(0)
    }
}

pub fn default_path() -> PathBuf {
    std::env::var_os("RTRT_PROXY_STATS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".rtrt")
                .join(DB_FILE_NAME)
        })
}

pub fn record_best_effort(record: ProxyRunRecord) {
    let _ = insert_run(&default_path(), &record);
}

pub fn insert_run(path: &Path, record: &ProxyRunRecord) -> Result<()> {
    let conn = open_writable(path)?;
    conn.execute(
        "INSERT INTO proxy_runs (ts, project, original_cmd, mode, input_chars, output_chars, saved_chars, saved_pct, exec_ms)
         VALUES (datetime('now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            record.project,
            record.original_cmd,
            record.mode,
            to_i64(record.input_chars),
            to_i64(record.output_chars),
            to_i64(record.saved_chars),
            record.saved_pct,
            to_i64(record.exec_ms),
        ],
    )?;
    Ok(())
}

pub fn reset(path: &Path) -> Result<()> {
    let conn = open_writable(path)?;
    conn.execute("DELETE FROM proxy_runs", [])?;
    Ok(())
}

pub fn load_summary(
    project: Option<&str>,
    bucket: Option<Bucket>,
    history: bool,
) -> Result<GainSummary> {
    let path = default_path();
    let mut summary = GainSummary {
        path: path.clone(),
        ..GainSummary::default()
    };
    let conn = match open_writable(&path) {
        Ok(conn) => conn,
        Err(err) => {
            summary.unavailable = Some(err.to_string());
            return Ok(summary);
        }
    };
    let (runs, input, output, saved, exec): (i64, i64, i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(input_chars), 0), COALESCE(SUM(output_chars), 0), COALESCE(SUM(saved_chars), 0), COALESCE(SUM(exec_ms), 0)
         FROM proxy_runs WHERE (?1 IS NULL OR project = ?1)",
        params![project],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    summary.total_runs = nonnegative_u64(runs);
    summary.input_chars = nonnegative_u64(input);
    summary.output_chars = nonnegative_u64(output);
    summary.saved_chars = nonnegative_u64(saved);
    summary.exec_ms = nonnegative_u64(exec);

    let display_count = derived_count(summary.total_runs);
    summary.top_commands = load_top_commands(&conn, project, display_count)?;
    summary.projects = load_projects(&conn, project)?;
    if history {
        summary.recent = load_recent(&conn, project, display_count)?;
    }
    if let Some(bucket) = bucket {
        summary.buckets = load_buckets(&conn, project, bucket)?;
    }
    Ok(summary)
}

pub fn load_savings_averages() -> SavingsAverages {
    let path = default_path();
    if !path.exists() {
        return SavingsAverages::default();
    }
    let Ok(conn) = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return SavingsAverages::default();
    };
    let mut rows = match conn.prepare("SELECT original_cmd, saved_chars FROM proxy_runs") {
        Ok(stmt) => stmt,
        Err(_) => return SavingsAverages::default(),
    };
    let iter = match rows.query_map([], |row| {
        let command: String = row.get(0)?;
        let saved: i64 = row.get(1)?;
        Ok((command, nonnegative_u64(saved)))
    }) {
        Ok(iter) => iter,
        Err(_) => return SavingsAverages::default(),
    };
    let mut exact: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut filter: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut token: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for row in iter.flatten() {
        let (command, saved) = row;
        accumulate(&mut exact, command.clone(), saved);
        if let Some(f) = rtrt_proxy::filter_for(&command) {
            accumulate(&mut filter, f.command.to_string(), saved);
        }
        if let Some(t) = command.split_whitespace().next() {
            accumulate(&mut token, t.to_string(), saved);
        }
    }
    SavingsAverages {
        exact: finish_average(exact),
        filter: finish_average(filter),
        token: finish_average(token),
    }
}

fn open_writable(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute(&format!("CREATE TABLE IF NOT EXISTS {TABLE_SCHEMA}"), [])?;
    Ok(conn)
}

fn load_top_commands(
    conn: &Connection,
    project: Option<&str>,
    count: usize,
) -> Result<Vec<CommandSavings>> {
    let mut stmt = conn.prepare(
        "SELECT original_cmd, COUNT(*), COALESCE(SUM(saved_chars), 0)
         FROM proxy_runs WHERE (?1 IS NULL OR project = ?1)
         GROUP BY original_cmd
         ORDER BY COALESCE(SUM(saved_chars), 0) DESC, COUNT(*) DESC, original_cmd ASC",
    )?;
    let rows = stmt.query_map(params![project], |row| {
        Ok(CommandSavings {
            command: row.get(0)?,
            runs: nonnegative_u64(row.get::<_, i64>(1)?),
            saved_chars: nonnegative_u64(row.get::<_, i64>(2)?),
        })
    })?;
    Ok(rows
        .filter_map(std::result::Result::ok)
        .take(count)
        .collect())
}

fn load_projects(conn: &Connection, project: Option<&str>) -> Result<Vec<ProjectSavings>> {
    let mut stmt = conn.prepare(
        "SELECT project, COUNT(*), COALESCE(SUM(saved_chars), 0)
         FROM proxy_runs WHERE (?1 IS NULL OR project = ?1)
         GROUP BY project
         ORDER BY COALESCE(SUM(saved_chars), 0) DESC, COUNT(*) DESC, project ASC",
    )?;
    let rows = stmt.query_map(params![project], |row| {
        Ok(ProjectSavings {
            project: row.get(0)?,
            runs: nonnegative_u64(row.get::<_, i64>(1)?),
            saved_chars: nonnegative_u64(row.get::<_, i64>(2)?),
        })
    })?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

fn load_recent(conn: &Connection, project: Option<&str>, count: usize) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "SELECT ts, project, original_cmd, mode, input_chars, output_chars, saved_chars, saved_pct, exec_ms
         FROM proxy_runs WHERE (?1 IS NULL OR project = ?1)
         ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project, to_i64(count as u64)], |row| {
        Ok(RunRow {
            ts: row.get(0)?,
            project: row.get(1)?,
            original_cmd: row.get(2)?,
            mode: row.get(3)?,
            input_chars: nonnegative_u64(row.get::<_, i64>(4)?),
            output_chars: nonnegative_u64(row.get::<_, i64>(5)?),
            saved_chars: nonnegative_u64(row.get::<_, i64>(6)?),
            saved_pct: row.get(7)?,
            exec_ms: nonnegative_u64(row.get::<_, i64>(8)?),
        })
    })?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

fn load_buckets(
    conn: &Connection,
    project: Option<&str>,
    bucket: Bucket,
) -> Result<Vec<BucketSavings>> {
    let bucket_sql = match bucket {
        Bucket::Daily => "date(ts)",
        Bucket::Weekly => "strftime('%Y-W%W', ts)",
        Bucket::Monthly => "strftime('%Y-%m', ts)",
    };
    let sql = format!(
        "SELECT {bucket_sql}, COUNT(*), COALESCE(SUM(saved_chars), 0)
         FROM proxy_runs WHERE (?1 IS NULL OR project = ?1)
         GROUP BY {bucket_sql}
         ORDER BY {bucket_sql} ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![project], |row| {
        Ok(BucketSavings {
            bucket: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            runs: nonnegative_u64(row.get::<_, i64>(1)?),
            saved_chars: nonnegative_u64(row.get::<_, i64>(2)?),
        })
    })?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

pub fn derived_count(total_rows: u64) -> usize {
    if total_rows == 0 {
        0
    } else {
        integer_sqrt_ceil(total_rows).max(1) as usize
    }
}

fn integer_sqrt_ceil(n: u64) -> u64 {
    let mut root = 0u64;
    while root.saturating_mul(root) < n {
        root = root.saturating_add(1);
    }
    root
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn nonnegative_u64(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}

fn accumulate(map: &mut BTreeMap<String, (u64, u64)>, key: String, saved: u64) {
    let entry = map.entry(key).or_default();
    entry.0 = entry.0.saturating_add(saved);
    entry.1 = entry.1.saturating_add(1);
}

fn finish_average(raw: BTreeMap<String, (u64, u64)>) -> BTreeMap<String, u64> {
    raw.into_iter()
        .filter_map(|(key, (sum, count))| (count > 0).then_some((key, sum / count)))
        .collect()
}
