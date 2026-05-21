#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
set -euo pipefail

###############################################################################
# rtrt uninstaller
#
# Interactive (asks before each step):
#   ./uninstall.sh
#
# Auto (removes binaries, keeps memory store + prompt registry):
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh \
#       | bash -s -- --confirm
#
# Full purge (binaries + ~/.rtrt + ~/.local/share/fastembed cache):
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh \
#       | bash -s -- --purge
#
# Flags:
#   --confirm           non-interactive, removes the three binaries only
#   --purge             non-interactive, also wipes ~/.rtrt + ~/.cache/fastembed
#   --dir <path>        install dir (default: $HOME/.local/bin)
###############################################################################

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
log()  { printf '%b[rtrt]%b %s\n' "$GREEN" "$NC" "$*"; }
warn() { printf '%b[warn]%b %s\n' "$YELLOW" "$NC" "$*" >&2; }
err()  { printf '%b[error]%b %s\n' "$RED" "$NC" "$*" >&2; }

INSTALL_DIR="${HOME}/.local/bin"
AUTO=false
PURGE=false
BINS=(rtrt rtrt-mcp rtrt-dashboard)

while [ $# -gt 0 ]; do
    case "$1" in
        --confirm) AUTO=true; shift ;;
        --purge)   PURGE=true; AUTO=true; shift ;;
        --dir)     INSTALL_DIR="${2:?--dir needs a path}"; shift 2 ;;
        -h|--help) sed -n '5,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *)
            err "unknown arg: $1"
            exit 2 ;;
    esac
done

ask() {
    if [ "$AUTO" = true ]; then return 0; fi
    printf '  %s (y/N): ' "$1"
    read -r answer
    case "$answer" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

echo
echo "=========================================="
echo " rtrt uninstaller"
echo "=========================================="
if [ "$PURGE" = true ]; then
    log "Mode: FULL PURGE (binaries + ~/.rtrt + fastembed cache)"
else
    log "Mode: BINARIES ONLY (use --purge for full wipe)"
fi
echo

if ask "Remove binaries from $INSTALL_DIR?"; then
    for bin in "${BINS[@]}"; do
        target="$INSTALL_DIR/$bin"
        if [ -f "$target" ]; then
            rm -f "$target"
            log "  removed $target"
        else
            warn "  skip $target (not present)"
        fi
    done
else
    warn "  skipped binary removal"
fi

if [ "$PURGE" = true ]; then
    for dir in "$HOME/.rtrt" "$HOME/.cache/fastembed" "$HOME/.local/share/fastembed"; do
        if [ -d "$dir" ]; then
            if ask "Wipe $dir?"; then
                rm -rf "$dir"
                log "  wiped $dir"
            fi
        fi
    done
else
    log "Local state at ~/.rtrt/ left intact. Use --purge to wipe."
fi

echo
log "rtrt uninstalled."
