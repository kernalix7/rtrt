import assert from "node:assert/strict"
import { createHmac } from "node:crypto"
import { chmod, readFile, lstat, symlink } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { createDashboardSupervisor } from "./runtime/dashboard-supervisor.js"
import { createDashboardPlugin } from "./runtime/dashboard-plugin.js"
import { runDashboardOpen } from "./bin/rtrt-dashboard-open.js"
import { fixture as rawFixture, TOKEN } from "./test-fixtures/dashboard.mjs"
import { openDashboardBrowser } from "./runtime/dashboard-process.js"
import { windowsAcl } from "./runtime/dashboard-acl.js"

async function fixture(t) {
  const f = await rawFixture(t)
  if (process.platform !== "win32") return f
  await windowsAcl(path.join(f.home, ".rtrt"), "private-create")
  await windowsAcl(f.state, "private-create")
  return { ...f, saveToken: async () => {
    const handle = await (await import("node:fs/promises")).open(f.envFile, "wx")
    try {
      await windowsAcl(f.envFile, "private-create")
      await handle.writeFile(`RTRT_DASHBOARD_TOKEN=${TOKEN}\n`)
    } finally { await handle.close() }
  } }
}

test("explicit open hands browser a private local file containing Rust-compatible bootstrap", async (t) => {
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
  assert.equal(opened[0], path.join(f.state, "bootstrap.html"))
  if (process.platform !== "win32") assert.equal((await lstat(opened[0])).mode & 0o777, 0o600)
  const body = await readFile(opened[0], "utf8")
  assert.ok(body.length < 512)
  const urls = body.match(/http:\/\/127\.0\.0\.1:7311\/#bootstrap=[A-Za-z0-9_-]{87}/g)
  assert.equal(urls?.length, 1)
  const url = new URL(urls[0])
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

test("Windows second ordinary open atomically replaces the existing private HTML", { skip: process.platform !== "win32" }, async (t) => {
  // Given: an owner-only state and token, with the first HTML open complete.
  const f = await fixture(t)
  await f.saveToken()
  const opened = []
  const supervisor = createDashboardSupervisor({
    env: { HOME: f.home }, probe: async () => "healthy",
    openBrowser: async (file) => opened.push(file),
  })
  assert.equal(await supervisor.open(), "Dashboard open requested.")
  const first = await readFile(path.join(f.state, "bootstrap.html"), "utf8")
  // When: the same operator opens the dashboard again.
  const result = await supervisor.open()
  // Then: only local paths were launched, and the protected destination was replaced.
  assert.equal(result, "Dashboard open requested.")
  assert.deepEqual(opened, [path.join(f.state, "bootstrap.html"), path.join(f.state, "bootstrap.html")])
  assert.notEqual(await readFile(opened[1], "utf8"), first)
  await windowsAcl(opened[1], "private-check")
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

test("ordinary launch arguments are an absolute local path, never a URL or fragment", async (t) => {
  // Given
  const f = await fixture(t)
  const opened = []
  const launch = async (binary, args) => opened.push({ binary, args })
  const file = path.join(f.state, "bootstrap.html")
  // When
  await openDashboardBrowser(file, { platform: "linux", launch })
  // Then
  assert.deepEqual(opened, [{ binary: "/usr/bin/xdg-open", args: [file] }])
  await assert.rejects(openDashboardBrowser("http://127.0.0.1:7311/", { platform: "linux", launch }))
  await assert.rejects(openDashboardBrowser(path.join(f.state, "#bootstrap=example"), { platform: "linux", launch }))
})

test("private bootstrap refuses preexisting permissive file and symlink", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  await f.saveToken()
  const file = path.join(f.state, "bootstrap.html")
  await readFile(f.envFile)
  await symlink(f.envFile, file)
  const supervisor = createDashboardSupervisor({ env: { HOME: f.home }, probe: async () => "healthy", openBrowser: async () => assert.fail("opened") })
  // When / Then
  assert.equal(await supervisor.open(), "Dashboard unavailable.")
  const { unlink, writeFile } = await import("node:fs/promises")
  await unlink(file)
  await writeFile(file, "existing", { mode: 0o600 })
  await chmod(file, 0o644)
  assert.equal(await supervisor.open(), "Dashboard unavailable.")
})
