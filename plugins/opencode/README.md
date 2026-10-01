# rtrt-agent

`rtrt-agent` is RTRT's OpenCode integration. It preserves parent session, agent,
worktree, invocation, and permission-broker context across supported hooks and keeps
the local dashboard available in the background.

## Install (v1 setup-managed wiring, v0.2.0)

This source targets v0.2.0. Installation requires the matching published npm release; a source version marker is not proof of publication. The older `rtrt-agent@0.1.7` release remains unchanged and does not include the native v2 entries described below.

For the matching npm release:

```sh
npm install rtrt-agent@0.2.0
```

Register the package in the singular `plugin` array in `opencode.json`:

```json
{
  "plugin": ["rtrt-agent@0.2.0"]
}
```

The v1 package root remains the named export `RtrtProvenance` only — its name is unchanged across the v0.2.0 source. The optional v1 TUI statusline stays a separate setup-managed integration and is not exported or packed by the v1 package. The newly staged native v2 source is described below and is not published in `rtrt-agent@0.1.7`.

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
action. Both paths use a 60-second fragment bootstrap and return only a fixed status;
credentials are never written into prompts or command templates. Startup is fail-soft:
if the binary or private state is unsafe or unavailable, OpenCode continues normally.
Existing `~/.rtrt` data is preserved.

`rtrt setup --agent opencode --apply` manages the full RTRT integration but performs no
npm installation; OpenCode installs the configured package at startup. Setup first writes
the exact `rtrt-agent@0.2.0` registration and every replacement managed asset. Only after
those writes succeed does it remove a recognized legacy RTRT plugin; an earlier failure
preserves the legacy runtime. Setup owns exact bare, pinned, ranged, tuple, and object forms
of `rtrt-agent` only and normalizes them to one exact `"rtrt-agent@0.2.0"` string. Old
unpublished `rtrt` and draft `rtrt-opencode` package specs are foreign and retain their
order. The direct registration above and `opencode plugin rtrt-agent@0.2.0 --global`
remain valid.

Managed-agent ownership state is read from the first nonempty root in this order:
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, then `~/.config/opencode`.
Coexistence is CI-gated against OMO 4.19.4 and was verified on OpenCode 1.18.29; neither
is a future-version guarantee. The unified RTRT release workflow publishes the exact
five exact-version dashboard platform packages before `rtrt-agent@0.2.0`, then publishes
the matching Rust release artifacts.

## Native OpenCode 2.0.20 Entries

The v0.2.0 manifest includes native v2 server and TUI files in the pack inventory. The older `rtrt-agent@0.1.7` release does not include or register those entries and remains unchanged. The native source targets exactly `@opencode/cli@2.0.20` and `@opencode/plugin@2.0.20`, not the 1.x SDK's v2 preview. The package root remains the v1 named export `RtrtProvenance` (name-only, unchanged across v0.2.0).

What is staged in the v0.2.0 source pack:

- A native `Plugin.Definition` default export at `./server`, constructed by the named `createNativeServer` factory and exposing `{ id: "rtrt-agent", setup }`. The v1 `RtrtProvenance` path stays separate.
- Single-element arrays for `./server` and `./tui` in `package.json` `exports`. OpenCode 1.18.33's legacy resolver ignores those arrays and falls back to `main` `index.js`; native 2.0.20 resolves them normally.
- A v1 TUI statusline that remains a separate `app_bottom` registration. The v2 native `rtrt-statusline.tsx` lives under `tui/v2/` and only runs in 2.x; v1 wiring is unchanged.
- A native `prompt.footer.status` registration and cleanup for both server and TUI hooks.

Tool-arg provenance, dashboard auto-startup, and an explicit `rtrt_dashboard_open` all live behind the native server. Permission policies are fail-closed: explicit `deny` is preserved across the v1 broker and the v2 evaluator. External Claude CLI permission broker support and per-call shell identity / provider-limit recovery are **unsupported** in the native v2 build because those surfaces are not in the 2.0.20 public API; they are not auto-approved via any workaround. The deny-by-default policy is unchanged from the v1 broker.

### Local source checkout (no npm install)

For development against a local checkout, OpenCode's native `plugins` config accepts a directory reference. Exact paths depend on your checkout, so only the shape is shown:

```json
{
  "plugins": [
    "/absolute/path/to/rtrt/plugins/opencode"
  ]
}
```

This directory form was verified locally with the actual `@opencode/cli@2.0.20` host in an isolated HOME/XDG environment. With that exact version: `server.js` is active, native TUI discovery reports `features.tui: true`, RTRT stdio MCP connects, strict TUI typechecking and bundling pass, and headless footer rendering at 120 and 40 columns passes. The verification does not cover the full interactive OpenCode TUI and is not a guarantee for other OpenCode versions; it is specific to the exact `@opencode/cli@2.0.20` host that was tested.

### Native MCP shape (informational, OpenCode 2 official schema)

The native local MCP shape uses `mcp.servers.rtrt.type: "local"` and a `command` array such as `["/absolute/path/to/rtrt-mcp", "--transport", "stdio"]`; the option names are `disabled` and `codemode`, both `false` in the sandbox source config. The release-pinned v1 registration is `rtrt-agent@0.2.0`. `rtrt setup --agent opencode --apply` still writes v1 configuration, not native v2 installation; setup itself does not install npm packages, and OpenCode resolves the configured package at startup.

Published `rtrt-agent@0.1.7` remains unchanged on the npm registry. The sandbox's default v2 configuration is MCP-only; the staged native plugin, footer, and forwarding adapter are not activated there. This source work does not promote, activate, or wire up a Docker deployment, the `rtrt collector` service, the operator-side `forward` plumbing, or any other operational services — those remain separate surfaces and are not enabled by the v0.2.0 source change.

## License

MIT
