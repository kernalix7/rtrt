import assert from "node:assert/strict"
import { chmod, copyFile, lstat, mkdir, writeFile } from "node:fs/promises"
import path from "node:path"
import { agentContract, platforms } from "./agent-contract.mjs"

assert.equal(process.argv.length, 5, "usage: stage-dashboard.mjs <rust-target> <executable> <output-directory>")
const [target, source, output] = process.argv.slice(2)
assert.ok(Object.hasOwn(platforms, target), "unsupported target")
const platform = platforms[target]
const [os, cpu] = platform.split("-")
const agent = await agentContract()
const stat = await lstat(source)
assert.ok(stat.isFile() && stat.size > 0, "dashboard source must be a nonempty regular file")
const name = `rtrt-dashboard-${platform}`
const binary = `bin/rtrt-dashboard${os === "win32" ? ".exe" : ""}`
const directory = path.resolve(output, name)
await mkdir(path.resolve(output), { recursive: true })
// Refuse stale staging directories instead of accidentally shipping earlier binaries.
await mkdir(directory)
await mkdir(path.join(directory, "bin"))
await copyFile(source, path.join(directory, binary))
await chmod(path.join(directory, binary), 0o755)
await copyFile(new URL("../../plugins/opencode/LICENSE", import.meta.url), path.join(directory, "LICENSE"))
await writeFile(path.join(directory, "package.json"), `${JSON.stringify({
  name,
  version: agent.version,
  description: `RTRT dashboard executable for ${platform}`,
  license: agent.license,
  repository: { ...agent.repository, directory: "packaging/npm" },
  os: [os],
  cpu: [cpu],
  files: [binary, "LICENSE"],
}, null, 2)}\n`)
