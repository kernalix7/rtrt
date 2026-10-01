import assert from "node:assert/strict"
import test from "node:test"
import { fileURLToPath } from "node:url"

import { createNativeServer } from "./server.js"

const directory = fileURLToPath(new URL("../../", import.meta.url))
const schedule = () => ({ cancel: () => {} })

function host(failAt = 0) {
  const callbacks = new Map()
  const tools = new Map()
  let registrations = 0
  const register = async (name, callback) => {
    if (++registrations === failAt) throw new Error("registration failed")
    callbacks.set(name, callback)
    return { dispose: async () => { callbacks.delete(name) } }
  }
  const ctx = {
    location: { directory, project: { id: "project-1", canonical: directory, directory } },
    session: { get: async () => undefined },
    permission: { hook: (name, callback) => register(`permission.${name}`, callback) },
    tool: {
      hook: (name, callback) => register(`tool.${name}`, callback),
      transform: (callback) => register("tool.transform", () => callback({ add: (tool) => tools.set(tool.name, tool) })),
    },
  }
  return { ctx, callbacks, tools }
}

test("retained callbacks cannot mutate calls or add tools after cleanup", async () => {
  // Given: the host retains native callbacks and an already registered tool.
  const h = host()
  let opens = 0
  const cleanup = await createNativeServer({
    supervisor: { ensure: async () => {}, open: async () => { opens++; return "Dashboard open requested." } },
    schedule,
  }).setup(h.ctx)
  const before = h.callbacks.get("tool.execute.before")
  const transform = h.callbacks.get("tool.transform")
  transform()
  const dashboard = h.tools.get("rtrt_dashboard_open")

  // When: cleanup runs before stale callbacks are invoked again.
  await cleanup()
  const event = { tool: "rtrt_agent_call", sessionID: "ses", id: "call", agent: "build", input: { prompt: "private" } }
  await before(event)
  let additions = 0
  transform({ add: () => { additions++ } })
  const result = await dashboard.execute({})

  // Then: no provenance, registration, or browser action survives shutdown.
  assert.deepEqual(event.input, { prompt: "private" })
  assert.equal(additions, 0)
  assert.equal(opens, 0)
  assert.deepEqual(result, { content: "Dashboard unavailable." })
})

test("cleanup surfaces disposal failures to concurrent callers while releasing other registrations", async () => {
  // Given: a host registration whose disposal fails after unregistering.
  const h = host()
  const register = h.ctx.tool.hook
  h.ctx.tool.hook = async (name, callback) => {
    const registration = await register(name, callback)
    if (name !== "execute.before") return registration
    return { dispose: async () => { await registration.dispose(); throw new Error("private disposal failure") } }
  }
  const cleanup = await createNativeServer({ supervisor: { ensure: async () => {} }, schedule }).setup(h.ctx)

  // When: host invokes cleanup concurrently.
  const results = await Promise.allSettled([cleanup(), cleanup()])

  // Then: both callers see a cleanup error, while every callback is unregistered.
  assert.deepEqual(results.map((result) => result.status), ["rejected", "rejected"])
  assert.deepEqual(results.map((result) => result.reason.message), ["RTRT native cleanup failed.", "RTRT native cleanup failed."])
  assert.deepEqual([...h.callbacks.keys()], [])
})

test("setup preserves registration error and reports a separate sanitized cleanup failure", async () => {
  // Given: the second registration and cleanup of the first both fail.
  const h = host(2)
  const register = h.ctx.tool.hook
  const reports = []
  h.ctx.tool.hook = async (name, callback) => {
    const registration = await register(name, callback)
    return { dispose: async () => { await registration.dispose(); throw new Error("private disposal failure") } }
  }

  // When: setup unwinds its acquired registration.
  await assert.rejects(createNativeServer({
    supervisor: { ensure: async () => {} }, schedule, report: (message) => reports.push(message),
  }).setup(h.ctx), /registration failed/)

  // Then: original failure remains primary and cleanup diagnostic leaks no private message.
  assert.deepEqual(reports, ["RTRT native cleanup failed."])
  assert.deepEqual([...h.callbacks.keys()], [])
})

test("dashboard startup reports only a fixed diagnostic without opening browser", async () => {
  // Given: scheduled startup fails with a credential-bearing dependency error.
  let run
  const reports = []
  let opens = 0
  const h = host()
  const cleanup = await createNativeServer({
    supervisor: { ensure: async () => { throw new Error("secret-token") }, open: async () => { opens++ } },
    schedule: (callback) => { run = callback; return { cancel: () => {} } },
    report: (message) => reports.push(message),
  }).setup(h.ctx)

  // When: the host runs the scheduled startup.
  await run()

  // Then: only a fixed diagnostic appears and no browser was opened.
  assert.deepEqual(reports, ["RTRT dashboard unavailable."])
  assert.equal(opens, 0)
  await cleanup()
})
