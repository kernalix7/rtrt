//! `rtrt service open|install|uninstall|status` — open or manage `rtrt-dashboard`.
//! background OS service so it starts on login and restarts on crash without
//! the user launching it by hand.
//!
//! - Linux  → systemd **user** unit at `~/.config/systemd/user/rtrt-dashboard.service`
//! - macOS  → launchd LaunchAgent at `~/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist`
//! - other  → not supported here (Windows uses the `install.ps1` scheduled task).
//!
//! Default behaviour is **dry-run**: print the unit + the commands that would
//! run. Pass `--apply` to write the file and enable the service.
//!
//! Because every manager this module drives is Unix-only, its helpers lose
//! their callers on Windows and go unreferenced there. That is the intended
//! shape, so the module states it once rather than per item.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::io::Read;

use anyhow::{Context, Result, bail};

const SYSTEMD_UNIT: &str = "rtrt-dashboard.service";
const LAUNCHD_LABEL: &str = "io.kodenet.rtrt-dashboard";
const OWNED_MARKER: &str = "rtrt-managed-dashboard-service";
const TOKEN_FILE: &str = "dashboard.env";
const MACHINE_MODE_ARG: &str = "--machine";
const STATE_DIR_ARG: &str = "--state-dir";
const MIGRATION_LOCK: &str = ".migration.lock";

#[derive(Clone, Copy)]
pub enum ServiceAction {
    Open { print_bootstrap: bool },
    Install,
    Uninstall,
    Status,
}

pub struct ServicePlan {
    pub action: ServiceAction,
    pub apply: bool,
    /// Resolved `rtrt-dashboard` binary path.
    pub binary: PathBuf,
}

pub fn run(plan: ServicePlan) -> Result<()> {
    if let ServiceAction::Open { print_bootstrap } = plan.action {
        return service_open(print_bootstrap);
    }
    match std::env::consts::OS {
        "linux" => systemd(&plan),
        "macos" => launchd(&plan),
        other => bail!(
            "rtrt service: unsupported OS `{other}`. On Windows the installer wires a \
             logon scheduled task; otherwise run `rtrt-dashboard` manually."
        ),
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot resolve home directory ($HOME unset)")
}

// ---------------------------------------------------------------------------
// Linux — systemd user unit
// ---------------------------------------------------------------------------

struct DashboardState {
    root: PathBuf,
    token_file: PathBuf,
}

fn dashboard_state(home: &Path) -> DashboardState {
    let root = home.join(".rtrt").join("dashboard");
    let token_file = root.join(TOKEN_FILE);
    DashboardState { root, token_file }
}

fn service_definition_path(home: &Path, os: &str) -> Option<PathBuf> {
    match os {
        "linux" => Some(
            home.join(".config")
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT),
        ),
        "macos" => Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        ),
        _ => None,
    }
}

const DASHBOARD_AUTHORITY: &str = "127.0.0.1:7311";
const DASHBOARD_URL: &str = "http://127.0.0.1:7311/";

fn service_open(print_bootstrap: bool) -> Result<()> {
    let home = home()?;
    let state = dashboard_state(&home);
    let service = service_definition_path(&home, std::env::consts::OS);
    let token = ensure_machine_dashboard_token(&state, &home, service.as_deref())?;
    check_dashboard_health()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    let credential = rtrt_core::dashboard_bootstrap::issue(&token, now)?;
    let navigation_id = format!("{now:x}-{:x}", std::process::id());
    let url = bootstrap_url(&credential, &navigation_id)?;
    if print_bootstrap {
        eprintln!(
            "Warning: this URL grants dashboard access for 60 seconds; share it with nobody."
        );
        println!("{url}");
        return Ok(());
    }
    let opener = trusted_opener()?;
    launch_opener(&opener, &url).with_context(|| {
        "browser opener failed; retry `rtrt service open` or explicitly run `rtrt service open --print-bootstrap`"
    })?;
    Ok(())
}

fn bootstrap_url(credential: &str, navigation_id: &str) -> Result<String> {
    anyhow::ensure!(
        credential.len() == rtrt_core::dashboard_bootstrap::ENCODED_LEN
            && credential
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid bootstrap credential format"
    );
    anyhow::ensure!(
        !navigation_id.is_empty()
            && navigation_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "invalid dashboard navigation id"
    );
    Ok(format!(
        "{DASHBOARD_URL}?open={navigation_id}#bootstrap={credential}"
    ))
}

