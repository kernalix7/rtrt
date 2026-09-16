import assert from "node:assert/strict"
import { chmod, lstat, mkdir, readFile, symlink, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { createDashboardSupervisor } from "./runtime/dashboard-supervisor.js"
import { fixture, TOKEN } from "./test-fixtures/dashboard.mjs"

function dependencies(f, overrides = {}) {
  return {
    env: { HOME: f.home },
    probe: async () => "healthy",
    resolveBinary: async () => { throw new Error("unexpected resolution") },
    launch: async () => { throw new Error("unexpected spawn") },
    openBrowser: async () => { throw new Error("unexpected browser") },
    wait: async () => {},
    ...overrides,
  }
}

test("ensure retains existing token when dashboard is healthy", async (t) => {
  // Given
  const f = await fixture(t)
  await f.saveToken()
  const supervisor = createDashboardSupervisor(dependencies(f))
  // When
  await supervisor.ensure()
  // Then
  assert.equal(await readFile(f.envFile, "utf8"), `RTRT_DASHBOARD_TOKEN=${TOKEN}\n`)
})

test("ensure probes again under lock and spawns only once for concurrent calls", async (t) => {
  // Given
  const f = await fixture(t)
  const calls = []
  let running = false
  const supervisor = createDashboardSupervisor(dependencies(f, {
    probe: async () => { calls.push("probe"); return running ? "healthy" : "offline" },
    resolveBinary: async () => { calls.push("resolve"); return "/exact/npm/bin/rtrt-dashboard" },
    launch: async (binary, args, options) => {
      assert.equal((await lstat(path.join(f.state, "startup.lock"))).mode & 0o777, 0o700)
      assert.match(await readFile(f.envFile, "utf8"), /^RTRT_DASHBOARD_TOKEN=[a-f0-9]{64}\n$/)
      calls.push({ binary, args, options })
      running = true
    },
  }))
  // When
  await Promise.all([supervisor.ensure(), supervisor.ensure()])
  // Then
  assert.deepEqual(calls.slice(0, 3), ["probe", "probe", "resolve"])
  const spawned = calls.filter((call) => typeof call === "object")
  assert.equal(spawned.length, 1)
  assert.deepEqual(spawned[0].args, ["--machine", "--state-dir", f.state])
  assert.equal(spawned[0].options.env.RTRT_DASHBOARD_BIND, "127.0.0.1:7311")
  assert.equal(spawned[0].options.env.RTRT_DASHBOARD_TOKEN, undefined)
  assert.equal(spawned[0].options.cwd, f.home)
})

test("ensure does not spawn when another starter wins before lock", async (t) => {
  // Given
  const f = await fixture(t)
  await f.saveToken()
  let probes = 0
  const supervisor = createDashboardSupervisor(dependencies(f, {
    probe: async () => ++probes === 1 ? "offline" : "healthy",
  }))
  // When
  await supervisor.ensure()
  // Then
  assert.equal(probes, 2)
})

for (const target of [".rtrt", ".rtrt/dashboard", ".rtrt/dashboard/dashboard.env", ".rtrt/dashboard/startup.lock"]) {
  test(`ensure refuses symlink at ${target}`, async (t) => {
    // Given
    const f = await fixture(t)
    const home = path.join(f.home, "linked-home")
    await mkdir(home, { mode: 0o700 })
    const candidate = path.join(home, target)
    await mkdir(path.dirname(candidate), { recursive: true, mode: 0o700 })
    await symlink(f.state, candidate)
    const supervisor = createDashboardSupervisor(dependencies({ home }))
    // When / Then
    await assert.rejects(supervisor.ensure(), /Dashboard unavailable/)
  })
}

for (const target of [".rtrt", ".rtrt/dashboard", ".rtrt/dashboard/dashboard.env", ".rtrt/dashboard/startup.lock"]) {
  test(`ensure refuses permissive Unix mode at ${target}`, { skip: process.platform === "win32" }, async (t) => {
    // Given
    const f = await fixture(t)
    await f.saveToken()
    await mkdir(path.join(f.state, "startup.lock"), { mode: 0o700 })
    await chmod(path.join(f.home, target), 0o777)
    const supervisor = createDashboardSupervisor(dependencies(f))
    // When / Then
    await assert.rejects(supervisor.ensure(), /Dashboard unavailable/)
  })
}

test("ensure refuses wrong Unix owner", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const supervisor = createDashboardSupervisor(dependencies(f, { uid: process.geteuid() + 1 }))
  // When / Then
  await assert.rejects(supervisor.ensure(), /Dashboard unavailable/)
})

test("ensure refuses malformed credentials instead of replacing them", async (t) => {
  // Given
  const f = await fixture(t)
  await writeFile(f.envFile, "RTRT_DASHBOARD_TOKEN=invalid\n", { mode: 0o600 })
  const supervisor = createDashboardSupervisor(dependencies(f))
  // When / Then
  await assert.rejects(supervisor.ensure(), /Dashboard unavailable/)
})

test("ensure refuses foreign listener without generating a token", async (t) => {
  // Given
  const f = await fixture(t)
  const supervisor = createDashboardSupervisor(dependencies(f, { probe: async () => "conflict" }))
  // When / Then
  await assert.rejects(supervisor.ensure(), /Dashboard unavailable/)
  await assert.rejects(lstat(f.envFile), { code: "ENOENT" })
})

test("ensure fails boundedly when a private lock is held", async (t) => {
  // Given
  const f = await fixture(t)
  await mkdir(path.join(f.state, "startup.lock"), { mode: 0o700 })
  const supervisor = createDashboardSupervisor(dependencies(f, { probe: async () => "offline" }))
  // When / Then
  await assert.rejects(supervisor.ensure(), /Dashboard unavailable/)
})
