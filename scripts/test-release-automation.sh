#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFLIGHT="$ROOT/scripts/release-preflight.sh"
WORKFLOW="$ROOT/.github/workflows/release.yml"
CI_WORKFLOW="$ROOT/.github/workflows/ci.yml"
SMOKE="$ROOT/scripts/smoke.sh"
HELPERS="$ROOT/scripts/release-helpers.sh"
PUBLISH_CRATES="$ROOT/scripts/publish-crates.sh"
INSTALL_SH="$ROOT/install.sh"
INSTALL_PS1="$ROOT/install.ps1"

case "${1:-}" in
    '') ;;
    --checksum-only) [ "$#" -eq 1 ] || exit 2 ;;
    *) echo 'usage: test-release-automation.sh [--checksum-only]' >&2; exit 2 ;;
esac

for tag in v0.1.1 REL-v0.1.1 v10.20.30 REL-v10.20.30; do
    "$PREFLIGHT" --validate-tag "$tag" >/dev/null
done

# Given: release helpers that must normalize platform-specific SHA tool output.
# When: the helper contract is loaded.
# Then: both GNU's binary marker and shasum's text record become one canonical record.
[ -f "$HELPERS" ] || { echo 'release helpers are missing' >&2; exit 1; }
# shellcheck source=release-helpers.sh
. "$HELPERS"
checksum_fixture=$(mktemp -d)
trap 'rm -rf "$checksum_fixture"' EXIT
printf 'release bytes\n' > "$checksum_fixture/archive.zip"
mkdir "$checksum_fixture/bin"
cat > "$checksum_fixture/bin/sha256sum" <<'EOF'
#!/usr/bin/env sh
printf 'ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789 *%s\n' "$1"
EOF
chmod +x "$checksum_fixture/bin/sha256sum"
checksum_record=$(PATH="$checksum_fixture/bin:$PATH" canonical_sha256_record "$checksum_fixture/archive.zip")
[ "$checksum_record" = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  archive.zip" ] || {
    printf 'non-canonical checksum record: %s\n' "$checksum_record" >&2
    exit 1
}
fallback_bin="$checksum_fixture/fallback-bin"
mkdir "$fallback_bin"
cat > "$fallback_bin/shasum" <<'EOF'
#!/bin/sh
shift 2
printf 'FEDCBA9876543210FEDCBA9876543210FEDCBA9876543210FEDCBA9876543210 ?%s\n' "$1"
EOF
chmod +x "$fallback_bin/shasum"
cat > "$fallback_bin/tr" <<'EOF'
#!/bin/bash
IFS= read -r input
printf '%s' "${input,,}"
EOF
chmod +x "$fallback_bin/tr"
checksum_record=$(PATH="$fallback_bin" canonical_sha256_record "$checksum_fixture/archive.zip")
[ "$checksum_record" = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210  archive.zip" ] || {
    printf 'non-canonical shasum checksum record: %s\n' "$checksum_record" >&2
    exit 1
}
rm -rf "$checksum_fixture"
trap - EXIT
[ "${1:-}" != --checksum-only ] || { echo 'release checksum portability checks passed'; exit 0; }

# Given: a version containing regular-expression metacharacters and a similar decoy heading.
changelog_fixture=$(mktemp)
trap 'rm -f "$changelog_fixture"' EXIT
cat > "$changelog_fixture" <<'EOF'
## [1x2+3] - decoy
wrong section
## [1.2+3] - exact
exact section
## [2.0.0]
next section
EOF
# When: headings are counted and extracted by the release helpers.
heading_count=$(changelog_heading_count "$changelog_fixture" '1.2+3')
section=$(extract_changelog_section "$changelog_fixture" '1.2+3')
# Then: only the byte-for-byte version heading matches and extraction stops at the next heading.
[ "$heading_count" -eq 1 ]
[ "$section" = $'## [1.2+3] - exact\nexact section' ]
rm -f "$changelog_fixture"
trap - EXIT
for tag in v0.1 v0.1.1-rc.1 v01.1.1 REL-v1.2.3-alpha 'v1.2.3;echo bad' refs/tags/v1.2.3; do
    if "$PREFLIGHT" --validate-tag "$tag" >/dev/null 2>&1; then
        printf 'expected invalid tag to fail: %s\n' "$tag" >&2
        exit 1
    fi
