import assert from "node:assert/strict"
import { execFile } from "node:child_process"
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { createServer } from "node:http"
import { once } from "node:events"
import { createInterface } from "node:readline"
import path from "node:path"
import { fileURLToPath } from "node:url"
import { promisify } from "node:util"
import test from "node:test"

const execute = promisify(execFile)
const native = process.env.RTRT_OMO_NATIVE_BIN
const cli = process.env.RTRT_NATIVE_RTRT_BIN
const mcp = process.env.RTRT_NATIVE_MCP_BIN
const tempRoot = fileURLToPath(new URL("../../.rtrt/tmp/", import.meta.url))

test("real OmO native discovers RTRT, filters bash output, and stamps MCP provenance", {
  skip: !native || !cli || !mcp,
  timeout: 60000,
}, async (t) => {
  // Given: isolated native state, real RTRT binaries, and a loopback-only model fixture.
  await mkdir(tempRoot, { recursive: true })
  const directory = await mkdtemp(path.join(tempRoot, "omo-native-host-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const home = path.join(directory, "home")
  const agent = path.join(home, ".omo/agent")
  const project = path.join(directory, "project")
  await mkdir(project, { recursive: true })
  await mkdir(agent, { recursive: true })
  await writeFile(path.join(directory, "package.json"), '{"type":"module"}')
  const env = {
    PATH: process.env.PATH, HOME: home, USERPROFILE: home,
    OMO_CODING_AGENT_DIR: agent, SENPI_CODING_AGENT_DIR: agent,
    PI_CODING_AGENT_DIR: agent, PI_OFFLINE: "1", NO_COLOR: "1",
    RTRT_CONFIG: path.join(home, ".rtrt/config.toml"),
    GIT_CONFIG_GLOBAL: path.join(home, "empty-gitconfig"),
  }
  await execute("git", ["init", "--quiet", project], { env })
  await writeFile(path.join(project, ".gitignore"), ".rtrt/\n.omo/\n")
  for (let index = 0; index < 20; index++) {
    await writeFile(path.join(project, `untracked-${index}.txt`), "fixture")
  }
  await execute(cli, ["setup", "--agent", "omo", "--binary", mcp, "--apply"], { cwd: project, env })
  const mcpConfig = JSON.parse(await readFile(path.join(agent, "mcp.json"), "utf8"))
  mcpConfig.settings = { searchThreshold: 1000 }
  Object.assign(mcpConfig.mcpServers.rtrt, {
    lifecycle: "eager", startupTimeoutMs: 5000, exposure: "direct", directTools: true,
  })
  await writeFile(path.join(agent, "mcp.json"), JSON.stringify(mcpConfig))
  const observations = path.join(directory, "observations.jsonl")
  await writeFile(path.join(agent, "extensions/zz-rtrt-acceptance.js"), `
import { appendFileSync } from "node:fs"
export default function (pi) {
  const record = data => appendFileSync(${JSON.stringify(observations)}, JSON.stringify(data) + "\\n")
  let resolveCatalog
  const catalogReady = new Promise(resolve => { resolveCatalog = resolve })
  pi.on("tool_activated", event => {
    record({ kind: "activated", names: event.toolNames })
    if (event.toolNames.includes("mcp_rtrt_templates_list")) resolveCatalog()
  })
  pi.rpc.handle("rtrt.acceptance.catalog", async () => {
    let deadline
    try {
      if (!pi.getAllTools().some(tool => tool.name === "mcp_rtrt_templates_list")) {
        await Promise.race([catalogReady, new Promise((_, reject) => {
          deadline = setTimeout(() => reject(new Error("Native MCP catalog activation timed out")), 15000)
        })])
      }
      const result = await pi.executeTool("mcp_rtrt_templates_list", {}, { activateInactiveTool: true })
      record({ kind: "catalog", result })
      return result
    } finally {
      clearTimeout(deadline)
    }
  })
  pi.on("session_start", (_event, ctx) => {
    record({ kind: "extensions", paths: ctx.loadedExtensionPaths })
  })
  pi.on("tool_result", (event, ctx) => {
    if (event.toolName === "bash") {
      record({ kind: "bash", content: event.content })
    }
    if (event.toolName !== "mcp_rtrt_agent_call") return
    const { invocation_id, parent_session_id, parent_call_id, parent_cwd, caller_agent } = event.input
    record({ kind: "provenance",
      invocation_id, parent_session_id, parent_call_id, parent_cwd, caller_agent,
      nativeSession: ctx.sessionManager.getSessionId()
    })
  })
}`)
  const requests = []
  let fixtureError
  const server = createServer(async (request, response) => {
    try {
      const chunks = []
      for await (const chunk of request) chunks.push(chunk)
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"))
      requests.push(body)
      response.writeHead(200, { "Content-Type": "text/event-stream" })
      const send = (delta, finish_reason = null) => response.write(`data: ${JSON.stringify({
        id: "native-fixture", object: "chat.completion.chunk", model: "fixture-v1",
        choices: [{ index: 0, delta, finish_reason }],
      })}\n\n`)
      if (requests.length === 1) {
        const names = body.tools.map((tool) => tool.function.name)
        assert.ok(names.includes("tool_search"), JSON.stringify(names))
        send({ role: "assistant", tool_calls: [
          { index: 0, id: "native_search", type: "function", function: {
            name: "tool_search", arguments: JSON.stringify({ query: "agent_call" }),
          } },
        ] })
        send({}, "tool_calls")
      } else if (requests.length === 2) {
        const names = body.tools.map((tool) => tool.function.name)
        assert.ok(names.includes("eval"), JSON.stringify(names))
        assert.ok(names.includes("mcp_rtrt_agent_call"), JSON.stringify({
          names, search: body.messages.filter((message) => message.role === "tool"),
        }))
        send({ role: "assistant", tool_calls: [
          { index: 0, id: "native_eval", type: "function", function: {
            name: "eval", arguments: JSON.stringify({
              language: "js",
              code: 'print((await tool.bash({command:"git status",run_in_background:false})).text)',
              summary: "Run a native command through codemode",
            }),
          } },
          { index: 1, id: "native_provenance", type: "function", function: {
            name: "mcp_rtrt_agent_call",
            arguments: JSON.stringify({ target: "__missing_native_acceptance__", prompt: "fixture" }),
          } },
        ] })
        send({}, "tool_calls")
      } else {
        send({ role: "assistant", content: "Acceptance complete." })
        send({}, "stop")
      }
      response.end("data: [DONE]\n\n")
    } catch (error) {
      fixtureError = error
      if (!response.headersSent) response.writeHead(200, { "Content-Type": "text/event-stream" })
      response.end(`data: ${JSON.stringify({
        id: "native-fixture-error", object: "chat.completion.chunk", model: "fixture-v1",
        choices: [{ index: 0, delta: { role: "assistant", content: "Fixture failed." }, finish_reason: "stop" }],
      })}\n\ndata: [DONE]\n\n`)
    }
  })
  t.after(() => { server.closeAllConnections(); server.close() })
  const listening = once(server, "listening")
  server.listen(0, "127.0.0.1")
  await listening
  const port = server.address().port
  await writeFile(path.join(agent, "models.json"), JSON.stringify({ providers: {
    "rtrt-fixture": {
      baseUrl: `http://127.0.0.1:${port}/v1`, api: "openai-completions", apiKey: "fixture-key",
      models: [{ id: "fixture-v1", contextWindow: 1000000, maxTokens: 4096 }],
    },
  } }))
  const { stdout: raw } = await execute("git", ["status"], { cwd: project, env })
  const expectedRun = execute(cli, ["proxy", "--", "git status"], { cwd: project, env })
  expectedRun.child.stdin.end(raw)
  const { stdout: expected } = await expectedRun
  assert.notEqual(expected, raw)

  // When: the actual native host executes the fixture model's real tool calls.
  const nativeRun = execute(native, [
    "--offline", "--no-session", "--no-skills", "--no-prompt-templates",
    "--no-themes", "--no-context-files", "--no-approve", "--no-model-fallback",
    "--permission-preset", "full-access", "--provider", "rtrt-fixture", "--model", "fixture-v1",
    "--mode", "rpc",
  ], { cwd: project, env, timeout: 45000, killSignal: "SIGKILL", maxBuffer: 8 * 1024 * 1024 })
  const send = (id, message) => nativeRun.child.stdin.write(JSON.stringify({
    id, type: "prompt", message,
  }) + "\n")
  createInterface({ input: nativeRun.child.stdout }).on("line", (line) => {
    if (!line.startsWith("{")) return
    const event = JSON.parse(line)
    if (event.type === "response" && event.id === "connect") {
      nativeRun.child.stdin.write(JSON.stringify({
        id: "catalog", type: "extension_request", name: "rtrt.acceptance.catalog", data: {},
      }) + "\n")
    } else if (event.type === "response" && event.id === "catalog") {
      if (event.success) send("run", "Run acceptance.")
      else { fixtureError = new Error(JSON.stringify(event)); nativeRun.child.stdin.end() }
    }
    else if (event.type === "agent_end") nativeRun.child.stdin.end()
  })
  send("connect", "/mcp reconnect rtrt")
  const { stdout, stderr } = await nativeRun

  // Then: discovery, filtered model-visible output, and actual invocation attribution agree.
  if (fixtureError) {
    const diagnostics = stdout.split("\n").filter((line) =>
      line.startsWith("RTRT_") || line.includes("MCP") || line.includes("error")).join("\n")
    throw new Error(`Native fixture failure: ${fixtureError.message}\n${stderr}\n${diagnostics}`, {
      cause: fixtureError,
    })
  }
  assert.equal(requests.length, 3, stderr + stdout.slice(-4000))
  const toolResults = requests[2].messages.filter((message) => message.role === "tool")
  const evaluated = toolResults.find((message) => message.tool_call_id === "native_eval")
  assert.ok(evaluated.content.includes(expected.trim()), evaluated.content)
  const records = (await readFile(observations, "utf8")).trim().split("\n").map((line) => JSON.parse(line))
  const catalog = records.find((record) => record.kind === "catalog")
  assert.notEqual(catalog.result.isError, true)
  assert.ok(catalog.result.content.length > 0)
  const bash = records.find((record) => record.kind === "bash")
  assert.equal(bash.content[0].text.trim(), expected.trim())
  const { paths } = records.find((record) => record.kind === "extensions")
  assert.ok(paths.includes(path.join(agent, "extensions/rtrt/index.js")))
  const provenance = records.find((record) => record.kind === "provenance")
  assert.match(provenance.invocation_id, /^[0-9a-f-]{36}$/)
  assert.equal(provenance.parent_session_id, provenance.nativeSession)
  assert.equal(provenance.parent_call_id, "native_provenance")
  assert.equal(provenance.parent_cwd, project)
  assert.equal(provenance.caller_agent, "omo-native")
})
