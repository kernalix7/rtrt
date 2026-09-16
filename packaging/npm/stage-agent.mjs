import assert from "node:assert/strict"
import { chmod, copyFile, mkdir, writeFile } from "node:fs/promises"
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
  await copyFile(new URL(`../../plugins/opencode/${file}`, import.meta.url), destination)
}
for (const binary of Object.values(agent.bin)) {
  assert.ok(agent.files.includes(binary.replace(/^\.\//, "")), "bin must be a declared package file")
  await chmod(path.join(directory, binary), 0o755)
}
await writeFile(path.join(directory, "package.json"), `${JSON.stringify(agent, null, 2)}\n`)
