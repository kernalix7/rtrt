import assert from "node:assert/strict"
import { createHmac } from "node:crypto"
import test from "node:test"
import { createDashboardSupervisor } from "./runtime/dashboard-supervisor.js"
import { createDashboardPlugin } from "./runtime/dashboard-plugin.js"
import { runDashboardOpen } from "./bin/rtrt-dashboard-open.js"
import { fixture, TOKEN } from "./test-fixtures/dashboard.mjs"

test("explicit open hands browser a Rust-compatible 60-second bootstrap, never a bearer token", async (t) => {
  // Given
  const f = await fixture(t)
  await f.saveToken()
  const opened = []
  const supervisor = createDashboardSupervisor({
    env: { HOME: f.home }, probe: async () => "healthy",
    now: () => 1_000_000, randomBytes: (size) => Buffer.alloc(size, 7),
    openBrowser: async (url) => opened.push(url),
    launch: async () => assert.fail("spawn"),
  })
  // When
  const result = await supervisor.open()
  // Then
  assert.equal(opened.length, 1)
  const url = new URL(opened[0])
  assert.equal(url.origin, "http://127.0.0.1:7311")
  assert.equal(url.search, "")
  const wire = Buffer.from(url.hash.slice("#bootstrap=".length), "base64url")
  assert.equal(wire.length, 65)
  assert.equal(wire[0], 1)
  assert.equal(wire.readBigUInt64BE(1), 1000n)
  assert.equal(wire.readBigUInt64BE(9), 1060n)
  const tag = createHmac("sha256", Buffer.from(TOKEN, "hex"))
    .update("rtrt-dashboard-browser-bootstrap\0v1\0").update(wire.subarray(0, 33)).digest()
  assert.deepEqual(wire.subarray(33), tag)
  assert.equal(typeof result, "string")
  assert.equal(result.includes(TOKEN) || result.includes("http") || result.includes("bootstrap"), false)
})

test("plugin load schedules startup without awaiting it or opening browser", async () => {
  // Given
  const scheduled = []
  const calls = []
  const hook = async () => {}
  const plugin = createDashboardPlugin({
    provenance: async () => ({ "chat.message": hook }),
    supervisor: { ensure: () => { calls.push("ensure"); return new Promise(() => {}) }, open: async () => { calls.push("open"); return "opened" } },
    schedule: (callback) => scheduled.push(callback),
  })
  // When
  const hooks = await plugin({})
  scheduled[0]()
  // Then
  assert.deepEqual(calls, ["ensure"])
  assert.equal(hooks["chat.message"], hook)
  assert.equal(hooks["command.execute.before"], undefined)
  assert.equal(hooks.config, undefined)
  assert.deepEqual(Object.keys(hooks.tool), ["rtrt_dashboard_open"])
  assert.deepEqual(hooks.tool.rtrt_dashboard_open.args, {})
})

test("explicit tool invokes opener and returns only safe status", async () => {
  // Given
  let calls = 0
  const plugin = createDashboardPlugin({
    provenance: async () => ({}), schedule: () => {},
    supervisor: { ensure: async () => {}, open: async () => { calls++; return "Dashboard open requested." } },
  })
  const hooks = await plugin({})
  // When
  const result = await hooks.tool.rtrt_dashboard_open.execute({})
  // Then
  assert.equal(calls, 1)
  assert.equal(result, "Dashboard open requested.")
})

test("open redacts errors from browser dependencies", async (t) => {
  // Given
  const f = await fixture(t)
  await f.saveToken()
  const supervisor = createDashboardSupervisor({
    env: { HOME: f.home }, probe: async () => "healthy",
    openBrowser: async (url) => { throw new Error(`${TOKEN} ${url}`) },
  })
  // When
  const result = await supervisor.open()
  // Then
  assert.equal(result, "Dashboard unavailable.")
})

test("bin requests explicit open with sanitized output", async () => {
  // Given
  const output = []
  // When
  const code = await runDashboardOpen({ open: async () => { throw new Error(TOKEN) }, write: (text) => output.push(text) })
  // Then
  assert.equal(code, 1)
  assert.deepEqual(output, ["Dashboard unavailable.\n"])
})
