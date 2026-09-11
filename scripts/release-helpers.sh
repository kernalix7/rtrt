#!/usr/bin/env bash

sha256_digest() {
    local file=${1:?file required} output digest
    if command -v sha256sum >/dev/null 2>&1; then
        output=$(sha256sum "$file")
    else
        output=$(shasum -a 256 "$file")
    fi
    digest=${output%%[[:space:]]*}
    digest=$(printf '%s' "$digest" | tr '[:upper:]' '[:lower:]')
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || {
        printf 'release: SHA-256 tool returned a malformed digest for %s\n' "$file" >&2
        return 1
    }
    printf '%s\n' "$digest"
}

canonical_sha256_record() {
    local file=${1:?file required} digest
    digest=$(sha256_digest "$file")
    printf '%s  %s\n' "$digest" "${file##*/}"
}

changelog_heading_count() {
    local file=${1:?file required} version=${2:?version required}
    awk -v version="$version" '
        BEGIN { heading = "## [" version "]"; count = 0 }
        index($0, heading) == 1 && (length($0) == length(heading) || substr($0, length(heading) + 1, 1) == " ") { count++ }
        END { print count }
    ' "$file"
}

extract_changelog_section() {
    local file=${1:?file required} version=${2:?version required}
    awk -v version="$version" '
        BEGIN { heading = "## [" version "]"; found = 0; inside = 0 }
        index($0, heading) == 1 && (length($0) == length(heading) || substr($0, length(heading) + 1, 1) == " ") {
            found++
            if (found > 1) exit 2
            inside = 1
            print
            next
        }
        inside && index($0, "## [") == 1 { exit }
        inside { print }
        END { if (found != 1) exit 1 }
    ' "$file"
}