done

python3 - "$WORKFLOW" "$SMOKE" "$CI_WORKFLOW" "$INSTALL_SH" "$INSTALL_PS1" "$HELPERS" "$PUBLISH_CRATES" <<'PY'
from pathlib import Path
import json
import re
import shlex
import subprocess
import sys

workflow = Path(sys.argv[1]).read_text()
smoke = Path(sys.argv[2]).read_text()
preflight = Path(sys.argv[1]).parents[2].joinpath("scripts/release-preflight.sh").read_text()
ci_workflow = Path(sys.argv[3]).read_text()
install_sh = Path(sys.argv[4]).read_text()
install_ps1 = Path(sys.argv[5]).read_text()
helpers = Path(sys.argv[6]).read_text()
publish_script = Path(sys.argv[7]).read_text()
root = Path(sys.argv[1]).parents[2]

workflow_paths = sorted(
    path
    for path in root.joinpath(".github/workflows").iterdir()
    if path.suffix in {".yml", ".yaml"}
)
for workflow_path in workflow_paths:
    workflow_name = workflow_path.name
    workflow_text = workflow_path.read_text()
    for match in re.finditer(r"(?m)^\s*-?\s*uses:\s*([^\s#]+)", workflow_text):
        action = match.group(1)
        if action.startswith("./"):
            continue
        if re.fullmatch(r"[^@]+@[0-9a-fA-F]{40}", action) is None:
            raise SystemExit(f"{workflow_name} workflow contains a mutable action reference: {action}")
    for match in re.finditer(
        r"(?ms)^\s*- uses: dtolnay/rust-toolchain@[^\n]+\n(?P<body>(?:\s{8,}[^\n]*\n)*)",
        workflow_text,
    ):
        if "toolchain:" not in match.group("body"):
            raise SystemExit(f"{workflow_name} Rust toolchain step must select its toolchain explicitly")

for match in re.finditer(
    r"(?ms)^\s*- uses: dtolnay/rust-toolchain@[^\n]+\n(?P<body>(?:\s{8,}[^\n]*\n)*)",
    ci_workflow,
):
    body = match.group("body")
    if "toolchain: stable" not in body and "toolchain: ${{ matrix.toolchain }}" not in body:
        raise SystemExit("every pinned CI Rust toolchain step must select stable or the matrix toolchain")

required = (
    "permissions:\n  contents: read",
    "publish-npm:\n",
    "id-token: write",
    "release:\n",
    "contents: write",
    "CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}",
    'if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then',
    "scripts/release-preflight.sh",
    "release-credentials:\n",
    "environment: crates-io-publish",
    "npm install --global npm@11.11.0",
    "npm ci --ignore-scripts",
    "npm pack --ignore-scripts --json",
    "softprops/action-gh-release@efb35369e0ad2afab669f228072c1b0d510eae64 # v3.0.3",
)
for fragment in required:
    if fragment not in workflow:
        raise SystemExit(f"missing release contract fragment: {fragment}")

if workflow.index("publish-npm:\n") > workflow.index("release:\n"):
    raise SystemExit("npm publication must precede the GitHub Release job")
if workflow.index("publish-crates:\n") > workflow.index("release:\n"):
    raise SystemExit("crates.io publication must precede the GitHub Release job")
if re.search(r"^env:\n(?:  .*\n)*  CARGO_REGISTRY_TOKEN:", workflow, re.MULTILINE):
    raise SystemExit("Cargo token must not be workflow-scoped")
if workflow.count("${{ secrets.CARGO_REGISTRY_TOKEN }}") != 2:
    raise SystemExit("Cargo token must appear once in credential preflight and once in cargo publish")
if re.search(r"(?m)^\s*cargo\s+publish\b[^\n]*\s--token(?:\s|=)", workflow):
    raise SystemExit("Cargo token must not be passed on the cargo publish process argv")
if "sleep 10  #" in workflow:
    raise SystemExit("fixed crates.io publication sleep remains")

def job_block(name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n.*?(?=^  [A-Za-z0-9_-]+:\n|\Z)",
        workflow,
    )
    if match is None:
        raise SystemExit(f"missing workflow job: {name}")
    return match.group(0)


