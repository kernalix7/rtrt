import assert from "node:assert/strict"
import { mkdtemp, lstat, open, rm, utimes, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

import { loadAgents } from "./permissions.js"

const tempRoot = fileURLToPath(new URL("../../../.rtrt/tmp/", import.meta.url))
const body = JSON.stringify({ owner: "rtrt-opencode-task-agents", version: 1, agents: ["rtrt-manager", "worker"] })

async function stateFile(t) {
  const directory = await mkdtemp(path.join(tempRoot, "v2-state-race-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const file = path.join(directory, "state.json")
  await writeFile(file, body, { mode: 0o600 })
  return file
}

test("verified state grants only its managed agents", async (t) => {
  // Given: owned, private, single-link managed state.
  const file = await stateFile(t)

  // When: the reader verifies state metadata.
  const agents = await loadAgents(file)

  // Then: only declared agents are trusted.
  assert.deepEqual([...agents], ["rtrt-manager", "worker"])
})

test("state close failure invalidates otherwise verified agents", async (t) => {
  // Given: an OS boundary whose close fails after a valid read.
  const file = await stateFile(t)
  const io = { lstat, open: async (...args) => {
    const handle = await open(...args)
    return {
      stat: (...input) => handle.stat(...input),
      readFile: (...input) => handle.readFile(...input),
      close: async () => { await handle.close(); throw new Error("close failed") },
    }
  } }

  // When: the reader closes its descriptor.
  const agents = await loadAgents(file, io)

  // Then: no authorization survives uncertain close.
  assert.equal(agents, undefined)
})

test("state path replacement after read invalidates agents", async (t) => {
  // Given: the final path identity changes between open and final lstat.
  const file = await stateFile(t)
  let fileStats = 0
  const io = { open, lstat: async (...args) => {
    const stat = await lstat(...args)
    if (args[0] !== file || ++fileStats !== 2) return stat
    return new Proxy(stat, { get: (target, key) => key === "ino" ? target.ino + 1n : Reflect.get(target, key) })
  } }

  // When: the reader verifies the path after reading bytes.
  const agents = await loadAgents(file, io)

  // Then: changed identity fails closed.
  assert.equal(agents, undefined)
})

test("state mtime change during read invalidates agents", async (t) => {
  // Given: bytes remain valid while the file timestamp changes during read.
  const file = await stateFile(t)
  const io = { lstat, open: async (...args) => {
    const handle = await open(...args)
    return {
      stat: (...input) => handle.stat(...input),
      readFile: async (...input) => {
        const raw = await handle.readFile(...input)
        await utimes(file, new Date(0), new Date(0))
        return raw
      },
      close: () => handle.close(),
    }
  } }

  // When: the reader checks metadata after reading.
  const agents = await loadAgents(file, io)

  // Then: changed mtime fails closed.
  assert.equal(agents, undefined)
})
