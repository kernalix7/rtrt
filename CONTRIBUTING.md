# Contributing to RTRT

**English** | [한국어](docs/CONTRIBUTING.ko.md)

Thank you for your interest in contributing to RTRT! This guide will help you get started.

## Prerequisites

- Rust stable 1.85+ (edition 2024). CI gates on `stable` and `beta`.
- A C toolchain for `rusqlite`'s bundled SQLite (`gcc` or `clang`).
- `git` for source control.

A `rust-toolchain.toml` pins the channel to `stable` with `rustfmt` and `clippy` so `rustup` selects the right toolchain automatically.

## Build

```bash
git clone https://github.com/kernalix7/rtrt.git
cd rtrt
cargo build --workspace
```

## Test

```bash
# All tests
cargo test --workspace

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Format check
cargo fmt --all -- --check
```

## Workflow

1. **Fork** the repository
2. Create a **feature branch** (`git checkout -b feat/my-feature`)
3. Write your changes following **conventional commits**
4. Submit a **Pull Request**

## PR Checklist

Before submitting a PR, ensure the following:

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` reports zero warnings
- [ ] `cargo fmt --all -- --check` passes
- [ ] Documentation is updated (if applicable)
- [ ] No hardcoded credentials, API keys, or personal info

## Commit Convention

This project follows [Conventional Commits](https://www.conventionalcommits.org/):

| Prefix | Purpose |
|--------|---------|
| `feat` | New feature |
| `fix` | Bug fix |
| `docs` | Documentation changes |
| `refactor` | Code refactoring (no feature change) |
| `test` | Adding or updating tests |
| `chore` | Maintenance tasks (CI, deps, etc.) |
| `perf` | Performance improvement |

### Examples

```
feat(compress): add wenyan classical-Chinese rule pack
fix(memory): escape FTS5 special characters in user queries
docs(architecture): describe the rule-protection pipeline
refactor(providers): collapse Anthropic + OpenAI shared headers
test(templates): cover python-uv post-init hook path
chore(ci): bump cargo-audit to 0.21
```

### No AI tool co-author trailers

Do **not** add `Co-authored-by:` trailers that name AI tools / coding agents. This applies to all of:

- `Co-authored-by: Claude <noreply@anthropic.com>` (and any other Anthropic email)
- `Co-authored-by: Cursor <cursoragent@cursor.com>`
- `Co-authored-by: Copilot <...>` (any GitHub Copilot variant)
- `Co-authored-by: <any other AI tool / agent identity>`

You wrote the patch — the human author of record is you. AI tooling doesn't get co-authorship credit in this repo regardless of how much it contributed. If you forgot and a trailer slipped in, we'll ask you to amend (or, for already-merged PRs, propose a coordinated history-rewrite via a follow-up PR).

Human co-authors (a colleague who pair-programmed with you on the change) are fine and welcome — those should use real human identities + emails.

## Writing release notes

Each version section in `CHANGELOG.md` (and `docs/CHANGELOG.ko.md`) starts with `### Highlights` — a one-sentence headline followed by 3–6 scannable bullets. This is what users see at the top of the GitHub release page: the release workflow extracts the version's section verbatim, so the first thing in the section is the first thing in the release body.

The detailed `### Added` / `### Changed` / `### Fixed` bullets follow underneath. They're for archeology and exhaustive tracking, not first-read.

Skeleton:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Highlights

**One-sentence headline.** Optional 1-2 sentence elaboration if needed.

- Most important user-visible change (one line, scannable)
- Second most important change
- (3-6 bullets max; no prose blocks)

### Added
- (detailed bullets)

### Changed
- (detailed bullets)

### Fixed
- (detailed bullets)
```

When cutting a release, push the `REL-vX.Y.Z` marker tag alongside the version tag — the release workflow keys off the `REL-` marker for body extraction.

```bash
git tag vX.Y.Z <commit>
git tag REL-vX.Y.Z vX.Y.Z^{}    # dereference to commit to avoid a nested-tag warning
git push origin vX.Y.Z REL-vX.Y.Z
```

### Crediting contributors in Highlights

When a Highlights bullet covers work that came from outside the maintainer (external PR or external bug report / feature request), credit the contributor inline:

| Source | Suffix |
|---|---|
| External PR (someone else's commits) | `(by @username, #PR)` |
| External issue / feature request (maintainer wrote the code) | `(reported by @username, #issue)` |
| Both — external report **and** external PR by the same person | `(by @username, #PR / #issue)` |

The "no AI tool co-author trailers" rule above is unrelated: it bans machine-generated attribution. Human contributors are credited liberally and explicitly.

## Security

If you discover a security vulnerability, please follow the process described in [SECURITY.md](SECURITY.md). **Do NOT open a public issue.**