preflight_job = job_block("preflight")
if re.search(r"\bcargo\s+(?:package|publish)\b", preflight_job):
    raise SystemExit("release preflight must not package or publish workspace crates")
if preflight_job.index("dtolnay/rust-toolchain@") > preflight_job.index("scripts/release-preflight.sh"):
    raise SystemExit("release preflight must select the pinned Rust toolchain before cargo metadata")

credential_job = job_block("release-credentials")
for fragment in (
    "if: needs.preflight.outputs.publish == 'true'",
    "environment: crates-io-publish",
    "CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}",
):
    if fragment not in credential_job:
        raise SystemExit(f"credential preflight missing: {fragment}")
if re.search(r"\b(?:cargo|npm)\s+publish\b", credential_job):
    raise SystemExit("credential preflight must not publish")

for job_name in ("publish-npm", "publish-crates"):
    job = job_block(job_name)
    if "release-credentials" not in job.partition("steps:")[0]:
        raise SystemExit(f"{job_name} must need release-credentials")
    if "if: needs.preflight.outputs.publish == 'true'" not in job:
        raise SystemExit(f"{job_name} must retain independent publish gating")

publish_crates_job = job_block("publish-crates")
if "scripts/publish-crates.sh" not in publish_crates_job:
    raise SystemExit("publish job must execute the tested crate publication script")
if 'cargo publish --locked -p "$crate"' not in publish_script:
    raise SystemExit("publish job must use locked Cargo publication with the step-scoped token env")
array_match = re.search(r"(?ms)^crates=\(\n(.*?)^\)", publish_script)
if array_match is None:
    raise SystemExit("publish-crates must define an explicit crates array")
publish_order = shlex.split(array_match.group(1))
metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
)
members = {package["name"] for package in metadata["packages"]}
if len(publish_order) != len(set(publish_order)) or set(publish_order) != members:
    raise SystemExit("publish crates array must exactly cover workspace members without duplicates")
position = {name: index for index, name in enumerate(publish_order)}
for package in metadata["packages"]:
    for dependency in package["dependencies"]:
        if dependency.get("path") is None or dependency["name"] not in members:
            continue
        if position[dependency["name"]] >= position[package["name"]]:
            raise SystemExit(
                f"publish order is not topological: {dependency['name']} must precede {package['name']}"
            )
for fragment in (
    "metadata=$(cargo metadata --locked --no-deps --format-version 1)",
    "declare -A publish_position",
    "select(.path != null)",
    "dependency must be published before dependent",
):
    if fragment not in publish_script:
        raise SystemExit(f"publish job missing runtime topology validation: {fragment}")
if publish_script.index("declare -A publish_position") > publish_script.index("sparse_index_status()"):
    raise SystemExit("publish topology validation must run before registry interaction")

ci_contract = re.search(
    r"(?ms)^  release-contract:\n.*?(?=^  [A-Za-z0-9_-]+:\n|\Z)",
    ci_workflow,
)
if ci_contract is None:
    raise SystemExit("CI must define a release-contract job")
for fragment in (
    "actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0 # v7.0.0",
    "persist-credentials: false",
    "dtolnay/rust-toolchain@d1031067263f94b142dd6c0ce24c5eb9d02d52a0 # stable",
):
    if fragment not in ci_contract.group(0):
        raise SystemExit(f"release-contract CI job missing immutable setup: {fragment}")
if "bash scripts/test-release-automation.sh" not in ci_contract.group(0):
    raise SystemExit("release-contract CI job must run the release automation test")
for fragment in ("windows-latest", "--checksum-only", "shell: bash"):
    if fragment not in ci_contract.group(0):
        raise SystemExit(f"release-contract CI job missing Windows checksum coverage: {fragment}")

npm_job = workflow[workflow.index("  publish-npm:\n"):workflow.index("  publish-crates:\n")]
if npm_job.index("npm install --global npm@11.11.0") > npm_job.index("npm publish"):
    raise SystemExit("pinned npm CLI must be installed before npm publish")
