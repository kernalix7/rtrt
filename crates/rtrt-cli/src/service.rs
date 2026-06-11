//! `rtrt service install|uninstall|status` — run `rtrt-dashboard` as a
//! background OS service so it starts on login and restarts on crash without
//! the user launching it by hand.
//!
//! - Linux  → systemd **user** unit at `~/.config/systemd/user/rtrt-dashboard.service`
//! - macOS  → launchd LaunchAgent at `~/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist`
//! - other  → not supported here (Windows uses the `install.ps1` scheduled task).
//!
//! Default behaviour is **dry-run**: print the unit + the commands that would
//! run. Pass `--apply` to write the file and enable the service.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const SYSTEMD_UNIT: &str = "rtrt-dashboard.service";
const LAUNCHD_LABEL: &str = "io.kodenet.rtrt-dashboard";

pub enum ServiceAction {
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

fn systemd_unit_body(binary: &str) -> String {
    // `%h` is the systemd specifier for the user's home, so the service reads
    // the same memory store as the CLI/MCP/hooks (`~/.rtrt/memory.sqlite`).
    format!(
        "[Unit]\n\
         Description=Retort (rtrt) dashboard — agent-context distillery web UI\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={binary}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         Environment=RTRT_MEMORY_PATH=%h/.rtrt/memory.sqlite\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn systemd(plan: &ServicePlan) -> Result<()> {
    let unit_path = home()?
        .join(".config")
        .join("systemd")
        .join("user")
        .join(SYSTEMD_UNIT);

    match plan.action {
        ServiceAction::Install => {
            let body = systemd_unit_body(&plan.binary.to_string_lossy());
            if !plan.apply {
                println!("[dry-run] would write {}", unit_path.display());
                println!("[dry-run] unit:\n{body}");
                println!("[dry-run] then: systemctl --user daemon-reload");
                println!("[dry-run] then: systemctl --user enable --now {SYSTEMD_UNIT}");
                println!("\nRe-run with --apply to install and start the service.");
                return Ok(());
            }
            if let Some(parent) = unit_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            std::fs::write(&unit_path, body)
                .with_context(|| format!("write {}", unit_path.display()))?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", "--now", SYSTEMD_UNIT])?;
            println!("installed + started {SYSTEMD_UNIT} (systemctl --user)");
            println!("  dashboard: http://127.0.0.1:7311");
            println!("  logs:  journalctl --user -u {SYSTEMD_UNIT} -f");
            println!("  stop:  systemctl --user disable --now {SYSTEMD_UNIT}");
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
            if unit_path.exists() {
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

fn launchd_plist_body(binary: &str, home: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{binary}</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>EnvironmentVariables</key>\n\t<dict>\n\
         \t\t<key>RTRT_MEMORY_PATH</key>\n\t\t<string>{home}/.rtrt/memory.sqlite</string>\n\
         \t</dict>\n\
         </dict>\n\
         </plist>\n"
    )
}

fn launchd(plan: &ServicePlan) -> Result<()> {
    let home = home()?;
    let plist_path = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));

    match plan.action {
        ServiceAction::Install => {
            let body = launchd_plist_body(&plan.binary.to_string_lossy(), &home.to_string_lossy());
            if !plan.apply {
                println!("[dry-run] would write {}", plist_path.display());
                println!("[dry-run] plist:\n{body}");
                println!("[dry-run] then: launchctl unload (if present) + load -w {LAUNCHD_LABEL}");
                println!("\nRe-run with --apply to install and start the service.");
                return Ok(());
            }
            if let Some(parent) = plist_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            std::fs::write(&plist_path, body)
                .with_context(|| format!("write {}", plist_path.display()))?;
            // Reload to pick up changes; ignore the unload error on first install.
            let _ = launchctl(&["unload", &plist_path.to_string_lossy()]);
            launchctl(&["load", "-w", &plist_path.to_string_lossy()])?;
            println!("installed + started {LAUNCHD_LABEL} (launchctl)");
            println!("  dashboard: http://127.0.0.1:7311");
            println!("  stop:  launchctl unload -w {}", plist_path.display());
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
            if plist_path.exists() {
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
