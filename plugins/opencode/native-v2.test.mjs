import assert from "node:assert/strict"
import { chmod, link, mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

import native, { createNativeServer } from "./server.js"

const project = fileURLToPath(new URL("../../", import.meta.url))
const tempRoot = path.join(project, ".rtrt", "tmp")

function host({ sessions = {}, failAt } = {}) {
  const callbacks = new Map()
  const disposed = []
  const tools = new Map([["other", { name: "other" }]])
  let registrations = 0
  const register = async (name, callback) => {
    if (++registrations === failAt) throw new Error("registration failed")
    callbacks.set(name, callback)
    return { dispose: async () => { disposed.push(name); callbacks.delete(name) } }
  }
  const ctx = {
    location: { directory: project, project: { id: "project-1", directory: project, canonical: project } },
    session: { get: async ({ sessionID }) => sessions[sessionID] },
    tool: {
      hook: (name, callback) => register(`tool.${name}`, callback),
      transform: (callback) => register("tool.transform", (editor = {
        list: () => [...tools.values()],
        add: (tool) => tools.set(tool.name, tool),
      }) => callback(editor)),
    },
    permission: { hook: (name, callback) => register(`permission.${name}`, callback) },
  }
  return { ctx, callbacks, disposed, tools }
}

const supervisor = () => ({ ensure: async () => {}, open: async () => "Dashboard open requested." })
const schedule = () => ({ cancel: () => {} })

test("native entrypoint has v2 id/setup and does not load classic root hooks", async () => {
  // Given: the package's native entrypoint and classic root.
  const root = await import("./index.js")
  // When: both surfaces are imported.
  // Then: native setup is distinct and root remains named-only.
  assert.equal(native.id, "rtrt-agent")
  assert.equal(typeof native.setup, "function")
  assert.deepEqual(Object.keys(root), ["RtrtProvenance"])
})

test("tool hooks stamp only RTRT calls with stable per-call provenance", async () => {
  // Given: the exact v2 hook payload with separate concurrent calls.
  const h = host()
  const cleanup = await createNativeServer({ supervisor: supervisor(), schedule }).setup(h.ctx)
  const before = h.callbacks.get("tool.execute.before")
  const after = h.callbacks.get("tool.execute.after")
  const first = { tool: "rtrt_agent_call", sessionID: "ses-a", id: "call-a", agent: "build", input: { prompt: "one" } }
  const second = { tool: "rtrt_agent_route", sessionID: "ses-a", id: "call-b", agent: "review", input: { prompt: "two" } }
  const unrelated = { tool: "bash", sessionID: "ses-a", id: "call-c", agent: "build", input: { command: "pwd" } }

  // When: hooks receive overlapping calls and complete one call.
  await Promise.all([before(first), before(second), before(unrelated)])
  const initialID = first.input.invocation_id
  await before(first)
  await after({ ...first, status: "completed", result: {} })
  first.input = { prompt: "retry" }
  await before(first)

  // Then: provenance belongs to exact call, never to other tools or completed calls.
  assert.equal(initialID.length > 0, true)
  assert.equal(first.input.invocation_id === initialID, false)
  assert.equal(second.input.invocation_id === initialID, false)
  assert.equal(second.input.caller_agent, "review")
  assert.equal(second.input.parent_session_id, "ses-a")
  assert.equal(second.input.parent_call_id, "call-b")
  assert.equal(second.input.parent_project, path.basename(project))
  assert.equal(second.input.parent_cwd, project)
  assert.deepEqual(unrelated.input, { command: "pwd" })
  assert.equal("permission_broker_url" in second.input, false)
  assert.equal("permission_broker_token" in second.input, false)
  await cleanup()
})

test("transform adds explicit dashboard-open tool without replacing prior tools or opening browser", async () => {
  // Given: a v2 tool editor with an existing tool and supervisor recording actions.
  const h = host()
  let opens = 0
  const instance = createNativeServer({
    supervisor: { ensure: async () => {}, open: async () => { opens++; return "Dashboard open requested." } },
    schedule,
  })
  const cleanup = await instance.setup(h.ctx)

  // When: the registered transform edits tools, and the operator invokes dashboard-open.
  h.callbacks.get("tool.transform")()
  const dashboard = h.tools.get("rtrt_dashboard_open")
  assert.deepEqual(dashboard.input, { type: "object", properties: {}, additionalProperties: false })
  assert.equal(h.tools.has("other"), true)
  assert.equal(opens, 0)
  const result = await dashboard.execute({})

  // Then: explicit invocation returns only fixed status; startup never opens browser.
  assert.deepEqual(result, { content: "Dashboard open requested." })
  assert.equal(opens, 1)
  await cleanup()
})

test("permission evaluation allows only exact pwd for a verified managed child", async () => {
  // Given: managed-agent state and flat v2 session.get responses.
  const directory = await mkdtemp(path.join(tempRoot, "v2-permissions-"))
  const state = path.join(directory, "managed.json")
  await writeFile(state, JSON.stringify({ owner: "rtrt-opencode-task-agents", version: 1, agents: ["rtrt-manager", "worker"] }), { mode: 0o600 })
  const sessions = {
    child: { id: "child", parentID: "parent", projectID: "project-1", agent: "worker", location: { directory: project } },
    parent: { id: "parent", projectID: "project-1", agent: "rtrt-manager", location: { directory: project } },
    foreign: { id: "foreign", parentID: "parent", projectID: "different", agent: "worker", location: { directory: project } },
  }
  try {
    const h = host({ sessions })
    const cleanup = await createNativeServer({ supervisor: supervisor(), schedule, managedAgentStatePath: state }).setup(h.ctx)
    const evaluate = h.callbacks.get("permission.evaluate")
    const cases = [
      { sessionID: "child", action: "shell", resources: ["pwd"], effect: "ask" },
      { sessionID: "child", action: "shell", resources: ["pwd; id"], effect: "ask" },
      { sessionID: "child", action: "shell", resources: ["pwd", "id"], effect: "ask" },
      { sessionID: "foreign", action: "shell", resources: ["pwd"], effect: "ask" },
      { sessionID: "missing", action: "shell", resources: ["pwd"], effect: "ask" },
      { sessionID: "child", action: "read", resources: ["pwd"], effect: "ask" },
      { sessionID: "child", action: "shell", resources: ["pwd"], effect: "deny" },
      { sessionID: "child", action: "bash", resources: ["pwd"], effect: "ask" },
    ]

    // When: the host evaluates the requests.
    await Promise.all(cases.map(evaluate))

    // Then: only managed child's exact command changes ask to allow.
    assert.deepEqual(cases.map((item) => item.effect), ["allow", "ask", "ask", "ask", "ask", "ask", "deny", "ask"])
    await cleanup()
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("missing, malformed, symlinked, and unbounded managed state never allow pwd", async (t) => {
  // Given: state that cannot prove ownership, despite a safe command.
  const directory = await mkdtemp(path.join(tempRoot, "v2-invalid-state-"))
  const missing = path.join(directory, "missing.json")
  const malformed = path.join(directory, "malformed.json")
  const linked = path.join(directory, "linked.json")
  const unbounded = path.join(directory, "unbounded.json")
  await writeFile(malformed, "{invalid")
  await writeFile(unbounded, JSON.stringify({
    owner: "rtrt-opencode-task-agents", version: 1, agents: ["rtrt-manager", "worker"],
    extra: Array(600).fill(0),
  }))
  await symlink(malformed, linked)
  try {
    for (const [label, managedAgentStatePath] of Object.entries({ missing, malformed, linked, unbounded })) {
      await t.test(label, async () => {
        const h = host({ sessions: {
          child: { id: "child", parentID: "parent", projectID: "project-1", agent: "worker", location: { directory: project } },
          parent: { id: "parent", projectID: "project-1", agent: "rtrt-manager", location: { directory: project } },
        } })
        const cleanup = await createNativeServer({ supervisor: supervisor(), schedule, managedAgentStatePath }).setup(h.ctx)
        const request = { sessionID: "child", action: "shell", resources: ["pwd"], effect: "ask" }

        // When: host evaluates the request.
        await h.callbacks.get("permission.evaluate")(request)

        // Then: unverifiable state leaves effect untouched.
        assert.equal(request.effect, "ask")
        await cleanup()
      })
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("unsafe ownership-state metadata and path ancestry never authorize pwd", async (t) => {
  // Given: valid managed-agent data under unsafe filesystem metadata.
  const directory = await mkdtemp(path.join(tempRoot, "v2-unsafe-state-"))
  const stateBody = JSON.stringify({ owner: "rtrt-opencode-task-agents", version: 1, agents: ["rtrt-manager", "worker"] })
  const permissive = path.join(directory, "permissive.json")
  const original = path.join(directory, "original.json")
  const linked = path.join(directory, "linked.json")
  const safeParent = path.join(directory, "safe-parent")
  const unsafeParent = path.join(directory, "unsafe-parent")
  const symlinkedParent = path.join(directory, "symlinked-parent")
  await writeFile(permissive, stateBody, { mode: 0o600 })
  await chmod(permissive, 0o644)
  await writeFile(original, stateBody, { mode: 0o600 })
  await link(original, linked)
  await mkdir(safeParent, { mode: 0o700 })
  await writeFile(path.join(safeParent, "state.json"), stateBody, { mode: 0o600 })
  await symlink(safeParent, symlinkedParent)
  await mkdir(unsafeParent, { mode: 0o700 })
  await chmod(unsafeParent, 0o777)
  await writeFile(path.join(unsafeParent, "state.json"), stateBody, { mode: 0o600 })
  const sessions = {
    child: { id: "child", parentID: "parent", projectID: "project-1", agent: "worker", location: { directory: project } },
    parent: { id: "parent", projectID: "project-1", agent: "rtrt-manager", location: { directory: project } },
  }
  try {
    for (const [label, managedAgentStatePath] of Object.entries({
      permissive, linked, symlinkedParent: path.join(symlinkedParent, "state.json"),
      unsafeParent: path.join(unsafeParent, "state.json"),
    })) {
      await t.test(label, async () => {
        const h = host({ sessions })
        const cleanup = await createNativeServer({ supervisor: supervisor(), schedule, managedAgentStatePath }).setup(h.ctx)
        const request = { sessionID: "child", action: "shell", resources: ["pwd"], effect: "ask" }

        // When: the host evaluates an otherwise safe command.
        await h.callbacks.get("permission.evaluate")(request)

        // Then: untrusted filesystem metadata leaves decision at ask.
        assert.equal(request.effect, "ask")
        await cleanup()
      })
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("partial setup failure unregisters acquired hooks and cancels scheduled startup", async () => {
  // Given: registration fails after first hook and a startup handle is acquired.
  const h = host({ failAt: 2 })
  let cancelled = 0
  const instance = createNativeServer({ supervisor: supervisor(), schedule: () => ({ cancel: () => { cancelled++ } }) })

  // When: native setup encounters the registration error.
  await assert.rejects(instance.setup(h.ctx), /registration failed/)

  // Then: previously registered callbacks and pending startup are gone.
  assert.deepEqual([...h.callbacks.keys()], [])
  assert.equal(h.disposed.length, 1)
  assert.equal(cancelled, 1)
})

test("cleanup prevents an in-flight permission evaluation from allowing after shutdown", async () => {
  // Given: a managed child whose session lookup is still pending.
  const directory = await mkdtemp(path.join(tempRoot, "v2-shutdown-"))
  const state = path.join(directory, "managed.json")
  await writeFile(state, JSON.stringify({ owner: "rtrt-opencode-task-agents", version: 1, agents: ["rtrt-manager", "worker"] }), { mode: 0o600 })
  let resolveSession
  const h = host()
  h.ctx.session.get = ({ sessionID }) => sessionID === "child"
    ? new Promise((resolve) => { resolveSession = resolve })
    : { id: "parent", projectID: "project-1", agent: "rtrt-manager", location: { directory: project } }
  try {
    const cleanup = await createNativeServer({ supervisor: supervisor(), schedule, managedAgentStatePath: state }).setup(h.ctx)
    const request = { sessionID: "child", action: "shell", resources: ["pwd"], effect: "ask" }
    const pending = h.callbacks.get("permission.evaluate")(request)

    // When: shutdown finishes before the session lookup resumes.
    await cleanup()
    resolveSession({ id: "child", parentID: "parent", projectID: "project-1", agent: "worker", location: { directory: project } })
    await pending

    // Then: stale evaluation cannot broaden the host's decision.
    assert.equal(request.effect, "ask")
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("cleanup disposes all native registrations exactly once", async () => {
  // Given: successful native setup.
  const h = host()
  const cleanup = await createNativeServer({ supervisor: supervisor(), schedule }).setup(h.ctx)

  // When: host invokes cleanup twice.
  await cleanup()
  await cleanup()

  // Then: each registration is disposed once, including permission and transform.
  assert.deepEqual([...h.callbacks.keys()], [])
  assert.deepEqual(h.disposed.toSorted(), ["permission.evaluate", "tool.execute.after", "tool.execute.before", "tool.transform"].toSorted())
})
