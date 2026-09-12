#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFLIGHT="$ROOT/scripts/release-preflight.sh"
WORKFLOW="$ROOT/.github/workflows/release.yml"
CI_WORKFLOW="$ROOT/.github/workflows/ci.yml"
SMOKE="$ROOT/scripts/smoke.sh"
HELPERS="$ROOT/scripts/release-helpers.sh"
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

# Given: an immutable paired release tag behind a patched default-branch workflow.
recovery_fixture=$(mktemp -d)
trap 'rm -rf "$recovery_fixture"' EXIT
git clone --quiet --no-tags "$ROOT" "$recovery_fixture/repo"
(
    cd "$recovery_fixture/repo"
    git tag v0.1.1
    git tag REL-v0.1.1
    release_sha=$(git rev-parse HEAD)
    git -c user.name='Release Test' -c user.email='release-test@example.invalid' \
        commit --quiet --allow-empty -m 'test recovery workflow'
    workflow_sha=$(git rev-parse HEAD)

    # When: a publish recovery is requested from a non-default ref.
    # Then: preflight rejects it before checking out release source.
    if RELEASE_EVENT=workflow_dispatch RELEASE_REF_NAME=feature RELEASE_SHA="$workflow_sha" \
        RELEASE_RECOVERY_TAG=REL-v0.1.1 RELEASE_WORKFLOW_REF=refs/heads/feature \
        RELEASE_DEFAULT_BRANCH_REF=refs/heads/main \
        scripts/release-preflight.sh >/dev/null 2>&1; then
        echo 'release recovery accepted a non-default workflow ref' >&2
        exit 1
    fi

    # When: the default branch requests recovery for the validated REL tag.
    # Then: preflight switches to the paired tag commit and enables publication.
    recovery_output="$recovery_fixture/recovery-output"
    RELEASE_EVENT=workflow_dispatch RELEASE_REF_NAME=main RELEASE_SHA="$workflow_sha" \
        RELEASE_RECOVERY_TAG=REL-v0.1.1 RELEASE_WORKFLOW_REF=refs/heads/main \
        RELEASE_DEFAULT_BRANCH_REF=refs/heads/main GITHUB_OUTPUT="$recovery_output" \
        scripts/release-preflight.sh >/dev/null
    [ "$(git rev-parse HEAD)" = "$release_sha" ]
    grep -Fx 'version=0.1.1' "$recovery_output" >/dev/null
    grep -Fx 'version_tag=v0.1.1' "$recovery_output" >/dev/null
    grep -Fx 'publish=true' "$recovery_output" >/dev/null
    grep -Fx "source_sha=$release_sha" "$recovery_output" >/dev/null
)
rm -rf "$recovery_fixture"
trap - EXIT

python3 - "$WORKFLOW" "$SMOKE" "$CI_WORKFLOW" "$INSTALL_SH" "$INSTALL_PS1" "$HELPERS" <<'PY'
from pathlib import Path
import re
import sys

workflow = Path(sys.argv[1]).read_text()
smoke = Path(sys.argv[2]).read_text()
preflight = Path(sys.argv[1]).parents[2].joinpath("scripts/release-preflight.sh").read_text()
ci_workflow = Path(sys.argv[3]).read_text()
install_sh = Path(sys.argv[4]).read_text()
install_ps1 = Path(sys.argv[5]).read_text()
helpers = Path(sys.argv[6]).read_text()
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
    "workflow_dispatch:\n    inputs:\n      release_tag:",
    "publish-npm:\n",
    "id-token: write",
    "release:\n",
    "contents: write",
    "scripts/release-preflight.sh",
    "npm install --global npm@11.11.0",
    "npm ci --ignore-scripts",
    "npm pack --ignore-scripts --json",
    "softprops/action-gh-release@efb35369e0ad2afab669f228072c1b0d510eae64 # v3.0.3",
)
for fragment in required:
    if fragment not in workflow:
        raise SystemExit(f"missing release contract fragment: {fragment}")

recovery_required = (
    "source_sha: ${{ steps.release.outputs.source_sha }}",
    "RELEASE_RECOVERY_TAG: ${{ inputs.release_tag }}",
    "RELEASE_WORKFLOW_REF: ${{ github.ref }}",
    "RELEASE_DEFAULT_BRANCH_REF: refs/heads/${{ github.event.repository.default_branch }}",
)
for fragment in recovery_required:
    if fragment not in workflow:
        raise SystemExit(f"release recovery contract missing: {fragment}")
source_checkout = "ref: ${{ needs.preflight.outputs.source_sha }}"
if workflow.count(source_checkout) != 3:
    raise SystemExit("build, npm package, and release jobs must checkout validated release source")
if "ref: ${{ inputs.release_tag" in workflow:
    raise SystemExit("unvalidated release input must not be passed to checkout")

for forbidden in (
    "release-credentials:\n",
    "publish-crates:\n",
    "CARGO_REGISTRY_TOKEN",
    "crates-io-publish",
    "scripts/publish-crates.sh",
):
    if forbidden in workflow:
        raise SystemExit(f"obsolete crates.io release contract remains: {forbidden}")
if re.search(r"\bcargo\s+publish\b", workflow):
    raise SystemExit("release workflow must not publish workspace crates")

if workflow.index("publish-npm:\n") > workflow.index("release:\n"):
    raise SystemExit("npm publication must precede the GitHub Release job")

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

npm_job = job_block("publish-npm")
npm_dependencies = npm_job.partition("steps:")[0]
if re.search(r"(?m)^\s+needs:\s*(?:preflight|\[[^\]]*\bpreflight\b[^\]]*\])\s*$", npm_dependencies) is None:
    raise SystemExit("npm publication must need release preflight")
for fragment in (
    "if: needs.preflight.outputs.publish == 'true'",
    "id-token: write",
    "npm publish",
    "--provenance",
    "--access public",
):
    if fragment not in npm_job:
        raise SystemExit(f"npm OIDC publication contract missing: {fragment}")
local_tarball_publish = (
    'npm publish "./package/rtrt-agent-${RELEASE_VERSION}.tgz" --access public --provenance'
)
if local_tarball_publish not in npm_job:
    raise SystemExit("npm publish must use an explicit relative path for the packed tarball")

release_job = job_block("release")
release_dependencies = release_job.partition("steps:")[0]
for dependency in ("build", "publish-npm"):
    needs_dependency = re.search(
        rf"(?m)^\s+needs:\s*\[[^\]]*\b{re.escape(dependency)}\b[^\]]*\]\s*$",
        release_dependencies,
    )
    if needs_dependency is None:
        raise SystemExit(f"GitHub Release job must need {dependency}")

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

if npm_job.index("npm install --global npm@11.11.0") > npm_job.index("npm publish"):
    raise SystemExit("pinned npm CLI must be installed before npm publish")
for fragment in (".dist.integrity", ".dist.shasum", "npm-version.tgz", "openssl dgst -sha512 -binary"):
    if fragment not in npm_job:
        raise SystemExit(f"npm rerun verification must compare registry metadata to package bytes: {fragment}")

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
    'validate_paired_tags "$version_tag" "$source_sha"',
    'git checkout --quiet --detach "$source_sha"',
    "printf 'source_sha=%s\\n' \"$source_sha\"",
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