for fragment in (".dist.integrity", ".dist.shasum", "npm-version.tgz", "openssl dgst -sha512 -binary"):
    if fragment not in npm_job:
        raise SystemExit(f"npm rerun verification must compare registry metadata to package bytes: {fragment}")

for fragment in (
    'cargo package --locked -p "$crate"',
    '${crate}-${RELEASE_VERSION}.crate',
    "https://index.crates.io/",
    ".cksum // empty",
    'sha256_digest "$package"',
):
    if fragment not in publish_script:
        raise SystemExit(f"crates rerun verification must compare registry checksum to package bytes: {fragment}")
publication_loop = publish_script.rsplit('for crate in "${crates[@]}"; do', 1)[1]
if publication_loop.index('cargo package --locked -p "$crate"') > publication_loop.index('sparse_index_status "$crate"'):
    raise SystemExit("each crate must be packaged before accepting its existing registry version")
for fragment in ("canonical_sha256_record", "tr '[:upper:]' '[:lower:]'", 'printf \'%s  %s\\n\''):
    if fragment not in helpers:
        raise SystemExit(f"release helpers missing canonical checksum behavior: {fragment}")
if re.search(r'\$0\s*~[^\n]*version', helpers) or re.search(r'grep[^\n]*\$version', preflight):
    raise SystemExit("changelog version must not be interpolated into a regular expression")
if "extract_changelog_section CHANGELOG.md \"$RELEASE_VERSION\"" not in workflow:
    raise SystemExit("GitHub Release notes must use literal changelog extraction")

for installer_name, installer, fragments in (
    (
        "install.sh",
        install_sh,
        ("checksum file is missing", "64 hexadecimal", "checksum filename does not match", "checksum mismatch"),
    ),
    (
        "install.ps1",
        install_ps1,
        ("checksum file is missing", "^[0-9a-fA-F]{64}$", "checksum filename does not match", "checksum mismatch"),
    ),
):
    for fragment in fragments:
        if fragment not in installer:
            raise SystemExit(f"{installer_name} missing fail-closed checksum contract: {fragment}")

preflight_required = (
    'git rev-parse --verify "${release_sha}^{commit}"',
    "for changelog in CHANGELOG.md docs/CHANGELOG.ko.md",
    "claude_plugin_version=$(jq -r .version plugins/claude-code/rtrt/.claude-plugin/plugin.json)",
    "homebrew_version=$(awk",
    "homebrew_url=$(awk",
    '*/archive/refs/tags/"v${version}.tar.gz")',
    'package_name=$(jq -r .name plugins/opencode/package.json)',
    'lock_name=$(jq -r .name plugins/opencode/package-lock.json)',
    'lock_root_name=$(jq -r \'.packages[""].name\' plugins/opencode/package-lock.json)',
)
for fragment in preflight_required:
    if fragment not in preflight:
        raise SystemExit(f"missing release preflight contract: {fragment}")
if preflight.count("cargo metadata --locked --no-deps --format-version 1") != 2:
    raise SystemExit("both release preflight metadata reads must use the lockfile")
if re.search(r"sed[^\n]*\|\s*head", preflight):
    raise SystemExit("release preflight must not use an unbounded sed/head pipeline")

smoke_required = (
    "new dev",
    "--provider anthropic",
    "--provider openai",
    "--provider openai-compat",
    '--base-url "$OPENAI_COMPAT_BASE_URL"',
    "--machine --state-dir",
    "MCP stdio JSON-RPC handshake",
    "Accept: application/json, text/event-stream",
    "MCP_CODE=000",
    "notifications/initialized",
    "unset RTRT_MEMORY_PATH",
    "result.protocolVersion",
    "result.serverInfo",
    "non-JSON stdout",
)
for fragment in smoke_required:
    if fragment not in smoke:
        raise SystemExit(f"missing smoke contract fragment: {fragment}")
for obsolete in ("rtrt-dashboard --version", "new rust-cli", "|| echo 000"):
    if obsolete in smoke:
        raise SystemExit(f"obsolete smoke invocation remains: {obsolete}")
if re.search(r'exec\s+"\$RTRT"\s+mcp\s+--binary', smoke):
    raise SystemExit("HTTP smoke must supervise rtrt-mcp directly so cleanup cannot orphan it")
