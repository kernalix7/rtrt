import assert from "node:assert/strict"
import { mkdir, mkdtemp, writeFile, rm } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

import {
  createRtrtExtension,
  isEligibleBashToolResult,
} from "./rtrt.mjs"

const tempRoot = fileURLToPath(new URL("../../.rtrt/tmp/", import.meta.url))

function installExtension({ binary = "/bin/false", toolPrefix = "mcp" } = {}) {
  const handlers = new Map()
  const pi = { on: (event, handler) => handlers.set(event, handler) }
  createRtrtExtension({ binary, toolPrefix })(pi)
  return handlers
}

function ctx({ sessionId = "ses-1", cwd = "/work", signal } = {}) {
  const controller = signal ? undefined : new AbortController()
  return {
    cwd,
    sessionManager: { getSessionId: () => sessionId },
    signal: signal ?? controller.signal,
  }
}

test("factory declares the pinned MCP server through the native registration API", () => {
  // Given: installed CLI and MCP executables from native setup.
  const servers = []
  const pi = {
    on() {},
    registerMcpServer: (name, config) => servers.push({ name, config }),
  }
  // When: the host loads the extension factory.
  createRtrtExtension({ binary: "/installed/rtrt", mcpBinary: "/installed/rtrt-mcp" })(pi)
  // Then: native MCP discovery receives the exact executable without shell arguments.
  assert.deepEqual(servers, [{
    name: "rtrt", config: { type: "stdio", command: "/installed/rtrt-mcp", args: [] },
  }])
})

test("Given exact RTRT tools When tool_call runs Then provenance is isolated and stable", async () => {
  const handlers = installExtension({ toolPrefix: "acme" })
  const toolCall = handlers.get("tool_call")
  const c = ctx({ sessionId: "ses-a", cwd: "/work/a" })
  const call = {
    toolName: "acme_rtrt_agent_call",
    toolCallId: "call-1",
    input: { prompt: "go" },
  }
  const route = {
    toolName: "acme_rtrt_agent_route",
    toolCallId: "call-2",
    input: { prompt: "route" },
  }
  const bash = { toolName: "bash", toolCallId: "call-3", input: { command: "pwd" } }

  await toolCall(call, c)
  const firstId = call.input.invocation_id
  await toolCall(call, c)
  await toolCall(route, c)
  await toolCall(bash, c)

  assert.ok(firstId)
  assert.equal(call.input.invocation_id, firstId)
  assert.notEqual(route.input.invocation_id, firstId)
  assert.equal(call.input.parent_session_id, "ses-a")
  assert.equal(call.input.parent_call_id, "call-1")
  assert.equal(call.input.caller_agent, "omo-native")
  assert.equal(call.input.parent_cwd, "/work/a")
  assert.equal("parent_project" in call.input, false)
  assert.equal("parent_worktree" in call.input, false)
  assert.deepEqual(bash.input, { command: "pwd" })
})

test("Given completed or reset sessions When cleanup runs Then invocation ids rotate", async () => {
  const handlers = installExtension()
  const toolCall = handlers.get("tool_call")
  const toolResult = handlers.get("tool_result")
  const sessionStart = handlers.get("session_start")
  const c = ctx()
  const event = {
    toolName: "mcp_rtrt_agent_call",
    toolCallId: "call-1",
    input: {},
  }

  await toolCall(event, c)
  const initial = event.input.invocation_id
  await toolResult({ toolName: "mcp_rtrt_agent_call", toolCallId: "call-1" }, c)
  event.input = {}
  await toolCall(event, c)
  assert.notEqual(event.input.invocation_id, initial)

  await toolCall(event, c)
  const beforeReset = event.input.invocation_id
  await sessionStart({}, c)
  event.input = {}
  await toolCall(event, c)
  assert.notEqual(event.input.invocation_id, beforeReset)
})

test("Given eligible bash output When proxy changes stdout Then only content is patched", async (t) => {
  await mkdir(tempRoot, { recursive: true })
  const dir = await mkdtemp(path.join(tempRoot, "omo-proxy-"))
  t.after(() => rm(dir, { recursive: true, force: true }))
  await writeFile(path.join(dir, "package.json"), '{"type":"module"}')
  const binary = path.join(dir, "rtrt-stub")
  await writeFile(binary, `#!/usr/bin/env node
const args = process.argv.slice(2)
if (args[0] !== 'proxy' || args[1] !== '--') process.exit(2)
const chunks = []
process.stdin.on('data', (chunk) => chunks.push(chunk))
process.stdin.on('end', () => {
  const body = Buffer.concat(chunks).toString('utf8')
  process.stdout.write('filtered:' + body)
})
`, { mode: 0o700 })
  const handlers = installExtension({ binary })
  const toolResult = handlers.get("tool_result")
  const event = {
    toolName: "bash",
    toolCallId: "bash-1",
    isError: false,
    input: { command: "echo hi" },
    content: [{ type: "text", text: "raw-output", annotations: { audience: ["assistant"] } }],
    details: { exitCode: 0 },
    usage: { total: 1 },
  }

  const patch = await toolResult(event, ctx({ cwd: dir }))
  assert.deepEqual(patch, {
    content: [{ type: "text", text: "filtered:raw-output", annotations: { audience: ["assistant"] } }],
  })
  assert.equal(event.content[0].text, "raw-output")
  assert.equal(event.input.command, "echo hi")
  assert.equal(event.details.exitCode, 0)
})

test("Given ineligible bash results When tool_result runs Then output is preserved", async () => {
  const handlers = installExtension({ binary: "/definitely/missing/rtrt" })
  const toolResult = handlers.get("tool_result")
  const base = {
    toolName: "bash",
    toolCallId: "bash-2",
    input: { command: "echo hi" },
    content: [{ type: "text", text: "same" }],
  }
  const cases = [
    { ...base, isError: true },
    { ...base, input: { command: "echo hi", run_in_background: true } },
    { ...base, content: [{ type: "text", text: "a" }, { type: "text", text: "b" }] },
    { ...base, content: [{ type: "image", data: "x" }] },
    { ...base, input: { command: "   " } },
    { ...base, input: { command: "git status && git diff" } },
    { ...base, input: { command: "git status | cat" } },
    { ...base, input: { command: "git status\nprintf extra" } },
    { ...base, input: { command: "git status > result.txt" } },
    { ...base, input: { command: "git status $(printf extra)" } },
  ]

  for (const event of cases) {
    assert.equal(isEligibleBashToolResult(event), false)
    assert.equal(await toolResult(event, ctx()), undefined)
  }

  const unchanged = { ...base, isError: false }
  assert.equal(isEligibleBashToolResult(unchanged), true)
  assert.equal(await toolResult(unchanged, ctx()), undefined)
})
