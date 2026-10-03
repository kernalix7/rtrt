import assert from "node:assert/strict"
import path from "node:path"
import test from "node:test"
import { windowsAcl } from "./runtime/dashboard-acl.js"

const systemRoot = "C:\\Windows"
const target = "C:\\Users\\fixture\\.rtrt\\dashboard"

test("Windows ACL invokes one absolute PS5 runner with a 45000ms bound and isolated environment", async () => {
  // Given: a portable injected runner rather than a Windows process.
  const calls = []
  const run = async (...args) => { calls.push(args) }

  // When: the real wrapper requests a private ACL check.
  await windowsAcl(target, "private-check", { run, systemRoot })

  // Then: one bounded, hidden, noninteractive PS5 invocation receives only policy inputs.
  assert.equal(calls.length, 1)
  const [binary, args, options] = calls[0]
  assert.equal(binary, path.win32.join(systemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe"))
  assert.equal(path.win32.isAbsolute(binary), true)
  assert.deepEqual(args.slice(0, 3), ["-NoProfile", "-NonInteractive", "-Command"])
  assert.equal(args.length, 4)
  assert.equal(typeof args[3], "string")
  assert.ok(args[3].length > 0)
  assert.deepEqual(options, {
    windowsHide: true,
    timeout: 45_000,
    maxBuffer: 4096,
    env: { SystemRoot: systemRoot, RTRT_ACL_PATH: target, RTRT_ACL_ACTION: "private-check" },
  })
})

test("Windows ACL refuses a relative target without invoking the runner", async () => {
  // Given: a valid system root and an observable injected runner.
  const calls = []
  const run = async (...args) => { calls.push(args) }

  // When: the target is not absolute.
  await assert.rejects(windowsAcl("relative\\dashboard", "private-check", { run, systemRoot }), {
    message: "Dashboard unavailable.",
  })

  // Then: no process was requested.
  assert.equal(calls.length, 0)
})

test("Windows ACL refuses a relative system root without invoking the runner", async () => {
  // Given: an absolute target and an observable injected runner.
  const calls = []
  const run = async (...args) => { calls.push(args) }

  // When: the system root is not absolute.
  await assert.rejects(windowsAcl(target, "private-check", { run, systemRoot: "relative\\Windows" }), {
    message: "Dashboard unavailable.",
  })

  // Then: no process was requested.
  assert.equal(calls.length, 0)
})

test("Windows ACL refuses a missing system root without invoking the runner", async () => {
  // Given: an absolute target and an observable injected runner.
  const calls = []
  const run = async (...args) => { calls.push(args) }

  // When: the system root is unavailable.
  await assert.rejects(windowsAcl(target, "private-check", { run, systemRoot: "" }), {
    message: "Dashboard unavailable.",
  })

  // Then: no process was requested.
  assert.equal(calls.length, 0)
})

test("Windows ACL runner failure has one attempt and exposes only generic refusal", async () => {
  // Given: a runner that rejects with secret-bearing subprocess details.
  const sentinel = "sentinel-stderr-secret-argv"
  const calls = []
  const run = async (...args) => {
    calls.push(args)
    const error = new Error(`stderr: ${sentinel}; argv: ${sentinel}`)
    error.stderr = sentinel
    error.cmd = sentinel
    throw error
  }

  // When: the real wrapper encounters a subprocess failure.
  const refusal = await windowsAcl(target, "private-check", { run, systemRoot }).then(
    () => assert.fail("subprocess failure must be refused"),
    (error) => error,
  )

  // Then: no retry/fallback, no detail or cause on the public error.
  assert.equal(calls.length, 1)
  assert.equal(refusal.message, "Dashboard unavailable.")
  assert.equal(refusal.cause, undefined)
  assert.equal(refusal.stderr, undefined)
  assert.equal(refusal.cmd, undefined)
  assert.equal(String(refusal).includes(sentinel), false)
})
