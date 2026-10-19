# OmO native integration

**English** | [한국어](OMO_NATIVE.ko.md)

This integration targets the OmO/Senpi native extension API, separately from the
OpenCode 2.x server plugin and legacy oh-my-openagent integration.

## Install

Support for `rtrt setup --agent omo` starts at **v0.2.4**. Earlier releases, including
v0.2.3, do not contain this setup target. Pick the install path that matches how
you obtained the binaries.

### Pinned release binary (recommended)

Pin the v0.2.4 release tarball, opt out of the installer's Claude refresh and
Linux OpenCode bootstrap, and skip the dashboard background service. The
absolute `~/.local/bin/` paths make sure an older `rtrt` or `rtrt-mcp` earlier on
PATH cannot win the call:

```sh
# Linux / macOS / WSL — pin v0.2.4, no agent setup, no dashboard service
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/v0.2.4/install.sh \
  | sh -s -- --version v0.2.4 --no-setup --no-service

# Dry-run setup first (prints the intended writes, writes nothing)
"$HOME/.local/bin/rtrt" setup --agent omo \
  --binary "$HOME/.local/bin/rtrt-mcp"

# Then apply the same command with --apply
"$HOME/.local/bin/rtrt" setup --agent omo \
  --binary "$HOME/.local/bin/rtrt-mcp" --apply
```

The two setup commands share every flag except `--apply`. Without `--apply`,
setup prints the intended changes and exits without writing. After `--apply`,
reload or restart OmO so the host re-reads the merged `mcp.json` and the
extension directory.

### Windows pinned release binary

```powershell
# PowerShell — pin v0.2.4, no agent setup, no dashboard service
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/v0.2.4/install.ps1))) `
  -Version v0.2.4 -NoSetup -NoService

# Dry-run setup first
& "$env:LOCALAPPDATA\Programs\rtrt\rtrt.exe" setup --agent omo `
  --binary "$env:LOCALAPPDATA\Programs\rtrt\rtrt-mcp.exe"

# Then --apply
& "$env:LOCALAPPDATA\Programs\rtrt\rtrt.exe" setup --agent omo `
  --binary "$env:LOCALAPPDATA\Programs\rtrt\rtrt-mcp.exe" --apply
```

Use the absolute `.exe` paths the installer drops under
`%LOCALAPPDATA%\Programs\rtrt\`. Restart OmO after `--apply` so the new MCP entry
and the `extensions/rtrt/` directory are picked up. `-NoService` keeps the
dashboard logon task off. The `rtrt-mcp` stdio process is started on demand
by OmO independently of that task.

### From source (alternative)

`cargo install --path` works when you build the workspace yourself. Use absolute
paths so an older binary earlier on PATH cannot win:

```sh
cargo install --path crates/rtrt-cli --force
cargo install --path crates/rtrt-mcp --force
"$HOME/.cargo/bin/rtrt" setup --agent omo --binary "$HOME/.cargo/bin/rtrt-mcp"
"$HOME/.cargo/bin/rtrt" setup --agent omo --binary "$HOME/.cargo/bin/rtrt-mcp" --apply
```

The first setup command is a dry-run. On Windows, use the corresponding absolute
paths to the installed `.exe` files. Reload or restart OmO after applying
changes.

The agent directory uses the first nonempty absolute value of
`OMO_CODING_AGENT_DIR`, `SENPI_CODING_AGENT_DIR`, or `PI_CODING_AGENT_DIR`,
then defaults to `~/.omo/agent`. Relative overrides are refused.

Setup manages three files:

- `<agentDir>/mcp.json`: merges `mcpServers.rtrt` with stdio transport, the
  absolute MCP executable, and empty `args`. It omits `cwd` so the host starts
  the server in the active project. Other servers and settings remain intact.
- `<agentDir>/extensions/rtrt/index.js`: the native-discoverable entry, pinned
  to the setup CLI's `rtrt` executable for output filtering.
- `<agentDir>/extensions/rtrt/runtime.mjs`: the dependency-free native adapter.

The entry uses the existing MCP `settings.toolPrefix`, defaulting to `mcp`.
Setup does not replace native tool-search or promotion settings.
The runtime also declares the pinned server through the native factory-time
`registerMcpServer` API so it participates in native and codemode tool discovery.
The host retains precedence for existing user MCP configuration. Rerun setup if
you change the tool prefix.

## Ownership and removal

Setup refuses symlink destinations or directory components, foreign entry files,
modified runtime files, and a conflicting or modified `mcpServers.rtrt` entry.
Foreign files elsewhere in the extension directory are preserved. JSONC comments
and unrelated configuration survive the surgical merge. Before the first config
change, the original is retained at `mcp.json.bak`; repeated setup converges.

`rtrt uninstall --agent omo` is also a dry-run. With `--apply`, it removes
only the owned MCP entry, `index.js`, and `runtime.mjs`. It preserves other
servers, user files, the backup, and the directory itself. Modified owned content
is refused rather than deleted. Reload or restart the host afterward.

## Native behavior

For the native MCP tools `mcp_rtrt_agent_call` and
`mcp_rtrt_agent_route` (using the configured prefix), `tool_call` adds a
stable per-call UUID in `invocation_id`, plus `parent_session_id`,
`parent_call_id`, `caller_agent: "omo-native"`, and `parent_cwd`.
The MCP server still owns project attribution. Other calls are unchanged.

Simple successful foreground bash results containing one text block are passed to
`rtrt proxy -- <original-command>` over stdin. The command is not executed
again, its arguments are not rewritten, and native permission decisions remain
unchanged. The filter runs in the native session's cwd. Only result content is
patched; metadata and text annotations are retained.

Compound commands, pipes, redirections, substitutions, multiline commands, errors,
explicit background requests, nontext or multiblock results, and input or output
above 256 KiB are preserved. Missing executables, nonzero filter exits,
cancellation, and the 1.5-second total deadline also preserve the native result.
Session shutdown cancels pending filtering and clears invocation state.

The adapter does not auto-capture prompts, emulate Claude permission brokerage,
replace native memory or provider routing, alter the footer or side panel, or
start a dashboard. Existing explicit service commands remain available.

## Verification

```sh
node --test plugins/omo/*.test.mjs
cargo test -p rtrt-cli --test omo_setup
```

The node suites exercise provenance, output filtering, process boundaries,
Unicode, cancellation, and byte/time bounds. The CLI suite uses isolated HOME and
agent directories for install, idempotence, refusal, and removal checks.
The native acceptance test is opt-in and needs three absolute executable paths:

```sh
cargo build -p rtrt-cli -p rtrt-mcp
RTRT_OMO_NATIVE_BIN=/absolute/path/to/omo \
RTRT_NATIVE_RTRT_BIN="$PWD/target/debug/rtrt" \
RTRT_NATIVE_MCP_BIN="$PWD/target/debug/rtrt-mcp" \
node --test plugins/omo/*.test.mjs
```

Verified locally on Linux with OmO native 5.1.35: factory discovery, a successful
MCP templates call, search promotion, codemode bash filtering, and per-call
provenance. The test uses isolated HOME/agent/project directories and a loopback
model fixture, not live provider credentials. It awaits native RPC connection
responses and tool activation rather than assuming the first model request has
a ready catalog.
Without these variables, only the external-host test is skipped.
