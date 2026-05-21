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
rtrt compress -l ultra < verbose.md             # Caveman-style rule rewrite
rtrt compress --llm --provider openai-compat \  # LLM rewrite (Ollama / any provider)
   --base-url http://127.0.0.1:11434/v1 --model llama3.2 < verbose.md
rtrt proxy "git status" < git-status-output     # Filter command output
rtrt signatures --lang rust < src/file.rs       # tree-sitter signature map
rtrt repo-map crates/rtrt-core                  # signature map of a directory
rtrt discover                                   # find proxy candidates in shell history
rtrt templates                                  # list built-in templates
rtrt new rust-cli ./hello --var project_name=hello
rtrt setup --agent claude --apply               # wire RTRT into Claude Code's MCP config
rtrt memory save --project p --kind note "fact"
rtrt memory recall --project p --query rust
rtrt memory extract --project p --provider openai-compat \
   --base-url http://127.0.0.1:11434/v1 --model llama3.2 < passage.md
rtrt memory compress --project p --keep 20 --provider anthropic --model claude-haiku-4-5
rtrt prompt save greet "say hi" --meta env=dev
rtrt prompt get greet
rtrt docs facebook/react --topic hooks          # context7 library docs
rtrt provider chat --model claude-haiku-4-5 "ping"
rtrt-dashboard                                  # http://127.0.0.1:3111 (tabs: metrics / templates / stats)
rtrt-mcp --memory ~/.rtrt/memory.sqlite         # stdio MCP server, 6 tools
```

See [docs/USAGE.md](docs/USAGE.md) for the full CLI, MCP tool surface, and dashboard tour.

## Key features

<table>
<tr><td width="50%">

**Output compression**
- Rule-based rewriter with levels `lite` / `full` / `ultra` / `extreme`
- Drops fillers, pleasantries, hedging, discourse markers; ultra rewrites verbose phrases; extreme drops qualifiers too
- Code blocks, inline code, URLs, and quoted error strings preserved; secret-shaped substrings (AWS / GH / OpenAI / Anthropic / Bearer / private-key) auto-redacted before the rule pass
- Measured savings on representative AI prose: short ~32%, mixed ~18%, long ~15%; code-heavy ~6% (intentional, we never rewrite code)
- LLM-backed compression mode (Ollama-compatible) is the path to caveman-class 50–75% savings
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
- SQLite + FTS5 store with `scope / project / kind / body` schema; tiers: `user` / `agent` / `session` / `project`
- BM25 (`recall_bm25`), dense-vector cosine (`recall_vector`), and BM25 ⊕ vector RRF (`recall_hybrid`)
- Sub-linear ANN via `HnswIndex` behind the `hnsw` feature (`instant-distance`)
- Directed labelled edges + `recall_via_graph` BFS for entity / relation traversal
- `all-MiniLM-L6-v2` embeddings (`embeddings` feature, fastembed); attach via `MemoryStore::with_embedder` to auto-embed every `save`
- LLM extract / compress via any `Provider` — local Ollama included — under the `llm` feature
- [Details →](docs/FEATURES.md#persistent-memory)

</td><td width="50%">

**Multi-provider routing**
- Provider trait with built-in Anthropic / OpenAI / OpenAI-compatible adapters
- OpenAI-compatible base URL covers Ollama, llama.cpp, vLLM, LM Studio
- `Gateway` fronts every provider behind one entry; per-request `RequestMetric` (id / parent_id / cost / latency) feeds the dashboard `/api/metrics`
- `Budget::new(usd)` fails-fast when the cumulative cost cap is hit
- `Context7Client` fetches version-pinned library docs (`rtrt docs facebook/react --topic hooks`)
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
- `rtrt-mcp` (rmcp 1.x, stdio) ships 6 tools: `compress`, `memory_save`, `memory_recall`, `templates_list`, `templates_scaffold`, `provider_chat`
- `rtrt-dashboard` (axum) tabs: gateway metrics (live KPI + per-request table), templates, savings; endpoints: `/api/chat`, `/api/metrics`, `/api/templates*`, `/api/stats`
- `rtrt setup --agent <name>` writes the MCP config for Claude / Cursor / Codex / Windsurf
- Versioned prompt registry under `~/.rtrt/prompts/<name>/<NNNN>.toml` (`rtrt prompt {save,get,list,versions}`)
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
| [INSPIRATION.md](docs/INSPIRATION.md) | Idea backlog from 15+ AI-tooling projects, mapped to RTRT crates |
| [CHANGELOG.md](CHANGELOG.md) | Full version history |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup and workflow |
| [SECURITY.md](SECURITY.md) | Security disclosure process |

## Crates

| Crate | Role |
|-------|------|
| `rtrt-core` | Shared types, plugin trait, errors, config |
| `rtrt-compress` | Rule rewriter + secret redactor + tree-sitter signature extractor + LLM compression mode |
| `rtrt-proxy` | Command-output filter (rtk-style) |
| `rtrt-memory` | SQLite + FTS5 BM25 + vector + graph + HNSW + LLM-driven extract/compress |
| `rtrt-providers` | Multi-provider chat trait + Gateway + Budget + Context7 doc fetcher |
| `rtrt-templates` | Built-in + custom scaffolds + handlebars rendering + `PromptRegistry` |
| `rtrt-mcp` | rmcp 1.x stdio MCP server (6 tools) |
| `rtrt-dashboard` | Axum web dashboard + REST API (`/api/{chat,metrics,templates,stats}`) |
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
- [x] `rtrt-compress` rule engine + extreme level + secret redactor + tree-sitter signatures + LLM mode
- [x] `rtrt-proxy` filters for git + cargo
- [x] `rtrt-memory` SQLite + FTS5 BM25 + dense-vector + RRF hybrid + edges graph + HNSW + memory tiers
- [x] `rtrt-memory` LLM-driven extract / compress / archival via any provider (local Ollama OK)
- [x] `rtrt-templates` 6 built-ins + custom loader + handlebars + versioned `PromptRegistry`
- [x] `rtrt-providers` real Anthropic / OpenAI / OpenAI-compatible HTTP + streaming + Gateway + Budget + Context7 docs
- [x] `rtrt-mcp` rmcp stdio transport with 6 tools (`compress`, `memory_*`, `templates_*`, `provider_chat`)
- [x] `rtrt-dashboard` axum UI with tabs (metrics / templates / stats) + `/api/chat` gateway endpoint
- [x] `install.sh` + `install.ps1` one-liners + `release.yml` 5-target build matrix
- [x] `rtrt setup --agent <name>` wires RTRT into Claude / Cursor / Codex / Windsurf
- [x] criterion benchmark harness + per-fixture savings table
- [ ] MCP HTTP / SSE transport (stdio is shipped)
- [ ] `caveman-shrink`-style MCP tool-description compression middleware
- [ ] `recall_via_graph` driven by LLM entity extraction (mem0 entity linking)
- [ ] Helicone-style retry / fallback routing across providers
- [ ] First tagged release (`v0.2.0-rc1`)

## Inspired by

RTRT borrows ideas from many other projects. See [docs/INSPIRATION.md](docs/INSPIRATION.md) for the full source list — per-project ideas, the RTRT crate they fit, and priority. Legal attribution lives in [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md#reference-projects-inspiration-only-no-code-redistributed). When an idea ships in a release, the CHANGELOG entry credits the source inline.

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
