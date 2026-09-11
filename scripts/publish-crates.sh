#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/scripts/release-helpers.sh"

RELEASE_VERSION=${RELEASE_VERSION:?RELEASE_VERSION is required}
[ -n "${CARGO_REGISTRY_TOKEN:-}" ] || {
    echo '::error::CARGO_REGISTRY_TOKEN is required; configure it in the crates-io-publish environment before running a REL tag' >&2
    exit 1
}
POLL_ATTEMPTS=${RTRT_INDEX_POLL_ATTEMPTS:-18}
POLL_DELAY=${RTRT_INDEX_POLL_DELAY:-10}
[[ "$POLL_ATTEMPTS" =~ ^[1-9][0-9]*$ ]] || { echo '::error::invalid sparse-index poll attempts' >&2; exit 2; }
[[ "$POLL_DELAY" =~ ^[0-9]+$ ]] || { echo '::error::invalid sparse-index poll delay' >&2; exit 2; }

crates=(
    rtrt-core rtrt-providers rtrt-compress rtrt-proxy rtrt-memory
    rtrt-templates rtrt-security rtrt-eval rtrt-mcp rtrt-dashboard rtrt-cli
)
metadata=$(cargo metadata --locked --no-deps --format-version 1)
expected=$(printf '%s\n' "${crates[@]}" | sort)
actual=$(jq -r '.packages[].name' <<<"$metadata" | sort)
[ "$expected" = "$actual" ] || {
    echo '::error::publish order is out of sync with workspace members' >&2
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") || true
    exit 1
}

declare -A publish_position=()
for index in "${!crates[@]}"; do publish_position["${crates[$index]}"]=$index; done
while IFS=$'\t' read -r dependent dependency; do
    if (( publish_position[$dependency] >= publish_position[$dependent] )); then
        echo "::error::dependency must be published before dependent: $dependency -> $dependent" >&2
        exit 1
    fi
done < <(jq -r '
    [.packages[].name] as $members
    | .packages[] as $package
    | $package.dependencies[]
    | select(.path != null)
    | select(.name as $dependency | $members | index($dependency))
    | "\($package.name)\t\(.name)"
' <<<"$metadata")

package_root=${CARGO_TARGET_DIR:-target}/package
sparse_index_path() {
    local crate=${1,,} length=${#1}
    case "$length" in
        1) printf '1/%s\n' "$crate" ;;
        2) printf '2/%s\n' "$crate" ;;
        3) printf '3/%s/%s\n' "${crate:0:1}" "$crate" ;;
        *) printf '%s/%s/%s\n' "${crate:0:2}" "${crate:2:2}" "$crate" ;;
    esac
}

sparse_index_status() {
    local crate=$1 package=$2 response status records count expected_checksum actual_checksum
    response=$(mktemp)
    status=$(curl --silent --show-error --output "$response" --write-out '%{http_code}' \
        --user-agent 'rtrt-release-workflow' "https://index.crates.io/$(sparse_index_path "$crate")" || true)
    if [ "$status" = 404 ]; then rm -f "$response"; return 1; fi
    if [ "$status" != 200 ]; then
        rm -f "$response"
        echo "::error::crates.io sparse index returned HTTP ${status:-000} for $crate" >&2
        return 2
    fi
    if ! jq -Rce 'fromjson' "$response" >/dev/null; then
        rm -f "$response"
        echo "::error::crates.io sparse index returned malformed metadata for $crate" >&2
        return 2
    fi
    records=$(jq -Rrc --arg version "$RELEASE_VERSION" 'fromjson | select(.vers == $version)' "$response")
    rm -f "$response"
    count=$(printf '%s\n' "$records" | awk 'NF { count++ } END { print count + 0 }')
    [ "$count" -le 1 ] || { echo "::error::crates.io sparse index returned duplicate $crate@$RELEASE_VERSION records" >&2; return 2; }
    [ "$count" -eq 1 ] || return 1
    expected_checksum=$(jq -r '.cksum // empty' <<<"$records")
    [[ "$expected_checksum" =~ ^[0-9a-fA-F]{64}$ ]] || {
        echo "::error::crates.io sparse index returned a malformed checksum for $crate" >&2
        return 2
    }
    actual_checksum=$(sha256_digest "$package")
    [ "$actual_checksum" = "${expected_checksum,,}" ] || {
        echo "::error::existing $crate@$RELEASE_VERSION checksum does not match $package" >&2
        return 2
    }
}

for crate in "${crates[@]}"; do
    cargo package --locked -p "$crate"
    package="$package_root/${crate}-${RELEASE_VERSION}.crate"
    [ -f "$package" ] || { echo "::error::cargo package did not produce $package" >&2; exit 1; }
    rc=0
    sparse_index_status "$crate" "$package" || rc=$?
    if [ "$rc" -eq 0 ]; then
        echo "== $crate@$RELEASE_VERSION already published and verified =="
        continue
    fi
    [ "$rc" -eq 1 ] || exit "$rc"
    cargo publish --locked -p "$crate"
    for ((attempt = 1; attempt <= POLL_ATTEMPTS; attempt++)); do
        rc=0
        sparse_index_status "$crate" "$package" || rc=$?
        [ "$rc" -ne 0 ] || break
        [ "$rc" -eq 1 ] || exit "$rc"
        if [ "$attempt" -eq "$POLL_ATTEMPTS" ]; then
            echo "::error::$crate@$RELEASE_VERSION did not become visible in the sparse index" >&2
            exit 1
        fi
        sleep "$POLL_DELAY"
    done
done
