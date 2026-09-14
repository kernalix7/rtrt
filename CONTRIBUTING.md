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

Do **not** add co-author trailers that name AI tools or coding agents. Generator-credit
trailers and prose attribution to an AI tool are prohibited as well.

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

### npm trusted publishing

`rtrt-agent` ships to npm exclusively through npm trusted publishing (OIDC). The release workflow accepts no long-lived npm token. Workspace crates are not published to crates.io, so no `CARGO_REGISTRY_TOKEN` is required either.

The npm package's trusted publisher must match the repository workflow exactly. Configure it on npmjs.com under the `rtrt-agent` package settings with these fields:

| Publisher field | Value |
|---|---|
| Publisher | GitHub Actions |
| Organization or user | `kernalix7` |
| Repository | `rtrt` |
| Workflow filename | `release.yml` |
| Environment name | `npm-publish` |
| Allowed actions | Enable direct `npm publish` |

What the repository side guarantees, and what tests and preflight checks can verify from the tree:

- The publish job runs on GitHub-hosted `ubuntu-latest` (self-hosted runners aren't accepted by npm's OIDC exchange).
- The job declares `id-token: write` plus `contents: read`, and the workflow-level default is `contents: read`.
- Node `>=22.14.0` (currently 24) and npm `>=11.5.1` (currently 11.11.0), the minimums npm requires for OIDC publishing.
- `actions/setup-node` sets `registry-url` so npm targets `https://registry.npmjs.org`.
- The publish job references or injects no `NPM_TOKEN`, `NODE_AUTH_TOKEN`, or `CARGO_REGISTRY_TOKEN`. If one appears there, the run is misconfigured; don't add a token fallback.
- Publication runs `npm publish ... --provenance`, so every release carries a SLSA provenance attestation linked to the workflow run.

What the repository side can't see: none of the checks above can inspect npm account settings. Before every release, open the package's trusted publisher configuration on npmjs.com and confirm each field in the table. Treat a publish failure with an OIDC or `E404`/`E403` message as an account-side mismatch first. npm doesn't let you edit the allowed action on an existing publisher; if the allowed action needs to change, delete the publisher and recreate it with the values above.

### Cutting a release

The release uses two tags on the same merged `main` commit. Push them together in one atomic push; the release workflow extracts the version body using the `REL-` marker.

- `vX.Y.Z` triggers the `release.yml` validate-and-build job. It produces the per-platform Rust binaries and publishes them only as Actions artifacts; no GitHub Release is created on this run.
- `REL-vX.Y.Z` re-runs the build, then runs the npm publish job (trusted publishing pushes `rtrt-agent` to npm), then creates/updates the GitHub Release under `vX.Y.Z` and attaches the five per-platform binary archives plus their checksums. GitHub auto-generates the source archive from the `vX.Y.Z` tag.

```bash
GIT_MASTER=1 git checkout main
GIT_MASTER=1 git pull --ff-only origin main
GIT_MASTER=1 git tag vX.Y.Z HEAD
GIT_MASTER=1 git tag REL-vX.Y.Z HEAD
GIT_MASTER=1 git push --atomic origin vX.Y.Z REL-vX.Y.Z
```

### Recovering a failed release run

If a tag run fails after the tags are already pushed (for example, the trusted publisher was misconfigured), don't move or re-push the tags and don't add a token fallback. Fix the account-side setting, then re-run the workflow by hand:

1. Open **Actions → Release → Run workflow** on the `main` branch.
2. Set `release_tag` to the existing paired tag. `REL-vX.Y.Z` rebuilds, publishes to npm, and creates/updates the GitHub Release; `vX.Y.Z` only rebuilds and uploads Actions artifacts.
3. The dispatched run checks out that tag's commit, so the tag must already exist on `origin` and both tags must still point at the same commit.

The manual run uses the same `npm-publish` environment and OIDC exchange as a tag push, so the publisher table above applies unchanged.

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
