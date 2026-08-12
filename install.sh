#!/usr/bin/env sh
# SPDX-License-Identifier: MIT
# RTRT installer — Linux / macOS / WSL.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
#   ./install.sh [--main] [--ref TAG] [--source PATH] [--version vX.Y.Z]
#                [--dir PATH] [--skip-deps] [--dry-run] [--help]
#
# Version selection (default: latest GitHub release, falls back to --main):
#   --main              Build from git main HEAD. Same as --ref main.
#                       (env: RTRT_REF=main)
#   --ref TAG           Build from a specific tag / branch / commit.
#                       (env: RTRT_REF=<ref>)
#   --version vX.Y.Z    Pin a specific release tarball (skip source build).
#
# Local-path option (offline / air-gapped):
#   --source PATH       Build from a local copy instead of git clone.
#                       (env: RTRT_SOURCE)
#
# Install dir + toolchain:
#   --dir PATH          Install dir (default: $HOME/.local/bin).
#   --skip-deps         Skip the toolchain check. Fail early if cargo / git
#                       aren't already present.
#                       (env: RTRT_SKIP_DEPS=1)
#   --no-setup          Disable all agent setup: Claude refresh plus Linux
#                       OpenCode strict bootstrap/session migration.
#                       (env: RTRT_NO_SETUP=1)
#   --no-service        Don't install the rtrt-dashboard background service
#                       (systemd --user on Linux, launchd on macOS).
#                       (env: RTRT_NO_SERVICE=1)
#
# Compat shims:
#   --uninstall         Defers to uninstall.sh logic; deletes the three
#                       binaries from --dir without touching ~/.rtrt.
#   --dry-run           Print intended actions without writing anything.

set -eu
# pipefail is not guaranteed by POSIX sh; enable it where the shell supports
# it (bash, zsh, dash >= 0.5.12, POSIX 2024 shells) — hence the subshell probe.
# shellcheck disable=SC3040
if (set -o pipefail) 2>/dev/null; then set -o pipefail; fi

REPO="kernalix7/rtrt"
BINS="rtrt rtrt-mcp rtrt-dashboard"

if [ -z "${HOME:-}" ]; then
    printf '[error] HOME is not set — cannot pick an install dir.\n' >&2
    exit 1
fi

# Env-var fallbacks. Flags take precedence when both are set.
RTRT_REF="${RTRT_REF:-}"
RTRT_SOURCE="${RTRT_SOURCE:-}"
RTRT_SKIP_DEPS="${RTRT_SKIP_DEPS:-}"

VERSION=""
REF=""
SOURCE_PATH=""
INSTALL_DIR="${HOME}/.local/bin"
SKIP_DEPS=0
UNINSTALL=0
DRY_RUN=0
# Auto agent setup: refresh prior Claude wiring and securely bootstrap an
# eligible Linux OpenCode installation without authorizing installer cwd.
# `--no-setup` / RTRT_NO_SETUP=1 disables.
case "${RTRT_NO_SETUP:-0}" in 1) NO_SETUP=1 ;; *) NO_SETUP=0 ;; esac
# Auto-start: install rtrt-dashboard as a background OS service so it runs
# without `rtrt-dashboard` being launched by hand. `--no-service` /
# RTRT_NO_SERVICE=1 disables. Skipped on platforms without systemd/launchd.
case "${RTRT_NO_SERVICE:-0}" in 1) NO_SERVICE=1 ;; *) NO_SERVICE=0 ;; esac

# ---------- colour logger ----------
if [ -t 1 ]; then
    C_RED=$(printf '\033[0;31m'); C_GREEN=$(printf '\033[0;32m')
    C_YELLOW=$(printf '\033[1;33m'); C_RESET=$(printf '\033[0m')
else
    C_RED=""; C_GREEN=""; C_YELLOW=""; C_RESET=""
