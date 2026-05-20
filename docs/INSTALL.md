# Install

**English** | [한국어](INSTALL.ko.md)

RTRT is in alpha. The supported install path today is **from source via `cargo`**. Pre-built binaries, crates.io publishes, and one-line install scripts are listed below as planned.

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

## One-liner (planned)

```bash
# macOS / Linux / WSL
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

The installers will pick the matching pre-built binary from the latest GitHub release and place it in a user-local path (`~/.local/bin/` or `%LOCALAPPDATA%\Programs\rtrt\`). Not yet wired — track [#1](https://github.com/kernalix7/rtrt/issues).

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

## Uninstall

Remove the cargo-installed binaries:

```bash
cargo uninstall rtrt-cli rtrt-mcp rtrt-dashboard
```

Remove on-disk state (memory store, custom templates):

```bash
rm -rf ~/.rtrt/
```

Remove the repo clone if you no longer need it.
