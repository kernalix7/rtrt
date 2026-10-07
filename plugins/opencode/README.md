# rtrt-agent

`rtrt-agent` is RTRT's OpenCode integration. It preserves parent session, agent,
worktree, invocation, and permission-broker context across supported hooks and keeps
the local dashboard available in the background.

## Install (v1 setup-managed wiring, v0.2.2)

This source targets `v0.2.2`. Installation requires a matching published npm release;
the source tree alone is not proof of publication. The current install command is:

```sh
npm install rtrt-agent@0.2.2
```

Register it in `opencode.json`:

```json
{
  "plugin": ["rtrt-agent@0.2.2"]
}
```

The source tree carries the staged native v2 entries on top of the v1 contract; the
`v0.2.2` npm release publishes that source. The already-published `v0.2.0` and
`v0.2.1` npm releases are immutable and do not change. The older
`rtrt-agent@0.1.7` release remains unchanged on npm and does not include the
native v2 entries described below.

The v1 package root keeps the named export `RtrtProvenance` only — its name is unchanged
across the v0.2.2 source. The optional v1 TUI statusline stays a separate setup-managed
integration and is not exported or packed by the v1 package. The native v2 entries
documented below are present in the source pack and added under the v0.2.2 npm release
manifest; their host-side activation depends on the boundary described in
[Native OpenCode 2.0.20 Entries](#native-opencode-2020-entries).

Compared to `v0.2.1`, `v0.2.2` is a release-publisher maintenance change only:
the GitHub release discovery, draft handling, and asset ID pinning logic were
reworked while paired tag / byte / inventory checks and the no-asset-clobber guard
are unchanged. No new user-facing behavior, native v2 host activation surface,
setup flow, or binary ACL hardening is introduced beyond what `v0.2.1` already
shipped.

## Dashboard

On plugin load, `rtrt-agent` schedules `rtrt-dashboard` as a detached per-user process
on `127.0.0.1:7311`. Startup never waits for dashboard readiness and never opens a
browser. The matching platform executable comes from an exact-version optional npm
package; no preinstalled `rtrt`, `PATH` lookup, install script, or project build is used.

Open the browser only when requested:

```sh
rtrt-dashboard-open
```

An agent may invoke the argument-free `rtrt_dashboard_open` tool for the same explicit
action. Both paths write a bounded `bootstrap.html` inside protected dashboard state
through an exclusive same-directory temporary file, owner-only permissions or ACL
**before** writing HTML, then rename. The ordinary opener argv passes only a local
absolute file path; URL, fragment, or `bootstrap=` opener arguments are rejected. The
60-second single-use HMAC credential, nonce replay, Origin, bearer exchange, and UI
fragment clearing remain unchanged.

`rtrt service open --print-bootstrap` still prints only the short-lived warning URL and
does not launch a browser. Without `--print-bootstrap`, the Linux/macOS `rtrt service
open` path uses the same owner-private `bootstrap.html` and absolute local file path
argv. On Windows, `rtrt service open` is refused; open <http://127.0.0.1:7311/> and
enter the token in the dashboard's bootstrap prompt.

Startup is fail-soft: if the binary or private state is unsafe or unavailable, OpenCode
continues normally. Existing `~/.rtrt` data is preserved.

`rtrt setup --agent opencode --apply` manages the full RTRT integration but performs no
npm installation; OpenCode installs the configured package at startup. Setup first writes
the exact `rtrt-agent@0.2.2` registration and every replacement managed asset. Only after
those writes succeed does it remove a recognized legacy RTRT plugin; an earlier failure
preserves the legacy runtime. Setup owns exact bare, pinned, ranged, tuple, and object forms
of `rtrt-agent` only and normalizes them to one exact `"rtrt-agent@0.2.2"` string. Old
unpublished `rtrt` and draft `rtrt-opencode` package specs are foreign and retain their
order. The direct registration above and `opencode plugin rtrt-agent@0.2.2 --global`
remain valid.

Managed-agent ownership state is read from the first nonempty root in this order:
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, then `~/.config/opencode`.
Coexistence is CI-gated against OMO 4.19.4 and was verified on OpenCode 1.18.29; neither
is a future-version guarantee. The unified RTRT release workflow publishes the exact
five exact-version dashboard platform packages before `rtrt-agent@0.2.2`, then publishes
the matching Rust release artifacts.

### Existing Windows state and credential ACL

Existing npm-managed `.rtrt`, `dashboard`, `startup.lock`, `dashboard.env`, and
`bootstrap.html` paths are validated before use. A new directory or file gets
owner-only permissions or ACL (current SID FullControl only, inheritance disabled)
before sensitive bytes. Existing paths with any non-owner write ACE or an inherited
permissive ACE on private credential or state files are refused: startup fails
closed and OpenCode continues without the dashboard. The SYSTEM/Admins and
trusted-OS-binary exemptions used for the public dashboard binary do not extend to
private credential or state ACLs. Older pre-existing unsafe state does not get
silently rewritten to "look painless". The `v0.2.1` npm release introduced the
fresh-state hardening and refusal of unsafe existing state, and that behavior is
retained in `v0.2.2`. The remediation does not introduce deletion or automatic
ACL repair, and does not promise a migration that renames operator files. The
already-published `v0.2.0` and `v0.2.1` npm releases are immutable; `v0.2.0`
does not contain the fresh-state hardening or unsafe-state refusal fixes, and
`v0.2.1` is the source of those fixes for installs on or after that tag.

## Native OpenCode 2.0.20 Entries

The `v0.2.2` npm release ships native v2 server and TUI files in the pack inventory.
The older `rtrt-agent@0.1.7` release on npm does not include or register those entries
and stays unchanged. The native source targets exactly `@opencode/cli@2.0.20` and
`@opencode/plugin@2.0.20`, not the 1.x SDK's v2 preview. The package root remains the
v1 named export `RtrtProvenance` (name-only, unchanged across v0.2.2).

What is in the `v0.2.2` pack:

- A native `Plugin.Definition` default export at `./server`, constructed by the named
  `createNativeServer` factory and exposing `{ id: "rtrt-agent", setup }`. The v1
  `RtrtProvenance` path stays separate.
- Single-element arrays for `./server` and `./tui` in `package.json` `exports`. OpenCode
  1.18.33's legacy resolver ignores those arrays and falls back to `main` `index.js`;
  native 2.0.20 resolves them normally.
- A v1 TUI statusline that remains a separate `app_bottom` registration. The v2 native
  `rtrt-statusline.tsx` lives under `tui/v2/` and only runs in 2.x; v1 wiring is
  unchanged.
- A native `prompt.footer.status` registration and cleanup for both server and TUI
  hooks.

Tool-arg provenance, dashboard auto-startup, and an explicit `rtrt_dashboard_open` all
live behind the native server. Permission policies are fail-closed: explicit `deny`
is preserved across the v1 broker and the v2 evaluator. External Claude CLI permission
broker support and per-call shell identity / provider-limit recovery are
**unsupported** in the native v2 build because those surfaces are not in the 2.0.20
public API; they are not auto-approved via any workaround. The deny-by-default policy
is unchanged from the v1 broker.

### Configuring the native server AND the native TUI

Native OpenCode 2.0.20 reads two files for plugin configuration, and they are not the
same surface:

- `opencode.json` controls the **server inventory**. The package contributes a string
  source target; server options (per the 2.0.20 schema) are not propagated from this
  surface.
- `<OPENCODE_CONFIG_DIR>/cli.json` controls the **native CLI options**. The native
  host reads `cli.json` (or its CLI config content override) separately from the
  project/server `opencode.json`. The package's TUI `options` (notably `bin`) must
  appear here.

`bin` distinguishes explicit and omitted options. An **explicit** `options.bin`
must be an absolute path to a real `rtrt` executable; when set, native 2.0.20
never falls back to `RTRT_BIN`, `PATH`, or any other candidate — an invalid,
missing, or non-absolute explicit bin makes the footer show
`N/A | STATE UNKNOWN | COST N/A | CTX N/A | DEGRADED`. When `options.bin` is
**omitted**, the shared runner consults `RTRT_BIN` first and then `PATH`'s
`rtrt`; if neither works, the same unavailable state is shown. Server options
such as the `bin` value are not forwarded to the TUI from `opencode.json`; they
only reach the TUI when also written into `cli.json`. Configure both files with
the same `options.bin`.

Example with a real release binary on disk (the form exercised by the native
fixture, where both `opencode.json` and `cli.json` carry the same `plugins`
entry):

```jsonc
// /absolute/path/to/checkout/opencode.json  (server inventory)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt-agent",
      "options": { "bin": "/absolute/path/to/rtrt-0.2.2/rtrt" }
    }
  ]
}
```

```jsonc
// /absolute/path/to/<OPENCODE_CONFIG_DIR>/cli.json  (native TUI options)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt-agent",
      "options": { "bin": "/absolute/path/to/rtrt-0.2.2/rtrt" }
    }
  ]
}
```

The two files configure different host surfaces. The native CLI owns TUI options and
will not pick them up from `opencode.json` alone; the server does not forward them.
Replacing `bin` with a relative path, a `PATH` lookup, or an auto-detected release path
is not supported by the package. The default absolute binary is the operator's own
release build.

### Local source checkout (no npm install)

For development against a local checkout, OpenCode's native `plugins` config accepts a
directory reference. Exact paths depend on your checkout, so only the shape is shown
(both files mirror the executed fixture):

```jsonc
// opencode.json (server inventory)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt/plugins/opencode",
      "options": { "bin": "/absolute/path/to/rtrt" }
    }
  ]
}
```

```jsonc
// <OPENCODE_CONFIG_DIR>/cli.json (native TUI options)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt/plugins/opencode",
      "options": { "bin": "/absolute/path/to/rtrt" }
    }
  ]
}
```

This two-file form was verified locally with the actual `@opencode/cli@2.0.20` host in
an isolated HOME/XDG environment. With that exact version: `server.js` is active,
native TUI discovery reports `features.tui: true`, RTRT stdio MCP connects, strict
TUI typechecking and bundling pass, and headless footer rendering at 120 and 40
columns passes against a real release `rtrt` binary and a deliberately not-installed
binary (missing → `N/A | STATE UNKNOWN | ...`). The verification does not cover the
full interactive OpenCode TUI and is not a guarantee for other OpenCode versions; it
is specific to the exact `@opencode/cli@2.0.20` host that was tested.

### Native MCP shape (informational, OpenCode 2 official schema)

The native local MCP shape uses `mcp.servers.rtrt.type: "local"` and a `command` array
such as `["/absolute/path/to/rtrt-mcp", "--transport", "stdio"]`; the option names are
`disabled` and `codemode`, both `false` in the sandbox source config. The
release-pinned v1 registration is `rtrt-agent@0.2.2`. `rtrt setup --agent opencode
--apply` still writes v1 configuration, not native v2 installation; setup itself does
not install npm packages, and OpenCode resolves the configured package at startup.

Published `rtrt-agent@0.1.7` remains unchanged on the npm registry. The sandbox's
default v2 configuration is MCP-only; the staged native plugin, footer, and forwarding
adapter are not activated there. This source work does not promote, activate, or wire
up a Docker deployment, the `rtrt collector` service, the operator-side `forward`
plumbing, or any other operational services — those remain separate surfaces and are
not enabled by the v0.2.2 source change.

### Windows native support

The npm package's dashboard supervisor runs on Windows (`.rtrt`, `dashboard`,
`startup.lock`, `dashboard.env`, `bootstrap.html` ACL enforcement via the bundled
PowerShell policy, binary trust, and ordinary open path — exercised by
`dashboard-acl.test.mjs`, `dashboard-binary.test.mjs`, and `dashboard-open.test.mjs`).
The Unix supervisor test exercises the same surfaces on Unix and is not in the
selected Windows lane. The historical v0.2.1 reference run
<https://github.com/kernalix7/rtrt/actions/runs/37133785080> recorded 24 selected
Windows-only acceptance cases and a separate 18-case system-drive binary resolver
TAP pass, plus a D-root refusal proof; name-filter-only skips are allowed, mandatory
D-root refusal is not waived, and selected Windows-only cases must run. That run is
historical evidence of the v0.2.1 candidate, **not** fresh approval of `v0.2.2`;
this README does not claim a fresh `windows-latest` run has already passed for the
v0.2.2 candidate, which must independently clear the same gate before release. The
`rtrt-dashboard-open` binary path is shipped on Windows, but `rtrt service open`
from the Rust CLI is refused on Windows; open <http://127.0.0.1:7311/> and enter
the token in the dashboard's bootstrap prompt.

## License

MIT
