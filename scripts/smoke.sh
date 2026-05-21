#!/usr/bin/env bash
# Live-key smoke test for the RTRT toolchain.
#
# Runs the surfaces that need real API keys + a localhost dashboard +
# a localhost MCP server. Designed to be the gate before tagging the
# first release: every section that has the required credentials runs,
# every section that doesn't is reported as SKIP, and the script exits
# non-zero only if a section that *did* run failed.
#
# Usage:
#   scripts/smoke.sh                # auto-detect keys, skip what's missing
#   scripts/smoke.sh --keep         # leave dashboard / mcp running after exit
#   scripts/smoke.sh --dashboard-port 7311 --mcp-port 7312
#
# Required for the corresponding sections (each optional):
#   ANTHROPIC_API_KEY              → anthropic provider chat
#   OPENAI_API_KEY                 → openai provider chat (streaming)
#   OPENAI_COMPAT_BASE_URL + …MODEL → local Ollama / vLLM / LM Studio
#
# Always runs:
#   - rtrt --version
#   - rtrt compress / proxy / templates / new / repo-map
#   - rtrt-dashboard /healthz + /api/stats + /api/templates
#   - rtrt-mcp stdio handshake (--help; full handshake is left to MCP clients)
#
# Output: one line per check with PASS / FAIL / SKIP and a summary footer.

set -uo pipefail

DASHBOARD_PORT="${DASHBOARD_PORT:-7311}"
MCP_PORT="${MCP_PORT:-7312}"
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --keep)             KEEP=1 ;;
        --dashboard-port)   DASHBOARD_PORT="$2"; shift ;;
        --mcp-port)         MCP_PORT="$2"; shift ;;
        -h|--help)
            sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "smoke: unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

PASS=0
FAIL=0
SKIP=0
FAILED_NAMES=()

report() {
    local status="$1" name="$2" extra="${3:-}"
    case "$status" in
        PASS) PASS=$((PASS + 1));    printf '  \033[32mPASS\033[0m  %s%s\n' "$name" "${extra:+ — $extra}" ;;
        FAIL) FAIL=$((FAIL + 1));    FAILED_NAMES+=("$name"); printf '  \033[31mFAIL\033[0m  %s%s\n' "$name" "${extra:+ — $extra}" ;;
        SKIP) SKIP=$((SKIP + 1));    printf '  \033[33mSKIP\033[0m  %s%s\n' "$name" "${extra:+ — $extra}" ;;
    esac
}

run_check() {
    local name="$1"; shift
    local out
    if out=$("$@" 2>&1); then
        report PASS "$name"
    else
        report FAIL "$name" "$(printf '%s' "$out" | head -n 1)"
    fi
}

section() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

# ---------------------------------------------------------------------------
section "Build artefacts"
RTRT="${RTRT:-rtrt}"
DASHBOARD_BIN="${RTRT_DASHBOARD_BIN:-rtrt-dashboard}"
MCP_BIN="${RTRT_MCP_BIN:-rtrt-mcp}"

if ! command -v "$RTRT" >/dev/null 2>&1; then
    echo "smoke: '$RTRT' not on PATH; build the workspace first (cargo build --release)" >&2
    exit 2
fi

run_check "rtrt --version"          "$RTRT" --version
run_check "rtrt-dashboard --version" "$DASHBOARD_BIN" --version
run_check "rtrt-mcp --version"       "$MCP_BIN" --version

# ---------------------------------------------------------------------------
section "CLI surfaces (no keys required)"

if OUT=$(printf '%s' "the bug is really really bad and definitely happening now" \
        | "$RTRT" compress -l ultra 2>&1); then
    report PASS "rtrt compress -l ultra" "out=${#OUT}c"
else
    report FAIL "rtrt compress -l ultra" "$OUT"
fi

if OUT=$(printf '%s\n' "  M file.rs" "?? new.rs" "?? other.rs" \
        | "$RTRT" proxy "git status" 2>&1); then
    report PASS "rtrt proxy git status" "lines=$(printf '%s' "$OUT" | wc -l)"
else
    report FAIL "rtrt proxy git status" "$OUT"
fi

if OUT=$("$RTRT" templates 2>&1); then
    report PASS "rtrt templates"
else
    report FAIL "rtrt templates" "$OUT"
fi

TMP_PROJECT="$(mktemp -d)/hello-rtrt-smoke"
if OUT=$("$RTRT" new rust-cli "$TMP_PROJECT" --var project_name=hello-rtrt-smoke 2>&1); then
    report PASS "rtrt new rust-cli"
else
    report FAIL "rtrt new rust-cli" "$OUT"
fi

if OUT=$("$RTRT" repo-map crates/rtrt-memory 2>&1 | head -c 2048); then
    report PASS "rtrt repo-map crates/rtrt-memory" "head=${#OUT}c"
else
    report FAIL "rtrt repo-map" "$OUT"
fi

