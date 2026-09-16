import assert from "node:assert/strict"
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises"
import path from "node:path"

// Install the locked SDK separately: unpublished optional binaries must neither
// be resolved by npm ci nor added to the source lockfile during release tests.
assert.equal(process.argv.length, 3, "usage: stage-test-dependencies.mjs <output-directory>")
const lockUrl = new URL("../../plugins/opencode/package-lock.json", import.meta.url)
const lock = JSON.parse(await readFile(lockUrl, "utf8"))
const agent = JSON.parse(await readFile(new URL("../../plugins/opencode/package.json", import.meta.url), "utf8"))
assert.deepEqual(lock.packages[""].dependencies, agent.dependencies, "SDK lockfile differs from source")
assert.equal(lock.version, agent.version, "lockfile version differs from source")
const directory = path.resolve(process.argv[2])
await mkdir(directory, { recursive: true })
await copyFile(lockUrl, path.join(directory, "package-lock.json"))
await writeFile(path.join(directory, "package.json"), `${JSON.stringify(lock.packages[""], null, 2)}\n`)