fi
log()  { printf '%s[rtrt]%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
err()  { printf '%s[error]%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; }

# Self-contained usage text — `sed "$0"` breaks under `curl | sh` where $0 is
# the shell binary, so the flag summary is duplicated here instead.
usage() {
    cat <<'EOF'
RTRT installer — Linux / macOS / WSL.

Usage:
  curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
  ./install.sh [--main] [--ref TAG] [--source PATH] [--version vX.Y.Z]
               [--dir PATH] [--skip-deps] [--no-setup] [--no-service]
               [--uninstall] [--dry-run] [--help]

Version selection (default: latest GitHub release, falls back to --main):
  --main              Build from git main HEAD. Same as --ref main. (env: RTRT_REF=main)
  --ref TAG           Build from a tag / branch / commit. (env: RTRT_REF=<ref>)
  --version vX.Y.Z    Pin a specific release tarball (skip source build).

Local-path option (offline / air-gapped):
  --source PATH       Build from a local copy instead of git clone. (env: RTRT_SOURCE)

Install dir + toolchain:
  --dir PATH          Install dir (default: $HOME/.local/bin).
  --skip-deps         Skip the toolchain check. (env: RTRT_SKIP_DEPS=1)
  --no-setup          Disable Claude refresh and Linux OpenCode bootstrap/session migration. (env: RTRT_NO_SETUP=1)
  --no-service        Don't install the rtrt-dashboard background service. (env: RTRT_NO_SERVICE=1)

Compat shims:
  --uninstall         Deletes the three binaries from --dir; use uninstall.sh for the full flow.
  --dry-run           Print intended actions without writing anything.
EOF
}

# ---------- arg parse ----------
while [ $# -gt 0 ]; do
    case "$1" in
    --main|--dev)    REF="main"; shift ;;
    --ref)           REF="${2:?--ref needs a value}"; shift 2 ;;
    --source)        SOURCE_PATH="${2:?--source needs a path}"; shift 2 ;;
    --version)       VERSION="${2:?--version needs a value}"; shift 2 ;;
    --dir)           INSTALL_DIR="${2:?--dir needs a path}"; shift 2 ;;
    --skip-deps)     SKIP_DEPS=1; shift ;;
    --uninstall)     UNINSTALL=1; shift ;;
    --no-setup)      NO_SETUP=1; shift ;;
    --no-service)    NO_SERVICE=1; shift ;;
    --dry-run)       DRY_RUN=1; shift ;;
    -h|--help)       usage; exit 0 ;;
    *)
        err "unknown arg: $1"; exit 2 ;;
    esac
done

# Apply env-var fallbacks (flags above already took precedence).
[ -z "$REF" ] && REF="$RTRT_REF"
[ -z "$SOURCE_PATH" ] && SOURCE_PATH="$RTRT_SOURCE"
[ "$SKIP_DEPS" -eq 0 ] && [ -n "$RTRT_SKIP_DEPS" ] && SKIP_DEPS=1

run() {
    action="$1"
    shift
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '[dry-run] %s\n' "$action"
    else
        "$@"
    fi
}

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "required command not found: $1"
        exit 1
    fi
}

