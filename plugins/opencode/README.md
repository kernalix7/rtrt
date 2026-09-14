# rtrt-agent

`rtrt-agent` is RTRT's OpenCode provenance plugin. It preserves parent session,
agent, worktree, invocation, and permission-broker context across supported OpenCode
hooks while keeping the broker loopback-only.

## Install

```sh
npm install rtrt-agent@0.1.2
```

Register the package in the singular `plugin` array in `opencode.json`:

```json
{
  "plugin": ["rtrt-agent@0.1.2"]
}
```

OpenCode loads the named `RtrtProvenance` export. The package intentionally has no
default export. The optional RTRT TUI statusline remains a separate source integration
and is not exported or packed by this package.

`rtrt setup --agent opencode --apply` manages the full RTRT integration but performs no
npm installation; OpenCode installs the configured package at startup. Setup first writes
the exact `rtrt-agent@0.1.2` registration and every replacement managed asset. Only after
those writes succeed does it remove a recognized legacy RTRT plugin; an earlier failure
preserves the legacy runtime. Setup owns exact bare, pinned, ranged, tuple, and object forms
of `rtrt-agent` only and normalizes them to one exact `"rtrt-agent@0.1.2"` string. Old
unpublished `rtrt` and draft `rtrt-opencode` package specs are foreign and retain their
order. The direct registration above and `opencode plugin rtrt-agent@0.1.2 --global`
remain valid.

Managed-agent ownership state is read from the first nonempty root in this order:
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, then `~/.config/opencode`.
Coexistence is CI-gated against OMO 4.19.4 and was verified on OpenCode 1.18.29; neither
is a future-version guarantee. The unified RTRT release workflow publishes the exact
`rtrt-agent@0.1.2` package together with the matching Rust release.

## License

MIT
