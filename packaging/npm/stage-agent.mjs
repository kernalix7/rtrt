import assert from "node:assert/strict"
import { chmod, copyFile, cp, mkdir, writeFile } from "node:fs/promises"
import path from "node:path"
import { agentContract } from "./agent-contract.mjs"

assert.equal(process.argv.length, 3, "usage: stage-agent.mjs <output-directory>")
const agent = await agentContract()
const directory = path.resolve(process.argv[2])
await mkdir(path.dirname(directory), { recursive: true })
await mkdir(directory)
for (const file of agent.files) {
  assert.ok(!path.isAbsolute(file) && !file.split(/[\\/]/).includes(".."), "package file must be relative")
  const destination = path.join(directory, file)
  await mkdir(path.dirname(destination), { recursive: true })
  if (file === "THIRD_PARTY_NOTICES")
    await cp(new URL("../../THIRD_PARTY_NOTICES", import.meta.url), destination, { recursive: true })
  else await copyFile(new URL(`../../plugins/opencode/${file}`, import.meta.url), destination)
}
for (const binary of Object.values(agent.bin)) {
  assert.ok(agent.files.includes(binary.replace(/^\.\//, "")), "bin must be a declared package file")
  await chmod(path.join(directory, binary), 0o755)
}
// The source manifest's only scripts are dev-only (`test`, `sync:platforms`) and no
// lifecycle hook is needed at install or pack time, so the published manifest omits
// `scripts` entirely. Shallow-copy first so the agentContract source object is untouched.
const published = { ...agent }
delete published.scripts
await writeFile(path.join(directory, "package.json"), `${JSON.stringify(published, null, 2)}\n`)
