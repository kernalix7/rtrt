# Install

**English** | [한국어](INSTALL.ko.md)

RTRT is in alpha. Two install paths are supported today: **one-line script** and **from source via `cargo`**. The one-liner fetches the latest release binary and verifies its checksum. It falls back to a source build from `main` only when the latest release is unavailable or does not exist. Once a release is selected, any release asset or checksum failure fails closed and never falls back to source.

## One-liner (recommended)

```bash
# Linux / macOS / WSL — latest release; only an unavailable or absent release falls back to `--main`
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

The installers detect OS + arch, download the matching tarball / zip from the latest GitHub Release, verify the SHA256, and drop `rtrt` / `rtrt-mcp` / `rtrt-dashboard` into `~/.local/bin/` (Linux/macOS) or `%LOCALAPPDATA%\Programs\rtrt\` (Windows).

### Flags + environment variables

| Flag | PowerShell | Env var | Purpose |
|------|-----------|---------|---------|
| `--version vX.Y.Z` | `-Version` | — | Pin a specific release tarball (skip source build) |
| `--main` (alias for `--ref main`) | `-Main` | `RTRT_REF=main` | Build from git main HEAD |
| `--ref TAG` | `-Ref` | `RTRT_REF` | Build from a specific tag / branch / commit |
| `--source PATH` | `-Source` | `RTRT_SOURCE` | Build from a local copy (offline / air-gapped) |
| `--dir PATH` | `-InstallDir` | — | Install dir (default: `~/.local/bin` / `%LOCALAPPDATA%\Programs\rtrt`) |
| `--skip-deps` | `-SkipDeps` | `RTRT_SKIP_DEPS=1` | Skip the cargo / git toolchain check |
| `--no-setup` | `-NoSetup` | `RTRT_NO_SETUP=1` | Disable Claude refresh and Linux OpenCode bootstrap/session migration |
| `--no-service` | `-NoService` | `RTRT_NO_SERVICE=1` | Don't auto-start the `rtrt-dashboard` background service |
| `--uninstall` | `-Uninstall` | — | Data-preserving compatibility shim; full interactive/purge flow uses the platform uninstaller |
| `--dry-run` | `-DryRun` | — | Print intended actions without writing |

Flags take precedence over the env-var equivalents. When the latest release is unavailable or does not exist and no flag is set, the installer prints a notice and falls back to `--ref main` automatically. After a release is selected, a missing or invalid asset or checksum fails the installation instead of falling back to source.

On Linux/WSL, existing OpenCode config/managed state, or an exact safe absolute `command -v opencode` result, plus operator-installed fixed `bwrap`, makes a non-root installation eligible for automatic bootstrap using the exact newly installed `rtrt`. No directory is searched and OpenCode is not executed. This machine-only setup authorizes no checkout, then losslessly migrates existing global sessions; migration/setup failures leave binaries and source data intact and print manual commands. Native Windows and macOS skip strict `bwrap` setup. Later `rtrt opencode --` authorizes only its explicitly selected checkout.

### Background dashboard service

By default the installer starts one per-user, machine-scope `rtrt-dashboard`, making the web UI at <http://127.0.0.1:7311> available after login. It has no repository cwd, project slug, `RTRT_MEMORY_PATH`, or token argv; the dashboard reads exactly `~/.rtrt/dashboard/dashboard.env` and lists verified stores under `~/.rtrt/projects`. The selector's **All projects** choice is an aggregate view; select a concrete project before project-specific writes. Pass `--no-service` (`-NoService` on Windows, or `RTRT_NO_SERVICE=1`) to skip service and token creation. `--dry-run` / `-DryRun` writes nothing; normal piped/noninteractive installation still installs the service.

- **Linux** — a systemd **user** unit at `~/.config/systemd/user/rtrt-dashboard.service`.
- **macOS** — a launchd LaunchAgent at `~/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist`.
- **Windows** — an installer-owned logon scheduled task named `rtrt-dashboard`, invoking `rtrt-dashboard.exe --machine --state-dir "%USERPROFILE%\.rtrt\dashboard"`. Its private state ACL is limited to the installing user; reinstall reuses the 32-byte CSPRNG token and updates only a recognized owned task.

Manage Linux/macOS directly with `~/.local/bin/rtrt service install|uninstall|status` (dry-run by default, pass `--apply`). Windows task generation belongs to the installer. Unix uninstall and `install.ps1 -Uninstall` remove only recognized owned service/task definitions; machine token and project databases remain unless explicit purge is selected.

On Windows, the installed task uses `%LOCALAPPDATA%\Programs\rtrt\rtrt-dashboard.exe`. Windows `rtrt service` management/opening is not currently supported; open <http://127.0.0.1:7311/> and enter the token only in the dashboard's bootstrap prompt. Never put the token in a command, URL, or task definition.

Examples:

```bash
# Pin a release
curl -fsSL .../install.sh | sh -s -- --version v0.1.1