# ---------------------------------------------------------------------------
section "Provider chat — Anthropic"
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    if OUT=$("$RTRT" provider chat --model claude-haiku-4-5 "ping" 2>&1); then
        report PASS "anthropic chat" "${OUT:0:80}"
    else
        report FAIL "anthropic chat" "$OUT"
    fi
else
    report SKIP "anthropic chat" "ANTHROPIC_API_KEY unset"
fi

section "Provider chat — OpenAI"
if [ -n "${OPENAI_API_KEY:-}" ]; then
    if OUT=$("$RTRT" provider chat --model gpt-5.4-mini --stream "count to 3" 2>&1); then
        report PASS "openai stream chat" "${OUT:0:80}"
    else
        report FAIL "openai stream chat" "$OUT"
    fi
else
    report SKIP "openai stream chat" "OPENAI_API_KEY unset"
fi

section "Provider chat — OpenAI-compatible (local)"
if [ -n "${OPENAI_COMPAT_BASE_URL:-}" ] && [ -n "${OPENAI_COMPAT_MODEL:-}" ]; then
    if OUT=$("$RTRT" provider chat --model "$OPENAI_COMPAT_MODEL" "hi" 2>&1); then
        report PASS "openai-compat chat" "${OUT:0:80}"
    else
        report FAIL "openai-compat chat" "$OUT"
    fi
else
    report SKIP "openai-compat chat" "OPENAI_COMPAT_BASE_URL or _MODEL unset"
fi

# ---------------------------------------------------------------------------
section "Dashboard (loopback)"
DASHBOARD_PID=""
DASHBOARD_URL="http://127.0.0.1:${DASHBOARD_PORT}"
RTRT_DASHBOARD_BIND="127.0.0.1:${DASHBOARD_PORT}" "$DASHBOARD_BIN" >/tmp/rtrt-smoke-dashboard.log 2>&1 &
DASHBOARD_PID=$!
sleep 1
for _ in 1 2 3 4 5; do
    if curl -fsS "${DASHBOARD_URL}/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if curl -fsS "${DASHBOARD_URL}/healthz" 2>/dev/null | grep -q ok; then
    report PASS "dashboard /healthz"
    if OUT=$(curl -fsS "${DASHBOARD_URL}/api/templates" 2>&1); then
        report PASS "dashboard /api/templates" "len=${#OUT}c"
    else
        report FAIL "dashboard /api/templates" "$OUT"
    fi
    if OUT=$(curl -fsS "${DASHBOARD_URL}/api/stats" 2>&1); then
        report PASS "dashboard /api/stats" "len=${#OUT}c"
    else
        report FAIL "dashboard /api/stats" "$OUT"
    fi
else
    report FAIL "dashboard /healthz" "did not respond on ${DASHBOARD_URL}"
fi

if [ "$KEEP" -eq 0 ] && [ -n "$DASHBOARD_PID" ]; then
    kill "$DASHBOARD_PID" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
section "MCP (loopback HTTP)"
MCP_PID=""
MCP_TOKEN="$(openssl rand -hex 16 2>/dev/null || echo smoke-token-$$)"
RTRT_MEMORY_PATH="$(mktemp -t rtrt-smoke-XXXX.sqlite)"
RTRT_MCP_HTTP_TOKEN="$MCP_TOKEN" RTRT_MEMORY_PATH="$RTRT_MEMORY_PATH" \
    "$MCP_BIN" --transport http --bind "127.0.0.1:${MCP_PORT}" >/tmp/rtrt-smoke-mcp.log 2>&1 &
MCP_PID=$!
sleep 1
for _ in 1 2 3 4 5; do
    if curl -fsS -o /dev/null -H "Authorization: Bearer $MCP_TOKEN" \
        "http://127.0.0.1:${MCP_PORT}/mcp" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

# A 405 is fine here — `GET /mcp` is not the right verb but the server is up.
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $MCP_TOKEN" \
    "http://127.0.0.1:${MCP_PORT}/mcp" 2>/dev/null || echo 000)
if [ "$HTTP_CODE" != "000" ]; then
    report PASS "mcp http reachable" "status=$HTTP_CODE"
else
    report FAIL "mcp http reachable" "no response"
fi

HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' \
    "http://127.0.0.1:${MCP_PORT}/mcp" 2>/dev/null || echo 000)
if [ "$HTTP_CODE" = "401" ]; then
    report PASS "mcp bearer guard" "rejects missing token with 401"
else
    report FAIL "mcp bearer guard" "expected 401, got $HTTP_CODE"
fi

if [ "$KEEP" -eq 0 ] && [ -n "$MCP_PID" ]; then
    kill "$MCP_PID" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
section "Summary"
printf '  %d pass / %d fail / %d skip\n' "$PASS" "$FAIL" "$SKIP"
if [ "$FAIL" -gt 0 ]; then
    printf '\n  Failed:\n'
    for n in "${FAILED_NAMES[@]}"; do printf '    - %s\n' "$n"; done
    exit 1
fi
exit 0
