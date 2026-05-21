#!/usr/bin/env sh
# RTRT installer — Linux / macOS / WSL.
#
# One-liner install (latest release):
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
#
# One-liner uninstall:
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh \
#       | bash -s -- --confirm        # binaries only
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh \
#       | bash -s -- --purge          # binaries + ~/.rtrt + caches
#
# Flags:
#   --version vX.Y.Z   pin a specific release (default: latest)
#   --main             ignore releases; clone main and `cargo build --release`
#   --dir <path>       install dir (default: $HOME/.local/bin)
#   --uninstall        compatibility shim — defers to uninstall.sh logic
#   --dry-run          print intended actions without writing anything

set -eu

REPO="kernalix7/rtrt"
BINS="rtrt rtrt-mcp rtrt-dashboard"

VERSION=""
USE_MAIN=0
INSTALL_DIR="${HOME}/.local/bin"
UNINSTALL=0
DRY_RUN=0

# ---------- arg parse ----------
while [ $# -gt 0 ]; do
    case "$1" in
    --version)    VERSION="${2:-}"; shift 2 ;;
    --main)       USE_MAIN=1; shift ;;
    --dir)        INSTALL_DIR="${2:-}"; shift 2 ;;
    --uninstall)  UNINSTALL=1; shift ;;
    --dry-run)    DRY_RUN=1; shift ;;
    -h|--help)
        sed -n '2,20p' "$0"; exit 0 ;;
    *)
        printf 'unknown arg: %s\n' "$1" >&2; exit 2 ;;
    esac
done

run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '[dry-run] %s\n' "$*"
    else
        eval "$@"
    fi
}

# ---------- uninstall path ----------
# Compatibility shim — the canonical uninstaller lives in uninstall.sh
# (interactive + --confirm + --purge modes). The branch below keeps
# `install.sh --uninstall` working for users who memorised it.
if [ "$UNINSTALL" -eq 1 ]; then
    printf '== rtrt uninstall (compat shim) ==\n'
    printf 'For an interactive / purge flow, use uninstall.sh instead:\n'
    printf '  curl -fsSL https://raw.githubusercontent.com/%s/main/uninstall.sh | bash -s -- --confirm\n\n' "$REPO"
    for bin in $BINS; do
        target="$INSTALL_DIR/$bin"
        if [ -f "$target" ]; then
            run "rm -f \"$target\""
            printf '  removed %s\n' "$target"
        else
            printf '  skip %s (not present)\n' "$target"
        fi
    done
    printf 'rtrt uninstalled. Local state under ~/.rtrt/ is untouched; remove manually if desired.\n'
    exit 0
fi

# ---------- arch + os detection ----------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
Linux*)    OS_TAG="unknown-linux-gnu" ;;
Darwin*)   OS_TAG="apple-darwin" ;;
MINGW*|MSYS*|CYGWIN*)
    printf 'Windows shell detected — use install.ps1 instead:\n' >&2
    printf '  iwr https://raw.githubusercontent.com/%s/main/install.ps1 | iex\n' "$REPO" >&2
    exit 2 ;;
*)
    printf 'unsupported OS: %s\n' "$OS" >&2; exit 2 ;;
esac

case "$ARCH" in
x86_64|amd64)    ARCH_TAG="x86_64" ;;
arm64|aarch64)   ARCH_TAG="aarch64" ;;
*)
    printf 'unsupported arch: %s\n' "$ARCH" >&2; exit 2 ;;
esac

TARGET_TRIPLE="${ARCH_TAG}-${OS_TAG}"
printf '== rtrt install ==\n'
printf '  target: %s\n' "$TARGET_TRIPLE"
printf '  prefix: %s\n' "$INSTALL_DIR"

install_check() {
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            printf '\nWARNING: %s is not on $PATH.\n' "$INSTALL_DIR"
            printf '  Add this to your shell rc:\n'
            printf '    export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
            ;;
    esac
    printf 'rtrt installed:\n'
    for bin in $BINS; do
        printf '  %s/%s\n' "$INSTALL_DIR" "$bin"
    done
    printf '\nNext:\n'
    printf '  rtrt --version\n'
    printf '  rtrt info\n'
    printf '  rtrt templates\n'
}

