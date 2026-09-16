#!/usr/bin/env bash
set -euo pipefail

[ "$#" -eq 3 ] || { echo 'usage: publish-package.sh <name> <version> <archive>' >&2; exit 1; }
name=$1
version=$2
archive=$3
case "$name" in
  rtrt-dashboard-linux-x64|rtrt-dashboard-linux-arm64|rtrt-dashboard-darwin-x64|rtrt-dashboard-darwin-arm64|rtrt-dashboard-win32-x64) ;;
  *) echo 'unsupported platform package' >&2; exit 1 ;;
esac
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || exit 1
[ -s "$archive" ] || { echo 'missing package archive' >&2; exit 1; }

# The caller runs at the project root, including artifact-only publishing jobs.
mkdir -p .rtrt/tmp
temporary=$(mktemp -d .rtrt/tmp/npm-publish.XXXXXX)
trap 'rm -rf "$temporary"' EXIT
metadata="$temporary/version.json"
url="https://registry.npmjs.org/${name}/${version}"
actual="sha512-$(openssl dgst -sha512 -binary "$archive" | openssl base64 -A)"

verify() {
  jq -e --arg name "$name" --arg version "$version" --arg integrity "$actual" \
    '.name == $name and .version == $version and .dist.integrity == $integrity' "$metadata" >/dev/null || {
    echo 'registry package identity or integrity differs from local artifact' >&2
    return 1
  }
}

status=$(curl --silent --show-error --connect-timeout 10 --max-time 60 \
  --output "$metadata" --write-out '%{http_code}' "$url")
case "$status" in
  200) verify; exit ;;
  404) npm publish "$archive" --access public --provenance --ignore-scripts ;;
  *) echo "npm registry returned HTTP $status" >&2; exit 1 ;;
esac

# Visibility plus identity/integrity is the dependency barrier for rtrt-agent.
for attempt in {1..12}; do
  status=$(curl --silent --show-error --connect-timeout 10 --max-time 60 \
    --output "$metadata" --write-out '%{http_code}' "$url" || true)
  case "$status" in
    200) verify; exit ;;
    404|429|5??|000) ;;
    *) echo "npm registry returned HTTP $status" >&2; exit 1 ;;
  esac
  [ "$attempt" -eq 12 ] || sleep 10
done
echo "${name}@${version} did not become visible" >&2
exit 1
