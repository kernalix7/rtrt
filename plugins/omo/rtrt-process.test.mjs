import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import { randomUUID } from "node:crypto"
import { EventEmitter } from "node:events"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

import { filterCommandOutput } from "./rtrt.mjs"

function childStream(onInput = () => {}) {
  const child = new EventEmitter()
  child.stdout = new EventEmitter()
  child.stdin = new EventEmitter()
  child.stdin.end = (input) => queueMicrotask(() => onInput(child, input))
  child.exitCode = null
  child.kill = (signal) => { child.killedWith = signal; return true }
  return child
}

const sessionDir = path.dirname(fileURLToPath(import.meta.url))
const request = (changes = {}) => ({
  binary: process.execPath, command: "git status; do-not-execute", text: "original",
  cwd: sessionDir, ...changes,
})

test("proxy receives argv, stdin and session cwd without executing the original command", async () => {
  // Given: a real subprocess observing the proxy protocol.
  const fixture = `let text = "";
process.stdin.on("data", chunk => text += chunk);
process.stdin.on("end", () => process.stdout.write(JSON.stringify({
  args: process.argv.slice(1), text, cwd: process.cwd()
})));`
  // When: the adapter starts the executable using the native session directory.
  const output = await filterCommandOutput(request(), (binary, args, options) =>
    spawn(binary, ["-e", fixture, "--", ...args], options))
  // Then: the shell syntax remains an argument, not another executable.
  assert.deepEqual(JSON.parse(output), {
    args: ["proxy", "--", "git status; do-not-execute"],
    text: "original", cwd: sessionDir,
  })
})

test("proxy preserves Unicode split across stdout chunks", async () => {
  // Given: a UTF-8 character split inside its byte sequence.
  const bytes = Buffer.from("필터 출력")
  const child = childStream((process) => {
    process.stdout.emit("data", bytes.subarray(0, 1))
    process.stdout.emit("data", bytes.subarray(1))
    process.emit("close", 0)
  })
  // When: the process output is collected.
  const output = await filterCommandOutput(request(), () => child)
  // Then: byte chunking cannot corrupt text.
  assert.equal(output, "필터 출력")
})

test("missing or failing proxy processes preserve the original native result", async () => {
  // Given: one missing executable and one real process that exits unsuccessfully.
  const missing = path.join(process.cwd(), `.missing-${randomUUID()}`)
  // When: either process cannot produce a successful filter result.
  const absent = await filterCommandOutput(request({ binary: missing }))
  const failed = await filterCommandOutput(request(), (binary, _args, options) =>
    spawn(binary, ["-e", "process.stdin.resume(); process.exitCode = 7"], options))
  // Then: no replacement content is returned.
  assert.equal(absent, undefined)
  assert.equal(failed, undefined)
})

test("proxy cancellation kills in-flight work without replacing output", async () => {
  // Given: a process waiting for completion and a subscribed cancellation signal.
  const controller = new AbortController()
  const child = childStream()
  const result = filterCommandOutput(request({ signal: controller.signal }), () => child)
  // When: the native session cancels that work.
  controller.abort()
  // Then: cancellation is propagated immediately.
  assert.equal(await result, undefined)
  assert.equal(child.killedWith, "SIGKILL")
})

test("the total proxy deadline is enforced without timing-dependent waits", async (t) => {
  // Given: an in-flight process and a controlled deadline clock.
  t.mock.timers.enable({ apis: ["setTimeout"] })
  const child = childStream()
  const result = filterCommandOutput(request(), () => child)
  // When: exactly the documented deadline expires.
  t.mock.timers.tick(1500)
  // Then: the process cannot keep the native hook waiting.
  assert.equal(await result, undefined)
  assert.equal(child.killedWith, "SIGKILL")
})

test("stdin pipe errors preserve the native result", async () => {
  // Given: a process that closes stdin before accepting its body.
  const child = childStream((process) => process.stdin.emit("error", new Error("EPIPE")))
  // When: the proxy receives the command output.
  const result = await filterCommandOutput(request(), () => child)
  // Then: a stream error does not escape into the native runtime.
  assert.equal(result, undefined)
})

test("oversized input or output cannot replace the native result", async () => {
  // Given: output above the adapter's byte limit.
  const huge = "x".repeat(256 * 1024 + 1)
  const child = childStream((process) => process.stdout.emit("data", Buffer.from(huge)))
  // When: either side exceeds the bound.
  const input = await filterCommandOutput(request({ text: huge }))
  const output = await filterCommandOutput(request(), () => child)
  // Then: both paths leave native content intact.
  assert.equal(input, undefined)
  assert.equal(output, undefined)
})
