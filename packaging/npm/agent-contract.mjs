import assert from "node:assert/strict"
import { access, readFile, writeFile } from "node:fs/promises"
import { pathToFileURL } from "node:url"

export const platforms = Object.freeze({
  "x86_64-unknown-linux-gnu": "linux-x64",
  "aarch64-unknown-linux-gnu": "linux-arm64",
  "x86_64-apple-darwin": "darwin-x64",
  "aarch64-apple-darwin": "darwin-arm64",
  "x86_64-pc-windows-msvc": "win32-x64",
})
const manifestUrl = new URL("../../plugins/opencode/package.json", import.meta.url)

export async function agentContract() {
  const manifest = JSON.parse(await readFile(manifestUrl, "utf8"))
  assert.match(manifest.version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/)
  assert.equal(process.env.RELEASE_VERSION ?? manifest.version, manifest.version, "release version differs from source")
  const dependencies = Object.fromEntries(Object.values(platforms).map((platform) => [
    `rtrt-dashboard-${platform}`, manifest.version,
  ]))
  assert.deepEqual(manifest.optionalDependencies, dependencies, "run npm run sync:platforms after a version change")
  return manifest
}

if (process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url) {
  if (process.argv[2] === "--sync") {
    const manifest = JSON.parse(await readFile(manifestUrl, "utf8"))
    const optionalDependencies = Object.fromEntries(Object.values(platforms).map((platform) => [
      `rtrt-dashboard-${platform}`, manifest.version,
    ]))
    await writeFile(manifestUrl, `${JSON.stringify({ ...manifest, optionalDependencies }, null, 2)}\n`)
  } else {
    assert.equal(process.argv.length, 2, "usage: agent-contract.mjs [--sync]")
    const manifest = await agentContract()
    // npm silently ignores missing entries in `files`; release packaging must not.
    await Promise.all(manifest.files.map((file) => access(new URL(file, manifestUrl))))
    await Promise.all(Object.values(manifest.bin).map((file) => access(new URL(file, manifestUrl))))
    for (const hook of ["preinstall", "install", "postinstall", "prepare", "prepack"])
      assert.equal(manifest.scripts[hook], undefined, "installation/packing must not need lifecycle hooks")
  }
}
