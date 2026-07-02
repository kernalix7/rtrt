//! `rtrt doctor` — one-shot health check across the CLI's installation
//! surface: PATH binaries, Claude Code integration (hooks / MCP /
//! statusline — shared with `rtrt project status`/`health` via
//! [`crate::setup`]), the memory store, detected agents/providers, the
//! dashboard service, and the provider-usage ledger.
//!
//! Every row is a real, local probe — nothing here is fabricated. Only the
//! memory-store check is `critical`; every other row is informational, so a
//! fresh machine with nothing configured yet prints WARN rows, not FAIL.

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use crate::setup::{self, CheckState};

const DASHBOARD_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// (binary name, safe to spawn `<binary> --version`). `rtrt` and `rtrt-mcp`
/// are clap-based and exit immediately on `--version`. `rtrt-dashboard` has
/// no CLI arg parsing at all — it unconditionally binds the axum server, so
/// spawning it to "probe a version" would start a real, possibly
/// port-colliding, HTTP server instead. Its row reports presence on PATH
/// only; `dashboard_row` below checks whether it's actually reachable.
const BINARIES: &[(&str, bool)] = &[
    ("rtrt", true),
    ("rtrt-mcp", true),
    ("rtrt-dashboard", false),
];
/// Mirrors `rtrt_providers::usage_ledger`'s private `LEDGER_FILE_NAME` /
/// `RTRT_PROVIDER_USAGE_PATH` convention — that crate doesn't export the
/// path, so it's resolved the same way here.
const LEDGER_FILE_NAME: &str = "provider-usage.tsv";
const DOCTOR_STATE_WIDTH: usize = 5;
const DOCTOR_GROUP_WIDTH: usize = 10;
const DOCTOR_CHECK_WIDTH: usize = 18;

struct DoctorRow {
    group: &'static str,
    check: &'static str,
    state: CheckState,
    detail: String,
    /// Only a critical FAIL row flips the process exit code.
    critical: bool,
}

pub fn run(json: bool) -> Result<()> {
    let rows = collect();
    if json {
        print_json(&rows)?;
    } else {
        print_table(&rows);
    }
    if rows
        .iter()
        .any(|row| row.critical && row.state == CheckState::Fail)
    {
        std::process::exit(1);
    }
    Ok(())
}

fn collect() -> Vec<DoctorRow> {
    let mut rows: Vec<DoctorRow> = BINARIES
        .iter()
        .map(|&(name, probe)| binary_row(name, probe))
        .collect();
    rows.extend(claude_rows());
    rows.push(memory_row());
    rows.push(detect_row());
    rows.push(dashboard_row());
    rows.push(usage_ledger_row());
    rows
}

