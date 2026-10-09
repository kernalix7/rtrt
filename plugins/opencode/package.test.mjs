import assert from "node:assert/strict"
import { randomUUID } from "node:crypto"
import { access, mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import test from "node:test"
import { pathToFileURL } from "node:url"

import { __createRtrtProvenanceForTest } from "./rtrt-provenance.js"

const noopClient = () => {
  const call = async () => ({ data: {} })
  return new Proxy({}, { get: () => new Proxy({}, { get: () => call }) })
}

const createTestProvenance = (context) =>
  __createRtrtProvenanceForTest(context, {
    clientFactory: () => ({}),
    managedAgentStatePath: path.join(tmpdir(), `rtrt-opencode-managed-${randomUUID()}.json`),
  })

test("package retains classic root and exposes separate native server and TUI", async () => {
  // Given: the package manifest and its self-referenced entrypoints.
  const manifest = JSON.parse(await readFile(new URL("./package.json", import.meta.url), "utf8"))

  // When: a consumer imports the package by name.
  const pluginModule = await import("rtrt-agent")
  const native = await import("rtrt-agent/server")
  const tuiEntry = import.meta.resolve("rtrt-agent/tui")

  // Then: OpenCode sees separate native entrypoints, while root stays named-only.
  assert.equal(manifest.name, "rtrt-agent")
  assert.equal(manifest.version, "0.2.3")
  assert.equal(manifest.type, "module")
  assert.equal(manifest.main, "./index.js")
  assert.deepEqual(manifest.exports, {
    ".": "./index.js",
    "./server": ["./server.js"],
    "./tui": ["./tui/v2/rtrt-statusline.tsx"],
  })
  assert.deepEqual(
    ["package.json", ...manifest.files].toSorted(),
    [
      "LICENSE", "README.ko.md", "README.md", "bin/rtrt-dashboard-open.js", "index.js",
       "package.json", "rtrt-provenance.js", "server.js", "v2/permissions.js", "runtime/dashboard-binary.js",
       "runtime/dashboard-bootstrap.js", "runtime/dashboard-acl.js", "runtime/dashboard-acl.ps1",
       "runtime/dashboard-files.js", "runtime/dashboard-plugin.js", "runtime/dashboard-process.js",
       "runtime/dashboard-supervisor.js", "tui/index.js", "tui/rtrt-statusline-core.mjs",
       "tui/rtrt-statusline-core.d.mts",
       "tui/v2/native-runtime.mjs", "tui/v2/native-state.mjs", "tui/v2/rtrt-statusline.tsx",
        "THIRD_PARTY_NOTICES",
    ].toSorted(),
  )
  assert.deepEqual(manifest.dependencies, { "@opencode-ai/sdk": "1.15.13" })
  assert.deepEqual(manifest.peerDependencies, {
    "@opencode/plugin": ">=1.18.33 <3", "@opentui/core": ">=0.4.5",
    "@opentui/solid": ">=0.4.5", "solid-js": ">=1.9.0",
  })
  assert.deepEqual(manifest.peerDependenciesMeta, {
    "@opencode/plugin": { optional: true }, "@opentui/core": { optional: true },
    "@opentui/solid": { optional: true }, "solid-js": { optional: true },
  })
  assert.deepEqual(Object.keys(pluginModule), ["RtrtProvenance"])
  assert.equal(typeof pluginModule.RtrtProvenance, "function")
  assert.equal(Object.values(pluginModule).every((entry) => typeof entry === "function"), true)
  assert.deepEqual(Object.keys(native), ["createNativeServer", "default"])
  assert.equal(typeof native.default.setup, "function")
  assert.equal(tuiEntry.endsWith("/tui/v2/rtrt-statusline.tsx"), true)
  assert.equal(manifest.scripts.test.includes("native-v2-lifecycle.test.mjs"), true)
  assert.equal(manifest.scripts.test.includes("v2/permissions-security.test.mjs"), true)
})

test("dependency-only lock keeps host peers but excludes unpublished platforms", async () => {
  // Given: npm's generated source lock and package manifest.
  const manifest = JSON.parse(await readFile(new URL("./package.json", import.meta.url), "utf8"))
  const lock = JSON.parse(await readFile(new URL("./package-lock.json", import.meta.url), "utf8"))

  // When: the release test stage consumes the root lock entry.
  const root = lock.packages[""]

  // Then: the SDK and peer metadata remain, without platform resolution during npm ci.
  assert.deepEqual(root.dependencies, manifest.dependencies)
  assert.deepEqual(root.peerDependencies, manifest.peerDependencies)
  assert.equal(root.optionalDependencies, undefined)
  assert.equal(Object.keys(lock.packages).some((name) => name.startsWith("node_modules/rtrt-dashboard-")), false)
})

test("native subpaths stay outside v1.18.33 resolver's string/object export forms", async () => {
  // Given: v1.18.33 extractExportValue accepts strings and import/default objects, not arrays.
  const manifest = JSON.parse(await readFile(new URL("./package.json", import.meta.url), "utf8"))

  // When: the manifest is prepared for both v1 and v2 hosts.
  const nativeServer = manifest.exports["./server"]
  const nativeTui = manifest.exports["./tui"]

  // Then: v1 server falls back to classic main; v2 can select native first entries.
  assert.equal(manifest.main, "./index.js")
  assert.deepEqual(nativeServer, ["./server.js"])
  assert.deepEqual(nativeTui, ["./tui/v2/rtrt-statusline.tsx"])
})

test("provenance and a sentinel plugin run sequentially in both orders", async (t) => {
  // Given: a deterministic sentinel with the same hook as the provenance plugin.
  const directory = await mkdtemp(path.join(tmpdir(), "rtrt-opencode-coexist-"))
  try {
    for (const order of [
      ["rtrt-agent", "sentinel"],
      ["sentinel", "rtrt-agent"],
    ]) {
      await t.test(order.join(" then "), async () => {
        const plugins = {
          "rtrt-agent": await createTestProvenance({
            client: noopClient(),
            directory,
            worktree: directory,
            project: {},
          }),
          sentinel: {
            "chat.params": async (_input, output) => {
              output.sentinel = true
            },
            dispose: async () => {},
          },
        }
        const output = { options: {}, temperature: 0.2, topP: 1 }
        const sequence = []

        // When: OpenCode invokes each plugin hook sequentially in configured order.
        for (const name of order) {
          sequence.push(name)
          await plugins[name]["chat.params"]?.(
            { sessionID: `coexist-${order[0]}`, agent: "build", model: { providerID: "openai", modelID: "gpt-5" } },
            output,
          )
        }

        // Then: all hooks complete in order and the sentinel's mutation survives.
        assert.deepEqual(sequence, order)
        assert.equal(output.sentinel, true)
        for (const name of order.toReversed()) await plugins[name].dispose?.()
      })
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("provenance, sentinel, and oh-my-openagent 4.19.4 coexist in both orders", async (t) => {
  // Given: an explicitly provided local package directory for optional local compatibility coverage.
  const configured = process.env.RTRT_OMO_PACKAGE_DIR
  if (configured === undefined) {
    t.skip("set RTRT_OMO_PACKAGE_DIR to run the real oh-my-openagent compatibility test")
    return
  }
  const omoDirectory = path.resolve(configured)
  await access(path.join(omoDirectory, "dist", "index.js"))
  const manifest = JSON.parse(await readFile(path.join(omoDirectory, "package.json"), "utf8"))
  assert.equal(manifest.version, "4.19.4")
  const { omoPlugin } = await import(pathToFileURL(path.join(omoDirectory, "dist", "index.js")))
  const directory = await mkdtemp(path.join(tmpdir(), "rtrt-opencode-omo-"))
  const previousTelemetry = process.env.OMO_DISABLE_POSTHOG
  process.env.OMO_DISABLE_POSTHOG = "1"

  try {
    for (const order of [
      ["rtrt-agent", "sentinel", "omo"],
      ["omo", "sentinel", "rtrt-agent"],
    ]) {
      await t.test(order.join(" then "), async () => {
        const context = {
          client: noopClient(),
          directory,
          worktree: directory,
          project: {},
          serverUrl: new URL("http://127.0.0.1:4096"),
          $: () => {},
        }
        const plugins = {
          omo: await omoPlugin(context),
          "rtrt-agent": await createTestProvenance(context),
          sentinel: {
            "chat.params": async (_input, output) => {
              output.sentinel = true
            },
            dispose: async () => {},
          },
        }
        const output = { options: {}, temperature: 0.2, topP: 1 }
        const sequence = []

        // When: OpenCode invokes all real and sentinel hooks sequentially.
        for (const name of order) {
          sequence.push(name)
          await plugins[name]["chat.params"]?.(
            { sessionID: `omo-${order[0]}`, agent: "build", model: { providerID: "openai", modelID: "gpt-5" } },
            output,
          )
        }

        // Then: all hooks complete in order and preserve prior mutations.
        assert.deepEqual(sequence, order)
        assert.equal(output.sentinel, true)
        for (const name of order.toReversed()) await plugins[name].dispose?.()
      })
    }
  } finally {
    if (previousTelemetry === undefined) delete process.env.OMO_DISABLE_POSTHOG
    else process.env.OMO_DISABLE_POSTHOG = previousTelemetry
    await rm(directory, { recursive: true, force: true })
  }
})