fn check_dashboard_health() -> Result<()> {
    wait_for_dashboard_health(
        probe_dashboard_health,
        7,
        std::time::Duration::from_millis(100),
    )
}

fn wait_for_dashboard_health(
    mut probe: impl FnMut() -> Result<()>,
    attempts: usize,
    delay: std::time::Duration,
) -> Result<()> {
    anyhow::ensure!(attempts > 0, "dashboard health attempts must be positive");
    let mut last_error = None;
    for attempt in 0..attempts {
        match probe() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < attempts && !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("dashboard health check failed")))
}

fn probe_dashboard_health() -> Result<()> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let address: SocketAddr = DASHBOARD_AUTHORITY.parse().expect("fixed socket address");
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200))
        .with_context(
            || "dashboard is not reachable at http://127.0.0.1:7311; check `rtrt service status`",
        )?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:7311\r\nConnection: close\r\n\r\n")?;
    let mut response = [0_u8; 128];
    let read = stream.read(&mut response)?;
    anyhow::ensure!(
        response[..read].starts_with(b"HTTP/1.1 200")
            || response[..read].starts_with(b"HTTP/1.0 200"),
        "dashboard health check failed; check `rtrt service status`"
    );
    Ok(())
}

fn trusted_opener() -> Result<PathBuf> {
    let path = opener_path_for_os(std::env::consts::OS)?;
    validate_opener(&path)?;
    Ok(path)
}

fn opener_path_for_os(os: &str) -> Result<PathBuf> {
    Ok(match os {
        "linux" => PathBuf::from("/usr/bin/xdg-open"),
        "macos" => PathBuf::from("/usr/bin/open"),
        other => bail!("browser opening is unsupported on `{other}`"),
    })
}