# ---------- uninstall path ----------
# Compatibility shim — the canonical uninstaller lives in uninstall.sh
# (interactive + --confirm + --purge modes). The branch below keeps
# `install.sh --uninstall` working for users who memorised it.
if [ "$UNINSTALL" -eq 1 ]; then
    log "== rtrt uninstall (compat shim) =="
    log "For an interactive / purge flow, use uninstall.sh instead:"
    log "  curl -fsSL https://raw.githubusercontent.com/$REPO/main/uninstall.sh | bash -s -- --confirm"
    echo
    rtrt_bin="$INSTALL_DIR/rtrt"
    registry="$HOME/.config/opencode/.rtrt-sandbox-state.json"
    managed_shell_without_registry=0
    if [ ! -e "$registry" ]; then
        for config in "$HOME/.config/opencode/opencode.json" "$HOME/.config/opencode/opencode.jsonc"; do
            if [ -f "$config" ] \
                && { grep -F "\"shell\":\"$rtrt_bin\"" "$config" >/dev/null 2>&1 \
                    || grep -F "\"shell\": \"$rtrt_bin\"" "$config" >/dev/null 2>&1; }; then
                managed_shell_without_registry=1
            fi
        done
    fi
    if [ "$managed_shell_without_registry" -eq 1 ]; then
        err "OpenCode shell points to $rtrt_bin but ownership registry is missing; refusing deletion. Restore the shell entry manually first."
        exit 1
    fi
    if [ -e "$registry" ]; then
        if [ ! -x "$rtrt_bin" ]; then
            err "OpenCode sandbox is managed but $rtrt_bin is unavailable; refusing to delete binaries."
            exit 1
        fi
        if [ "$DRY_RUN" -eq 1 ]; then
            printf '[dry-run] %s uninstall --agent opencode --apply\n' "$rtrt_bin"
        elif ! "$rtrt_bin" uninstall --agent opencode --apply; then
            err "could not restore OpenCode shell; binaries retained. Run: $rtrt_bin uninstall --agent opencode --apply"
            exit 1
        fi
    fi
    for bin in $BINS; do
        target="$INSTALL_DIR/$bin"
        if [ -f "$target" ]; then
            run "remove $target" rm -f "$target"
            log "  removed $target"
        else
            warn "  skip $target (not present)"
        fi
    done
    log "rtrt uninstalled. Local state under ~/.rtrt/ is untouched."
    exit 0
fi

# ---------- arch + os detection ----------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
Linux*)    OS_TAG="unknown-linux-gnu" ;;
Darwin*)   OS_TAG="apple-darwin" ;;
MINGW*|MSYS*|CYGWIN*)
    err "Windows shell detected — use install.ps1 instead:"
    err "  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex"
    exit 2 ;;
*)
    err "unsupported OS: $OS"; exit 2 ;;
esac

case "$ARCH" in
x86_64|amd64)    ARCH_TAG="x86_64" ;;
arm64|aarch64)   ARCH_TAG="aarch64" ;;
*)
    err "unsupported arch: $ARCH"; exit 2 ;;
esac

TARGET_TRIPLE="${ARCH_TAG}-${OS_TAG}"
log "== rtrt install =="
log "  target: $TARGET_TRIPLE"
log "  prefix: $INSTALL_DIR"

install_check() {
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            echo
            warn "$INSTALL_DIR is not on \$PATH."
            warn "  Add to your shell rc:"
            warn "    export PATH=\"$INSTALL_DIR:\$PATH\""
            echo
            ;;
    esac
    log "rtrt installed:"
    for bin in $BINS; do
        log "  $INSTALL_DIR/$bin"
    done
    reconfigure_if_present
    bootstrap_opencode_if_eligible
    install_dashboard_service
    echo
    log "Next:"
    log "  rtrt --version"
    log "  rtrt info"
    log "  rtrt templates"
}

