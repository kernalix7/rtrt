# Install

**English** | [한국어](INSTALL.ko.md)

RTRT is in alpha. Two install paths are supported today: **one-line script** (fetches the binary or builds from `main` if no release matches) and **from source via `cargo`**. Pre-built binary releases land with `v0.2.0` — until then the one-liner falls back to `--main` automatically when invoked with that flag.

## One-liner (recommended)

```bash
# Linux / macOS / WSL — latest release, auto-falls back to `--main` if none yet
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
| `--uninstall` | `-Uninstall` | — | Compatibility shim — defers to `uninstall.sh` / `uninstall.ps1` |
| `--dry-run` | `-DryRun` | — | Print intended actions without writing |

Flags take precedence over the env-var equivalents. When no release exists and no flag is set, the installer prints a notice and falls back to `--ref main` automatically.

Examples:

```bash
# Pin a release
curl -fsSL .../install.sh | sh -s -- --version v0.2.0

# Track a topic branch
RTRT_REF=feature/cache curl -fsSL .../install.sh | sh

# Install from a local clone (offline)
sh install.sh --source ~/code/rtrt

# Drop binaries somewhere custom + skip toolchain check
sh install.sh --dir /opt/rtrt/bin --skip-deps
```

### Uninstall (one-liner)

```bash
# Linux / macOS / WSL — binaries only (state under ~/.rtrt left intact)
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --confirm

# Full purge — binaries + ~/.rtrt + fastembed model cache
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --purge
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Confirm'
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Purge'
```

Both `uninstall.sh` and `uninstall.ps1` also run interactively when executed locally without `--confirm` / `-Confirm` — they ask before each step. `install.sh --uninstall` / `install.ps1 -Uninstall` stay as compatibility shims that delete the binaries without touching state.

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

Repeat for `crates/rtrt-mcp` and `crates/rtrt-dashboard` if you want the MCP server and dashboard binaries globally available.

## crates.io (planned)

```bash
cargo install rtrt-cli         # `rtrt` binary
cargo install rtrt-mcp         # MCP server
cargo install rtrt-dashboard   # web dashboard
```

Not yet published.

## Pre-built binaries (planned)

GitHub Releases will publish:

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

`rtrt info` should print the version + crate manifest. `rtrt templates` should list six built-in templates.

## Uninstall (manual)

If you installed from source via `cargo install`, remove the binaries with:

```bash
cargo uninstall rtrt-cli rtrt-mcp rtrt-dashboard
```

For the curl-installer flow, prefer the one-liners under [Uninstall (one-liner)](#uninstall-one-liner) above. They live as standalone scripts (`uninstall.sh` / `uninstall.ps1`) and accept `--confirm` (binaries only) or `--purge` (binaries + `~/.rtrt` + fastembed model cache).

Manual state cleanup:

```bash
rm -rf ~/.rtrt/                       # memory store, prompt registry, custom templates
rm -rf ~/.cache/fastembed/             # ONNX model cache (only present if `embeddings` feature ran)
```

Remove the repo clone if you no longer need it.
