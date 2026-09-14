#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/scripts/release-helpers.sh"

stable_tag_version() {
    local tag=${1:?tag required}
    if [[ "$tag" =~ ^(REL-)?v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
        printf '%s.%s.%s\n' "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[4]}"
        return 0
    fi
    printf 'release-preflight: invalid stable release tag: %s\n' "$tag" >&2
    return 1
}

release_commit_for_tag() {
    local tag=${1:?tag required}
    git rev-parse --verify "refs/tags/${tag}^{commit}" 2>/dev/null || {
        echo "release-preflight: release tag $tag is missing" >&2
        return 1
    }
}

validate_paired_tags() {
    local version_tag=${1:?version tag required}
    local release_commit=${2:?release commit required}
    local paired_tag paired_commit
    for paired_tag in "$version_tag" "REL-$version_tag"; do
        paired_commit=$(release_commit_for_tag "$paired_tag") || return 1
        [ "$paired_commit" = "$release_commit" ] || {
            echo "release-preflight: $paired_tag points to $paired_commit, expected $release_commit" >&2
            return 1
        }
    done
}

if [ "${1:-}" = --validate-tag ]; then
    [ "$#" -eq 2 ] || { echo 'usage: release-preflight.sh --validate-tag <tag>' >&2; exit 2; }
    stable_tag_version "$2"
    exit
fi
[ "$#" -eq 0 ] || { echo 'usage: release-preflight.sh [--validate-tag <tag>]' >&2; exit 2; }

event=${RELEASE_EVENT:?RELEASE_EVENT is required}
ref_name=${RELEASE_REF_NAME:?RELEASE_REF_NAME is required}
release_sha=${RELEASE_SHA:?RELEASE_SHA is required}
recovery_tag=${RELEASE_RECOVERY_TAG:-}
source_sha=$release_sha

if [ "$event" = workflow_dispatch ]; then
    workflow_commit=$(git rev-parse --verify "${release_sha}^{commit}") || {
        echo "release-preflight: event SHA $release_sha does not resolve to a commit" >&2
        exit 1
    }
    head_commit=$(git rev-parse --verify 'HEAD^{commit}')
    [ "$head_commit" = "$workflow_commit" ] || {
        echo "release-preflight: checkout $head_commit does not match event commit $workflow_commit" >&2
        exit 1
    }
    if [ -n "$recovery_tag" ]; then
        workflow_ref=${RELEASE_WORKFLOW_REF:?RELEASE_WORKFLOW_REF is required for recovery}
        default_branch_ref=${RELEASE_DEFAULT_BRANCH_REF:?RELEASE_DEFAULT_BRANCH_REF is required for recovery}
        [ "$workflow_ref" = "$default_branch_ref" ] || {
            echo "release-preflight: recovery must run from $default_branch_ref, found $workflow_ref" >&2
            exit 1
        }
        version=$(stable_tag_version "$recovery_tag")
        version_tag="v$version"
        publish=false
        [[ "$recovery_tag" == REL-v* ]] && publish=true
        source_sha=$(release_commit_for_tag "$recovery_tag")
        validate_paired_tags "$version_tag" "$source_sha"
        git checkout --quiet --detach "$source_sha"
    else
        version=$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[0].version')
        version_tag="v$version"
        publish=false
        source_sha=$workflow_commit
    fi
else
    version=$(stable_tag_version "$ref_name")
    version_tag="v$version"
    publish=false
    [[ "$ref_name" == REL-v* ]] && publish=true

    release_commit=$(git rev-parse --verify "${release_sha}^{commit}") || {
        echo "release-preflight: event SHA $release_sha does not resolve to a commit" >&2
        exit 1
    }
    head_commit=$(git rev-parse --verify 'HEAD^{commit}')
    [ "$head_commit" = "$release_commit" ] || {
        echo "release-preflight: checkout $head_commit does not match event commit $release_commit" >&2
        exit 1
    }
    validate_paired_tags "$version_tag" "$release_commit"
    source_sha=$release_commit
fi

workspace_version=$(awk '
    /^\[workspace\.package\]$/ { in_package=1; next }
    in_package && /^\[/ { exit }
    in_package && /^version = "/ {
        value=$0
        sub(/^version = "/, "", value)
        sub(/"$/, "", value)
        print value
        exit
    }
' Cargo.toml)
[ "$workspace_version" = "$version" ] || {
    echo "release-preflight: workspace version $workspace_version does not match $version" >&2
    exit 1
}

bad_crates=$(cargo metadata --locked --no-deps --format-version 1 \
    | jq -r --arg version "$version" '.packages[] | select(.version != $version) | "\(.name)=\(.version)"')
[ -z "$bad_crates" ] || {
    printf 'release-preflight: crate versions do not match %s:\n%s\n' "$version" "$bad_crates" >&2
    exit 1
}

package_version=$(jq -r .version plugins/opencode/package.json)
package_name=$(jq -r .name plugins/opencode/package.json)
package_repository_url=$(jq -r .repository.url plugins/opencode/package.json)
package_repository_directory=$(jq -r .repository.directory plugins/opencode/package.json)
lock_version=$(jq -r .version plugins/opencode/package-lock.json)
lock_name=$(jq -r .name plugins/opencode/package-lock.json)
lock_root_version=$(jq -r '.packages[""].version' plugins/opencode/package-lock.json)
lock_root_name=$(jq -r '.packages[""].name' plugins/opencode/package-lock.json)
[ "$package_name" = rtrt-agent ] || {
    echo "release-preflight: npm package name must be rtrt-agent, found $package_name" >&2
    exit 1
}
[ "$package_repository_url" = git+https://github.com/kernalix7/rtrt.git ] || {
    echo "release-preflight: npm repository.url must be git+https://github.com/kernalix7/rtrt.git, found $package_repository_url" >&2
    exit 1
}
[ "$package_repository_directory" = plugins/opencode ] || {
    echo "release-preflight: npm repository.directory must be plugins/opencode, found $package_repository_directory" >&2
    exit 1
}
for candidate in "$lock_name" "$lock_root_name"; do
    [ "$candidate" = "$package_name" ] || {
        echo "release-preflight: npm lock name $candidate does not match $package_name" >&2
        exit 1
    }
done
for candidate in "$package_version" "$lock_version" "$lock_root_version"; do
    [ "$candidate" = "$version" ] || {
        echo "release-preflight: npm metadata version $candidate does not match $version" >&2
        exit 1
    }
done

claude_plugin_version=$(jq -r .version plugins/claude-code/rtrt/.claude-plugin/plugin.json)
[ "$claude_plugin_version" = "$version" ] || {
    echo "release-preflight: Claude plugin version $claude_plugin_version does not match $version" >&2
    exit 1
}

homebrew_version=$(awk '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' packaging/homebrew/rtrt.rb)
homebrew_url=$(awk '$1 == "url" { gsub(/"/, "", $2); print $2; exit }' packaging/homebrew/rtrt.rb)
[ "$homebrew_version" = "$version" ] || {
    echo "release-preflight: Homebrew version $homebrew_version does not match $version" >&2
    exit 1
}
case "$homebrew_url" in
    */archive/refs/tags/"v${version}.tar.gz") ;;
    *) echo "release-preflight: Homebrew URL does not reference v$version" >&2; exit 1 ;;
esac

for changelog in CHANGELOG.md docs/CHANGELOG.ko.md; do
    [ "$(changelog_heading_count "$changelog" "$version")" -eq 1 ] || {
        echo "release-preflight: $changelog must contain exactly one section for $version" >&2
        exit 1
    }
done

if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        printf 'version=%s\n' "$version"
        printf 'version_tag=%s\n' "$version_tag"
        printf 'publish=%s\n' "$publish"
        printf 'source_sha=%s\n' "$source_sha"
    } >> "$GITHUB_OUTPUT"
fi
printf 'release-preflight: %s metadata is consistent (publish=%s)\n' "$version_tag" "$publish"
