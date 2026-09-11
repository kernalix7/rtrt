#!/usr/bin/env bash
# Live-key pre-tag smoke test. Credential-dependent checks are skipped when the
# corresponding environment variable is absent; local CLI/server checks always run.
set -uo pipefail

DASHBOARD_PORT=${DASHBOARD_PORT:-7311}
MCP_PORT=${MCP_PORT:-7312}
KEEP=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1 ;;
        --dashboard-port) [ "$#" -ge 2 ] || exit 2; DASHBOARD_PORT=$2; shift ;;
        --mcp-port) [ "$#" -ge 2 ] || exit 2; MCP_PORT=$2; shift ;;
        -h|--help)
            printf 'usage: scripts/smoke.sh [--keep] [--dashboard-port PORT] [--mcp-port PORT]\n'
            exit 0
            ;;
        *) printf 'smoke: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
    shift
done

case "$DASHBOARD_PORT" in ''|*[!0-9]*) echo 'smoke: ports must be non-empty decimal integers' >&2; exit 2 ;; esac
case "$MCP_PORT" in ''|*[!0-9]*) echo 'smoke: ports must be non-empty decimal integers' >&2; exit 2 ;; esac

PASS=0
FAIL=0
SKIP=0
FAILED_NAMES=()
DASHBOARD_PID=
MCP_PID=
TMP_ROOT=$(mktemp -d)

cleanup() {
    if [ "$KEEP" -eq 0 ]; then
        [ -z "$DASHBOARD_PID" ] || kill "$DASHBOARD_PID" 2>/dev/null || true
        [ -z "$MCP_PID" ] || kill "$MCP_PID" 2>/dev/null || true
        rm -rf "$TMP_ROOT"
    else
        printf 'smoke: retained temporary state at %s\n' "$TMP_ROOT"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

report() {
    local status=$1 name=$2 extra=${3:-}
    case "$status" in
        PASS) PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m  %s%s\n' "$name" "${extra:+ — $extra}" ;;
        FAIL) FAIL=$((FAIL + 1)); FAILED_NAMES+=("$name"); printf '  \033[31mFAIL\033[0m  %s%s\n' "$name" "${extra:+ — $extra}" ;;
        SKIP) SKIP=$((SKIP + 1)); printf '  \033[33mSKIP\033[0m  %s%s\n' "$name" "${extra:+ — $extra}" ;;
    esac
}

run_check() {
    local name=$1 out first_line
    shift
    if out=$("$@" 2>&1); then
        report PASS "$name"
    else
        first_line=${out%%$'\n'*}
        report FAIL "$name" "$first_line"
    fi
}

section() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

RTRT=${RTRT:-rtrt}
DASHBOARD_BIN=${RTRT_DASHBOARD_BIN:-rtrt-dashboard}
MCP_BIN=${RTRT_MCP_BIN:-rtrt-mcp}
for binary in "$RTRT" "$DASHBOARD_BIN" "$MCP_BIN"; do
    command -v "$binary" >/dev/null 2>&1 || {
        printf "smoke: '%s' is not executable from PATH\n" "$binary" >&2
        exit 2
    }
done

section 'Build artifacts'
run_check 'rtrt --version' "$RTRT" --version
run_check 'rtrt-mcp --version' "$MCP_BIN" --version

section 'CLI surfaces (no keys required)'
if OUT=$(printf '%s' 'the bug is really really bad and definitely happening now' | "$RTRT" compress -l ultra 2>&1); then
    report PASS 'rtrt compress -l ultra' "out=${#OUT}c"
else
    report FAIL 'rtrt compress -l ultra' "$OUT"
fi
if OUT=$(printf '%s\n' '  M file.rs' '?? new.rs' '?? other.rs' | "$RTRT" proxy 'git status' 2>&1); then
    report PASS 'rtrt proxy git status' "lines=$(printf '%s' "$OUT" | wc -l)"
else
    report FAIL 'rtrt proxy git status' "$OUT"
fi
run_check 'rtrt templates' "$RTRT" templates
TMP_PROJECT="$TMP_ROOT/hello-rtrt-smoke"
if OUT=$("$RTRT" new dev "$TMP_PROJECT" --var project_name=hello-rtrt-smoke 2>&1); then
    report PASS 'rtrt new dev'
else
    report FAIL 'rtrt new dev' "$OUT"
