// Opt-in: RTRT_NATIVE_OPENCODE_BIN=/absolute/path/to/opencode node --test plugins/opencode/native-v2-runtime.test.mjs
import assert from "node:assert/strict"
import { spawn, spawnSync } from "node:child_process"
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { setTimeout as delay } from "node:timers/promises"
import { fileURLToPath } from "node:url"

const root = fileURLToPath(new URL("../../", import.meta.url))
const source = fileURLToPath(new URL("./", import.meta.url))
const binary = process.env.RTRT_NATIVE_OPENCODE_BIN
const mcp = path.join(root, "target/release/rtrt-mcp")
const tmp = path.join(root, ".rtrt/tmp")

test("real v2 host loads source server plugin and connects RTRT stdio MCP", {
  skip: !binary && "set RTRT_NATIVE_OPENCODE_BIN to the absolute native OpenCode v2 binary",
  timeout: 55_000,
}, async () => {
  // Given: a git-backed project, private config/home, and the actual host executable.
  assert.ok(path.isAbsolute(binary), "RTRT_NATIVE_OPENCODE_BIN must be absolute")
  await mkdir(tmp, { recursive: true })
  const fixture = await mkdtemp(path.join(tmp, "native-v2-host-"))
  const project = path.join(fixture, "project")
  const home = path.join(fixture, "home")
  const config = path.join(fixture, "config")
  const env = {
    PATH: "/usr/bin:/bin", HOME: home, XDG_CONFIG_HOME: config,
    XDG_DATA_HOME: path.join(fixture, "data"), XDG_CACHE_HOME: path.join(fixture, "cache"),
    XDG_STATE_HOME: path.join(fixture, "state"), OPENCODE_CONFIG_DIR: path.join(config, "opencode"),
    BUN_INSTALL_CACHE_DIR: path.join(fixture, "bun-cache"), CI: "1", NO_COLOR: "1",
  }
  let child
  let output = ""
  let pluginResponse
  let mcpResponse
  let failure
  let reportedVersion
  const evidence = path.join(tmp, `native-v2-runtime-${path.basename(fixture)}.json`)
  try {
    await Promise.all([mkdir(project), mkdir(home), mkdir(env.OPENCODE_CONFIG_DIR, { recursive: true })])
    const initialized = spawnSync("git", ["init", "--quiet", "--initial-branch=main", project], {
      env: { ...env, GIT_MASTER: "1", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null" },
      encoding: "utf8", timeout: 5000,
    })
    assert.equal(initialized.status, 0, initialized.stderr || String(initialized.error))
    await writeFile(path.join(project, "opencode.json"), JSON.stringify({
      "$schema": "https://opencode.ai/config.json",
      plugins: [source],
      mcp: { servers: { rtrt: { type: "local", command: [mcp, "--transport", "stdio"] } } },
    }))
    const version = spawnSync(binary, ["--version"], { env, encoding: "utf8", timeout: 5000 })
    assert.equal(version.status, 0, version.stderr || String(version.error))
    assert.match(version.stdout, /^opencode v2\.0\.20\s*$/)
    reportedVersion = version.stdout.trim()

    // When: OpenCode itself resolves the directory's ./server export and starts MCP.
    child = spawn(binary, ["serve", "--hostname", "127.0.0.1", "--port", "0"], {
      cwd: project, env, detached: true, stdio: ["ignore", "pipe", "pipe"],
    })
    for (const stream of [child.stdout, child.stderr]) {
      stream.setEncoding("utf8")
      stream.on("data", (chunk) => { output = (output + chunk).slice(-65_536) })
    }
    const deadline = Date.now() + 38_000
    let url
    let password
    while (Date.now() < deadline) {
      if (child.exitCode !== null) throw new Error(`OpenCode exited ${child.exitCode}`)
      url = output.match(/server listening on (http:\/\/127\.0\.0\.1:\d+)/)?.[1]
      password = output.match(/server password (\S+)/)?.[1]
      if (url && password) break
      await delay(150)
    }
    assert.ok(url && password, "OpenCode did not announce an ephemeral loopback port and password")
    const get = async (endpoint) => {
      const response = await fetch(`${url}${endpoint}`, {
        headers: { Authorization: `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}` },
        signal: AbortSignal.timeout(3000),
      })
      const body = await response.text()
      assert.equal(response.status, 200, `${endpoint}: ${response.status} ${body}`)
      return JSON.parse(body)
    }
    while (Date.now() < deadline) {
      pluginResponse = await get("/api/plugin")
      mcpResponse = await get("/api/mcp")
      if (pluginResponse.data?.some((entry) => entry.id === "rtrt-agent" && entry.state?.status === "active" &&
        entry.source?.path === path.join(source, "server.js")) &&
        mcpResponse.data?.some((entry) => entry.name === "rtrt" && entry.status?.status === "connected")) break
      await delay(250)
    }

    // Then: the native plugin is active, not a classic-default load failure, and stdio connected.
    const loaded = pluginResponse.data?.find((entry) => entry.id === "rtrt-agent")
    assert.equal(loaded?.state?.status, "active", `source plugin: ${JSON.stringify(loaded)}`)
    assert.equal(loaded.source?.path, path.join(source, "server.js"))
    assert.equal(loaded.features?.tui, true, "local source plugin must expose its native TUI entry")
    assert.ok(mcpResponse.data?.some((entry) => entry.name === "rtrt" && entry.status?.status === "connected"),
      `RTRT stdio MCP not connected: ${JSON.stringify(mcpResponse)}`)
  } catch (error) {
    failure = String(error.stack ?? error)
    throw error
  } finally {
    if (child?.pid) {
      try { process.kill(-child.pid, "SIGTERM") } catch (error) { if (error.code !== "ESRCH") throw error }
      await Promise.race([new Promise((resolve) => child.once("exit", resolve)), delay(2000)])
      if (child.exitCode === null) {
        try { process.kill(-child.pid, "SIGKILL") } catch (error) { if (error.code !== "ESRCH") throw error }
      }
    }
    await writeFile(evidence, JSON.stringify({
      binary, version: reportedVersion, source, mcp,
      pluginResponse, mcpResponse, failure,
      output: output.replace(/(server password )\S+/g, "$1[redacted]"),
    }, null, 2))
    await rm(fixture, { recursive: true, force: true })
    console.log(`Real OpenCode v2 evidence: ${evidence}`)
  }
})