#[cfg(unix)]
fn validate_opener(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("trusted browser opener unavailable at {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "refusing unsafe browser opener {}",
        path.display()
    );
    anyhow::ensure!(metadata.uid() == 0, "browser opener must be owned by root");
    anyhow::ensure!(
        metadata.permissions().mode() & 0o022 == 0 && metadata.permissions().mode() & 0o111 != 0,
        "refusing unsafe browser opener permissions"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_opener(_path: &Path) -> Result<()> {
    bail!("trusted browser opener is unavailable")
}

fn launch_opener(opener: &Path, url: &str) -> Result<()> {
    let status = opener_command(opener, url)
        .status()
        .with_context(|| format!("execute trusted browser opener {}", opener.display()))?;
    anyhow::ensure!(status.success(), "browser opener exited unsuccessfully");
    Ok(())
}

fn opener_command(opener: &Path, url: &str) -> Command {
    let mut command = Command::new(opener);
    command.arg(url);
    command
}

fn systemd_path(value: &str, directive: &str) -> Result<String> {
    anyhow::ensure!(
        Path::new(value).is_absolute(),
        "{directive} path is not absolute: {value:?}"
    );
    anyhow::ensure!(
        !value.contains(['\n', '\0']),
        "{directive} path contains a newline or NUL"
    );

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character == '%' {
            escaped.push_str("%%");
        } else if character == '\\'
            || character == '"'
            || character.is_whitespace()
            || character.is_control()
        {
            let mut bytes = [0; 4];
            for byte in character.encode_utf8(&mut bytes).bytes() {
                escaped.push_str(&format!("\\x{byte:02x}"));
            }
        } else {
            escaped.push(character);
        }
    }
    Ok(escaped)
}

fn systemd_exec_arg(value: &str) -> Result<String> {
    anyhow::ensure!(
        !value.contains(['\n', '\0']),
        "ExecStart argument contains a newline or NUL"
    );

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            character if character.is_control() => {
                let mut bytes = [0; 4];
                for byte in character.encode_utf8(&mut bytes).bytes() {
                    escaped.push_str(&format!("\\x{byte:02x}"));
                }
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

fn systemd_unit_body(binary: &str, state_root: &str) -> Result<String> {
    anyhow::ensure!(
        Path::new(binary).is_absolute(),
        "ExecStart executable is not absolute: {binary:?}"
    );
    anyhow::ensure!(
        Path::new(state_root).is_absolute(),
        "dashboard state directory is not absolute: {state_root:?}"
    );
    Ok(format!(
        "# {OWNED_MARKER}\n\
         [Unit]\n\
         Description=Retort (rtrt) dashboard — agent-context distillery web UI\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} {} {} {}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_exec_arg(binary)?,
        systemd_exec_arg(MACHINE_MODE_ARG)?,
        systemd_exec_arg(STATE_DIR_ARG)?,
        systemd_exec_arg(state_root)?,
    ))
}

fn systemd_install_commands() -> [&'static [&'static str]; 3] {
    [
        &["daemon-reload"],
        &["enable", SYSTEMD_UNIT],
        &["restart", SYSTEMD_UNIT],
    ]
}

fn run_systemd_install_commands(mut execute: impl FnMut(&[&str]) -> Result<()>) -> Result<()> {
    for args in systemd_install_commands() {
        execute(args)?;
    }
    Ok(())
}

fn systemd(plan: &ServicePlan) -> Result<()> {
    let unit_path = home()?
        .join(".config")
        .join("systemd")
        .join("user")
        .join(SYSTEMD_UNIT);

    match plan.action {
        ServiceAction::Open { .. } => unreachable!("open handled before OS service dispatch"),
        ServiceAction::Install => {
            let home = home()?;
            let state = dashboard_state(&home);
            let body = systemd_unit_body(
                &plan.binary.to_string_lossy(),
                &state.root.to_string_lossy(),
            )?;
            if !plan.apply {
                println!("[dry-run] would write {}", unit_path.display());
                println!("[dry-run] unit:\n{body}");
                for args in systemd_install_commands() {
                    println!("[dry-run] then: systemctl --user {}", args.join(" "));
                }
                println!("[dry-run] token file: {}", state.token_file.display());
                println!("\nRe-run with --apply to install and start the service.");
                return Ok(());
            }
            if let Some(parent) = unit_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            let _token = ensure_machine_dashboard_token(&state, &home, Some(&unit_path))?;
            write_private_owned_file(&unit_path, body.as_bytes())?;
            // `enable --now` does not restart an already-running unit. Always issue an
            // explicit restart so reinstalling picks up the rewritten unit and binary.
            run_systemd_install_commands(systemctl)?;
            check_dashboard_health().context("dashboard service did not become ready")?;
            println!("installed + started {SYSTEMD_UNIT} (systemctl --user)");
            println!("  dashboard: http://127.0.0.1:7311");
            println!("  logs:  journalctl --user -u {SYSTEMD_UNIT} -f");
            println!("  stop:  systemctl --user disable --now {SYSTEMD_UNIT}");
            println!("  token file: {}", state.token_file.display());
            Ok(())
        }
        ServiceAction::Uninstall => {
            if !plan.apply {
                println!("[dry-run] would: systemctl --user disable --now {SYSTEMD_UNIT}");
                println!("[dry-run] would remove {}", unit_path.display());
                println!("\nRe-run with --apply to stop and remove the service.");
                return Ok(());
            }
            // Best-effort: the service may already be gone.
            let _ = systemctl(&["disable", "--now", SYSTEMD_UNIT]);
            if owned_service_file(&unit_path)? {
                std::fs::remove_file(&unit_path)
                    .with_context(|| format!("remove {}", unit_path.display()))?;
            }
            let _ = systemctl(&["daemon-reload"]);
            println!("removed {SYSTEMD_UNIT}");
            Ok(())
        }
        ServiceAction::Status => {
            // `status` returns non-zero when inactive; surface output either way.
            let _ = Command::new("systemctl")
                .args(["--user", "status", SYSTEMD_UNIT, "--no-pager"])
                .status();
            Ok(())
        }
    }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl --user (is systemd available?)")?;
    if !status.success() {
        bail!("systemctl --user {} exited with {status}", args.join(" "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// macOS — launchd LaunchAgent
// ---------------------------------------------------------------------------

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launchd_plist_body(binary: &str, state_root: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<!-- {OWNED_MARKER} -->\n\
         \t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{}</string>\n\
         \t\t<string>{MACHINE_MODE_ARG}</string>\n\
         \t\t<string>{STATE_DIR_ARG}</string>\n\
         \t\t<string>{}</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(binary),
        xml_escape(state_root),
    )
}

fn launchd(plan: &ServicePlan) -> Result<()> {
    let home = home()?;
    let plist_path = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));

    match plan.action {
        ServiceAction::Open { .. } => unreachable!("open handled before OS service dispatch"),
        ServiceAction::Install => {
            let state = dashboard_state(&home);
            if !plan.apply {
                println!("[dry-run] would write {}", plist_path.display());
                println!(
                    "[dry-run] plist:\n{}",
                    launchd_plist_body(
                        &plan.binary.to_string_lossy(),
                        &state.root.to_string_lossy()
                    )
                );
                println!("[dry-run] then: launchctl unload (if present) + load -w {LAUNCHD_LABEL}");
                println!("[dry-run] token file: {}", state.token_file.display());
                println!("\nRe-run with --apply to install and start the service.");
                return Ok(());
            }
            if let Some(parent) = plist_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            let _token = ensure_machine_dashboard_token(&state, &home, Some(&plist_path))?;
            let body = launchd_plist_body(
                &plan.binary.to_string_lossy(),
                &state.root.to_string_lossy(),
            );
            write_private_owned_file(&plist_path, body.as_bytes())?;
            // Reload to pick up changes; ignore the unload error on first install.
            let _ = launchctl(&["unload", &plist_path.to_string_lossy()]);
            launchctl(&["load", "-w", &plist_path.to_string_lossy()])?;
            check_dashboard_health().context("dashboard service did not become ready")?;
            println!("installed + started {LAUNCHD_LABEL} (launchctl)");
            println!("  dashboard: http://127.0.0.1:7311");
            println!("  stop:  launchctl unload -w {}", plist_path.display());
            println!("  token file: {}", state.token_file.display());
            Ok(())
        }
        ServiceAction::Uninstall => {
            if !plan.apply {
                println!(
                    "[dry-run] would: launchctl unload -w {}",
                    plist_path.display()
                );
                println!("[dry-run] would remove {}", plist_path.display());
                println!("\nRe-run with --apply to stop and remove the service.");
                return Ok(());
            }
            let _ = launchctl(&["unload", "-w", &plist_path.to_string_lossy()]);
            if owned_service_file(&plist_path)? {
                std::fs::remove_file(&plist_path)
                    .with_context(|| format!("remove {}", plist_path.display()))?;
            }
            println!("removed {LAUNCHD_LABEL}");
            Ok(())
        }
        ServiceAction::Status => {
            let _ = Command::new("launchctl")
                .args(["list", LAUNCHD_LABEL])
                .status();
            Ok(())
        }
    }
}

fn launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .context("failed to run launchctl")?;
    if !status.success() {
        bail!("launchctl {} exited with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(unix)]
struct MigrationLock(std::fs::File);

#[cfg(unix)]
impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path, home: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let home_metadata =
        std::fs::symlink_metadata(home).with_context(|| format!("inspect {}", home.display()))?;
    anyhow::ensure!(
        home_metadata.is_dir() && !home_metadata.file_type().is_symlink(),
        "refusing unsafe home directory"
    );
    let owner = home_metadata.uid();
    let relative = path
        .strip_prefix(home)
        .context("dashboard state path escaped home")?;
    let mut current = home.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == owner,
                "refusing unsafe dashboard directory {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .with_context(|| format!("mkdir {}", current.display()))?;
            }
            Err(error) => return Err(error.into()),
        }
        std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_migration_lock(root: &Path) -> Result<MigrationLock> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let path = root.join(MIGRATION_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("lock dashboard token migration at {}", path.display()))?;
    let linked = std::fs::symlink_metadata(&path)?;
    let opened = file.metadata()?;
    anyhow::ensure!(
        linked.is_file()
            && !linked.file_type().is_symlink()
            && linked.dev() == opened.dev()
            && linked.ino() == opened.ino(),
        "refusing unsafe dashboard migration lock {}",
        path.display()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    fs2::FileExt::lock_exclusive(&file)
        .with_context(|| format!("lock dashboard token migration at {}", path.display()))?;
    Ok(MigrationLock(file))
}

#[cfg(unix)]
fn random_dashboard_token() -> Result<String> {
    let mut random = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open OS CSPRNG")?
        .read_exact(&mut random)
        .context("read OS CSPRNG")?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
fn write_token_atomically(path: &Path, token: &str) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("token file has no parent")?;
    let temporary = parent.join(format!(".dashboard.env.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        writeln!(file, "RTRT_DASHBOARD_TOKEN={token}")?;
        file.sync_all()?;
        anyhow::ensure!(
            std::fs::symlink_metadata(path).is_err(),
            "dashboard token appeared during migration"
        );
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn legacy_token_from_owned_service(
    home: &Path,
    service_path: Option<&Path>,
) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;

    let Some(service_path) = service_path else {
        return Ok(None);
    };
    let metadata = match std::fs::symlink_metadata(service_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let owner = std::fs::symlink_metadata(home)?.uid();
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.uid() == owner,
        "refusing unsafe dashboard service file {}",
        service_path.display()
    );
    let body = std::fs::read_to_string(service_path)?;
    if !service_body_is_owned(&body) {
        return Ok(None);
    }

    let projects = home.join(".rtrt").join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry?;
        let entry_metadata = std::fs::symlink_metadata(entry.path())?;
        anyhow::ensure!(
            entry_metadata.is_dir() && !entry_metadata.file_type().is_symlink(),
            "refusing unsafe legacy dashboard project directory {}",
            entry.path().display()
        );
        let candidate = entry.path().join("dashboard").join(TOKEN_FILE);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {
                let token = read_existing_dashboard_token(&candidate, home)?;
                let systemd_reference =
                    systemd_path(&candidate.to_string_lossy(), "EnvironmentFile")?;
                if body.contains(&systemd_reference) || body.contains(&token) {
                    matches.push(token);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::ensure!(
        matches.len() <= 1,
        "multiple legacy dashboard tokens match the owned service"
    );
    Ok(matches.pop())
}

#[cfg(unix)]
fn ensure_machine_dashboard_token(
    state: &DashboardState,
    home: &Path,
    legacy_service: Option<&Path>,
) -> Result<String> {
    ensure_private_directory(&state.root, home)?;
    let _lock = acquire_migration_lock(&state.root)?;
    match std::fs::symlink_metadata(&state.token_file) {
        Ok(_) => return read_existing_dashboard_token(&state.token_file, home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let token = match legacy_token_from_owned_service(home, legacy_service)? {
        Some(token) => token,
        None => random_dashboard_token()?,
    };
    write_token_atomically(&state.token_file, &token)?;
    read_existing_dashboard_token(&state.token_file, home)
}

#[cfg(not(unix))]
fn ensure_machine_dashboard_token(
    _state: &DashboardState,
    _home: &Path,
    _legacy_service: Option<&Path>,
) -> Result<String> {
    bail!("dashboard service token installation is unsupported on this OS")
}

#[cfg(all(test, unix))]
fn ensure_dashboard_token_in(path: &Path, home: &Path) -> Result<String> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let owner = std::fs::symlink_metadata(home)
        .with_context(|| format!("inspect {}", home.display()))?
        .uid();
    let relative = path
        .strip_prefix(home)
        .context("dashboard token path escaped home")?;
    let mut current = home.to_path_buf();
    for component in relative
        .parent()
        .context("token file has no parent")?
        .components()
    {
        current.push(component);
        if !current.exists() {
            std::fs::create_dir(&current)
                .with_context(|| format!("mkdir {}", current.display()))?;
        }
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("inspect {}", current.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "refusing unsafe dashboard directory {}",
            current.display()
        );
        anyhow::ensure!(
            metadata.uid() == owner,
            "refusing dashboard directory owned by another user: {}",
            current.display()
        );
        std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700))?;
    }

    if path.exists() || std::fs::symlink_metadata(path).is_ok() {
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "refusing unsafe dashboard token file {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.uid() == owner,
            "refusing dashboard token file owned by another user: {}",
            path.display()
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let body = std::fs::read_to_string(path)?;
        return parse_token_file(&body);
    }

    let mut random = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open OS CSPRNG")?
        .read_exact(&mut random)
        .context("read OS CSPRNG")?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    use std::io::Write;
    writeln!(file, "RTRT_DASHBOARD_TOKEN={token}")?;
    file.sync_all()?;
    Ok(token)
}

fn parse_token_file(body: &str) -> Result<String> {
    let token = body
        .strip_suffix('\n')
        .unwrap_or(body)
        .strip_prefix("RTRT_DASHBOARD_TOKEN=")
        .context("invalid dashboard token file")?;
    anyhow::ensure!(
        token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid dashboard token file"
    );
    Ok(token.to_string())
}

#[cfg(unix)]
fn read_existing_dashboard_token(path: &Path, home: &Path) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let home_metadata =
        std::fs::symlink_metadata(home).with_context(|| format!("inspect {}", home.display()))?;
    anyhow::ensure!(
        home_metadata.is_dir() && !home_metadata.file_type().is_symlink(),
        "refusing unsafe home directory"
    );
    let owner = home_metadata.uid();
    let relative = path
        .strip_prefix(home)
        .context("dashboard token path escaped home")?;
    let mut current = home.to_path_buf();
    for component in relative
        .parent()
        .context("token file has no parent")?
        .components()
    {
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("inspect {}", current.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "refusing unsafe dashboard directory {}",
            current.display()
        );
        anyhow::ensure!(
            metadata.uid() == owner && metadata.permissions().mode() & 0o077 == 0,
            "refusing unsafe dashboard directory permissions or ownership: {}",
            current.display()
        );
    }
    let symlink_metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("dashboard token file unavailable at {}", path.display()))?;
    anyhow::ensure!(
        symlink_metadata.is_file() && !symlink_metadata.file_type().is_symlink(),
        "refusing unsafe dashboard token file {}",
        path.display()
    );
    anyhow::ensure!(
        symlink_metadata.uid() == owner && symlink_metadata.permissions().mode() & 0o077 == 0,
        "refusing unsafe dashboard token file permissions or ownership"
    );
    anyhow::ensure!(symlink_metadata.len() <= 96, "invalid dashboard token file");
    let file = std::fs::File::open(path)
        .with_context(|| format!("open dashboard token file {}", path.display()))?;
    let opened_metadata = file.metadata()?;
    anyhow::ensure!(
        opened_metadata.dev() == symlink_metadata.dev()
            && opened_metadata.ino() == symlink_metadata.ino(),
        "dashboard token file changed while opening"
    );
    let mut body = String::new();
    file.take(97).read_to_string(&mut body)?;
    anyhow::ensure!(body.len() <= 96, "invalid dashboard token file");
    parse_token_file(&body)
}

#[cfg(not(unix))]
fn read_existing_dashboard_token(_path: &Path, _home: &Path) -> Result<String> {
    bail!("dashboard service token reading is unsupported on this OS")
}

#[cfg(unix)]
fn write_private_owned_file(path: &Path, body: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        let home_uid = std::fs::symlink_metadata(home()?)?.uid();
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "refusing unsafe service file {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.uid() == home_uid,
            "refusing service file owned by another user: {}",
            path.display()
        );
        let existing = std::fs::read_to_string(path)?;
        anyhow::ensure!(
            service_body_is_owned(&existing),
            "refusing to overwrite unowned service file {}",
            path.display()
        );
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    file.write_all(body)?;
    file.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_owned_file(_path: &Path, _body: &[u8]) -> Result<()> {
    bail!("dashboard service installation is unsupported on this OS")
}

fn owned_service_file(path: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "refusing unsafe service file {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = std::fs::symlink_metadata(home()?)?.uid();
        anyhow::ensure!(
            metadata.uid() == owner,
            "refusing service file owned by another user: {}",
            path.display()
        );
    }
    let body = std::fs::read_to_string(path)?;
    anyhow::ensure!(
        service_body_is_owned(&body),
        "refusing to remove unowned service file {}",
        path.display()
    );
    Ok(true)
}

fn service_body_is_owned(body: &str) -> bool {
    body.lines().any(|line| {
        matches!(
            line.trim(),
            "# rtrt-managed-dashboard-service" | "<!-- rtrt-managed-dashboard-service -->"
        )
    }) || (body.contains("Description=Retort (rtrt) dashboard") && body.contains("ExecStart="))
        || (body.contains("<string>io.kodenet.rtrt-dashboard</string>")
            && body.contains("<key>ProgramArguments</key>"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn systemd_plain_paths_have_exact_unquoted_directives() {
        let body = systemd_unit_body(
            "/usr/local/bin/rtrt-dashboard",
            "/home/demo/.rtrt/dashboard",
        )
        .unwrap();
        assert!(!body.contains("WorkingDirectory="));
        assert!(!body.contains("EnvironmentFile="));
        assert!(body.contains(
            "ExecStart=\"/usr/local/bin/rtrt-dashboard\" \"--machine\" \"--state-dir\" \"/home/demo/.rtrt/dashboard\"\n"
        ));
    }

    #[test]
    fn systemd_paths_escape_spaces_quotes_and_backslashes() {
        let body = systemd_unit_body(
            "/home/demo/bin dir/\"rtrt\"\\dashboard",
            "/home/demo/state dir/\"dashboard\"\\root",
        )
        .unwrap();
        assert!(body.contains(
            "ExecStart=\"/home/demo/bin dir/\\\"rtrt\\\"\\\\dashboard\" \"--machine\" \"--state-dir\" \"/home/demo/state dir/\\\"dashboard\\\"\\\\root\"\n"
        ));
    }

    #[test]
    fn systemd_paths_escape_percent_specifiers() {
        let body =
            systemd_unit_body("/home/%h/bin/rtrt%dashboard", "/home/%h/%i/dashboard").unwrap();
        assert!(body.contains("\"/home/%%h/%%i/dashboard\""));
        assert!(body.contains("ExecStart=\"/home/%%h/bin/rtrt%%dashboard\""));
    }

    #[test]
    fn systemd_values_reject_newline_nul_and_relative_paths() {
        assert!(systemd_unit_body("/usr/bin/rtrt\nnext", "/state").is_err());
        assert!(systemd_unit_body("/usr/bin/rtrt", "/state\nnext").is_err());
        assert!(systemd_unit_body("/usr/bin/rtrt", "/state\0dir").is_err());
        assert!(systemd_unit_body("relative", "/state").is_err());
        assert!(systemd_unit_body("/usr/bin/rtrt", "relative").is_err());
    }

    #[test]
    fn systemd_install_always_reloads_enables_and_restarts_in_order() {
        let mut executed = Vec::new();
        run_systemd_install_commands(|args| {
            executed.push(args.join(" "));
            Ok(())
        })
        .unwrap();

        assert_eq!(
            executed,
            [
                "daemon-reload",
                "enable rtrt-dashboard.service",
                "restart rtrt-dashboard.service",
            ]
        );
    }

    #[test]
    fn systemd_install_stops_after_failed_command() {
        let mut executed = Vec::new();
        let result = run_systemd_install_commands(|args| {
            executed.push(args.join(" "));
            anyhow::ensure!(args != ["enable", SYSTEMD_UNIT], "enable failed");
            Ok(())
        });

        assert!(result.is_err());
        assert_eq!(executed, ["daemon-reload", "enable rtrt-dashboard.service"]);
    }

    #[test]
    fn dashboard_health_waits_for_delayed_readiness_and_returns_last_error() {
        let mut attempts = 0;
        wait_for_dashboard_health(
            || {
                attempts += 1;
                anyhow::ensure!(attempts >= 3, "not ready {attempts}");
                Ok(())
            },
            5,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!(attempts, 3);

        let error = wait_for_dashboard_health(
            || anyhow::bail!("still starting"),
            2,
            std::time::Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "still starting");
    }

    #[test]
    fn service_ownership_requires_an_exact_marker_line() {
        let body = systemd_unit_body("/usr/bin/rtrt", "/state").unwrap();
        assert!(service_body_is_owned(&body));
        assert!(service_body_is_owned(
            "\t<!-- rtrt-managed-dashboard-service -->\n"
        ));
        assert!(!service_body_is_owned(
            "# prefix-rtrt-managed-dashboard-service-suffix\n"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generated_systemd_unit_passes_systemd_analyze_when_available() {
        if Command::new("systemd-analyze")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("rtrt % dashboard");
        let state = directory.path().join("dashboard % state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let body = systemd_unit_body(binary.to_str().unwrap(), state.to_str().unwrap()).unwrap();
        let unit = directory.path().join("rtrt-dashboard.service");
        std::fs::write(&unit, body).unwrap();

        let output = Command::new("systemd-analyze")
            .args(["--user", "verify", unit.to_str().unwrap()])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Failed to setup working directory: Read-only file system") {
            return;
        }
        assert!(
            output.status.success(),
            "systemd-analyze verify failed:\n{}",
            stderr
        );
    }

    #[test]
    fn token_file_is_private_and_reused() {
        let home = tempfile::tempdir().unwrap();
        let path = home
            .path()
            .join(".rtrt/projects/demo/dashboard/dashboard.env");
        let first = ensure_dashboard_token_in(&path, home.path()).unwrap();
        let second = ensure_dashboard_token_in(&path, home.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn token_path_rejects_symlink() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("target");
        std::fs::write(&target, "do not touch").unwrap();
        let dashboard = home.path().join(".rtrt/projects/demo/dashboard");
        std::fs::create_dir_all(&dashboard).unwrap();
        let path = dashboard.join("dashboard.env");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(ensure_dashboard_token_in(&path, home.path()).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "do not touch");
    }

    #[test]
    fn machine_token_promotes_single_owned_legacy_service_atomically_and_idempotently() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home
            .path()
            .join(".rtrt/projects/demo/dashboard/dashboard.env");
        let legacy_token = ensure_dashboard_token_in(&legacy, home.path()).unwrap();
        let service = home.path().join("rtrt-dashboard.service");
        let legacy_body = format!(
            "# {OWNED_MARKER}\n[Service]\nEnvironmentFile={}\nExecStart=/bin/false\n",
            systemd_path(&legacy.to_string_lossy(), "EnvironmentFile").unwrap()
        );
        std::fs::write(&service, legacy_body).unwrap();
        let state = dashboard_state(home.path());

        let promoted = ensure_machine_dashboard_token(&state, home.path(), Some(&service)).unwrap();
        let repeated = ensure_machine_dashboard_token(&state, home.path(), Some(&service)).unwrap();

        assert_eq!(promoted, legacy_token);
        assert_eq!(repeated, legacy_token);
        assert!(legacy.exists(), "migration must retain legacy token file");
        assert_eq!(
            std::fs::metadata(&state.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&state.token_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let lock = state.root.join(MIGRATION_LOCK);
        assert_eq!(
            std::fs::metadata(lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn machine_token_rejects_symlinked_state_and_secret() {
        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let rtrt = home.path().join(".rtrt");
        std::fs::create_dir(&rtrt).unwrap();
        std::os::unix::fs::symlink(&outside, rtrt.join("dashboard")).unwrap();
        let state = dashboard_state(home.path());
        assert!(ensure_machine_dashboard_token(&state, home.path(), None).is_err());

        std::fs::remove_file(rtrt.join("dashboard")).unwrap();
        ensure_private_directory(&state.root, home.path()).unwrap();
        let target = home.path().join("target.env");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &state.token_file).unwrap();
        assert!(ensure_machine_dashboard_token(&state, home.path(), None).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn service_file_is_private_and_symlinks_fail() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("service");
        write_private_owned_file(&path, OWNED_MARKER.as_bytes()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&path).unwrap();
        let target = home.path().join("target");
        std::fs::write(&target, "foreign").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(write_private_owned_file(&path, b"replacement").is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "foreign");
    }

    #[test]
    fn foreign_service_file_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("service");
        std::fs::write(&path, "foreign service").unwrap();
        assert!(write_private_owned_file(&path, OWNED_MARKER.as_bytes()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "foreign service");
    }

    #[test]
    fn launchd_values_are_xml_escaped() {
        let body = launchd_plist_body("/tmp/a&b", "/tmp/<state>&");
        assert!(body.contains("/tmp/a&amp;b"));
        assert!(body.contains("/tmp/&lt;state&gt;&amp;"));
        assert!(body.contains("<string>--machine</string>"));
        assert!(!body.contains("WorkingDirectory"));
        assert!(!body.contains("RTRT_DASHBOARD_TOKEN"));
    }

    #[test]
    fn open_url_and_exact_opener_argv_never_contain_long_token() {
        use std::ffi::OsStr;

        const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let credential =
            rtrt_core::dashboard_bootstrap::issue_with_nonce(TOKEN, 1_000, 60, [3; 16]).unwrap();
        let url = bootstrap_url(&credential, "3e8-2a").unwrap();
        assert!(!url.contains(TOKEN));
        assert_eq!(
            url,
            format!("{DASHBOARD_URL}?open=3e8-2a#bootstrap={credential}")
        );
        let path = opener_path_for_os("linux").unwrap();
        let command = opener_command(&path, &url);
        assert_eq!(command.get_program(), OsStr::new("/usr/bin/xdg-open"));
        assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new(&url)]);
        assert_ne!(command.get_program(), OsStr::new("sh"));
    }

    #[test]
    fn open_read_rejects_unsafe_token_file_without_changing_it() {
        let home = tempfile::tempdir().unwrap();
        let path = home
            .path()
            .join(".rtrt/projects/demo/dashboard/dashboard.env");
        let token = ensure_dashboard_token_in(&path, home.path()).unwrap();
        assert_eq!(
            read_existing_dashboard_token(&path, home.path()).unwrap(),
            token
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_existing_dashboard_token(&path, home.path()).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        std::fs::remove_file(&path).unwrap();
        let target = home.path().join("target.env");
        std::fs::write(&target, format!("RTRT_DASHBOARD_TOKEN={token}\n")).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_existing_dashboard_token(&path, home.path()).is_err());
    }

    #[test]
    fn unsafe_opener_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("opener");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        let link = directory.path().join("open");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(validate_opener(&link).is_err());
    }
}