# ---------- source-build path ----------
if [ "$USE_MAIN" -eq 1 ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        printf 'cargo not found; install Rust (https://rustup.rs) and retry, or omit --main.\n' >&2
        exit 1
    fi
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT INT TERM
    printf '  source build from main into %s\n' "$WORK"
    run "git clone --depth 1 https://github.com/$REPO \"$WORK\""
    run "cd \"$WORK\" && cargo build --release --workspace"
    run "mkdir -p \"$INSTALL_DIR\""
    for bin in $BINS; do
        run "install -m 0755 \"$WORK/target/release/$bin\" \"$INSTALL_DIR/$bin\""
    done
    install_check
    exit 0
fi

# ---------- release tarball path ----------
need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "$1" >&2; exit 1
    fi
}
need_cmd curl
need_cmd tar
need_cmd uname

if [ -z "$VERSION" ]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -1)"
    if [ -z "$VERSION" ]; then
        printf '  no GitHub Release published yet — falling back to source build (--main).\n'
        printf '  Pass --version vX.Y.Z to pin a specific release once one is cut.\n\n'
        if ! command -v cargo >/dev/null 2>&1; then
            printf 'cargo not found; install Rust (https://rustup.rs) and retry, or wait for a tagged release.\n' >&2
            exit 1
        fi
        WORK="$(mktemp -d)"
        trap 'rm -rf "$WORK"' EXIT INT TERM
        printf '  source build from main into %s\n' "$WORK"
        run "git clone --depth 1 https://github.com/$REPO \"$WORK\""
        run "cd \"$WORK\" && cargo build --release --workspace"
        run "mkdir -p \"$INSTALL_DIR\""
        for bin in $BINS; do
            run "install -m 0755 \"$WORK/target/release/$bin\" \"$INSTALL_DIR/$bin\""
        done
        install_check
        exit 0
    fi
fi
printf '  version: %s\n' "$VERSION"

ASSET="rtrt-${VERSION#v}-${TARGET_TRIPLE}.tar.gz"
URL="https://github.com/$REPO/releases/download/${VERSION}/${ASSET}"
CHECKSUM_URL="${URL}.sha256"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

printf '  downloading %s\n' "$URL"
run "curl -fsSL -o \"$WORK/$ASSET\" \"$URL\""

if [ "$DRY_RUN" -eq 0 ]; then
    if EXPECTED="$(curl -fsSL "$CHECKSUM_URL" 2>/dev/null | awk '{print $1}' | head -1)"; then
        if [ -n "$EXPECTED" ]; then
            ACTUAL="$(sha256sum "$WORK/$ASSET" 2>/dev/null | awk '{print $1}')"
            if [ -z "$ACTUAL" ]; then
                ACTUAL="$(shasum -a 256 "$WORK/$ASSET" 2>/dev/null | awk '{print $1}')"
            fi
            if [ -n "$ACTUAL" ] && [ "$ACTUAL" != "$EXPECTED" ]; then
                printf 'checksum mismatch:\n  expected %s\n  actual   %s\n' "$EXPECTED" "$ACTUAL" >&2
                exit 1
            fi
            printf '  checksum: ok\n'
        else
            printf '  checksum: no SHA256 file at release; skipping verification\n'
        fi
    else
        printf '  checksum: SHA256 file not yet attached; skipping verification\n'
    fi
fi

run "tar -xzf \"$WORK/$ASSET\" -C \"$WORK\""
run "mkdir -p \"$INSTALL_DIR\""
for bin in $BINS; do
    src=""
    for candidate in "$WORK/$bin" "$WORK/${ASSET%.tar.gz}/$bin"; do
        if [ -f "$candidate" ]; then src="$candidate"; break; fi
    done
    if [ -z "$src" ]; then
        printf 'binary missing from tarball: %s\n' "$bin" >&2; exit 1
    fi
    run "install -m 0755 \"$src\" \"$INSTALL_DIR/$bin\""
done

install_check
