#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
set -euo pipefail

###############################################################################
# rtrt uninstaller
#
# Interactive (asks before each step):
#   ./uninstall.sh
#
# Auto (removes agent wiring + service + binaries, keeps memory store +
# prompt registry under ~/.rtrt):
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh \
#       | bash -s -- --confirm
#
# Full purge (the above + ~/.rtrt + fastembed model caches):
#   curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh \
#       | bash -s -- --purge
#
# Flags:
#   --confirm           non-interactive: Claude Code wiring + service + binaries
#   --purge             non-interactive, also wipes ~/.rtrt + fastembed caches
#   --dir <path>        install dir (default: $HOME/.local/bin)
###############################################################################

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
log()  { printf '%b[rtrt]%b %s\n' "$GREEN" "$NC" "$*"; }
warn() { printf '%b[warn]%b %s\n' "$YELLOW" "$NC" "$*" >&2; }
err()  { printf '%b[error]%b %s\n' "$RED" "$NC" "$*" >&2; }

usage() {
    cat <<'EOF'
rtrt uninstaller

Interactive (asks before each step):
  ./uninstall.sh

Auto (removes agent wiring + service + binaries, keeps ~/.rtrt):
  curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --confirm

Full purge (the above + ~/.rtrt + fastembed model caches):
  curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --purge

Flags:
  --confirm           non-interactive: Claude Code wiring + service + binaries
  --purge             non-interactive, also wipes ~/.rtrt + fastembed caches
  --dir <path>        install dir (default: $HOME/.local/bin)
EOF
}

if [ -z "${HOME:-}" ]; then
    err 'HOME is not set — refusing to guess paths for removal.'
    exit 1
fi

INSTALL_DIR="${HOME}/.local/bin"
AUTO=false
PURGE=false
BINS=(rtrt rtrt-mcp rtrt-dashboard)

while [ $# -gt 0 ]; do
    case "$1" in
        --confirm) AUTO=true; shift ;;
        --purge)   PURGE=true; AUTO=true; shift ;;
        --dir)     INSTALL_DIR="${2:?--dir needs a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *)
            err "unknown arg: $1"
            exit 2 ;;
    esac
done

# Piped interactive runs cannot prompt (stdin is the script itself).
if [ "$AUTO" = false ] && [ ! -t 0 ]; then
    err "stdin is not a terminal — pass --confirm (keep ~/.rtrt) or --purge (wipe everything)."
    exit 2
fi

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

# Resolve the rtrt binary: prefer the install dir, fall back to $PATH so a
# custom --dir install can still be unwired.
rtrt_bin="$INSTALL_DIR/rtrt"
if [ ! -x "$rtrt_bin" ]; then
    rtrt_bin="$(command -v rtrt 2>/dev/null || true)"
fi

# Unwire the Claude Code integration first, while the rtrt binary still
# exists to do it — otherwise ~/.claude/settings.json keeps hooks + a
# statusline that point at deleted binaries and every session logs errors.
# Best-effort: no prior `rtrt setup` is fine.
claude_wired=false
if grep -q '"rtrt"' "$HOME/.claude.json" 2>/dev/null; then claude_wired=true; fi
if grep -q 'rtrt hook' "$HOME/.claude/settings.json" 2>/dev/null; then claude_wired=true; fi
if [ "$claude_wired" = true ]; then
    if [ -n "$rtrt_bin" ] && [ -x "$rtrt_bin" ]; then
        if ask "Remove the Claude Code integration (MCP server, hooks, statusline, skills)?"; then
            if "$rtrt_bin" uninstall --agent claude --plugin --apply >/dev/null 2>&1; then
                log "  Claude Code integration removed (~/.claude.json, ~/.claude/settings.json)"
                log "  restart Claude Code to drop the unloaded MCP server + hooks"
            else
                warn "  could not unwire Claude Code — run: rtrt uninstall --agent claude --plugin --apply"
            fi
        fi
    else
        warn "  Claude Code is wired to rtrt but no rtrt binary was found to unwire it."
        warn "  Reinstall rtrt and run: rtrt uninstall --agent claude --plugin --apply"
        warn "  (or hand-edit ~/.claude.json + ~/.claude/settings.json)"
    fi
fi

# Stop + remove the dashboard service. Best-effort: a missing service /
# binary is fine; when the binary is already gone, fall back to removing the
# unit / LaunchAgent files the installer created.
service_unit="$HOME/.config/systemd/user/rtrt-dashboard.service"
launch_agent="$HOME/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist"
if [ -e "$service_unit" ] || [ -e "$launch_agent" ] || [ -n "$rtrt_bin" ]; then
    if ask "Stop + remove the rtrt-dashboard service?"; then
        if [ -n "$rtrt_bin" ] && [ -x "$rtrt_bin" ] \
            && "$rtrt_bin" service uninstall --apply >/dev/null 2>&1; then
            log "  dashboard service removed"
        elif [ -e "$service_unit" ]; then
            systemctl --user disable --now rtrt-dashboard.service >/dev/null 2>&1 || true
            rm -f "$service_unit"
            systemctl --user daemon-reload >/dev/null 2>&1 || true
            log "  dashboard service removed (unit file cleanup)"
        elif [ -e "$launch_agent" ]; then
            launchctl unload -w "$launch_agent" >/dev/null 2>&1 || true
            rm -f "$launch_agent"
            log "  dashboard service removed (LaunchAgent cleanup)"
        else
            warn "  no dashboard service to remove (or systemd/launchd absent)"
        fi
    fi
fi

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
    warn "purge is irreversible — memory store, prompt registry, and model caches will be deleted."
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