# Track a topic branch
RTRT_REF=feature/cache curl -fsSL .../install.sh | sh

# Install from a local clone (offline)
sh install.sh --source ~/code/rtrt

# Drop binaries somewhere custom + skip toolchain check
sh install.sh --dir /opt/rtrt/bin --skip-deps
```

### Uninstall (one-liner)

```bash
# Linux / macOS / WSL — unwires OpenCode and Claude Code, removes
# the dashboard service + binaries; state under ~/.rtrt left intact
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --confirm

# Full purge — the above + ~/.rtrt + fastembed model cache
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --purge
```

```powershell
# Windows PowerShell — `irm | iex` cannot forward parameters, so wrap in a scriptblock
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1))) -Confirm
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1))) -Purge
```

The uninstaller restores OpenCode's recorded prior shell before deleting binaries, removes managed agent/service surfaces, and preserves session databases and other data unless `--purge` / `-Purge` is passed.

Both uninstallers also support interactive local use. Compatibility shims preserve data; `install.sh --uninstall` first restores managed OpenCode shell ownership and refuses unsafe deletion.

## From source (current)

Requires:

- Rust stable 1.85+ (edition 2024). `rustup install stable` if missing.
- A C toolchain for the `rusqlite` bundled SQLite build (`gcc` or `clang`).

```bash
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo build --release --workspace
```

The build produces three binaries under `target/release/`:

- `rtrt` — top-level CLI (`crates/rtrt-cli`)
- `rtrt-mcp` — MCP server (`crates/rtrt-mcp`)
- `rtrt-dashboard` — web dashboard (`crates/rtrt-dashboard`)

Install the CLI on your `PATH`:

```bash
cargo install --path crates/rtrt-cli
```

Repeat for `crates/rtrt-mcp` and `crates/rtrt-dashboard` if you want the MCP server and dashboard binaries globally available. These commands build from the local clone; workspace crates are not published to crates.io.

## Pre-built binaries

The v0.1.1 GitHub Release channel publishes:

- `rtrt-<version>-x86_64-unknown-linux-gnu.tar.gz`
- `rtrt-<version>-aarch64-unknown-linux-gnu.tar.gz`
- `rtrt-<version>-x86_64-apple-darwin.tar.gz`
- `rtrt-<version>-aarch64-apple-darwin.tar.gz`
- `rtrt-<version>-x86_64-pc-windows-msvc.zip`

Each archive bundles `rtrt`, `rtrt-mcp`, and `rtrt-dashboard`.

## Verifying the install

```bash
rtrt --version
rtrt info
rtrt templates
```

`rtrt info` should print the version and 11-crate workspace manifest. `rtrt templates` should list the four built-in templates: `dev`, `design`, `plan`, and `standardization`.

## Uninstall (manual)

If you installed from source via `cargo install`, remove the binaries with:

```bash
cargo uninstall rtrt-cli rtrt-mcp rtrt-dashboard
```

For the curl-installer flow, prefer the one-liners under [Uninstall (one-liner)](#uninstall-one-liner) above. They live as standalone scripts (`uninstall.sh` / `uninstall.ps1`) and accept `--confirm` (Claude Code wiring + service + binaries, data kept) or `--purge` (the above + `~/.rtrt` + fastembed model cache).

Manual state cleanup:

```bash
rm -rf ~/.rtrt/                       # memory store, prompt registry, custom templates
rm -rf ~/.cache/fastembed/             # ONNX model cache (only present if `embeddings` feature ran)
```

Remove the repo clone if you no longer need it.
