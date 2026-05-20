# Third-Party Licenses

RTRT is MIT-licensed (see [LICENSE](LICENSE)). This document lists the
third-party Rust crates RTRT depends on at runtime or in development,
together with their upstream licenses.

## Runtime dependencies

Always pulled in by at least one workspace crate.

| Crate | License | Used in |
|-------|---------|---------|
| [anyhow](https://crates.io/crates/anyhow) | MIT OR Apache-2.0 | binaries (error reporting) |
| [thiserror](https://crates.io/crates/thiserror) | MIT OR Apache-2.0 | `rtrt-core` (error derive) |
| [serde](https://crates.io/crates/serde) + [serde_json](https://crates.io/crates/serde_json) | MIT OR Apache-2.0 | all crates (serialization) |
| [tokio](https://crates.io/crates/tokio) | MIT | async runtime |
| [tracing](https://crates.io/crates/tracing) + [tracing-subscriber](https://crates.io/crates/tracing-subscriber) | MIT | structured logging |
| [clap](https://crates.io/crates/clap) | MIT OR Apache-2.0 | `rtrt-cli` argument parsing |
| [async-trait](https://crates.io/crates/async-trait) | MIT OR Apache-2.0 | `rtrt-core` plugin trait, `rtrt-providers` |
| [reqwest](https://crates.io/crates/reqwest) | MIT OR Apache-2.0 | `rtrt-providers` HTTP client (rustls only, no native-tls) |
| [axum](https://crates.io/crates/axum) | MIT | `rtrt-dashboard` HTTP server |
| [tower](https://crates.io/crates/tower) + [tower-http](https://crates.io/crates/tower-http) | MIT | `rtrt-dashboard` middleware |
| [regex](https://crates.io/crates/regex) | MIT OR Apache-2.0 | `rtrt-compress`, `rtrt-proxy` |
| [once_cell](https://crates.io/crates/once_cell) | MIT OR Apache-2.0 | `rtrt-compress`, `rtrt-proxy`, `rtrt-templates` |
| [rusqlite](https://crates.io/crates/rusqlite) | MIT | `rtrt-memory` (with `bundled` SQLite) |
| [toml](https://crates.io/crates/toml) | MIT OR Apache-2.0 | `rtrt-templates` manifest parsing |
| [walkdir](https://crates.io/crates/walkdir) | MIT OR Apache-2.0 | `rtrt-templates` custom-template scan |
| [dirs](https://crates.io/crates/dirs) | MIT OR Apache-2.0 | `rtrt-templates` `~/.rtrt/templates` lookup |

## Bundled native code

| Component | License | Bundled via |
|-----------|---------|-------------|
| [SQLite](https://www.sqlite.org/copyright.html) | public domain | `rusqlite`'s `bundled` feature (statically linked into `rtrt-memory`) |

## TLS

`reqwest` is configured with the `rustls-tls` feature and `default-features = false`, so RTRT does **not** link against system OpenSSL or platform native-tls. The TLS stack at runtime is:

- [rustls](https://crates.io/crates/rustls) — Apache-2.0 OR ISC OR MIT
- [webpki-roots](https://crates.io/crates/webpki-roots) — MPL-2.0
- [ring](https://crates.io/crates/ring) — ISC + OpenSSL + custom (see crate's README)

## Development-only dependencies

These ship in the `[dev-dependencies]` table or in CI tooling, not in published binaries.

| Crate | License |
|-------|---------|
| [cargo-audit](https://crates.io/crates/cargo-audit) | MIT OR Apache-2.0 |
| [cargo-deny](https://crates.io/crates/cargo-deny) | MIT OR Apache-2.0 |

## Reference projects (inspiration only, no code redistributed)

RTRT re-implements ideas from these projects in Rust. No source code is copied or vendored. The per-idea mapping (which RTRT crate borrows what) lives in [docs/INSPIRATION.md](docs/INSPIRATION.md).

**Direct one-to-one inspiration:**

- **[caveman](https://github.com/JuliusBrussee/caveman)** — output simplification rules. RTRT's `rtrt-compress` is an independent Rust implementation of the same idea.
- **[agentmemory](https://github.com/rohitg00/agentmemory)** — SQLite-backed memory + hybrid recall. RTRT's `rtrt-memory` borrows the schema concept and embeddings target (`all-MiniLM-L6-v2`); the recall implementation is independent.
- **[rtk](https://github.com/rtk-ai/rtk)** — CLI proxy for command-output reduction. RTRT's `rtrt-proxy` is an independent Rust implementation.
- **[codex-plugin-cc](https://github.com/openai/codex-plugin-cc)** — single-provider Codex integration for Claude Code. RTRT's multi-provider design is broader and does not derive from codex-plugin-cc source.

**Inspiration backlog (RTRT may borrow specific ideas; no source copied):**

- **Output compression**: [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua), [yamadashy/repomix](https://github.com/yamadashy/repomix).
- **Persistent memory & retrieval**: [mem0ai/mem0](https://github.com/mem0ai/mem0), [chroma-core/chroma](https://github.com/chroma-core/chroma), [letta-ai/letta](https://github.com/letta-ai/letta), [cpacker/MemGPT](https://github.com/cpacker/MemGPT), [qdrant/qdrant](https://github.com/qdrant/qdrant), [lancedb/lancedb](https://github.com/lancedb/lancedb), [neuml/txtai](https://github.com/neuml/txtai).
- **Multi-provider gateway & orchestration**: [Helicone/helicone](https://github.com/Helicone/helicone), [sobelio/llm-chain](https://github.com/sobelio/llm-chain), [upstash/context7](https://github.com/upstash/context7).
- **Templates & agent scaffolds**: [mufeedvh/code2prompt](https://github.com/mufeedvh/code2prompt), [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI), [dust-tt/dust](https://github.com/dust-tt/dust).
- **Observability & cost tracking**: [langfuse/langfuse](https://github.com/langfuse/langfuse), [Doriandarko/claude-engineer](https://github.com/Doriandarko/claude-engineer), [Aider-AI/aider](https://github.com/Aider-AI/aider).

When an idea ships, the CHANGELOG entry credits the source inline (`(inspired by [project-name](url))`) and any per-feature `THIRD_PARTY_LICENSES.md` entry moves up to the "Direct one-to-one inspiration" list.

If you find any attribution gap, please open an issue.
