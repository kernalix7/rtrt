# Security Policy

**English** | [한국어](docs/SECURITY.ko.md)

## Supported Versions

| Version | Supported |
|---------|-----------|
| Latest (`0.1.x`) | Yes |
| Older | No |

## Reporting a Vulnerability

Please report security vulnerabilities through GitHub Security Advisories:

**[Report a vulnerability](https://github.com/kernalix7/rtrt/security/advisories/new)**

**Do NOT open a public issue for security vulnerabilities.**

### What to Include

- **Description**: A clear description of the vulnerability
- **Steps to Reproduce**: Detailed steps to reproduce the issue
- **Impact**: The potential impact of the vulnerability
- **Affected Components**: Which crate(s) or binaries are affected (e.g. `rtrt-mcp`, `rtrt-dashboard`)
- **Environment**:
  - Operating system and version
  - Rust toolchain version (`rustc --version`)
  - RTRT version (`rtrt --version`)
  - Provider(s) configured (if relevant)

## Response Timeline

| Step | Timeframe |
|------|-----------|
| Acknowledgment | Within 48 hours |
| Assessment | Within 7 days |
| Fix | Within 30 days |

## Scope

The following areas are considered in scope for security reports:

- **Command injection in `rtrt-proxy` / `rtrt-cli`**: untrusted command output or arguments routed through a shell.
- **Path traversal in `rtrt-templates`**: a template manifest that writes outside the user-specified target directory.
- **SQL injection in `rtrt-memory`**: untrusted strings flowing into FTS5 `MATCH` or other SQL clauses without parameter binding.
- **Credential exposure**: API keys, bearer tokens, or local memory contents leaked through logs, dashboard responses, or MCP tool calls.
- **Dashboard server-side request forgery**: an HTTP endpoint that makes outbound calls based on untrusted input.
- **Denial of service via the dashboard or MCP**: trivially-crashable endpoints or memory-blowup payloads.

## Out of Scope

The following are considered out of scope:

- Attacks requiring physical access to the host.
- Social engineering attacks.
- Vulnerabilities in third-party dependencies (report these to the upstream project; RTRT will track and bump when patched).
- Denial of service that requires `--bind 0.0.0.0` (the dashboard defaults to `127.0.0.1`).

## Security Best Practices

This project follows these security practices:

- **Parameterized SQL only**: `rtrt-memory` uses `rusqlite::params!`; no string concatenation into SQL.
- **Allowlist regexes at boundaries**: template names, command names, and other strings used as filesystem-path components are validated before use.
- **List-args subprocess only**: `rtrt-templates` post-init hooks invoke commands with an explicit argv list; `shell=true` is never used on guest-derived strings.
- **No secrets in code or git**: API keys, bearer tokens, and credentials are never committed. The default `.gitignore` denylists `.env`, `*.pem`, `*.key`, and AI-tool scratch files.
- **Local-bound by default**: `rtrt-dashboard` binds `127.0.0.1` and `rtrt-mcp` uses stdio; remote exposure is opt-in.
- **No telemetry**: RTRT collects no usage metrics. The "savings" stats shown on the dashboard are computed from local-only data.

## Provider Credentials

API keys for chat providers (Anthropic, OpenAI, OpenAI-compatible endpoints) are read from environment variables and never written to disk by RTRT itself. The dashboard does not expose credential-bearing endpoints.

If you observe a credential being logged, persisted, or surfaced in an API response — treat it as a security bug and follow the disclosure process above.

## Attribution

We appreciate responsible disclosure and will credit reporters in release notes (unless anonymity is preferred).