# Passive eligibility only. Rust performs authoritative ownership, mode,
# canonical-path, config, backend-usability, and registry checks.
safe_passive_executable() {
    candidate="$1"
    case "$candidate" in
        /*) ;;
        *) return 1 ;;
    esac
    case "$candidate" in
        *'
'*) return 1 ;;
    esac
    [ -f "$candidate" ] && [ ! -L "$candidate" ] && [ -x "$candidate" ] || return 1
    candidate_uid="$(stat -c %u "$candidate" 2>/dev/null || printf invalid)"
    { [ "$candidate_uid" = 0 ] || [ "$candidate_uid" = "$(id -u)" ]; } || return 1
    mode="$(stat -c %a "$candidate" 2>/dev/null || printf invalid)"
    case "$mode" in *[2367][0-7]|*[0-7][2367]|invalid) return 1 ;; esac

    ancestor="${candidate%/*}"
    [ -n "$ancestor" ] || ancestor=/
    while :; do
        [ -d "$ancestor" ] && [ ! -L "$ancestor" ] || return 1
        ancestor_uid="$(stat -c %u "$ancestor" 2>/dev/null || printf invalid)"
        { [ "$ancestor_uid" = 0 ] || [ "$ancestor_uid" = "$(id -u)" ]; } || return 1
        mode="$(stat -c %a "$ancestor" 2>/dev/null || printf invalid)"
        case "$mode" in *[2367][0-7]|*[0-7][2367]|invalid) return 1 ;; esac
        [ "$ancestor" = / ] && break
        ancestor="${ancestor%/*}"
        [ -n "$ancestor" ] || ancestor=/
    done
}

bootstrap_opencode_if_eligible() {
    [ "$NO_SETUP" -eq 1 ] && return 0
    [ "$DRY_RUN" -eq 1 ] && return 0
    [ "$(uname -s)" = "Linux" ] || return 0
    [ "$(id -u)" -ne 0 ] || { warn "skipping OpenCode bootstrap for root/sudo install"; return 0; }
    if [ ! -d "$HOME" ] || [ -L "$HOME" ]; then
        warn "skipping OpenCode bootstrap: HOME is not a real directory"
        return 0
    fi
    home_uid="$(stat -c %u "$HOME" 2>/dev/null || printf invalid)"
    [ "$home_uid" = "$(id -u)" ] || { warn "skipping OpenCode bootstrap: HOME is not owned by invoking user"; return 0; }
    evidence=0
    [ -f "$HOME/.config/opencode/opencode.json" ] && evidence=1
    [ -f "$HOME/.config/opencode/opencode.jsonc" ] && evidence=1
    [ -f "$HOME/.config/opencode/.rtrt-sandbox-state.json" ] && evidence=1
    if [ "$evidence" -eq 0 ]; then
        opencode_evidence="$(command -v opencode 2>/dev/null || true)"
        safe_passive_executable "$opencode_evidence" || return 0
    fi
    bwrap_evidence=""
    for candidate in /usr/bin/bwrap /bin/bwrap; do
        mode="$(stat -c %a "$candidate" 2>/dev/null || printf invalid)"
        case "$mode" in *[2367][0-7]|*[0-7][2367]|invalid) safe_mode=0 ;; *) safe_mode=1 ;; esac
        if [ -f "$candidate" ] && [ ! -L "$candidate" ] && [ -x "$candidate" ] \
            && [ "$(stat -c %u "$candidate" 2>/dev/null || printf invalid)" = 0 ] \
            && [ "$safe_mode" -eq 1 ]; then
            bwrap_evidence="$candidate"; break
        fi
    done
    [ -n "$bwrap_evidence" ] || return 0
    rtrt_bin="$INSTALL_DIR/rtrt"
    echo
    log "eligible OpenCode installation detected — applying strict machine bootstrap"
    if ! "$rtrt_bin" setup --agent opencode --sandbox --machine-only --apply; then
        warn "OpenCode bootstrap failed; RTRT binaries remain installed. Run: $rtrt_bin setup --agent opencode --sandbox --machine-only --apply"
        return 0
    fi
    if "$rtrt_bin" opencode sessions apply; then
        log "  existing global OpenCode sessions migrated to project-private stores"
    else
        warn "OpenCode session migration failed; source database was retained. Run: $rtrt_bin opencode sessions apply"
    fi
}

# Install rtrt-dashboard as a background OS service (systemd --user on Linux,
# launchd on macOS) so it auto-starts on login and restarts on crash — no need
# to run `rtrt-dashboard` by hand. Default-on; `--no-service` /
# RTRT_NO_SERVICE=1 disables. Best-effort: a failure here never fails the
# install (the user can still run `rtrt-dashboard` manually).
install_dashboard_service() {
    [ "$NO_SERVICE" -eq 1 ] && return 0
    [ "$DRY_RUN" -eq 1 ] && return 0
    rtrt_bin="$INSTALL_DIR/rtrt"
    [ -x "$rtrt_bin" ] || return 0
    # Only Linux (systemd) + macOS (launchd) are wired in `rtrt service`.
    case "$(uname -s)" in
        Linux|Darwin) ;;
        *) return 0 ;;
    esac
    echo
    log "installing rtrt-dashboard background service"
    if "$rtrt_bin" service install --apply; then
        log "  dashboard service started — http://127.0.0.1:7311"
        log "  stop/remove: rtrt service uninstall --apply"
    else
        warn "  service install failed; binaries remain installed. Retry: $rtrt_bin service install --apply"
    fi
}

# When a prior `rtrt setup --agent claude` is detected, refresh the MCP
# config + hooks against the just-installed binary. This keeps the
# wiring in lockstep with binary upgrades so the user never has to run
# uninstall / setup by hand after an install. Skipped on --no-setup, in
# --dry-run, and when no prior Claude setup is found.
reconfigure_if_present() {
    [ "$NO_SETUP" -eq 1 ] && return 0
    [ "$DRY_RUN" -eq 1 ] && return 0
    rtrt_bin="$INSTALL_DIR/rtrt"
    [ -x "$rtrt_bin" ] || return 0
    claude_json="${HOME}/.claude.json"
    settings_json="${HOME}/.claude/settings.json"
    found=0
    if [ -f "$claude_json" ] && grep -q '"rtrt"' "$claude_json" 2>/dev/null; then
        found=1
    fi
    if [ -f "$settings_json" ] && grep -q 'rtrt hook' "$settings_json" 2>/dev/null; then
        found=1
    fi
    [ "$found" -eq 0 ] && return 0
    echo
    log "existing Claude Code setup detected — refreshing against new binary"
    "$rtrt_bin" uninstall --agent claude --plugin --apply >/dev/null 2>&1 || true
    if "$rtrt_bin" setup --agent claude --plugin --apply >/dev/null 2>&1; then
        log "  re-applied MCP config + hooks (~/.claude.json, ~/.claude/settings.json)"
    else
        warn "  setup refresh failed — run: rtrt setup --agent claude --plugin --apply"
    fi
    warn "  restart Claude Code to load the refreshed MCP server + hooks"
}

# ---------- source build helper ----------
build_from_source() {
    src_dir="$1"
    if [ "$SKIP_DEPS" -eq 0 ]; then
        need_cmd cargo
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '[dry-run] cargo build --release --workspace (in %s)\n' "$src_dir"
    else
        (cd "$src_dir" && cargo build --release --workspace)
    fi
    run "mkdir $INSTALL_DIR" mkdir -p "$INSTALL_DIR"
    for bin in $BINS; do
        run "install $bin" install -m 0755 "$src_dir/target/release/$bin" "$INSTALL_DIR/$bin"
    done
    install_check
}

# ---------- --source PATH (local copy) ----------
if [ -n "$SOURCE_PATH" ]; then
    if [ ! -d "$SOURCE_PATH" ]; then
        err "--source path is not a directory: $SOURCE_PATH"
        exit 1
    fi
    log "  source: $SOURCE_PATH (local)"
    build_from_source "$SOURCE_PATH"
    exit 0
fi

# ---------- --ref / --main (git clone) ----------
if [ -n "$REF" ]; then
    if [ "$SKIP_DEPS" -eq 0 ]; then
        need_cmd git
        need_cmd cargo
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        WORK="${TMPDIR:-/tmp}/rtrt-install-dry-run"
    else
        WORK="$(mktemp -d)"
        trap 'rm -rf "$WORK"' EXIT INT TERM
    fi
    log "  ref: $REF (source build into $WORK)"
    # Group the fallback: a plain `a || b && c` would run the checkout even
    # after a successful shallow clone.
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '[dry-run] clone ref %s into %s\n' "$REF" "$WORK"
    elif ! git clone --depth 1 --branch "$REF" "https://github.com/$REPO" "$WORK" 2>/dev/null; then
        git clone "https://github.com/$REPO" "$WORK"
        git -C "$WORK" checkout "$REF"
    fi
    build_from_source "$WORK"
    exit 0
fi

# ---------- release tarball path ----------
if [ "$SKIP_DEPS" -eq 0 ]; then
    need_cmd curl
    need_cmd tar
    need_cmd uname
fi

if [ -z "$VERSION" ]; then
    # A 404 (zero releases yet) or offline curl must not abort under
    # set -e / pipefail — an empty VERSION is the fallback signal.
    VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -1)" || VERSION=""
    if [ -z "$VERSION" ]; then
        warn "no GitHub Release published yet — falling back to source build from main."
        warn "Pass --version vX.Y.Z to pin a release once one is cut, or --ref BRANCH to track a different branch."
        echo
        REF="main"
        if [ "$SKIP_DEPS" -eq 0 ]; then
            need_cmd git
            need_cmd cargo
        fi
        if [ "$DRY_RUN" -eq 1 ]; then
            WORK="${TMPDIR:-/tmp}/rtrt-install-dry-run"
        else
            WORK="$(mktemp -d)"
            trap 'rm -rf "$WORK"' EXIT INT TERM
        fi
        log "  ref: main (auto-fallback into $WORK)"
        run "clone main into $WORK" git clone --depth 1 "https://github.com/$REPO" "$WORK"
        build_from_source "$WORK"
        exit 0
    fi
fi
log "  version: $VERSION"

ASSET="rtrt-${VERSION#v}-${TARGET_TRIPLE}.tar.gz"
URL="https://github.com/$REPO/releases/download/${VERSION}/${ASSET}"
CHECKSUM_URL="${URL}.sha256"

if [ "$DRY_RUN" -eq 1 ]; then
    WORK="${TMPDIR:-/tmp}/rtrt-install-dry-run"
else
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT INT TERM
fi

log "  downloading $URL"
run "download $ASSET" curl -fsSL -o "$WORK/$ASSET" "$URL"

if [ "$DRY_RUN" -eq 0 ]; then
    # A missing .sha256 asset (`curl -f` fails) must surface as an empty
    # EXPECTED — not abort the install under set -e / pipefail.
    EXPECTED="$(curl -fsSL "$CHECKSUM_URL" 2>/dev/null | awk '{print $1}' | head -1)" || EXPECTED=""
    if [ -n "$EXPECTED" ]; then
        ACTUAL="$(sha256sum "$WORK/$ASSET" 2>/dev/null | awk '{print $1}')" || ACTUAL=""
        if [ -z "$ACTUAL" ]; then
            ACTUAL="$(shasum -a 256 "$WORK/$ASSET" 2>/dev/null | awk '{print $1}')" || ACTUAL=""
        fi
        if [ -z "$ACTUAL" ]; then
            err "a SHA256 checksum is published for $ASSET but neither sha256sum nor shasum is available to verify it."
            err "Install coreutils (sha256sum) or perl (shasum), or verify manually against:"
            err "  $CHECKSUM_URL"
            exit 1
        fi
        if [ "$ACTUAL" != "$EXPECTED" ]; then
            err "checksum mismatch:"
            err "  expected $EXPECTED"
            err "  actual   $ACTUAL"
            exit 1
        fi
        log "  checksum: ok"
    else
        warn "  checksum: no SHA256 file attached to the release; skipping verification"
    fi
fi

run "extract $ASSET" tar -xzf "$WORK/$ASSET" -C "$WORK"
run "mkdir $INSTALL_DIR" mkdir -p "$INSTALL_DIR"
for bin in $BINS; do
    if [ "$DRY_RUN" -eq 1 ]; then
        # Nothing was downloaded in dry-run mode, so skip the existence probe.
        run "install $bin" install -m 0755 "$WORK/$bin" "$INSTALL_DIR/$bin"
        continue
    fi
    src=""
    for candidate in "$WORK/$bin" "$WORK/${ASSET%.tar.gz}/$bin"; do
        if [ -f "$candidate" ]; then src="$candidate"; break; fi
    done
    if [ -z "$src" ]; then
        err "binary missing from tarball: $bin"; exit 1
    fi
    run "install $bin" install -m 0755 "$src" "$INSTALL_DIR/$bin"
done

install_check