fi
if OUT=$("$RTRT" repo-map crates/rtrt-memory 2>&1); then
    OUT=${OUT:0:2048}
    report PASS 'rtrt repo-map crates/rtrt-memory' "head=${#OUT}c"
else
    report FAIL 'rtrt repo-map' "$OUT"
fi

section 'Provider chat — Anthropic'
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    if OUT=$("$RTRT" provider chat --provider anthropic --model claude-haiku-4-5 ping 2>&1); then
        report PASS 'anthropic chat' "${OUT:0:80}"
    else
        report FAIL 'anthropic chat' "$OUT"
    fi
else
    report SKIP 'anthropic chat' 'ANTHROPIC_API_KEY unset'
fi

section 'Provider chat — OpenAI'
if [ -n "${OPENAI_API_KEY:-}" ]; then
    if OUT=$("$RTRT" provider chat --provider openai --model gpt-5.4-mini --stream 'count to 3' 2>&1); then
        report PASS 'openai stream chat' "${OUT:0:80}"
    else
        report FAIL 'openai stream chat' "$OUT"
    fi
else
    report SKIP 'openai stream chat' 'OPENAI_API_KEY unset'
fi

section 'Provider chat — OpenAI-compatible'
if [ -n "${OPENAI_COMPAT_BASE_URL:-}" ] && [ -n "${OPENAI_COMPAT_MODEL:-}" ]; then
    if OUT=$(RTRT_OPENAI_COMPAT_API_KEY="${OPENAI_COMPAT_API_KEY:-}" \
        "$RTRT" provider chat --provider openai-compat \
        --base-url "$OPENAI_COMPAT_BASE_URL" --model "$OPENAI_COMPAT_MODEL" hi 2>&1); then
        report PASS 'openai-compat chat' "${OUT:0:80}"
    else
        report FAIL 'openai-compat chat' "$OUT"
    fi
else
    report SKIP 'openai-compat chat' 'OPENAI_COMPAT_BASE_URL or OPENAI_COMPAT_MODEL unset'
fi

section 'Dashboard (loopback machine mode)'
DASHBOARD_HOME="$TMP_ROOT/dashboard-home"
DASHBOARD_STATE="$DASHBOARD_HOME/.rtrt/dashboard"
mkdir -p "$DASHBOARD_STATE"
chmod 700 "$DASHBOARD_HOME/.rtrt" "$DASHBOARD_STATE"
DASHBOARD_TOKEN="dashboard-smoke-token-$$"
printf 'RTRT_DASHBOARD_TOKEN=%s\n' "$DASHBOARD_TOKEN" > "$DASHBOARD_STATE/dashboard.env"
chmod 600 "$DASHBOARD_STATE/dashboard.env"
DASHBOARD_URL="http://127.0.0.1:${DASHBOARD_PORT}"
(
    unset RTRT_MEMORY_PATH
    exec env HOME="$DASHBOARD_HOME" RTRT_DASHBOARD_BIND="127.0.0.1:${DASHBOARD_PORT}" \
        "$DASHBOARD_BIN" --machine --state-dir "$DASHBOARD_STATE"
) >"$TMP_ROOT/dashboard.log" 2>&1 &
DASHBOARD_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
    curl -fsS "$DASHBOARD_URL/healthz" >/dev/null 2>&1 && break
    sleep 1
done
if OUT=$(curl -fsS "$DASHBOARD_URL/healthz" 2>&1) && [[ "$OUT" == *ok* ]]; then
    report PASS 'dashboard /healthz'
else
    report FAIL 'dashboard /healthz' "no response; log=$TMP_ROOT/dashboard.log"
fi
if OUT=$(curl -fsS -H "Authorization: Bearer $DASHBOARD_TOKEN" "$DASHBOARD_URL/api/templates" 2>&1); then
    report PASS 'dashboard authenticated /api/templates' "len=${#OUT}c"
else
    report FAIL 'dashboard authenticated /api/templates' "$OUT"
fi
DASHBOARD_CODE=$(curl -sS -o /dev/null -w '%{http_code}' "$DASHBOARD_URL/api/templates" 2>/dev/null || true)
[[ "$DASHBOARD_CODE" =~ ^[0-9]{3}$ ]] || DASHBOARD_CODE=000
if [ "$DASHBOARD_CODE" = 401 ]; then
    report PASS 'dashboard bearer guard' 'rejects missing token with 401'