if re.search(
    r'exec env HOME="\$MCP_HOME" RTRT_MCP_HTTP_TOKEN="\$MCP_TOKEN"\s+\\\n\s+"\$MCP_BIN" --transport http',
    smoke,
) is None:
    raise SystemExit("HTTP smoke must launch the authenticated rtrt-mcp process directly")
if smoke.count("''|*[!0-9]*") != 2:
    raise SystemExit("both smoke ports must reject empty and non-decimal values")
if smoke.count("unset RTRT_MEMORY_PATH") < 3:
    raise SystemExit("dashboard and both MCP transports must unset RTRT_MEMORY_PATH")
if "trap cleanup EXIT INT TERM" in smoke or smoke.count("trap cleanup EXIT") != 1:
    raise SystemExit("smoke cleanup must be attached only to EXIT")
for fragment in ("trap 'exit 130' INT", "trap 'exit 143' TERM"):
    if fragment not in smoke:
        raise SystemExit(f"smoke signal status contract missing: {fragment}")
PY

python3 - "$SMOKE" <<'PY'
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

smoke = Path(sys.argv[1])
for delivered_signal, expected_status in ((signal.SIGINT, 130), (signal.SIGTERM, 143)):
    # Given: a smoke run blocked after its supervised dashboard process has started.
    with tempfile.TemporaryDirectory(prefix="rtrt-smoke-signal-") as fixture_text:
        fixture = Path(fixture_text)
        binaries = fixture / "bin"
        temporary = fixture / "tmp"
        binaries.mkdir()
        temporary.mkdir()
        cli = binaries / "rtrt"
        cli.write_text("#!/bin/sh\nexit 0\n")
        server = binaries / "server"
        server.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$RTRT_TEST_READY\"\n"
            "trap 'exit 0' INT TERM\nwhile :; do sleep 1; done\n"
        )
        curl = binaries / "curl"
        curl.write_text("#!/bin/sh\nexit 22\n")
        for executable in (cli, server, curl):
            executable.chmod(0o755)
        ready = fixture / "ready"
        environment = os.environ | {
            "PATH": f"{binaries}:{os.environ['PATH']}",
            "RTRT": str(cli),
            "RTRT_DASHBOARD_BIN": str(server),
            "RTRT_MCP_BIN": str(server),
            "RTRT_TEST_READY": str(ready),
            "TMPDIR": str(temporary),
        }
        process = subprocess.Popen(
            [str(smoke), "--dashboard-port", "17311", "--mcp-port", "17312"],
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        deadline = time.monotonic() + 5
        while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.01)
        if not ready.exists():
            process.kill()
            raise SystemExit("smoke signal fixture never reached the synchronized process marker")
        # When: the whole smoke process group receives the terminal signal.
        os.killpg(process.pid, delivered_signal)
        status = process.wait(timeout=5)
        # Then: the shell reports conventional status and its EXIT cleanup removes temporary state.
        if status != expected_status:
            raise SystemExit(f"smoke signal {delivered_signal} returned {status}, expected {expected_status}")
        if list(temporary.iterdir()):
            raise SystemExit(f"smoke signal {delivered_signal} left temporary state behind")
PY

test_publish_crates_sparse_index() {
    fixture=$(mktemp -d)
    trap 'rm -rf "$fixture"' EXIT
    mkdir -p "$fixture/bin" "$fixture/state" "$fixture/target"
    real_cargo=$(command -v cargo)
    cat > "$fixture/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
command=$1
shift
case "$command" in
    metadata) exec "$RTRT_TEST_REAL_CARGO" metadata "$@" ;;
    package)
        while [ "$#" -gt 0 ]; do
            if [ "$1" = -p ]; then crate=$2; break; fi
            shift
        done
        printf 'package %s\n' "$crate" >> "$RTRT_TEST_EVENT_LOG"
        printf '%s\n' "$crate" >> "$RTRT_TEST_PACKAGE_LOG"
        while IFS=$'\t' read -r dependent dependency; do
            [ "$dependent" != "$crate" ] || [ -f "$RTRT_TEST_STATE/$dependency" ] || {
                printf 'dependency is not index-visible: %s -> %s\n' "$dependency" "$dependent" >&2
                exit 94
            }
        done < "$RTRT_TEST_DEPENDENCIES"
        mkdir -p "$CARGO_TARGET_DIR/package"
        printf 'package:%s\n' "$crate" > "$CARGO_TARGET_DIR/package/${crate}-${RELEASE_VERSION}.crate"
        ;;
    publish)
        [ "${CARGO_REGISTRY_TOKEN:-}" = release-secret ] || exit 91
        for argument in "$@"; do [ "$argument" != --token ] || exit 92; done
        while [ "$#" -gt 0 ]; do
            if [ "$1" = -p ]; then crate=$2; break; fi
            shift
        done
        printf 'publish %s\n' "$crate" >> "$RTRT_TEST_EVENT_LOG"
        printf '%s\n' "$crate" >> "$RTRT_TEST_PUBLISH_LOG"
        [ "$RTRT_TEST_PUBLISH_VISIBLE" != 1 ] || printf 'matching\n' > "$RTRT_TEST_STATE/$crate"
        ;;
    *) exit 93 ;;
