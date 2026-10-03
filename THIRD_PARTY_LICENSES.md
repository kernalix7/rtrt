# Third-Party Licenses

RTRT is MIT-licensed (see [LICENSE](LICENSE)). The tables below are selected
examples; the conservative default/release normal+build inventory for each of
the three product roots on five targets is [INVENTORY.json](THIRD_PARTY_NOTICES/INVENTORY.json).
It maps locked version/checksum/SPDX/upstream source and original notice bytes.
[INDEX.md](THIRD_PARTY_NOTICES/INDEX.md) and
[SUPPLEMENTS.json](THIRD_PARTY_NOTICES/SUPPLEMENTS.json) explain interpretation,
checksum-pinned VCS commits, full-text supplements for the eight crate archives
that contain no license file, and the separate embedded JS notices. A
`Cargo.toml` manifest is evidence of the declared SPDX expression, not full
license terms. Supplements are original pin texts or, where no upstream text
exists, the explicitly selected Apache-2.0 standard text. This is not a
compiled or retained-code SBOM or a legal certification.
The classic OpenCode plugin declares `@opencode-ai/sdk@1.15.13` as an npm
dependency. RTRT's agent tarball does not copy the SDK's `node_modules` bytes;
the package manager installs that dependency separately. Native OpenCode 2
plugin and renderer packages are optional host peers, not copied into the
agent tarball. Redistributing an installed dependency tree or a container
image requires reviewing that resolved tree's own licenses and notices.

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
| [hmac](https://crates.io/crates/hmac) + [sha2](https://crates.io/crates/sha2) | MIT OR Apache-2.0 | authenticated dashboard bootstrap credentials |
| [base64](https://crates.io/crates/base64) | MIT OR Apache-2.0 | URL-fragment-safe bootstrap encoding |
| [getrandom](https://crates.io/crates/getrandom) | MIT OR Apache-2.0 | OS CSPRNG bootstrap nonces |

## Bundled native code

| Component | License | Bundled via |
|-----------|---------|-------------|
| [SQLite](https://www.sqlite.org/copyright.html) | public domain | `rusqlite`'s `bundled` feature (statically linked into `rtrt-memory`) |

## Optional host executables

### bubblewrap (`bwrap`)

Eligible Linux/WSL installs automatically use an operator-installed
`/usr/bin/bwrap` or `/bin/bwrap` for secure machine bootstrap unless setup is
disabled with `--no-setup` / `RTRT_NO_SETUP=1`. Bubblewrap is **not bundled,
linked, copied, downloaded, or
auto-installed** by RTRT. RTRT searches no `PATH` entry and executes only a
root-owned, executable, non-group/world-writable fixed candidate.

The authoritative upstream [COPYING file](https://github.com/containers/bubblewrap/blob/main/COPYING)
for `containers/bubblewrap` is GNU Library General Public License version 2
(LGPL-2.0). This describes the operator-supplied host executable only; RTRT
does not bundle, link, copy, download, or install it. The operator's package
metadata remains authoritative for the installed version.

### Host prerequisites

`socat` may be required by Claude Code on a particular host. It is optional,
host-supplied, never bundled or installed by RTRT, and setup does not imply
user approval or install consent.

## TLS

`reqwest` is configured with the `rustls-tls` feature and `default-features = false`, so RTRT does **not** link against system OpenSSL or platform native-tls. The TLS stack at runtime is:

- [rustls](https://crates.io/crates/rustls) — Apache-2.0 OR ISC OR MIT
- [webpki-roots@0.26.11](https://crates.io/crates/webpki-roots/0.26.11) and [webpki-roots@1.0.7](https://crates.io/crates/webpki-roots/1.0.7) — CDLA-Permissive-2.0 (certificate data; both versions resolved)
- [ring@0.17.14](https://crates.io/crates/ring/0.17.14) — Apache-2.0 AND ISC in different files, including BoringSSL-derived code; see its upstream `LICENSE`, `LICENSE-BoringSSL`, `LICENSE-other-bits`, and bundled subpart notices in [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES/INDEX.md)
- [subtle@2.6.1](https://crates.io/crates/subtle/2.6.1) — BSD-3-Clause
- [matchit@0.8.4](https://crates.io/crates/matchit/0.8.4) — MIT **AND** BSD-3-Clause; both original license files shipped
- [rustls-webpki@0.103.15](https://crates.io/crates/rustls-webpki/0.103.15) — ISC; upstream LICENSE shipped

Other identified transitive dependency: [option-ext@0.2.0](https://crates.io/crates/option-ext/0.2.0) — MPL-2.0. The complete unmodified upstream source crate and its license text are bundled at `THIRD_PARTY_NOTICES/option-ext@0.2.0/SOURCE.crate` and `LICENSE.txt` in each of the five GitHub binary archives, five dashboard platform npm packages, and `rtrt-agent` npm package. [Notice index](THIRD_PARTY_NOTICES/INDEX.md) gives the checksum and local extraction instructions; the [versioned upstream archive](https://static.crates.io/crates/option-ext/option-ext-0.2.0.crate) is an additional option, not a required download. This source-code copy covers the MPL component only, not a relicensing of MIT-licensed RTRT. These targeted notices are not an exhaustive inventory or a legal certification; qualified counsel must assess remaining obligations and applicability to particular binaries.

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