fn binary_row(name: &'static str, probe: bool) -> DoctorRow {
    match find_on_path(name) {
        Some(path) => {
            let detail = if probe {
                probe_version(&path)
                    .map(|version| format!("{version} ({})", path.display()))
                    .unwrap_or_else(|| path.display().to_string())
            } else {
                format!(
                    "{} (no --version probe; see `dashboard service` row)",
                    path.display()
                )
            };
            DoctorRow {
                group: "binaries",
                check: name,
                state: CheckState::Pass,
                detail,
                critical: false,
            }
        }
        None if name == "rtrt" => DoctorRow {
            group: "binaries",
            check: name,
            state: CheckState::Warn,
            detail: format!(
                "not found on PATH; running instance reports v{}",
                env!("CARGO_PKG_VERSION")
            ),
            critical: false,
        },
        None => DoctorRow {
            group: "binaries",
            check: name,
            state: CheckState::Warn,
            detail: "not found on PATH".into(),
            critical: false,
        },
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        #[cfg(windows)]
        let candidate = candidate.with_extension("exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn probe_version(path: &PathBuf) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let text = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

fn claude_rows() -> Vec<DoctorRow> {
    let settings = setup::claude_settings_status(false);
    let (mcp_state, mcp_detail) = setup::claude_mcp_registered_status();
    vec![
        DoctorRow {
            group: "claude",
            check: "hooks",
            state: settings.hooks_state,
            detail: settings.hooks_detail,
            critical: false,
        },
        DoctorRow {
            group: "claude",
            check: "mcp",
            state: mcp_state,
            detail: mcp_detail,
            critical: false,
        },
        DoctorRow {
            group: "claude",
            check: "statusline",
            state: settings.statusline_state,
            detail: settings.statusline_detail,
            critical: false,
        },
    ]
}

fn memory_row() -> DoctorRow {
    // `health = true`: an existing-but-corrupt store is a real breakage
    // worth blocking on; a store that hasn't been created yet stays WARN.
    let (state, detail) = setup::memory_reachable_status(true);
    DoctorRow {
        group: "memory",
        check: "memory store",
        state,
        detail,
        critical: true,
    }
}

fn detect_row() -> DoctorRow {
    let tools = rtrt_core::detect_tools_with_config(crate::effective_config_for_cwd());
    let installed = tools.iter().filter(|tool| tool.installed).count();
    DoctorRow {
        group: "detect",
        check: "agents/providers",
        state: if installed > 0 {
            CheckState::Pass
        } else {
            CheckState::Warn
        },
        detail: format!(
            "{installed}/{} tools detected (see `rtrt detect`)",
            tools.len()
        ),
        critical: false,
    }
}

fn dashboard_row() -> DoctorRow {
    let bind = crate::effective_config_for_cwd().dashboard.bind;
    let (state, detail) = match bind.parse::<SocketAddr>() {
        Ok(addr) => {
            if TcpStream::connect_timeout(&addr, DASHBOARD_PROBE_TIMEOUT).is_ok() {
                (CheckState::Pass, format!("{bind}: listening"))
            } else {
                (CheckState::Warn, format!("{bind}: not listening"))
            }
        }
        Err(err) => (
            CheckState::Warn,
            format!("invalid dashboard bind `{bind}`: {err}"),
        ),
    };
    DoctorRow {
        group: "dashboard",
        check: "dashboard service",
        state,
        detail,
        critical: false,
    }
}

fn usage_ledger_row() -> DoctorRow {
    let path = usage_ledger_path();
    if !path.exists() {
        return DoctorRow {
            group: "usage",
            check: "usage ledger",
            state: CheckState::Warn,
            detail: format!("missing {}", path.display()),
            critical: false,
        };
    }
    let rows = std::fs::read_to_string(&path)
        .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    DoctorRow {
        group: "usage",
        check: "usage ledger",
        state: CheckState::Pass,
        detail: format!("{} ({rows} rows)", path.display()),
        critical: false,
    }
}

fn usage_ledger_path() -> PathBuf {
    if let Some(custom) = std::env::var_os("RTRT_PROVIDER_USAGE_PATH") {
        return PathBuf::from(custom);
    }
    setup::dirs_home()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".rtrt")
        .join(LEDGER_FILE_NAME)
}

fn print_table(rows: &[DoctorRow]) {
    println!(
        "{:<state_w$}  {:<group_w$}  {:<check_w$}  detail",
        "state",
        "group",
        "check",
        state_w = DOCTOR_STATE_WIDTH,
        group_w = DOCTOR_GROUP_WIDTH,
        check_w = DOCTOR_CHECK_WIDTH
    );
    for row in rows {
        println!(
            "{:<state_w$}  {:<group_w$}  {:<check_w$}  {}",
            row.state.as_str(),
            row.group,
            row.check,
            row.detail,
            state_w = DOCTOR_STATE_WIDTH,
            group_w = DOCTOR_GROUP_WIDTH,
            check_w = DOCTOR_CHECK_WIDTH
        );
    }
    let pass = rows.iter().filter(|r| r.state == CheckState::Pass).count();
    let warn = rows.iter().filter(|r| r.state == CheckState::Warn).count();
    let fail = rows.iter().filter(|r| r.state == CheckState::Fail).count();
    println!("summary: PASS={pass} WARN={warn} FAIL={fail}");
}

fn print_json(rows: &[DoctorRow]) -> Result<()> {
    let value: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "group": row.group,
                "check": row.check,
                "state": row.state.as_str(),
                "detail": row.detail,
                "critical": row.critical,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