esac
EOF
    cat > "$fixture/bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
output=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) output=$2; shift 2 ;;
        --write-out|--user-agent) shift 2 ;;
        --silent|--show-error) shift ;;
        *) url=$1; shift ;;
    esac
done
crate=${url##*/}
printf 'lookup %s\n' "$crate" >> "$RTRT_TEST_EVENT_LOG"
if [ ! -f "$RTRT_TEST_STATE/$crate" ]; then printf '404'; exit 0; fi
mode=$(cat "$RTRT_TEST_STATE/$crate")
case "$mode" in
    matching) checksum=$(sha256sum "$CARGO_TARGET_DIR/package/${crate}-${RELEASE_VERSION}.crate" | awk '{print $1}') ;;
    mismatch) checksum=0000000000000000000000000000000000000000000000000000000000000000 ;;
    malformed) printf 'not-json\n' > "$output"; printf '200'; exit 0 ;;
esac
printf '{"name":"%s","vers":"%s","cksum":"%s","yanked":false}\n' \
    "$crate" "$RELEASE_VERSION" "$checksum" > "$output"
printf '200'
EOF
    chmod +x "$fixture/bin/cargo" "$fixture/bin/curl"
    crates=(rtrt-core rtrt-providers rtrt-compress rtrt-proxy rtrt-memory rtrt-templates rtrt-security rtrt-eval rtrt-mcp rtrt-dashboard rtrt-cli)
    "$real_cargo" metadata --locked --no-deps --format-version 1 | jq -r '
        [.packages[].name] as $members
        | .packages[] as $package
        | $package.dependencies[]
        | select(.path != null)
        | select(.name as $dependency | $members | index($dependency))
        | "\($package.name)\t\(.name)"
    ' > "$fixture/dependencies.tsv"
    : > "$fixture/package.log"
    : > "$fixture/publish.log"
    : > "$fixture/event.log"

    # Given: an empty registry where packaging a dependent requires every workspace dependency
    # to have already become index-visible.
    # When: the first-time publication runs in topological order.
    PATH="$fixture/bin:$PATH" CARGO_TARGET_DIR="$fixture/target" CARGO_REGISTRY_TOKEN=release-secret \
        RELEASE_VERSION=0.1.1 RTRT_TEST_REAL_CARGO="$real_cargo" RTRT_TEST_PACKAGE_LOG="$fixture/package.log" \
        RTRT_TEST_PUBLISH_LOG="$fixture/publish.log" RTRT_TEST_EVENT_LOG="$fixture/event.log" \
        RTRT_TEST_DEPENDENCIES="$fixture/dependencies.tsv" RTRT_TEST_PUBLISH_VISIBLE=1 \
        RTRT_TEST_STATE="$fixture/state" RTRT_INDEX_POLL_DELAY=0 "$PUBLISH_CRATES"
    # Then: each crate is packaged, checked, published, and visible before the next package.
    expected_events=
    for crate in "${crates[@]}"; do
        expected_events+="package $crate"$'\n'"lookup $crate"$'\n'"publish $crate"$'\n'"lookup $crate"$'\n'
    done
    [ "$(cat "$fixture/event.log")"$'\n' = "$expected_events" ]
    [ "$(cat "$fixture/publish.log")" = "$(printf '%s\n' "${crates[@]}")" ]

    # Given: every crate is already present with the exact packaged checksum.
    : > "$fixture/package.log"
    : > "$fixture/publish.log"
    : > "$fixture/event.log"
    # When: publication is rerun.
    PATH="$fixture/bin:$PATH" CARGO_TARGET_DIR="$fixture/target" CARGO_REGISTRY_TOKEN=release-secret \
        RELEASE_VERSION=0.1.1 RTRT_TEST_REAL_CARGO="$real_cargo" RTRT_TEST_PACKAGE_LOG="$fixture/package.log" \
        RTRT_TEST_PUBLISH_LOG="$fixture/publish.log" RTRT_TEST_EVENT_LOG="$fixture/event.log" \
        RTRT_TEST_DEPENDENCIES="$fixture/dependencies.tsv" RTRT_TEST_PUBLISH_VISIBLE=1 \
        RTRT_TEST_STATE="$fixture/state" RTRT_INDEX_POLL_DELAY=0 "$PUBLISH_CRATES"
    # Then: package/check sequencing remains dependency-safe and no publish is repeated.
    expected_events=
    for crate in "${crates[@]}"; do
        expected_events+="package $crate"$'\n'"lookup $crate"$'\n'
    done
    [ "$(cat "$fixture/event.log")"$'\n' = "$expected_events" ]
    [ ! -s "$fixture/publish.log" ]

    # Given: a later existing version whose sparse-index checksum differs from the local package.
    printf 'mismatch\n' > "$fixture/state/rtrt-providers"
    : > "$fixture/package.log"
    : > "$fixture/publish.log"
    : > "$fixture/event.log"
    # When/Then: verification fails fatally before packaging any later crate.
    if PATH="$fixture/bin:$PATH" CARGO_TARGET_DIR="$fixture/target" CARGO_REGISTRY_TOKEN=release-secret \
        RELEASE_VERSION=0.1.1 RTRT_TEST_REAL_CARGO="$real_cargo" RTRT_TEST_PACKAGE_LOG="$fixture/package.log" \
        RTRT_TEST_PUBLISH_LOG="$fixture/publish.log" RTRT_TEST_EVENT_LOG="$fixture/event.log" \
        RTRT_TEST_DEPENDENCIES="$fixture/dependencies.tsv" RTRT_TEST_PUBLISH_VISIBLE=1 \
        RTRT_TEST_STATE="$fixture/state" RTRT_INDEX_POLL_DELAY=0 "$PUBLISH_CRATES" >/dev/null 2>&1; then
        echo 'publish accepted a mismatched sparse-index checksum' >&2
        exit 1
    fi
    [ ! -s "$fixture/publish.log" ]
    [ "$(cat "$fixture/package.log")" = $'rtrt-core\nrtrt-providers' ]

    # Given: malformed sparse-index metadata for the first crate.
    printf 'malformed\n' > "$fixture/state/rtrt-core"
    printf 'matching\n' > "$fixture/state/rtrt-providers"
    : > "$fixture/package.log"
    : > "$fixture/publish.log"
    : > "$fixture/event.log"
    # When/Then: malformed registry metadata is fatal before publication.
    if PATH="$fixture/bin:$PATH" CARGO_TARGET_DIR="$fixture/target" CARGO_REGISTRY_TOKEN=release-secret \
        RELEASE_VERSION=0.1.1 RTRT_TEST_REAL_CARGO="$real_cargo" RTRT_TEST_PACKAGE_LOG="$fixture/package.log" \
        RTRT_TEST_PUBLISH_LOG="$fixture/publish.log" RTRT_TEST_EVENT_LOG="$fixture/event.log" \
        RTRT_TEST_DEPENDENCIES="$fixture/dependencies.tsv" RTRT_TEST_PUBLISH_VISIBLE=1 \
        RTRT_TEST_STATE="$fixture/state" RTRT_INDEX_POLL_DELAY=0 "$PUBLISH_CRATES" >/dev/null 2>&1; then
        echo 'publish accepted a malformed sparse-index checksum' >&2
        exit 1
    fi
    [ ! -s "$fixture/publish.log" ]
    [ "$(cat "$fixture/package.log")" = rtrt-core ]

    # Given: publication succeeds but the sparse index remains absent.
    rm -f "$fixture/state"/*
    : > "$fixture/package.log"
    : > "$fixture/publish.log"
    : > "$fixture/event.log"
    # When/Then: bounded polling fails before attempting the dependent crate.
    if PATH="$fixture/bin:$PATH" CARGO_TARGET_DIR="$fixture/target" CARGO_REGISTRY_TOKEN=release-secret \
        RELEASE_VERSION=0.1.1 RTRT_TEST_REAL_CARGO="$real_cargo" RTRT_TEST_PACKAGE_LOG="$fixture/package.log" \
        RTRT_TEST_PUBLISH_LOG="$fixture/publish.log" RTRT_TEST_EVENT_LOG="$fixture/event.log" \
        RTRT_TEST_DEPENDENCIES="$fixture/dependencies.tsv" RTRT_TEST_PUBLISH_VISIBLE=0 \
        RTRT_TEST_STATE="$fixture/state" RTRT_INDEX_POLL_ATTEMPTS=2 RTRT_INDEX_POLL_DELAY=0 \
        "$PUBLISH_CRATES" >/dev/null 2>&1; then
        echo 'publish accepted a version that never reached the sparse index' >&2
        exit 1
    fi
    [ "$(cat "$fixture/publish.log")" = rtrt-core ]
    [ "$(cat "$fixture/package.log")" = rtrt-core ]
    rm -rf "$fixture"
    trap - EXIT
}

[ -x "$PUBLISH_CRATES" ] || { echo 'crate publication script is missing' >&2; exit 1; }
test_publish_crates_sparse_index

test_unix_installer_rejects_checksum() {
    mode=$1
    fixture=$(mktemp -d)
    trap 'rm -rf "$fixture"' EXIT INT TERM
    mkdir -p "$fixture/bin" "$fixture/home" "$fixture/install"
    printf 'not-an-archive\n' > "$fixture/archive"
    cat > "$fixture/bin/curl" <<'EOF'
#!/usr/bin/env sh
out=""
checksum=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out=$2; shift 2 ;;
        *.sha256) checksum=1; shift ;;
        *) shift ;;
    esac
done
if [ "$checksum" -eq 1 ]; then
    if [ "$RTRT_TEST_CHECKSUM_MODE" = missing ]; then exit 22; fi
    if [ "$RTRT_TEST_CHECKSUM_MODE" = wrong-name ]; then
        digest=$(sha256sum "$RTRT_TEST_ARCHIVE" | awk '{print $1}')
        printf '%s  %s\n' "$digest" 'different-release.tar.gz' > "$out"
        exit 0
    fi
    printf '%s\n' 'not-a-sha256  release.tar.gz' > "$out"
    exit 0
fi
if [ -n "$out" ]; then
    cp "$RTRT_TEST_ARCHIVE" "$out"
    exit 0
fi
exit 2
EOF
    cat > "$fixture/bin/tar" <<'EOF'
#!/usr/bin/env sh
printf 'tar invoked\n' > "$RTRT_TEST_TAR_MARKER"
exit 99
EOF
    chmod +x "$fixture/bin/curl" "$fixture/bin/tar"
    if PATH="$fixture/bin:$PATH" HOME="$fixture/home" \
        RTRT_TEST_ARCHIVE="$fixture/archive" RTRT_TEST_CHECKSUM_MODE="$mode" \
        RTRT_TEST_TAR_MARKER="$fixture/tar-called" \
        "$INSTALL_SH" --version v0.1.1 --skip-deps --no-setup --no-service \
        --dir "$fixture/install" >"$fixture/output" 2>&1; then
        printf 'install.sh accepted %s checksum fixture\n' "$mode" >&2
        exit 1
    fi
    if [ -e "$fixture/tar-called" ]; then
        printf 'install.sh extracted before rejecting %s checksum fixture\n' "$mode" >&2
        exit 1
    fi
    rm -rf "$fixture"
    trap - EXIT INT TERM
}

test_unix_installer_rejects_checksum missing
test_unix_installer_rejects_checksum malformed
test_unix_installer_rejects_checksum wrong-name

echo 'release automation contract checks passed'
