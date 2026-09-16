import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import { createHash } from "node:crypto"
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

const root = fileURLToPath(new URL("../../", import.meta.url))
for (const scenario of ["new", "existing", "mismatch", "http-error", "wrong-version"]) {
  test(`platform publication handles ${scenario} registry state`, async (t) => {
    // Given: local artifact bytes and CLI fakes at the registry/publisher boundary.
    const parent = path.join(root, ".rtrt/tmp")
    await mkdir(parent, { recursive: true })
    const directory = await mkdtemp(path.join(parent, "npm-publish-contract-"))
    t.after(() => rm(directory, { recursive: true, force: true }))
    const binaryDirectory = path.join(directory, "commands")
    await mkdir(binaryDirectory)
    const archive = path.join(directory, "package.tgz")
    const bytes = Buffer.from("packed artifact fixture")
    await writeFile(archive, bytes)
    await writeFile(path.join(directory, "registry.json"), JSON.stringify({
      name: "rtrt-dashboard-linux-x64",
      version: scenario === "wrong-version" ? "3.2.0" : "3.2.1",
      dist: { integrity: `sha512-${createHash("sha512").update(scenario === "mismatch" ? "other" : bytes).digest("base64")}` },
    }))
    await writeFile(path.join(binaryDirectory, "curl"), `#!/usr/bin/env bash
set -euo pipefail
output=''
while [ "$#" -gt 0 ]; do
  case "$1" in --output) output=$2; shift ;; esac
  shift
done
cp "$FIXTURE/registry.json" "$output"
case "$SCENARIO" in
  http-error) printf 503 ;;
  new) if [ -f "$FIXTURE/published" ]; then printf 200; else printf 404; fi ;;
  *) printf 200 ;;
esac
`, { mode: 0o755 })
    await writeFile(path.join(binaryDirectory, "npm"), `#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' "$@" > "$FIXTURE/published"
`, { mode: 0o755 })
    // When: the real release helper decides whether publication is safe/necessary.
    const result = spawnSync("bash", [path.join(root, "packaging/npm/publish-package.sh"),
      "rtrt-dashboard-linux-x64", "3.2.1", archive,
    ], { cwd: directory, encoding: "utf8", env: {
      ...process.env, PATH: `${binaryDirectory}:${process.env.PATH}`, FIXTURE: directory, SCENARIO: scenario,
    } })
    // Then: only a missing version publishes, with provenance and scripts disabled.
    assert.equal(result.status, ["new", "existing"].includes(scenario) ? 0 : 1, result.stderr)
    if (scenario === "new") {
      assert.deepEqual((await readFile(path.join(directory, "published"), "utf8")).trim().split("\n"),
        ["publish", archive, "--access", "public", "--provenance", "--ignore-scripts"])
    } else {
      await assert.rejects(readFile(path.join(directory, "published")), { code: "ENOENT" })
    }
  })
}