else
    report FAIL 'dashboard bearer guard' "expected 401, got $DASHBOARD_CODE"
fi

section 'MCP stdio JSON-RPC handshake'
MCP_PROJECT="$TMP_ROOT/mcp-project"
MCP_HOME="$TMP_ROOT/mcp-home"
mkdir -p "$MCP_PROJECT"
mkdir -p "$MCP_HOME/.rtrt"
chmod 700 "$MCP_HOME/.rtrt"
GIT_CONFIG_NOSYSTEM=1 git -C "$MCP_PROJECT" init -q
MCP_INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"rtrt-smoke","version":"0.1.1"}}}'
MCP_INITIALIZED='{"jsonrpc":"2.0","method":"notifications/initialized"}'
validate_mcp_stdio() {
    MCP_STDIO_OUT=$1 python3 - <<'PY'
import json
import os

lines = os.environ["MCP_STDIO_OUT"].splitlines()
if not lines:
    raise SystemExit("missing MCP stdout")
messages = []
for line in lines:
    try:
        messages.append(json.loads(line))
    except json.JSONDecodeError as error:
        raise SystemExit(f"non-JSON stdout line: {line!r}") from error
responses = [message for message in messages if message.get("id") == 1]
if len(responses) != 1:
    raise SystemExit("initialize response id did not match exactly once")
result = responses[0].get("result")
if not isinstance(result, dict) or not isinstance(result.get("protocolVersion"), str):
    raise SystemExit("missing result.protocolVersion")
if not isinstance(result.get("serverInfo"), dict):
    raise SystemExit("missing result.serverInfo")
PY
}
if MCP_STDIO_OUT=$(
    cd "$MCP_PROJECT" || exit 1
    unset RTRT_MEMORY_PATH
    printf '%s\n%s\n' "$MCP_INIT" "$MCP_INITIALIZED" | HOME="$MCP_HOME" "$MCP_BIN" 2>/dev/null
) && validate_mcp_stdio "$MCP_STDIO_OUT"; then
    report PASS 'mcp stdio initialize'
else
    report FAIL 'mcp stdio initialize' "${MCP_STDIO_OUT:-no response}"
fi

section 'MCP Streamable HTTP'
MCP_TOKEN="mcp-smoke-token-$$"
MCP_URL="http://127.0.0.1:${MCP_PORT}/mcp"
(
    cd "$MCP_PROJECT" || exit 1
    unset RTRT_MEMORY_PATH
    exec env HOME="$MCP_HOME" RTRT_MCP_HTTP_TOKEN="$MCP_TOKEN" \
        "$MCP_BIN" --transport http \
        --bind "127.0.0.1:${MCP_PORT}"
) >"$TMP_ROOT/mcp.log" 2>&1 &
MCP_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
    MCP_CODE=$(curl -sS -o /dev/null -w '%{http_code}' "$MCP_URL" 2>/dev/null || true)
    [[ "$MCP_CODE" =~ ^[0-9]{3}$ ]] || MCP_CODE=000
    [ "$MCP_CODE" != 000 ] && break
    sleep 1
done
MCP_CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H "Authorization: Bearer $MCP_TOKEN" --data "$MCP_INIT" "$MCP_URL" 2>/dev/null || true)
[[ "$MCP_CODE" =~ ^[0-9]{3}$ ]] || MCP_CODE=000
if [ "$MCP_CODE" = 200 ]; then
    report PASS 'mcp HTTP initialize' 'status=200'
else
    report FAIL 'mcp HTTP initialize' "expected 200, got $MCP_CODE; log=$TMP_ROOT/mcp.log"
fi
MCP_CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    --data "$MCP_INIT" "$MCP_URL" 2>/dev/null || true)
[[ "$MCP_CODE" =~ ^[0-9]{3}$ ]] || MCP_CODE=000
if [ "$MCP_CODE" = 401 ]; then
    report PASS 'mcp bearer guard' 'rejects missing token with 401'
else
    report FAIL 'mcp bearer guard' "expected 401, got $MCP_CODE"
fi

section 'Summary'
printf '  %d pass / %d fail / %d skip\n' "$PASS" "$FAIL" "$SKIP"
if [ "$FAIL" -gt 0 ]; then
    printf '\n  Failed:\n'
    for name in "${FAILED_NAMES[@]}"; do printf '    - %s\n' "$name"; done
    exit 1
fi
