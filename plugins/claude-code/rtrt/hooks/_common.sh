#!/usr/bin/env bash
# Shared helpers — read the hook payload from stdin and forward to the rtrt
# memory store. Two write paths:
#
#   1. RTRT_BIN is set or the `rtrt` CLI is on PATH → spawn `rtrt memory save`.
#   2. RTRT_DASHBOARD_URL is set                    → POST to /api/memory/save.
#
# Both write paths are best-effort. A hook never blocks Claude Code on
# capture failure; we exit 0 even on error so the conversation continues.
set -eu

PROJECT="${RTRT_PROJECT:-$(basename "${PWD:-default}")}"
KIND="${1:-event}"
BODY=$(cat - 2>/dev/null || true)
[ -z "$BODY" ] && exit 0

# Default the store path to ~/.rtrt/memory.sqlite when the caller has not
# set one. Keeps every hook, the MCP server, and ad-hoc CLI invocations
# on the same SQLite file instead of dropping each call into a
# cwd-relative `.rtrt/memory.sqlite` next to whatever directory Claude
# Code happened to be in.
if [ -z "${RTRT_MEMORY_PATH:-}" ]; then
    RTRT_MEMORY_PATH="${HOME:-.}/.rtrt/memory.sqlite"
    export RTRT_MEMORY_PATH
fi

# Strip control sequences and clip to 4 KB so a noisy tool result doesn't
# bloat the row.
BODY=$(printf '%s' "$BODY" | tr -d '\000-\010\013\014\016-\037' | head -c 4096)

write_via_cli() {
    bin="${RTRT_BIN:-rtrt}"
    if command -v "$bin" >/dev/null 2>&1; then
        printf '%s' "$BODY" | "$bin" memory save --project "$PROJECT" --kind "$KIND" --meta "source=claude-code" >/dev/null 2>&1 || return 1
        return 0
    fi
    return 1
}

write_via_http() {
    url="${RTRT_DASHBOARD_URL:-}"
    [ -z "$url" ] && return 1
    payload=$(jq -nc \
        --arg p "$PROJECT" --arg k "$KIND" --arg b "$BODY" \
        '{project:$p, kind:$k, body:$b, metadata:{source:"claude-code"}}' 2>/dev/null) || return 1
    auth=""
    [ -n "${RTRT_DASHBOARD_TOKEN:-}" ] && auth="-H Authorization: Bearer $RTRT_DASHBOARD_TOKEN"
    curl -fsS -X POST "$url/api/memory/save" \
        -H "Content-Type: application/json" \
        $auth \
        --data "$payload" >/dev/null 2>&1 || return 1
    return 0
}

write_via_cli || write_via_http || true
exit 0
