<div align="center">

# RTRT

### Cut tokens. Keep meaning. One Rust toolkit.

<p>Output simplification, command-output filtering, persistent project memory,<br>
multi-provider routing, and standardized project scaffolds — under one CLI,<br>
one MCP server, one web dashboard.</p>

<pre><code># Install (planned)
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh

# From source
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo install --path crates/rtrt-cli</code></pre>

[![Alpha](https://img.shields.io/badge/status-alpha-orange?style=for-the-badge)](#status-alpha)
[![Latest](https://img.shields.io/github/v/release/kernalix7/rtrt?include_prereleases&style=for-the-badge&label=latest&color=2962FF)](https://github.com/kernalix7/rtrt/releases)

[![license](https://img.shields.io/github/license/kernalix7/rtrt?style=flat-square&color=blue)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-CE412B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![edition](https://img.shields.io/badge/edition-2024-CE412B?style=flat-square)](https://doc.rust-lang.org/edition-guide/)
[![CI](https://img.shields.io/github/actions/workflow/status/kernalix7/rtrt/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/kernalix7/rtrt/actions/workflows/ci.yml)
[![stars](https://img.shields.io/github/stars/kernalix7/rtrt?style=flat-square&color=FFD93D&logo=github&logoColor=white)](https://github.com/kernalix7/rtrt/stargazers)
[![downloads](https://img.shields.io/github/downloads/kernalix7/rtrt/total?style=flat-square&color=2EA44F)](https://github.com/kernalix7/rtrt/releases)

###### Works on

[![Linux](https://img.shields.io/badge/Linux-FCC624?style=flat-square&logo=linux&logoColor=black)](https://www.kernel.org/)
[![macOS](https://img.shields.io/badge/macOS-000000?style=flat-square&logo=apple&logoColor=white)](https://www.apple.com/macos/)
[![Windows](https://img.shields.io/badge/Windows-0078D6?style=flat-square&logo=windows&logoColor=white)](https://www.microsoft.com/windows/)
[![WSL](https://img.shields.io/badge/WSL-4D4D4D?style=flat-square&logo=windows&logoColor=white)](https://learn.microsoft.com/windows/wsl/)

<sub>**English** &nbsp;·&nbsp; [한국어](docs/README.ko.md) &nbsp;·&nbsp; [Install](docs/INSTALL.md) &nbsp;·&nbsp; [Usage](docs/USAGE.md) &nbsp;·&nbsp; [Features](docs/FEATURES.md) &nbsp;·&nbsp; [Architecture](docs/ARCHITECTURE.md) &nbsp;·&nbsp; [Comparison](docs/COMPARISON.md)</sub>

</div>

---

> ### Status: Alpha
> RTRT is early. **v0.1.0** is a scaffold release: the workspace compiles, output compression / command-output filtering / SQLite-FTS5 BM25 recall / template scaffolding are usable end-to-end, but the MCP transport, provider chat clients, vector embeddings, and one-line install scripts are explicit stubs marked in the [roadmap](#roadmap). File issues at <https://github.com/kernalix7/rtrt/issues>.

RTRT consolidates four token-reduction techniques behind one CLI, one MCP server, and one web dashboard. It is written entirely in Rust, edition 2024, with zero unsafe in the core crates. Reference projects are reimplemented in Rust rather than vendored.

## Quick install

From source (recommended while pre-release):

```bash
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo install --path crates/rtrt-cli
```

Planned one-liners (not yet wired):

```bash
# macOS / Linux / WSL
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

See [docs/INSTALL.md](docs/INSTALL.md) for crates.io install, pre-built binaries, and uninstall.

## Launch

```bash
rtrt compress -l ultra < verbose.md            # Caveman-style rewrite
rtrt proxy "git status" < git-status-output    # Filter command output
rtrt templates                                 # List built-in templates
rtrt new rust-cli ./hello --var project_name=hello   # Scaffold a project
rtrt-dashboard                                 # Serve http://127.0.0.1:3111
rtrt-mcp                                       # Run the MCP server (stdio, planned)
```

See [docs/USAGE.md](docs/USAGE.md) for the full CLI, MCP tool surface, and dashboard tour.

## Key features

<table>
<tr><td width="50%">

**Output compression**
- Caveman-style terse rewriter with levels `lite`, `full`, `ultra`
- Code blocks, inline code, URLs, and quoted error strings preserved
- Plug-in rules: drop articles / fillers / pleasantries, collapse whitespace
- [Details →](docs/FEATURES.md#output-compression)

</td><td width="50%">

**Command-output filtering**
- `rtrt proxy "<cmd>"` collapses noisy CLI output before it reaches the LLM
- Built-in filters for `git status`, `git log`, `cargo build`, `cargo test`
- Drop-in proxy hook compatible with Claude Code `PreToolUse`
- [Details →](docs/FEATURES.md#command-output-filtering)

</td></tr>
<tr><td width="50%">

**Persistent project memory**
- SQLite + FTS5 store with `project / kind / body` schema
- BM25 recall via FTS5; vector + graph layers reserved in schema for v0.2
- Local-first embeddings target: `all-MiniLM-L6-v2` (offline)
- [Details →](docs/FEATURES.md#persistent-memory)

</td><td width="50%">

**Multi-provider routing**
- Provider trait with built-in Anthropic / OpenAI / OpenAI-compatible adapters
- OpenAI-compatible base URL covers Ollama, llama.cpp, vLLM, LM Studio
- Active provider per task; planned plugin slot for new providers
- [Details →](docs/FEATURES.md#multi-provider-routing)

</td></tr>
<tr><td width="50%">

**Standardized project scaffolds**
- Six built-in templates: `rust-cli`, `rust-lib`, `rust-axum`, `node-typescript`, `python-uv`, `go-cli`
- Web-selectable from the dashboard (`/api/templates`)
- Custom templates load from `~/.rtrt/templates/<name>/manifest.toml`
- Variable substitution (`{{project_name}}`, `{{author}}`, `{{license}}`) + optional post-init hooks
- [Details →](docs/FEATURES.md#project-scaffolds)

</td><td width="50%">

**MCP server + dashboard**
- `rtrt-mcp` exposes `compress`, `memory.save`, `memory.recall`, `provider.chat` as MCP tools (stdio transport planned)
- `rtrt-dashboard` (axum) serves token-savings stats, the template gallery, and a scaffold endpoint
- Plugin format planned for compression rules and provider adapters
- [Details →](docs/FEATURES.md#mcp-and-dashboard)

</td></tr>
</table>

See [docs/FEATURES.md](docs/FEATURES.md) for deep dives, including the rule-protection pipeline and the FTS5 recall query plan.

## Documentation

| Document | What's inside |
|----------|---------------|
| [INSTALL.md](docs/INSTALL.md) | Install paths — source, crates.io (planned), pre-built binaries (planned), uninstall |
| [USAGE.md](docs/USAGE.md) | CLI reference, MCP tools, dashboard tour, configuration file |
| [FEATURES.md](docs/FEATURES.md) | Compression rules, filter strategy, memory schema, multi-provider routing, templates |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | Workspace layout, crate boundaries, data flows |
| [COMPARISON.md](docs/COMPARISON.md) | RTRT vs caveman / agentmemory / rtk / codex-plugin-cc |
| [CHANGELOG.md](CHANGELOG.md) | Full version history |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup and workflow |
| [SECURITY.md](SECURITY.md) | Security disclosure process |

## Crates

| Crate | Role |
|-------|------|
| `rtrt-core` | Shared types, plugin trait, errors, config |
| `rtrt-compress` | Output compression engine (caveman-style) |
| `rtrt-proxy` | Command-output filter (rtk-style) |
| `rtrt-memory` | SQLite + FTS5 memory store with BM25 recall |
| `rtrt-providers` | Multi-provider chat client trait + adapters |
| `rtrt-templates` | Built-in + custom project scaffolds |
| `rtrt-mcp` | MCP server binary |
| `rtrt-dashboard` | Axum web dashboard binary |
| `rtrt-cli` | `rtrt` command-line entry point |

## Testing

```bash
# From repo root
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same three gates on every push and pull request to `main`.

## Roadmap

- [x] Workspace scaffold (9 crates, edition 2024)
- [x] `rtrt-compress` rule engine (lite/full/ultra, code-block-safe)
- [x] `rtrt-proxy` filters for git + cargo
- [x] `rtrt-memory` SQLite + FTS5 BM25 recall
- [x] `rtrt-templates` 6 built-ins + custom loader, web + CLI surfaces
- [x] `rtrt-dashboard` minimal axum UI
- [ ] `rtrt-compress` benchmark harness
- [ ] `rtrt-memory` vector + graph layers; `all-MiniLM-L6-v2` embeddings
- [ ] `rtrt-providers` real Anthropic + OpenAI clients (chat is currently a stub)
- [ ] `rtrt-mcp` stdio transport implementation
- [ ] One-line install scripts (`install.sh` / `install.ps1`)
- [ ] Claude Code plugin manifest

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, branch naming, commit conventions, and CI expectations.

## Security

For security issues, follow the process in [SECURITY.md](SECURITY.md).

## Star History

<a href="https://star-history.com/#kernalix7/rtrt&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=kernalix7/rtrt&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=kernalix7/rtrt&type=Date" />
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=kernalix7/rtrt&type=Date" />
  </picture>
</a>

## Support

If RTRT saves you a few thousand tokens:

[![Ko-fi](https://img.shields.io/badge/Ko--fi-F16061?logo=ko-fi&logoColor=white&style=for-the-badge)](https://ko-fi.com/kernalix7)
[![Fairy](https://img.shields.io/badge/🧚_Fairy-EE6E73?style=for-the-badge&logoColor=white)](https://fairy.hada.io/@kernalix7)

Ko-fi handles international cards and PayPal; fairy.hada.io is a Korean tipping platform. Bug reports, PRs, and stars are equally appreciated and free.

## License

[MIT](LICENSE) — Kim DaeHyun (kernalix7@kodenet.io)
