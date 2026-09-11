import assert from "node:assert/strict"
import { randomUUID } from "node:crypto"
import { mkdir, mkdtemp, rename, rm, stat, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import http from "node:http"
import test from "node:test"

import * as provenance from "./rtrt-provenance.js"

const {
  RtrtProvenance,
  __createRtrtProvenanceForTest,
  __resolveManagedAgentStatePathForTest,
} = provenance

const testClientFactory = () => ({
  permission: { list: async () => ({ data: [] }) },
  session: {
    permission: { create: async () => ({ data: { data: { effect: "deny" } } }) },
  },
})

const testLegacyClient = () => ({
  session: {
    abort: async () => ({ data: true }),
    get: async ({ path: { id } }) => ({ data: { id, agent: "rtrt-manager" } }),
  },
})

const isolatedManagedAgentStatePath = () =>
  path.join(tmpdir(), `rtrt-opencode-managed-${randomUUID()}.json`)

const createTestPlugin = (input) =>
  __createRtrtProvenanceForTest(
    { ...input, client: input.client ?? testLegacyClient() },
    {
      clientFactory: testClientFactory,
      managedAgentStatePath: isolatedManagedAgentStatePath(),
    },
  )

const makeTempDir = (prefix) => mkdtemp(path.join(tmpdir(), prefix))

const makeRepository = async (prefix) => {
  const repository = await makeTempDir(prefix)
  await mkdir(path.join(repository, ".git"))
  return repository
}

const makeLinkedRepository = async (prefix) => {
  const fixture = await makeTempDir(prefix)
  const main = path.join(fixture, "main-repository")
  const linked = path.join(fixture, "linked-worktree")
  const admin = path.join(main, ".git", "worktrees", "linked")
  await mkdir(admin, { recursive: true })
  await mkdir(linked)
  await writeFile(path.join(linked, ".git"), `gitdir: ${admin}\n`)
  await writeFile(path.join(admin, "commondir"), "../..\n")
  await writeFile(path.join(admin, "gitdir"), `${path.join(linked, ".git")}\n`)
  return { fixture, main, linked }
}

const hostTempEnv = () => ({
  TMPDIR: "host-tmpdir",
  TEMP: "host-temp",
  TMP: "host-tmp",
})

test("module exports the plugin and injected test constructor", () => {
  assert.deepEqual(Object.keys(provenance).sort(), [
    "RtrtProvenance",
    "__createRtrtProvenanceForTest",
    "__resolveManagedAgentStatePathForTest",
  ])
  assert.equal(typeof RtrtProvenance, "function")
  assert.equal(typeof __resolveManagedAgentStatePathForTest, "function")
})

test("plugin preserves every provenance hook with an async injected client factory", async () => {
  const hooks = await __createRtrtProvenanceForTest(
    { project: {}, client: testLegacyClient() },
    {
      clientFactory: async () => testClientFactory(),
      managedAgentStatePath: isolatedManagedAgentStatePath(),
    },
  )
  assert.deepEqual(Object.keys(hooks).sort(), [
    "chat.message",
    "chat.params",
    "dispose",
    "event",
    "permission.ask",
    "shell.env",
    "tool.execute.after",
    "tool.execute.before",
  ])
  await hooks.dispose()
})

test("shell.env confines temp variables and sanitizes traversal through plugin hooks", async () => {
  const repository = await makeRepository("rtrt-provenance-repo-")
  try {
    const hooks = await createTestPlugin({
      project: { worktree: repository },
      directory: path.join(repository, "nested"),
      worktree: repository,
    })
    await hooks["chat.message"]({ sessionID: "../unsafe/session", agent: "builder" })

    const output = { env: {} }
    await hooks["shell.env"](
      { sessionID: "../unsafe/session", callID: "call-1", cwd: repository },
      output,
    )

    const tempRoot = path.join(repository, ".rtrt", "tmp", "opencode")
    assert.equal(path.dirname(output.env.TMPDIR), tempRoot)
    assert.match(path.basename(output.env.TMPDIR), /^[A-Za-z0-9_-]+$/)
    assert.equal(output.env.TMPDIR.includes(`..${path.sep}`), false)
    assert.equal(output.env.TEMP, output.env.TMPDIR)
    assert.equal(output.env.TMP, output.env.TMPDIR)
    assert.equal(output.env.RTRT_PARENT_WORKTREE, repository)
    assert.equal(output.env.RTRT_PARENT_SESSION_ID, "../unsafe/session")
    assert.equal(output.env.RTRT_PARENT_CALL_ID, "call-1")
    assert.equal(output.env.RTRT_PARENT_AGENT, "builder")
    assert.equal(output.env.RTRT_OPENCODE_PLUGIN_ACTIVE, "1")
    assert.equal((await stat(output.env.TMPDIR)).isDirectory(), true)
    if (process.platform !== "win32") {
      assert.equal((await stat(output.env.TMPDIR)).mode & 0o777, 0o700)
    }

    const toolOutput = { args: {} }
    await hooks["tool.execute.before"](
      {
        tool: "rtrt_agent_route",
        sessionID: "../unsafe/session",
        callID: "call-1",
      },
      toolOutput,
    )
    assert.equal(toolOutput.args.invocation_id, output.env.RTRT_INVOCATION_ID)
    assert.equal(toolOutput.args.parent_worktree, repository)
    assert.equal(toolOutput.args.caller_agent, "builder")

    await hooks["tool.execute.after"]({ callID: "call-1" })
    const nextOutput = { env: {} }
    await hooks["shell.env"](
      { sessionID: "../unsafe/session", callID: "call-1", cwd: repository },
      nextOutput,
    )
    assert.notEqual(nextOutput.env.RTRT_INVOCATION_ID, output.env.RTRT_INVOCATION_ID)
  } finally {
    await rm(repository, { recursive: true, force: true })
  }
})

test("filesystem root and no-VCS project worktrees preserve host temp variables", async (t) => {
  const noVcs = await makeTempDir("rtrt-provenance-no-vcs-")
  try {
    for (const [name, worktree] of [
      ["filesystem root", path.parse(process.cwd()).root],
      ["directory without VCS metadata", noVcs],
    ]) {
      await t.test(name, async () => {
        const hooks = await createTestPlugin({
          project: { worktree },
          directory: noVcs,
          worktree: noVcs,
        })
        const output = { env: hostTempEnv() }
        await hooks["shell.env"](
          { sessionID: "session", callID: `call-${name}`, cwd: noVcs },
          output,
        )

        assert.equal(output.env.TMPDIR, "host-tmpdir")
        assert.equal(output.env.TEMP, "host-temp")
        assert.equal(output.env.TMP, "host-tmp")
        assert.equal(output.env.RTRT_PARENT_WORKTREE, undefined)
        assert.equal(output.env.RTRT_PARENT_PROJECT, undefined)
        assert.match(output.env.RTRT_INVOCATION_ID, /^[0-9a-f-]{36}$/)

        const toolOutput = { args: {} }
        await hooks["tool.execute.before"](
          { tool: "rtrt_agent_call", sessionID: "session", callID: `call-${name}` },
          toolOutput,
        )
        assert.equal(toolOutput.args.parent_worktree, undefined)
        assert.equal(toolOutput.args.parent_project, undefined)
      })
    }
  } finally {
    await rm(noVcs, { recursive: true, force: true })
  }
})

test(
  "shell.env rejects symlinked .rtrt and tmp roots",
  { skip: process.platform === "win32" },
  async (t) => {
    const fixture = await makeTempDir("rtrt-provenance-symlink-")
    try {
      for (const target of [".rtrt", path.join(".rtrt", "tmp")]) {
        await t.test(target, async () => {
          const repository = path.join(fixture, target.replaceAll(path.sep, "-").replace(".", ""))
          const outside = path.join(fixture, `${path.basename(repository)}-outside`)
          await mkdir(path.join(repository, ".git"), { recursive: true })
          await mkdir(outside)
          if (target !== ".rtrt") await mkdir(path.join(repository, ".rtrt"))
          await symlink(outside, path.join(repository, target), "dir")

          const hooks = await createTestPlugin({
            project: { worktree: repository },
            directory: repository,
          })
          const output = { env: hostTempEnv() }
          await assert.rejects(
            hooks["shell.env"](
              { sessionID: "session", callID: `call-${target}`, cwd: repository },
              output,
            ),
            /not a secure directory/,
          )
          assert.deepEqual(
            { TMPDIR: output.env.TMPDIR, TEMP: output.env.TEMP, TMP: output.env.TMP },
            hostTempEnv(),
          )
        })
      }
    } finally {
      await rm(fixture, { recursive: true, force: true })
    }
  },
)

test("permission.ask auto-allows only bounded project-local safe Bash commands", async (t) => {
  const repository = await makeRepository("rtrt-provenance-permission-")
  const outside = await makeTempDir("rtrt-provenance-outside-")
  await mkdir(path.join(repository, "src"))
  await writeFile(path.join(repository, "src", "lib.rs"), "pub fn value() {}\n")
  const hooks = await createTestPlugin({ project: { worktree: repository }, directory: repository })
  const decision = async (pattern, status = "ask", input = {}) => {
    const output = { status }
    await hooks["permission.ask"]({ type: "bash", pattern, sessionID: "permission-session", ...input }, output)
    return output.status
  }
  try {
    const safe = ["pwd", "pwd -L", "pwd -P"]
    for (const command of safe) {
      await t.test(`allow ${command}`, async () => assert.equal(await decision(command), "allow"))
    }

    // Path-bearing readers are resolved again by the shell after this hook
    // approves them, so a symlink swapped in between would redirect the read.
    const pathBearingReaders = [
      "ls -la src",
      "cat src/lib.rs",
      `stat "${path.join(repository, "src", "lib.rs")}"`,
      "head -n 2 src/lib.rs",
      "tail -n 2 src/lib.rs",
      "wc -l src/lib.rs",
    ]
    for (const command of pathBearingReaders) {
      await t.test(`ask ${command}`, async () => assert.equal(await decision(command), "ask"))
    }
    assert.equal(await decision(["pwd", "cat src/lib.rs"]), "ask")

    const braceSyntax = [
      ["balanced", "cat src/{lib.rs,missing.rs}"],
      ["unmatched opening", "cat src/{lib.rs"],
      ["unmatched closing", "cat src/lib.rs}"],
      ["quoted", 'cat "src/{lib.rs}"'],
      ["escaped", String.raw`cat src/\{lib.rs\}`],
      ["traversal expansion", "cat .{.,}/secret"],
    ]
    for (const [kind, command] of braceSyntax) {
      await t.test(`ask for ${kind} brace syntax`, async () => {
        assert.equal(await decision(command), "ask")
      })
    }

    const unsafe = [
      `cat ${path.join(outside, "secret")}`,
      "cat ../secret",
      "cat src/lib.rs > copy",
      "git status && rm -rf .",
      "cat $HOME/.ssh/config",
      "cat `pwd`",
      "echo $(pwd)",
      "ls *.rs",
      "rm src/lib.rs",
      "rmdir src",
      "truncate -s 0 src/lib.rs",
      "chmod 777 src/lib.rs",
      "npm install",
      "npm run lint",
      "pnpm add package",
      "pnpm build",
      "yarn test",
      "cargo fmt",
      "cargo fmt --check",
      "cargo fmt -- --config-path ../rustfmt.toml",
      "cargo +nightly fmt",
      "cargo build",
      "cargo test",
      "cargo clippy",
      "go test ./...",
      "pytest src",
      "rustfmt src/lib.rs",
      "prettier src/lib.rs",
      "prettier --config ../prettier.json src/lib.rs",
      "eslint src/lib.rs",
      "git status",
      "git -c core.pager=cat status",
      "cat",
      "head -n 2",
      "head --lines=/etc/passwd src/lib.rs",
      `head -n 2 ${path.join(outside, "secret")}`,
      "curl https://example.com",
      "wget https://example.com",
      "sudo ls",
      "docker build .",
      "python -c 'print(1)'",
      "make test",
      "find src -delete",
      "find src -exec cat {} +",
      "find src -execdir cat {} +",
      "find src -ok cat {} +",
      "tree -o listing.txt src",
      `cargo test --manifest-path=${path.join(outside, "Cargo.toml")}`,
      "git commit -m change",
      "git checkout main",
      "unknown-command src",
      "cat 'unterminated",
      `cat ${"a".repeat(8_193)}`,
      "cat src\nrm src/lib.rs",
    ]
    for (const command of unsafe) {
      await t.test(`ask ${command.slice(0, 60)}`, async () => assert.equal(await decision(command), "ask"))
    }
    assert.equal(await decision(["pwd", "rm src/lib.rs"]), "ask")
    assert.equal(await decision("pwd", "deny"), "deny")
    assert.equal(await decision("pwd", "allow"), "allow")
    assert.equal(await decision(undefined), "ask")
    assert.equal(await decision([]), "ask")
    assert.equal(await decision(["git status", 3]), "ask")
    assert.equal(await decision("git status", "ask", { type: "read" }), "ask")
    const aliasOutput = { status: "ask" }
    await hooks["permission.ask"]({ permission: "bash", pattern: "pwd", sessionID: "permission-session" }, aliasOutput)
    assert.equal(aliasOutput.status, "allow")
  } finally {
    await hooks.dispose()
    await rm(repository, { recursive: true, force: true })
    await rm(outside, { recursive: true, force: true })
  }
})

test("permission.asked replies once only for exact safe bash requests and deduplicates races", async () => {
  const repository = await makeRepository("rtrt-provenance-permission-event-")
  const replies = []
  let release
  const gate = new Promise((resolve) => { release = resolve })
  try {
    await mkdir(path.join(repository, "src"))
    await writeFile(path.join(repository, "src", "lib.rs"), "pub fn value() {}\n")
    const hooks = await __createRtrtProvenanceForTest(
      {
        project: { worktree: repository },
        directory: repository,
        client: {
          session: {
            get: async ({ path: { id } }) => ({ data: { data:
              id === "session_1" || id === "manager"
                ? { id, agent: "rtrt-manager" }
                : id === "managed_child"
                  ? { id, parentID: "manager", agent: "worker" }
                  : { id, agent: "builder" },
            } }),
            abort: async () => ({ data: true }),
          },
        },
      },
      {
        clientFactory: () => ({
          permission: {
            list: async () => ({ data: [
              { id: "permission_request_1", sessionID: "session_1", permission: "bash", patterns: ["pwd", "pwd -L"] },
              { id: "sdk_failure", sessionID: "session_1", permission: "bash", patterns: ["pwd"] },
              { id: "child_request", sessionID: "managed_child", permission: "bash", patterns: ["pwd"] },
              { id: "brace_expansion", sessionID: "session_1", permission: "bash", patterns: ["cat .{.,}/secret"] },
            ] }),
            reply: async (request) => {
              replies.push(request)
              if (request.requestID === "sdk_failure") throw new Error("unavailable")
              await gate
              return { data: true }
            },
          },
          session: {
            permission: { create: async () => ({ data: { data: { effect: "deny" } } }) },
          },
        }),
        managedAgentStatePath: isolatedManagedAgentStatePath(),
      },
    )
    const asked = sessionEvent("permission.asked", {
      id: "permission_request_1",
      sessionID: "session_1",
      permission: "bash",
      patterns: ["pwd", "pwd -L"],
      metadata: { command: "git status" },
      always: [],
      tool: { messageID: "message_1", callID: "call_1" },
    })
    const first = hooks.event(asked)
    const duplicate = hooks.event(asked)
    while (replies.length === 0) await new Promise((resolve) => setImmediate(resolve))
    assert.deepEqual(replies, [{ requestID: "permission_request_1", reply: "once" }])
    release()
    await Promise.all([first, duplicate])
    await hooks.event(asked)
    assert.equal(replies.length, 1)

    const rejected = [
      { ...asked.event.properties, id: "unsafe", patterns: ["git status && rm -rf ."] },
      { ...asked.event.properties, id: "compound", patterns: ["git status", "rm src/lib.rs"] },
      { ...asked.event.properties, id: "external", patterns: ["cat ../secret"] },
      { ...asked.event.properties, id: "path_bearing_reader", patterns: ["cat src/lib.rs"] },
      { ...asked.event.properties, id: "brace_expansion", patterns: ["cat .{.,}/secret"] },
      { ...asked.event.properties, id: "ambiguous", patterns: ["echo ok"] },
      { ...asked.event.properties, id: "non_bash", permission: "Bash" },
      { ...asked.event.properties, id: "missing_patterns", patterns: [] },
      { ...asked.event.properties, id: "malformed_patterns", patterns: ["git status", 7] },
      { ...asked.event.properties, id: "bad/id" },
      { ...asked.event.properties, id: "missing_session", sessionID: "" },
      { ...asked.event.properties, id: "forged_request", patterns: ["pwd"] },
      { ...asked.event.properties, id: "permission_request_1", sessionID: "unrelated", patterns: ["pwd"] },
    ]
    for (const properties of rejected) {
      await hooks.event(sessionEvent("permission.asked", properties))
    }
    assert.equal(replies.length, 1)
    const sdkFailure = sessionEvent("permission.asked", {
      ...asked.event.properties,
      id: "sdk_failure",
      patterns: ["pwd"],
    })
    await hooks.event(sdkFailure)
    await hooks.event(sdkFailure)
    assert.deepEqual(replies.at(-1), { requestID: "sdk_failure", reply: "once" })
    assert.equal(replies.length, 2)
    await hooks.event(sessionEvent("permission.asked", {
      id: "child_request", sessionID: "managed_child", permission: "bash", patterns: ["pwd"],
    }))
    assert.deepEqual(replies.at(-1), { requestID: "child_request", reply: "once" })
    await hooks.event(sessionEvent("session.deleted", { sessionID: "managed_child" }))
    await hooks.event(sessionEvent("permission.asked", {
      id: "child_request_2", sessionID: "managed_child", permission: "bash", patterns: ["pwd"],
    }))
    assert.equal(replies.length, 3)
    await hooks.dispose()
  } finally {
    await rm(repository, { recursive: true, force: true })
  }
})

const postJson = (url, body, headers = {}) =>
  new Promise((resolve, reject) => {
    const payload = typeof body === "string" ? body : JSON.stringify(body)
    const request = http.request(
      url,
      {
        method: "POST",
        headers: { "content-type": "application/json", "content-length": Buffer.byteLength(payload), ...headers },
      },
      (response) => {
        const chunks = []
        response.on("data", (chunk) => chunks.push(chunk))
        response.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8")
          resolve({ status: response.statusCode, body: JSON.parse(text) })
        })
      },
    )
    request.on("error", reject)
    request.end(payload)
  })

const brokerFixture = async ({
  effect = "allow",
  bodyLimit,
  maxPending,
  maxApprovalSessions,
  maxApprovalsPerSession,
  asyncFactory = false,
  directory = "/workspace",
  project = {},
} = {}) => {
  const calls = []
  const clientFactory = (options) => {
    assert.deepEqual(options, {
      baseUrl: "http://opencode.test:4096/",
      directory,
    })
    return {
      session: {
        permission: {
          create: async (request) => {
            calls.push(request)
            return { data: { data: { id: request.id, effect } } }
          },
        },
      },
    }
  }
  const hooks = await __createRtrtProvenanceForTest(
    {
      project,
      directory,
      serverUrl: new URL("http://opencode.test:4096/"),
      client: testLegacyClient(),
    },
    {
      bodyLimit,
      maxPending,
      maxApprovalSessions,
      maxApprovalsPerSession,
      clientFactory: asyncFactory ? async (options) => clientFactory(options) : clientFactory,
      managedAgentStatePath: isolatedManagedAgentStatePath(),
    },
  )
  await hooks["chat.params"]({ sessionID: "parent-session", agent: "builder" })
  const toolOutput = {
    args: {
      command:
        "claude -p task --permission-prompt-tool mcp__rtrt__permission_prompt --output-format json",
    },
  }
  await hooks["tool.execute.before"](
    { tool: "Bash", sessionID: "parent-session", callID: "parent-call" },
    toolOutput,
  )
  const shell = { env: {} }
  await hooks["shell.env"](
    { sessionID: "parent-session", callID: "parent-call", cwd: "/workspace" },
    shell,
  )
  const body = {
    version: 1,
    request_id: "claude-request-1",
    broker_nonce: shell.env.RTRT_PERMISSION_BROKER_NONCE,
    invocation_id: shell.env.RTRT_INVOCATION_ID,
    parent_session_id: "parent-session",
    parent_call_id: "parent-call",
    child_session_id: "child-session",
    tool_use_id: "tool-use-1",
    tool_name: "Bash",
    input: { command: "git status" },
  }
  const headers = {
    authorization: `Bearer ${shell.env.RTRT_PERMISSION_BROKER_TOKEN}`,
    "x-rtrt-broker-nonce": shell.env.RTRT_PERMISSION_BROKER_NONCE,
  }
  return { hooks, calls, shell, body, headers }
}

test("linked worktree keeps sibling main checkout outside permission and temp bounds", async () => {
  const { fixture, main, linked } = await makeLinkedRepository("rtrt-provenance-linked-")
  await writeFile(path.join(main, "main-only.txt"), "main checkout\n")
  await writeFile(path.join(linked, "linked-only.txt"), "linked worktree\n")
  const broker = await brokerFixture({ directory: linked, project: { worktree: linked } })
  try {
    assert.equal(
      broker.shell.env.TMPDIR,
      path.join(linked, ".rtrt", "tmp", "opencode", "parent-session"),
    )
    assert.equal(broker.shell.env.RTRT_PARENT_WORKTREE, linked)
    assert.equal(broker.shell.env.RTRT_PARENT_PROJECT, "linked-worktree")

    const toolOutput = { args: {} }
    await broker.hooks["tool.execute.before"](
      { tool: "rtrt_agent_call", sessionID: "parent-session", callID: "agent-call" },
      toolOutput,
    )
    assert.equal(toolOutput.args.parent_worktree, linked)
    assert.equal(toolOutput.args.parent_project, "linked-worktree")

    broker.body.tool_name = "Read"
    broker.body.input = { file_path: path.join(main, "main-only.txt") }
    const external = await postJson(
      broker.shell.env.RTRT_PERMISSION_BROKER_URL,
      broker.body,
      broker.headers,
    )
    assert.equal(external.body.decision, "reject")
    assert.deepEqual(broker.calls, [])

    const permissionOutput = { status: "ask" }
    await broker.hooks["permission.ask"]({
      type: "bash",
      pattern: `cat "${path.join(main, "main-only.txt")}"`,
      sessionID: "parent-session",
    }, permissionOutput)
    assert.equal(permissionOutput.status, "ask")
  } finally {
    await broker.hooks.dispose()
    await rm(fixture, { recursive: true, force: true })
  }
})

test("broker canonicalizes project-local typed resources", async (t) => {
  const repository = await makeRepository("rtrt-provenance-broker-path-")
  const nested = path.join(repository, "nested")
  const outside = path.join(path.dirname(repository), `${path.basename(repository)}-outside`)
  await mkdir(path.join(nested, "src"), { recursive: true })
  await writeFile(path.join(nested, "src", "lib.rs"), "pub fn value() {}\n")
  const cases = [
    ["relative existing read", "Read", { file_path: "src/lib.rs" }, "read", [path.join(nested, "src", "lib.rs")]],
    ["nonexistent edit with existing parent", "Edit", { file_path: "src/new/deep.rs" }, "edit", [path.join(nested, "src", "new", "deep.rs")]],
    ["Glob path", "Glob", { pattern: "**/*.rs", path: "src" }, "glob", [path.join(nested, "src")]],
    ["Glob default path", "Glob", { pattern: "**/*.rs" }, "glob", [nested]],
    ["Grep path", "Grep", { pattern: "needle", path: "src" }, "grep", [path.join(nested, "src")]],
    ["Grep default path", "Grep", { pattern: "needle" }, "grep", [nested]],
  ]
  try {
    for (const [name, tool, input, action, resources] of cases) {
      await t.test(name, async () => {
        const fixture = await brokerFixture({ directory: nested, project: { worktree: repository } })
        try {
          fixture.body.tool_name = tool
          fixture.body.input = input
          const response = await postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
          assert.equal(response.body.decision, "once")
          assert.equal(fixture.calls[0].action, action)
          assert.deepEqual(fixture.calls[0].resources, resources)
          assert.deepEqual(fixture.calls[0].save, [])
        } finally {
          await fixture.hooks.dispose()
        }
      })
    }
  } finally {
    await rm(repository, { recursive: true, force: true })
  }
})

test("broker rejects external, traversal, prefix-confused, missing-read, and symlink escapes", { skip: process.platform === "win32" }, async (t) => {
  const repository = await makeRepository("rtrt-provenance-broker-escape-")
  const nested = path.join(repository, "nested")
  const outside = path.join(path.dirname(repository), `${path.basename(repository)}-outside`)
  await mkdir(nested)
  await mkdir(outside)
  await writeFile(path.join(outside, "secret"), "secret\n")
  await symlink(outside, path.join(nested, "escape"), "dir")
  const cases = [
    ["parent traversal", "Edit", { file_path: "../escape.txt" }],
    ["absolute outside", "Read", { file_path: path.join(outside, "secret") }],
    ["prefix confusion", "NotebookEdit", { notebook_path: `${repository}-other/book.ipynb` }],
    ["missing read", "Read", { file_path: "missing.txt" }],
    ["symlink existing escape", "Read", { file_path: "escape/secret" }],
    ["symlink nonexistent edit escape", "Write", { file_path: "escape/new.txt" }],
    ["Glob symlink escape", "Glob", { pattern: "**/*", path: "escape" }],
    ["Grep external", "Grep", { pattern: "secret", path: outside }],
  ]
  try {
    for (const [name, tool, input] of cases) {
      await t.test(name, async () => {
        const fixture = await brokerFixture({ directory: nested, project: { worktree: repository } })
        try {
          fixture.body.tool_name = tool
          fixture.body.input = input
          const response = await postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
          assert.equal(response.body.decision, "reject")
          assert.deepEqual(fixture.calls, [])
        } finally {
          await fixture.hooks.dispose()
        }
      })
    }
  } finally {
    await rm(repository, { recursive: true, force: true })
    await rm(outside, { recursive: true, force: true })
  }
})

test("broker rejects missing, malformed, and NUL concrete Claude paths", async (t) => {
  for (const [name, tool, input] of [
    ["missing Read path", "Read", {}],
    ["missing Edit file_path", "Edit", { notebook_path: "book.ipynb" }],
    ["missing NotebookEdit notebook_path", "NotebookEdit", { file_path: "file.txt" }],
    ["malformed Glob path", "Glob", { pattern: "**/*", path: 7 }],
    ["malformed Grep path", "Grep", { pattern: "needle", path: {} }],
    ["NUL path", "Read", { file_path: "src\0secret" }],
  ]) {
    await t.test(name, async () => {
      const fixture = await brokerFixture()
      try {
        fixture.body.tool_name = tool
        fixture.body.input = input
        const response = await postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
        assert.equal(response.body.decision, "reject")
        assert.deepEqual(fixture.calls, [])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("broker path classification fails closed on non-existing host-specific traversal syntax", async () => {
  const fixture = await brokerFixture()
  try {
    fixture.body.tool_name = "Read"
    fixture.body.input = { file_path: "..\\outside\\file.txt" }
    await postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
    assert.deepEqual(fixture.calls, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("broker URL is loopback-only and credentials are scoped to exact marked invocation", async () => {
  const fixture = await brokerFixture()
  try {
    const url = new URL(fixture.shell.env.RTRT_PERMISSION_BROKER_URL)
    assert.equal(url.hostname, "127.0.0.1")
    assert.equal(url.pathname, "/rtrt/permission/v1")
    assert.ok(Number(url.port) > 0)

    for (const command of [
      " claude -p task --permission-prompt-tool mcp__rtrt__permission_prompt",
      "env claude -p task --permission-prompt-tool mcp__rtrt__permission_prompt",
      "claude task --permission-prompt-tool mcp__rtrt__permission_prompt",
      "claude -p task",
    ]) {
      const output = { args: { command } }
      await fixture.hooks["tool.execute.before"](
        { tool: "Bash", sessionID: "other", callID: command },
        output,
      )
      const shell = { env: {} }
      await fixture.hooks["shell.env"]({ sessionID: "other", callID: command, cwd: "/" }, shell)
      assert.equal(shell.env.RTRT_PERMISSION_BROKER_TOKEN, undefined)
    }
    const wrongCall = { env: {} }
    await fixture.hooks["shell.env"](
      { sessionID: "parent-session", callID: "wrong", cwd: "/" },
      wrongCall,
    )
    assert.equal(wrongCall.env.RTRT_PERMISSION_BROKER_URL, undefined)
  } finally {
    await fixture.hooks.dispose()
  }
})

test("broker maps immediate allow and deny with async and sync factories", async (t) => {
  for (const [effect, expected, asyncFactory] of [["allow", "once", true], ["deny", "reject", false]]) {
    await t.test(effect, async () => {
      const fixture = await brokerFixture({ effect, asyncFactory })
      try {
        const response = await postJson(
          fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
          fixture.body,
          fixture.headers,
        )
        assert.equal(response.body.decision, expected)
        assert.equal(fixture.calls.length, 1)
        assert.deepEqual(fixture.calls[0].resources, ["git status"])
        assert.equal(fixture.calls[0].action, "bash")
        assert.equal(fixture.calls[0].sessionID, "parent-session")
        assert.deepEqual(fixture.calls[0].save, [])
        assert.match(fixture.calls[0].id, /^[0-9a-f-]{36}$/)
        assert.equal("requestID" in fixture.calls[0], false)
        assert.deepEqual(fixture.calls[0].metadata, { source: "claude-cli", tool: "Bash" })
        assert.equal(fixture.calls[0].agent, "builder")
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("ask resolves only from matching once, always, or reject event", async (t) => {
  for (const reply of ["once", "always", "reject"]) {
    await t.test(reply, async () => {
      const fixture = await brokerFixture({ effect: "ask" })
      try {
        const pending = postJson(
          fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
          fixture.body,
          fixture.headers,
        )
        while (fixture.calls.length === 0) await new Promise((resolve) => setImmediate(resolve))
        await fixture.hooks.event({
          event: {
            type: "permission.v2.replied",
            properties: {
              sessionID: "parent-session",
              requestID: fixture.calls[0].id,
              reply,
            },
          },
        })
        assert.equal((await pending).body.decision, reply)
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

const replyToLatestAsk = async (fixture, pending, reply, expected = reply) => {
  const previous = fixture.calls.length
  while (fixture.calls.length === previous) await new Promise((resolve) => setImmediate(resolve))
  await fixture.hooks.event({
    event: {
      type: "permission.v2.replied",
      properties: {
        sessionID: fixture.calls.at(-1).sessionID,
        requestID: fixture.calls.at(-1).id,
        reply,
      },
    },
  })
  assert.equal((await pending).body.decision, expected)
}

const nextBrokerBody = (fixture, requestID, toolName = "Bash", input = { command: "git status" }) => ({
  ...fixture.body,
  request_id: requestID,
  tool_name: toolName,
  input,
})

const activateBrokerParent = async (fixture, sessionID, callID) => {
  await fixture.hooks["chat.params"]({ sessionID, agent: "builder" })
  await fixture.hooks["tool.execute.before"](
    { tool: "Bash", sessionID, callID },
    { args: { command: "claude -p task --permission-prompt-tool mcp__rtrt__permission_prompt" } },
  )
  const shell = { env: {} }
  await fixture.hooks["shell.env"]({ sessionID, callID, cwd: "/workspace" }, shell)
  return {
    body: {
      ...fixture.body,
      request_id: `request-${sessionID}`,
      broker_nonce: shell.env.RTRT_PERMISSION_BROKER_NONCE,
      invocation_id: shell.env.RTRT_INVOCATION_ID,
      parent_session_id: sessionID,
      parent_call_id: callID,
    },
    headers: {
      authorization: `Bearer ${shell.env.RTRT_PERMISSION_BROKER_TOKEN}`,
      "x-rtrt-broker-nonce": shell.env.RTRT_PERMISSION_BROKER_NONCE,
    },
  }
}

test("always approval is exact, parent-session scoped, and never persisted by OpenCode", async () => {
  const fixture = await brokerFixture({ effect: "ask" })
  try {
    const first = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
    await replyToLatestAsk(fixture, first, "always")
    assert.deepEqual(fixture.calls[0].save, [])

    const exact = await postJson(
      fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
      nextBrokerBody(fixture, "exact-reuse"),
      fixture.headers,
    )
    assert.equal(exact.body.decision, "always")
    assert.equal(fixture.calls.length, 1)

    for (const [requestID, toolName, input] of [
      ["different-resource", "Bash", { command: "git diff" }],
      ["different-action", "WebFetch", { url: "https://example.invalid/status" }],
    ]) {
      const pending = postJson(
        fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
        nextBrokerBody(fixture, requestID, toolName, input),
        fixture.headers,
      )
      await replyToLatestAsk(fixture, pending, "reject")
    }

    const other = await activateBrokerParent(fixture, "other-parent", "other-call")
    const otherPending = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, other.body, other.headers)
    await replyToLatestAsk(fixture, otherPending, "reject")
    assert.equal(fixture.calls.length, 4)
  } finally {
    await fixture.hooks.dispose()
  }
})

test("once and reject replies never populate session approvals", async (t) => {
  for (const reply of ["once", "reject"]) {
    await t.test(reply, async () => {
      const fixture = await brokerFixture({ effect: "ask" })
      try {
        const first = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
        await replyToLatestAsk(fixture, first, reply)
        const second = postJson(
          fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
          nextBrokerBody(fixture, `${reply}-again`),
          fixture.headers,
        )
        await replyToLatestAsk(fixture, second, "reject")
        assert.equal(fixture.calls.length, 2)
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("parent deletion clears approvals while child lifecycle events do not", async () => {
  const fixture = await brokerFixture({ effect: "ask" })
  try {
    const first = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
    await replyToLatestAsk(fixture, first, "always")
    for (const type of ["session.idle", "session.deleted"]) {
      await fixture.hooks.event({ event: { type, properties: { sessionID: "child-session" } } })
    }
    assert.equal((await postJson(
      fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
      nextBrokerBody(fixture, "after-child-lifecycle"),
      fixture.headers,
    )).body.decision, "always")

    await fixture.hooks.event({
      event: { type: "session.deleted", properties: { sessionID: "parent-session" } },
    })
    const replacement = await activateBrokerParent(fixture, "parent-session", "replacement-call")
    replacement.body.request_id = "after-parent-deletion"
    const pending = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, replacement.body, replacement.headers)
    await replyToLatestAsk(fixture, pending, "reject")
  } finally {
    await fixture.hooks.dispose()
  }
})

test("dispose and a new plugin process do not retain always approvals", async () => {
  const first = await brokerFixture({ effect: "ask" })
  const pending = postJson(first.shell.env.RTRT_PERMISSION_BROKER_URL, first.body, first.headers)
  await replyToLatestAsk(first, pending, "always")
  await first.hooks.dispose()

  const restarted = await brokerFixture({ effect: "ask" })
  try {
    const afterRestart = postJson(
      restarted.shell.env.RTRT_PERMISSION_BROKER_URL,
      restarted.body,
      restarted.headers,
    )
    await replyToLatestAsk(restarted, afterRestart, "reject")
    assert.equal(restarted.calls.length, 1)
  } finally {
    await restarted.hooks.dispose()
  }
})

test("approval cache bounds fail closed without exposing raw resources", async (t) => {
  await t.test("approvals per session", async () => {
    const fixture = await brokerFixture({ effect: "ask", maxApprovalsPerSession: 1 })
    try {
      const first = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
      await replyToLatestAsk(fixture, first, "always")
      const rawResource = "private-resource-never-in-cache-or-log"
      const bounded = postJson(
        fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
        nextBrokerBody(fixture, "bounded-approval", "Bash", { command: rawResource }),
        fixture.headers,
      )
      await replyToLatestAsk(fixture, bounded, "always", "reject")
      assert.equal(JSON.stringify(fixture.hooks).includes(rawResource), false)
    } finally {
      await fixture.hooks.dispose()
    }
  })

  await t.test("parent sessions", async () => {
    const fixture = await brokerFixture({ effect: "ask", maxApprovalSessions: 1 })
    try {
      const first = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, fixture.body, fixture.headers)
      await replyToLatestAsk(fixture, first, "always")
      const other = await activateBrokerParent(fixture, "bounded-parent", "bounded-call")
      const bounded = postJson(fixture.shell.env.RTRT_PERMISSION_BROKER_URL, other.body, other.headers)
      await replyToLatestAsk(fixture, bounded, "always", "reject")
    } finally {
      await fixture.hooks.dispose()
    }
  })
})

test("broker rejects wrong credentials, identity, malformed, oversized, and altered replay", async () => {
  const fixture = await brokerFixture({ bodyLimit: 512 })
  try {
    const url = fixture.shell.env.RTRT_PERMISSION_BROKER_URL
    assert.equal((await postJson(url, fixture.body, { ...fixture.headers, authorization: "Bearer wrong" })).body.decision, "reject")
    assert.equal((await postJson(url, fixture.body, { ...fixture.headers, "x-rtrt-broker-nonce": "wrong" })).body.decision, "reject")
    assert.equal((await postJson(url, { ...fixture.body, broker_nonce: "wrong" }, fixture.headers)).body.decision, "reject")
    assert.equal((await postJson(url, { ...fixture.body, invocation_id: "wrong" }, fixture.headers)).body.decision, "reject")
    assert.equal((await postJson(url, { ...fixture.body, parent_call_id: "wrong" }, fixture.headers)).body.decision, "reject")
    assert.equal((await postJson(url, "{bad", fixture.headers)).body.decision, "reject")
    assert.equal((await postJson(url, "x".repeat(513), fixture.headers)).body.decision, "reject")

    const accepted = await postJson(url, fixture.body, fixture.headers)
    assert.equal(accepted.body.decision, "once")
    assert.equal((await postJson(url, fixture.body, fixture.headers)).body.decision, "once")
    const altered = { ...fixture.body, input: { command: "git diff" } }
    assert.equal((await postJson(url, altered, fixture.headers)).body.decision, "reject")
    assert.equal(fixture.calls.length, 1)
  } finally {
    await fixture.hooks.dispose()
  }
})

test("global pending limit rejects saturation", async () => {
  const fixture = await brokerFixture({ effect: "ask", maxPending: 1 })
  try {
    const first = postJson(
      fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
      fixture.body,
      fixture.headers,
    )
    while (fixture.calls.length === 0) await new Promise((resolve) => setImmediate(resolve))
    const second = await postJson(
      fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
      { ...fixture.body, request_id: "claude-request-2" },
      fixture.headers,
    )
    assert.equal(second.status, 429)
    assert.equal(second.body.decision, "reject")
    await fixture.hooks["tool.execute.after"]({
      sessionID: "parent-session",
      callID: "parent-call",
    })
    assert.equal((await first).body.decision, "reject")
  } finally {
    await fixture.hooks.dispose()
  }
})

test("broker requires Content-Length and discards replies after client disconnect", async () => {
  const fixture = await brokerFixture({ effect: "ask" })
  try {
    const missingLength = await new Promise((resolve, reject) => {
      const request = http.request(
        fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
        { method: "POST", headers: { ...fixture.headers, "transfer-encoding": "chunked" } },
        (response) => {
          const chunks = []
          response.on("data", (chunk) => chunks.push(chunk))
          response.on("end", () => resolve({ status: response.statusCode, body: JSON.parse(Buffer.concat(chunks)) }))
        },
      )
      request.on("error", reject)
      request.end()
    })
    assert.equal(missingLength.status, 411)
    assert.equal(missingLength.body.decision, "reject")

    const payload = JSON.stringify(fixture.body)
    let abortRequest
    const disconnected = new Promise((resolve) => {
      abortRequest = http.request(
        fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
        {
          method: "POST",
          headers: {
            ...fixture.headers,
            "content-type": "application/json",
            "content-length": Buffer.byteLength(payload),
          },
        },
      )
      abortRequest.on("error", resolve)
      abortRequest.end(payload)
    })
    while (fixture.calls.length === 0) await new Promise((resolve) => setImmediate(resolve))
    abortRequest.destroy()
    await disconnected
    await fixture.hooks.event({
      event: {
        type: "permission.v2.replied",
        properties: {
          sessionID: "parent-session",
          requestID: fixture.calls[0].id,
          reply: "always",
        },
      },
    })
  } finally {
    await fixture.hooks.dispose()
  }
})

test("ask remains pending across elapsed time and then accepts a permission reply", async () => {
  const fixture = await brokerFixture({ effect: "ask" })
  try {
    let settled = false
    const pending = postJson(
      fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
      fixture.body,
      fixture.headers,
    ).finally(() => {
      settled = true
    })
    while (fixture.calls.length === 0) await new Promise((resolve) => setImmediate(resolve))
    await new Promise((resolve) => setTimeout(resolve, 30))
    assert.equal(settled, false)

    await fixture.hooks.event({
      event: {
        type: "permission.v2.replied",
        properties: {
          sessionID: "parent-session",
          requestID: fixture.calls[0].id,
          reply: "always",
        },
      },
    })
    assert.equal((await pending).body.decision, "always")
  } finally {
    await fixture.hooks.dispose()
  }
})

test("tool completion, session deletion, and disposal reject pending asks", async (t) => {
  for (const mode of ["completion", "session deletion", "dispose"]) {
    await t.test(mode, async () => {
      const fixture = await brokerFixture({ effect: "ask" })
      const pending = postJson(
        fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
        fixture.body,
        fixture.headers,
      )
      while (fixture.calls.length === 0) await new Promise((resolve) => setImmediate(resolve))
      if (mode === "completion") {
        await fixture.hooks["tool.execute.after"]({
          sessionID: "parent-session",
          callID: "parent-call",
        })
      } else if (mode === "session deletion") {
        await fixture.hooks.event({ event: { type: "session.deleted", properties: { sessionID: "parent-session" } } })
      } else if (mode === "dispose") {
        await fixture.hooks.dispose()
      }
      assert.equal((await pending).body.decision, "reject")
      await fixture.hooks.dispose()
    })
  }
})

const circuitFixture = async ({
  abort = async () => ({ data: true }),
  get,
  rememberManager = true,
  managedAgentStatePath,
  project = {},
  directory,
} = {}) => {
  const interrupts = []
  const abortRequests = []
  const gets = []
  const getRequests = []
  const sessions = new Map()
  if (rememberManager) sessions.set("manager", { id: "manager", agent: "rtrt-manager" })
  const getSession = get ?? (async ({ sessionID }) => ({ data: { data: sessions.get(sessionID) } }))
  const hooks = await __createRtrtProvenanceForTest(
    {
      project,
      directory,
      client: {
        session: {
          abort: async (request) => {
            abortRequests.push(request)
            interrupts.push({ sessionID: request.path.id })
            return abort(request)
          },
          get: async (request) => {
            getRequests.push(request)
            const logicalRequest = { sessionID: request.path.id }
            gets.push(logicalRequest)
            return getSession(logicalRequest)
          },
        },
      },
    },
    {
      clientFactory: () => ({
        session: {
          permission: { create: async () => ({ data: { data: { effect: "deny" } } }) },
        },
      }),
      managedAgentStatePath: managedAgentStatePath ?? isolatedManagedAgentStatePath(),
    },
  )
  if (rememberManager) {
    await hooks["chat.message"]({ sessionID: "manager", agent: "rtrt-manager" })
  }
  const event = hooks.event
  hooks.event = async (input) => {
    const candidate = input?.event ?? input
    if (candidate?.type === "session.created" && candidate.properties?.info) {
      const info = candidate.properties.info
      sessions.set(info.id, { ...info })
    } else if (candidate?.type === "session.deleted") {
      const sessionID = candidate.properties?.sessionID ?? candidate.properties?.info?.id
      sessions.delete(sessionID)
    }
    return event(input)
  }
  return { hooks, interrupts, abortRequests, gets, getRequests }
}

const sessionEvent = (type, properties) => ({ event: { type, properties } })

const sdkError = (name, data) => ({ name, data })

const writeManagedAgentState = async (directory, agents = ["rtrt-manager", "kimi-k3"]) => {
  const statePath = path.join(directory, ".rtrt-managed-state.json")
  await writeFile(statePath, JSON.stringify({
    owner: "rtrt-opencode-task-agents",
    version: 1,
    agents,
    models: {},
  }))
  return statePath
}

test("manager idle then overlapping created, busy, and Go retry aborts later managed child", async () => {
  const fixture = await circuitFixture()
  try {
    await fixture.hooks.event(sessionEvent("session.idle", { sessionID: "manager" }))
    const created = fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "later-child", parentID: "manager", agent: "glm" },
    }))
    const busy = fixture.hooks.event(sessionEvent("session.status", {
      sessionID: "later-child",
      status: { type: "busy" },
    }))
    const retry = fixture.hooks.event(sessionEvent("session.status", {
      sessionID: "later-child",
      status: {
        type: "retry",
        attempt: 1,
        message: "Weekly usage limit reached",
        action: { reason: "account_rate_limit" },
        next: 1_786_080_000_000,
      },
    }))
    await Promise.all([created, busy, retry])

    assert.deepEqual(fixture.abortRequests, [{ path: { id: "later-child" } }])
    assert.deepEqual(fixture.getRequests, [
      { path: { id: "later-child" } },
      { path: { id: "manager" } },
    ])
    assert.deepEqual(fixture.interrupts, [{ sessionID: "later-child" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("legacy abort accepts only exact true data and never uses v2 session cancellation", async (t) => {
  for (const [name, result] of [
    ["accepted", { data: true }],
    ["false data", { data: false }],
    ["missing data", {}],
    ["SDK error", { data: true, error: { name: "APIError" } }],
  ]) {
    await t.test(name, async () => {
      const fixture = await circuitFixture({ abort: async () => result })
      try {
        await fixture.hooks.event(sessionEvent("session.created", {
          info: { id: `abort-${name.replaceAll(" ", "-")}`, parentID: "manager", agent: "glm" },
        }))
        await fixture.hooks.event(sessionEvent("session.status", {
          sessionID: `abort-${name.replaceAll(" ", "-")}`,
          status: { type: "retry", action: { reason: "free_tier_limit" }, attempt: 1, next: 1 },
        }))
        assert.deepEqual(fixture.abortRequests, [{
          path: { id: `abort-${name.replaceAll(" ", "-")}` },
        }])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("validated RTRT-managed child under ordinary build parent interrupts once", async () => {
  const directory = await makeTempDir("rtrt-provenance-owned-agents-")
  try {
    const managedAgentStatePath = await writeManagedAgentState(directory)
    const fixture = await circuitFixture({ managedAgentStatePath })
    try {
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: "ses_0353d1378ffe2j73J7JLKDTh0z", agent: "build", directory: "/workspace/live" },
      }))
      await fixture.hooks.event(sessionEvent("session.created", {
        info: {
          id: "ses_02480da98ffeQ0BUBM2731axsA",
          parentID: "ses_0353d1378ffe2j73J7JLKDTh0z",
          agent: "kimi-k3",
          directory: "/workspace/live",
        },
      }))
      const retry = sessionEvent("session.status", {
        sessionID: "ses_02480da98ffeQ0BUBM2731axsA",
        status: {
          type: "retry",
          attempt: 1,
          message: "Weekly usage limit reached. It will reset in 2 days 14 hours.",
          action: { reason: "account_rate_limit" },
          next: 1_786_080_000_000,
        },
      })
      await fixture.hooks.event(retry)
      await fixture.hooks.event(retry)
      assert.deepEqual(fixture.interrupts, [{ sessionID: "ses_02480da98ffeQ0BUBM2731axsA" }])
      assert.deepEqual(fixture.abortRequests, [{
        path: { id: "ses_02480da98ffeQ0BUBM2731axsA" },
      }])
    } finally {
      await fixture.hooks.dispose()
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("managed-agent state resolver follows configured root precedence", async (t) => {
  const home = path.join(path.sep, "home", "rtrt-test")
  for (const [name, environment, expected] of [
    [
      "OpenCode config directory",
      { OPENCODE_CONFIG_DIR: "/configured/opencode", XDG_CONFIG_HOME: "/xdg", HOME: home },
      "/configured/opencode/agents/.rtrt-managed-state.json",
    ],
    [
      "XDG config home",
      { OPENCODE_CONFIG_DIR: "", XDG_CONFIG_HOME: "/xdg", HOME: home },
      "/xdg/opencode/agents/.rtrt-managed-state.json",
    ],
    [
      "HOME config directory after empty values",
      { OPENCODE_CONFIG_DIR: "", XDG_CONFIG_HOME: "", HOME: home },
      "/home/rtrt-test/.config/opencode/agents/.rtrt-managed-state.json",
    ],
    [
      "HOME config directory after NUL values",
      { OPENCODE_CONFIG_DIR: "\0invalid", XDG_CONFIG_HOME: "\0invalid", HOME: home },
      "/home/rtrt-test/.config/opencode/agents/.rtrt-managed-state.json",
    ],
  ]) {
    await t.test(name, () => {
      assert.equal(__resolveManagedAgentStatePathForTest(environment), expected)
    })
  }
})

test("managed-agent state loads from an explicitly resolved config path", async () => {
  const configRoot = await makeTempDir("rtrt-provenance-config-")
  const managedAgentStatePath = __resolveManagedAgentStatePathForTest({
    OPENCODE_CONFIG_DIR: path.join(configRoot, "opencode"),
    XDG_CONFIG_HOME: path.join(configRoot, "xdg"),
    HOME: path.join(configRoot, "home"),
  })
  try {
    await mkdir(path.dirname(managedAgentStatePath), { recursive: true })
    await writeFile(managedAgentStatePath, JSON.stringify({
      owner: "rtrt-opencode-task-agents",
      version: 1,
      agents: ["rtrt-manager", "kimi-k3"],
      models: {},
    }))
    const fixture = await circuitFixture({ managedAgentStatePath })
    try {
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: "config-parent", agent: "build", directory: "/workspace/live" },
      }))
      await fixture.hooks.event(sessionEvent("session.created", {
        info: {
          id: "config-child",
          parentID: "config-parent",
          agent: "kimi-k3",
          directory: "/workspace/live",
        },
      }))

      await fixture.hooks.event(sessionEvent("session.next.retried", {
        sessionID: "config-child",
        error: { statusCode: 429 },
      }))

      assert.deepEqual(fixture.interrupts, [{ sessionID: "config-child" }])
    } finally {
      await fixture.hooks.dispose()
    }
  } finally {
    await rm(configRoot, { recursive: true, force: true })
  }
})

test("validated ownership ignores unknown child agent under same build parent", async () => {
  const directory = await makeTempDir("rtrt-provenance-unknown-agent-")
  try {
    const managedAgentStatePath = await writeManagedAgentState(directory)
    const fixture = await circuitFixture({ managedAgentStatePath })
    try {
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: "build-parent", agent: "build", directory: "/workspace/live" },
      }))
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: "unknown-child", parentID: "build-parent", agent: "unknown-agent", directory: "/workspace/live" },
      }))
      await fixture.hooks.event(sessionEvent("session.next.retried", {
        sessionID: "unknown-child",
        error: { statusCode: 429 },
      }))
      assert.deepEqual(fixture.interrupts, [])
    } finally {
      await fixture.hooks.dispose()
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("missing, malformed, and symlink ownership state fail closed for build parents", async (t) => {
  const directory = await makeTempDir("rtrt-provenance-invalid-state-")
  try {
    const missing = path.join(directory, "missing.json")
    const malformed = path.join(directory, "malformed.json")
    const target = path.join(directory, "target.json")
    const linked = path.join(directory, "linked.json")
    await writeFile(malformed, JSON.stringify({ owner: "rtrt-opencode-task-agents", version: 1, agents: ["kimi-k3"] }))
    await writeManagedAgentState(directory)
    await rename(path.join(directory, ".rtrt-managed-state.json"), target)
    await symlink(target, linked, "file")
    for (const [name, managedAgentStatePath] of [["missing", missing], ["malformed", malformed], ["symlink", linked]]) {
      await t.test(name, async () => {
        const fixture = await circuitFixture({ managedAgentStatePath })
        try {
          await fixture.hooks.event(sessionEvent("session.created", { info: { id: `${name}-parent`, agent: "build" } }))
          await fixture.hooks.event(sessionEvent("session.created", {
            info: { id: `${name}-child`, parentID: `${name}-parent`, agent: "kimi-k3" },
          }))
          await fixture.hooks.event(sessionEvent("session.next.retried", {
            sessionID: `${name}-child`, error: { statusCode: 529 },
          }))
          assert.deepEqual(fixture.interrupts, [])
        } finally {
          await fixture.hooks.dispose()
        }
      })
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("validated ownership rejects child and parent project identity mismatch", async () => {
  const directory = await makeTempDir("rtrt-provenance-project-mismatch-")
  try {
    const managedAgentStatePath = await writeManagedAgentState(directory)
    const fixture = await circuitFixture({ managedAgentStatePath })
    try {
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: "other-project-parent", agent: "build", projectID: "project-a", directory: "/workspace/a" },
      }))
      await fixture.hooks.event(sessionEvent("session.created", {
        info: {
          id: "other-project-child",
          parentID: "other-project-parent",
          agent: "kimi-k3",
          projectID: "project-b",
          directory: "/workspace/b",
        },
      }))
      await fixture.hooks.event(sessionEvent("session.next.retried", {
        sessionID: "other-project-child",
        error: { statusCode: 429 },
      }))
      assert.deepEqual(fixture.interrupts, [])
    } finally {
      await fixture.hooks.dispose()
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("live glm session.error weekly-limit sequence interrupts before synthetic cancellation", async () => {
  const order = []
  const fixture = await circuitFixture({
    abort: async ({ path: { id } }) => {
      order.push(`abort:${id}`)
      return { data: true }
    },
  })
  try {
    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "glm-child", parentID: "manager", agent: "glm" },
    }))
    await fixture.hooks.event(sessionEvent("session.error", {
      sessionID: "glm-child",
      error: sdkError("APIError", {
        message: "AI_APICallError: Weekly usage limit reached. Resets in 2 days...",
      }),
    }))
    order.push("synthetic-cancel:73s")

    assert.deepEqual(fixture.interrupts, [{ sessionID: "glm-child" }])
    assert.deepEqual(order, ["abort:glm-child", "synthetic-cancel:73s"])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("session.error accepts exact APIError and UnknownError weekly phrases", async (t) => {
  for (const [name, error] of [
    ["APIError limit", sdkError("APIError", { message: "Weekly usage limit reached" })],
    ["APIError cap", sdkError("APIError", { message: "WEEKLY USAGE CAP reached" })],
    ["UnknownError limit", sdkError("UnknownError", { message: "Weekly usage limit reached" })],
    ["UnknownError cap", sdkError("UnknownError", { message: "weekly usage cap reached" })],
    ["APIError metadata", sdkError("APIError", { metadata: { code: "WEEKLY_USAGE_LIMIT" } })],
  ]) {
    await t.test(name, async () => {
      const fixture = await circuitFixture()
      try {
        const child = `session-error-${name.replaceAll(" ", "-")}`
        await fixture.hooks.event(sessionEvent("session.created", {
          info: { id: child, parentID: "manager" },
        }))
        await fixture.hooks.event(sessionEvent("session.error", { sessionID: child, error }))
        assert.deepEqual(fixture.interrupts, [{ sessionID: child }])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("session.error APIError 429 and 529 interrupt while UnknownError statuses do not", async () => {
  const fixture = await circuitFixture()
  try {
    for (const statusCode of [429, 529]) {
      const child = `api-${statusCode}`
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: child, parentID: "manager" },
      }))
      await fixture.hooks.event(sessionEvent("session.error", {
        sessionID: child,
        error: sdkError("APIError", { statusCode, message: "transport failure" }),
      }))
    }
    for (const statusCode of [429, 529]) {
      const child = `unknown-${statusCode}`
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: child, parentID: "manager" },
      }))
      await fixture.hooks.event(sessionEvent("session.error", {
        sessionID: child,
        error: sdkError("UnknownError", { statusCode, message: "rate limit" }),
      }))
    }
    assert.deepEqual(fixture.interrupts, [
      { sessionID: "api-429" },
      { sessionID: "api-529" },
    ])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("session.error rejects untrusted names, fields, prose, auth, 500, and output", async () => {
  const fixture = await circuitFixture()
  try {
    const errors = [
      sdkError("AuthenticationError", { statusCode: 429, message: "weekly usage limit" }),
      sdkError("ProviderError", { message: "weekly usage limit" }),
      sdkError("UnknownError", { message: "weekly allowance exhausted" }),
      sdkError("UnknownError", { message: "generic weekly rate or usage limit" }),
      sdkError("UnknownError", { metadata: { code: "weekly_usage_limit" } }),
      sdkError("APIError", { responseBody: "Weekly usage limit reached" }),
      sdkError("UnknownError", { responseBody: { message: "weekly usage cap" } }),
      sdkError("APIError", { statusCode: 401, message: "weekly usage limit" }),
      sdkError("APIError", { statusCode: 403, metadata: { code: "weekly_usage_limit" } }),
      sdkError("APIError", { statusCode: 500, message: "weekly usage limit" }),
      sdkError("APIError", { statusCode: 503, message: "capacity" }),
      sdkError("APIError", { message: "weekly limit reached" }),
      sdkError("APIError", { message: "usage limit reached" }),
      sdkError("APIError", { message: "rate limit reached" }),
      sdkError("UnknownError", { message: `weekly usage limit ${"x".repeat(4_097)}` }),
      sdkError("APIError", { metadata: { code: `weekly_usage_limit${"x".repeat(128)}` } }),
      sdkError("APIError", { output: "weekly usage limit" }),
      sdkError("APIError", { toolOutput: { message: "weekly usage cap" } }),
      { name: "APIError", data: "weekly usage limit" },
      { name: "APIError", message: "weekly usage limit" },
      null,
    ]
    for (const [index, error] of errors.entries()) {
      const child = `rejected-session-error-${index}`
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: child, parentID: "manager" },
      }))
      await fixture.hooks.event(sessionEvent("session.error", { sessionID: child, error }))
    }
    await fixture.hooks.event(sessionEvent("session.error", {
      session_id: "rejected-session-error-0",
      error: sdkError("APIError", { statusCode: 429 }),
    }))
    await fixture.hooks.event(sessionEvent("session.error", {
      error: sdkError("APIError", { statusCode: 429 }),
    }))
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("session.error, status, and next retry deduplicate one managed-child interrupt", async () => {
  const fixture = await circuitFixture()
  try {
    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "dedupe-child", parentID: "manager" },
    }))
    await fixture.hooks.event(sessionEvent("session.error", {
      sessionID: "dedupe-child",
      error: sdkError("APIError", { message: "weekly usage limit reached" }),
    }))
    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "dedupe-child", parentID: "manager" },
    }))
    await fixture.hooks.event(sessionEvent("session.status", {
      sessionID: "dedupe-child",
      status: { type: "retry", message: "weekly usage cap" },
    }))
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "dedupe-child",
      error: { statusCode: 429 },
    }))
    assert.deepEqual(fixture.interrupts, [{ sessionID: "dedupe-child" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("managed child 429 and 529 retries interrupt exactly once and tolerate interrupt failures", async (t) => {
  for (const [statusCode, failure] of [[429, "returned"], [529, "thrown"]]) {
    await t.test(String(statusCode), async () => {
      const fixture = await circuitFixture({
        abort: failure === "returned"
          ? async () => ({ error: { message: "unavailable" } })
          : async () => { throw new Error("unavailable") },
      })
      try {
        const child = `child-${statusCode}`
        await fixture.hooks.event(sessionEvent("session.created", { info: { id: child, parentID: "manager" } }))
        const retry = sessionEvent("session.next.retried", {
          sessionID: child,
          error: { statusCode },
        })
        await fixture.hooks.event(retry)
        await fixture.hooks.event(retry)
        assert.deepEqual(fixture.interrupts, [{ sessionID: child }])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

/* Legacy next.retried prose/metadata is intentionally unsupported; only exact 429/529 remains.
test("managed child weekly usage limit retries interrupt exactly once", async (t) => {
  const cases = [
    ["metadata code", { metadata: { code: "weekly_usage_limit" } }],
    ["metadata type", { metadata: { type: "WEEKLY-USAGE-LIMIT" } }],
    ["metadata reason weekly_limit", { metadata: { reason: " weekly_limit " } }],
    ["metadata reason usage_limit_weekly", { metadata: { reason: "usage_limit_weekly" } }],
    ["message limit", { message: "Provider says: Weekly Usage Limit reached." }],
    ["message cap", { message: "Your WEEKLY USAGE CAP has been reached" }],
  ]
  for (const [name, error] of cases) {
    await t.test(name, async () => {
      const fixture = await circuitFixture()
      try {
        const child = `weekly-${name.replaceAll(" ", "-")}`
        await fixture.hooks.event(sessionEvent("session.created", { info: { id: child, parentID: "manager" } }))
        const retry = sessionEvent("session.next.retried", { sessionID: child, error })
        await fixture.hooks.event(retry)
        await fixture.hooks.event(retry)
        assert.deepEqual(fixture.interrupts, [{ sessionID: child }])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})
*/

test("uncached managed children recover ownership through SDK child and parent lookup", async () => {
  const fixture = await circuitFixture({
    rememberManager: false,
    get: async ({ sessionID }) => ({
      data: { data: sessionID === "resumed-child"
        ? { id: sessionID, parentID: "resumed-parent", title: "ignored" }
        : { id: sessionID, agent: "rtrt-manager" } },
    }),
  })
  try {
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "resumed-child",
      error: { statusCode: 429, responseBody: "not retained" },
    }))
    assert.deepEqual(fixture.gets, [
      { sessionID: "resumed-child" },
      { sessionID: "resumed-parent" },
    ])
    assert.deepEqual(fixture.interrupts, [{ sessionID: "resumed-child" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("session.error before session.created recovers exact manager ownership", async () => {
  const fixture = await circuitFixture({
    rememberManager: false,
    get: async ({ sessionID }) => ({
      data: { data: sessionID === "early-child"
        ? { id: sessionID, parentID: "early-parent", agent: "glm" }
        : { id: sessionID, agent: "rtrt-manager" } },
    }),
  })
  try {
    await fixture.hooks.event(sessionEvent("session.error", {
      sessionID: "early-child",
      error: sdkError("UnknownError", {
        message: "AI_APICallError: Weekly usage limit reached. Resets in 2 days...",
      }),
    }))
    assert.deepEqual(fixture.gets, [
      { sessionID: "early-child" },
      { sessionID: "early-parent" },
    ])
    assert.deepEqual(fixture.interrupts, [{ sessionID: "early-child" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("session.error lookup rejects non-manager parent ownership", async () => {
  const fixture = await circuitFixture({
    rememberManager: false,
    get: async ({ sessionID }) => ({
      data: sessionID === "foreign-child"
        ? { id: sessionID, parentID: "foreign-parent" }
        : { id: sessionID, agent: "rtrt-manager-helper" },
    }),
  })
  try {
    await fixture.hooks.event(sessionEvent("session.error", {
      sessionID: "foreign-child",
      error: sdkError("APIError", { statusCode: 429 }),
    }))
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("uncached child requires SDK-confirmed manager parent despite agents map", async () => {
  const fixture = await circuitFixture({
    get: async ({ sessionID }) => ({ data: sessionID === "manager"
      ? { id: sessionID, agent: "rtrt-manager" }
      : { id: sessionID, parentID: "manager" } }),
  })
  try {
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "map-child",
      error: { statusCode: 529 },
    }))
    assert.deepEqual(fixture.gets, [{ sessionID: "map-child" }, { sessionID: "manager" }])
    assert.deepEqual(fixture.interrupts, [{ sessionID: "map-child" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("uncached lookup fails closed for wrong, absent, malformed, and failed ownership", async (t) => {
  const cases = [
    ["no parent", async ({ sessionID }) => ({ data: { id: sessionID } })],
    ["wrong parent", async ({ sessionID }) => ({ data: sessionID === "child"
      ? { id: sessionID, parentID: "parent" }
      : { id: sessionID, agent: "builder" } })],
    ["malformed", async () => ({ data: { data: "not-a-session" } })],
    ["error", async () => { throw new Error("lookup failed") }],
  ]
  for (const [name, get] of cases) {
    await t.test(name, async () => {
      const fixture = await circuitFixture({ rememberManager: false, get })
      try {
        await fixture.hooks.event(sessionEvent("session.next.retried", {
          sessionID: "child",
          error: { statusCode: 429 },
        }))
        assert.deepEqual(fixture.interrupts, [])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("Go retry status actions and documented phrase fallback abort cached children", async (t) => {
  const statuses = [
    ["account weekly", { type: "retry", attempt: 1, message: "Weekly usage limit reached", action: { reason: "account_rate_limit" }, next: 42 }],
    ["account five hour", { type: "retry", attempt: 2, message: "5-hour usage limit reached", action: { reason: "account_rate_limit" }, next: 43 }],
    ["free tier", { type: "retry", attempt: 1, message: "quota", action: { reason: "free_tier_limit" }, next: 44 }],
    ["fallback weekly", { type: "retry", message: "Weekly usage limit reached" }],
    ["fallback five hour", { type: "retry", message: "5 hour usage cap reached" }],
  ]
  for (const [name, status] of statuses) {
    await t.test(name, async () => {
      const fixture = await circuitFixture()
      try {
        const child = `status-${name.replaceAll(" ", "-")}`
        await fixture.hooks.event(sessionEvent("session.created", {
          info: { id: child, parentID: "manager" },
        }))
        await fixture.hooks.event(sessionEvent("session.status", { sessionID: child, status }))
        assert.deepEqual(fixture.interrupts, [{ sessionID: child }])
      } finally {
        await fixture.hooks.dispose()
      }
    })
  }
})

test("status path ignores non-retry and generic or untrusted weekly prose", async () => {
  const fixture = await circuitFixture()
  try {
    const statuses = [
      { type: "busy", message: "weekly usage limit" },
      { type: "idle", action: { reason: "weekly_usage_limit" } },
      { type: "retry", message: "weekly allowance exhausted" },
      { type: "retry", message: "generic usage limit" },
      { type: "retry", action: { reason: "rate_limit", message: "rate limit" } },
      { type: "retry", message: "weekly usage limit reached", action: { reason: "auth" } },
      { type: "retry", responseBody: { message: "weekly usage limit" } },
      { type: "retry", output: "weekly usage cap" },
      { type: "retry", action: { body: "weekly usage limit" } },
    ]
    for (const [index, status] of statuses.entries()) {
      const child = `generic-status-${index}`
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: child, parentID: "manager" },
      }))
      await fixture.hooks.event(sessionEvent("session.status", { sessionID: child, status }))
    }
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("simultaneous status and next retries share one lookup and one interrupt", async () => {
  let release
  const childLookup = new Promise((resolve) => { release = resolve })
  const fixture = await circuitFixture({ get: async ({ sessionID }) => sessionID === "manager"
    ? { data: { id: sessionID, agent: "rtrt-manager" } }
    : childLookup })
  try {
    const status = fixture.hooks.event(sessionEvent("session.status", {
      sessionID: "race-child",
      status: { type: "retry", message: "weekly usage cap" },
    }))
    const next = fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "race-child",
      error: { statusCode: 429 },
    }))
    await new Promise((resolve) => setImmediate(resolve))
    release({ data: { id: "race-child", parentID: "manager" } })
    await Promise.all([status, next])
    assert.deepEqual(fixture.gets, [{ sessionID: "race-child" }, { sessionID: "manager" }])
    assert.deepEqual(fixture.interrupts, [{ sessionID: "race-child" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("idle and dispose cancel late ownership recovery", async (t) => {
  for (const mode of ["idle", "dispose"]) {
    await t.test(mode, async () => {
      let release
      const lookup = new Promise((resolve) => { release = resolve })
      const fixture = await circuitFixture({ get: async () => lookup })
      const retry = fixture.hooks.event(sessionEvent("session.next.retried", {
        sessionID: `${mode}-race-child`,
        error: { statusCode: 429 },
      }))
      await new Promise((resolve) => setImmediate(resolve))
      if (mode === "idle") {
        await fixture.hooks.event(sessionEvent("session.idle", { sessionID: `${mode}-race-child` }))
      } else {
        await fixture.hooks.dispose()
      }
      release({ data: { id: `${mode}-race-child`, parentID: "manager" } })
      await retry
      assert.deepEqual(fixture.interrupts, [])
      await fixture.hooks.dispose()
    })
  }
})

test("idle, deleted, and dispose races prevent late session.error interruption or resurrection", async (t) => {
  for (const mode of ["idle", "deleted", "dispose"]) {
    await t.test(mode, async () => {
      let release
      const lookup = new Promise((resolve) => { release = resolve })
      const fixture = await circuitFixture({ get: async () => lookup })
      const child = `${mode}-error-race-child`
      const errorEvent = sessionEvent("session.error", {
        sessionID: child,
        error: sdkError("APIError", { statusCode: 429 }),
      })
      const pending = fixture.hooks.event(errorEvent)
      await new Promise((resolve) => setImmediate(resolve))
      if (mode === "dispose") {
        await fixture.hooks.dispose()
      } else {
        await fixture.hooks.event(sessionEvent(`session.${mode}`, { sessionID: child }))
      }
      release({ data: { id: child, parentID: "manager" } })
      await pending

      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: child, parentID: "manager", agent: "glm" },
      }))
      await fixture.hooks.event(errorEvent)
      assert.deepEqual(fixture.interrupts, [])
      await fixture.hooks.dispose()
    })
  }
})

test("only structured exact provider-limit statuses interrupt managed children", async () => {
  const fixture = await circuitFixture()
  try {
    for (const [index, error] of [
      { statusCode: 500 },
      { statusCode: 401 },
      { statusCode: 403 },
      {},
      { message: "rate limit" },
      { message: "usage limit" },
      { message: "weekly allowance exhausted" },
      { responseBody: { message: "weekly usage limit" } },
      { metadata: { code: "weekly" } },
      { metadata: { code: "rate_limit" } },
      { statusCode: 500, message: "weekly usage limit" },
      { statusCode: 401, metadata: { code: "weekly_usage_limit" } },
      { statusCode: 403, message: "weekly usage cap" },
      { statusCode: "429" },
    ].entries()) {
      const child = `ignored-${index}`
      await fixture.hooks.event(sessionEvent("session.created", { info: { id: child, parentID: "manager" } }))
      await fixture.hooks.event(sessionEvent("session.next.retried", { sessionID: child, error }))
    }
    await fixture.hooks.event(sessionEvent("session.status", {
      sessionID: "ignored-0",
      message: "weekly usage limit",
    }))
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "ignored-0",
      error: {},
      output: { message: "weekly usage cap" },
    }))
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("unrelated parents, agents, manager parent, and untracked workers never interrupt", async () => {
  const fixture = await circuitFixture()
  try {
    await fixture.hooks["chat.params"]({ sessionID: "builder-parent", agent: "builder" })
    await fixture.hooks.event(sessionEvent("session.created", { info: { id: "builder-child", parentID: "builder-parent" } }))
    await fixture.hooks.event(sessionEvent("session.created", { info: { id: "unknown-child", parentID: "unknown" } }))
    for (const sessionID of ["builder-child", "unknown-child", "manager", "native-worker"]) {
      await fixture.hooks.event(sessionEvent("session.next.retried", {
        sessionID,
        error: { statusCode: 429 },
      }))
    }
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("idle and deleted events clean tracked child state", async () => {
  const fixture = await circuitFixture()
  try {
    for (const [child, cleanup] of [["idle-child", "session.idle"], ["deleted-child", "session.deleted"]]) {
      await fixture.hooks.event(sessionEvent("session.created", { info: { id: child, parentID: "manager" } }))
      await fixture.hooks.event(sessionEvent(cleanup, { sessionID: child }))
      await fixture.hooks.event(sessionEvent("session.next.retried", {
        sessionID: child,
        error: { statusCode: 529 },
      }))
    }
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("deleted child tombstone prevents a delayed created event from resurrecting state", async () => {
  const fixture = await circuitFixture()
  try {
    await fixture.hooks.event(sessionEvent("session.deleted", {
      info: { id: "deleted-before-created", parentID: "manager" },
    }))
    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "deleted-before-created", parentID: "manager", agent: "glm" },
    }))
    await fixture.hooks.event(sessionEvent("session.status", {
      sessionID: "deleted-before-created",
      status: { type: "retry", action: { reason: "account_rate_limit" }, attempt: 1, next: 1 },
    }))
    assert.deepEqual(fixture.abortRequests, [])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("tracked child saturation remains bounded without evicting active children", async () => {
  const fixture = await circuitFixture()
  try {
    for (let index = 0; index < 129; index += 1) {
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: `bounded-${index}`, parentID: "manager" },
      }))
    }
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "bounded-128",
      error: { statusCode: 429 },
    }))
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "bounded-0",
      error: { statusCode: 429 },
    }))
    assert.deepEqual(fixture.interrupts, [{ sessionID: "bounded-0" }])
  } finally {
    await fixture.hooks.dispose()
  }
})

test("provider-limit interruption does not settle Claude permission approval waits", async () => {
  const fixture = await brokerFixture({ effect: "ask" })
  try {
    await fixture.hooks["chat.message"]({ sessionID: "manager", agent: "rtrt-manager" })
    await fixture.hooks.event(sessionEvent("session.created", { info: { id: "managed-child", parentID: "manager" } }))
    const pending = postJson(
      fixture.shell.env.RTRT_PERMISSION_BROKER_URL,
      fixture.body,
      fixture.headers,
    )
    while (fixture.calls.length === 0) await new Promise((resolve) => setImmediate(resolve))
    await fixture.hooks.event(sessionEvent("session.next.retried", {
      sessionID: "managed-child",
      error: { statusCode: 429 },
    }))
    let settled = false
    pending.finally(() => { settled = true })
    await new Promise((resolve) => setImmediate(resolve))
    assert.equal(settled, false)
    await fixture.hooks.event(sessionEvent("permission.v2.replied", {
      sessionID: "parent-session",
      requestID: fixture.calls[0].id,
      reply: "once",
    }))
    assert.equal((await pending).body.decision, "once")
  } finally {
    await fixture.hooks.dispose()
  }
})

/* Removed: native Go limits are delivered by structured session.status events.
const liveLogLine = ({
  sessionID = "log-child",
  agent = "glm",
  providerID = "opencode-go",
  mode = "subagent",
  level = "ERROR",
  message = "stream error",
  error = "AI_APICallError: Weekly usage limit reached. Resets later.",
  nested = false,
} = {}) => `time=2026-08-07T00:00:00Z level=${level} message=${JSON.stringify(message)} providerID=${providerID} mode=${mode} session.id=${sessionID} agent=${agent} ${nested ? `error=${JSON.stringify({ error })}` : `error.error=${JSON.stringify(error)}`}\n`

const pollUntil = async (predicate, timeout = 1_500) => {
  const deadline = Date.now() + timeout
  while (!predicate()) {
    if (Date.now() >= deadline) assert.fail("timed out waiting for log watcher")
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
}

const FLATTENED_LIVE_LIMIT_LINE = "time=2026-08-07T00:00:00Z level=ERROR message=\"stream error\" providerID=opencode-go mode=subagent session.id=literal-child agent=glm error.error=\"AI_APICallError: Weekly usage limit reached. Resets later.\"\n"

test("literal flattened live log line interrupts its exact tracked child", async () => {
  const directory = await makeTempDir("rtrt-provenance-log-literal-")
  const logPath = path.join(directory, "opencode.log")
  await writeFile(logPath, "pre-start\n")
  const fixture = await circuitFixture({ logPath, watchInterval: 20 })
  try {
    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "literal-child", parentID: "manager", agent: "glm" },
    }))
    await appendFile(logPath, FLATTENED_LIVE_LIMIT_LINE)
    await pollUntil(() => fixture.interrupts.length === 1)
    assert.deepEqual(fixture.interrupts, [{ sessionID: "literal-child" }])
  } finally {
    await fixture.hooks.dispose()
    await rm(directory, { recursive: true, force: true })
  }
})

test("disposing one project plugin preserves another project log watcher", async () => {
  const directory = await makeTempDir("rtrt-provenance-log-shared-")
  const logPath = path.join(directory, "opencode.log")
  await writeFile(logPath, "pre-start\n")
  const first = await circuitFixture({ logPath, watchInterval: 20 })
  const second = await circuitFixture({ logPath, watchInterval: 20 })
  try {
    await second.hooks.event(sessionEvent("session.created", {
      info: { id: "surviving-child", parentID: "manager", agent: "glm" },
    }))
    await first.hooks.dispose()
    await appendFile(logPath, liveLogLine({ sessionID: "surviving-child" }))
    await pollUntil(() => second.interrupts.length === 1)
    assert.deepEqual(second.interrupts, [{ sessionID: "surviving-child" }])
  } finally {
    await first.hooks.dispose()
    await second.hooks.dispose()
    await rm(directory, { recursive: true, force: true })
  }
})

test("exact live provider-limit log lines interrupt matching tracked children once", async (t) => {
  for (const [name, error] of [
    ["weekly", "AI_APICallError: Weekly usage limit reached. Resets later."],
    ["five-hour", "AI_RetryError: 5-hour usage limit reached. Resets later."],
    ["weekly-cap", "AI_APICallError: Weekly usage cap reached."],
  ]) {
    await t.test(name, async () => {
      const directory = await makeTempDir(`rtrt-provenance-log-${name}-`)
      const logPath = path.join(directory, "opencode.log")
      await writeFile(logPath, liveLogLine({ sessionID: "historical", error }))
      const fixture = await circuitFixture({ logPath, watchInterval: 20 })
      try {
        const child = `${name}-child`
        await fixture.hooks.event(sessionEvent("session.created", {
          info: { id: child, parentID: "manager", agent: "glm" },
        }))
        const line = liveLogLine({ sessionID: child, error })
        await appendFile(logPath, line.slice(0, 47))
        await new Promise((resolve) => setTimeout(resolve, 35))
        assert.deepEqual(fixture.interrupts, [])
        await appendFile(logPath, line.slice(47))
        await pollUntil(() => fixture.interrupts.length === 1)
        await appendFile(logPath, line)
        await new Promise((resolve) => setTimeout(resolve, 60))
        assert.deepEqual(fixture.interrupts, [{ sessionID: child }])
      } finally {
        await fixture.hooks.dispose()
        await rm(directory, { recursive: true, force: true })
      }
    })
  }
})

test("log fallback retains bounded nested error JSON compatibility", async () => {
  const directory = await makeTempDir("rtrt-provenance-log-nested-")
  const logPath = path.join(directory, "opencode.log")
  await writeFile(logPath, "pre-start\n")
  const fixture = await circuitFixture({ logPath, watchInterval: 20 })
  try {
    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "nested-child", parentID: "manager", agent: "glm" },
    }))
    await appendFile(logPath, liveLogLine({ sessionID: "nested-child", nested: true }))
    await pollUntil(() => fixture.interrupts.length === 1)
    assert.deepEqual(fixture.interrupts, [{ sessionID: "nested-child" }])
  } finally {
    await fixture.hooks.dispose()
    await rm(directory, { recursive: true, force: true })
  }
})

test("log fallback rejects malformed, oversized, generic, and mismatched envelopes", async () => {
  const directory = await makeTempDir("rtrt-provenance-log-reject-")
  const logPath = path.join(directory, "opencode.log")
  await writeFile(logPath, liveLogLine({ sessionID: "old-child" }))
  const fixture = await circuitFixture({ logPath, watchInterval: 20, rememberManager: false })
  try {
    for (const child of ["right-child", "wrong-session"]) {
      await fixture.hooks.event(sessionEvent("session.created", {
        info: { id: child, parentID: "parent", agent: "glm" },
      }))
    }
    const rejected = [
      liveLogLine({ sessionID: "right-child", providerID: "ollama" }),
      liveLogLine({ sessionID: "right-child", mode: "primary" }),
      liveLogLine({ sessionID: "right-child", level: "INFO" }),
      liveLogLine({ sessionID: "right-child", message: "retrying" }),
      liveLogLine({ sessionID: "right-child", agent: "kimi" }),
      liveLogLine({ sessionID: "right-child" }),
      liveLogLine({ sessionID: "untracked-child" }),
      liveLogLine({ sessionID: "right-child", error: "AI_APICallError: generic usage limit" }),
      liveLogLine({ sessionID: "right-child", error: "ProviderError: Weekly usage limit reached" }),
      "level=ERROR message=\"stream error\" providerID=opencode-go mode=subagent session.id=bad/id agent=glm error={bad}\n",
      `${"x".repeat(16_385)}${liveLogLine({ sessionID: "right-child" })}`,
    ]
    await appendFile(logPath, rejected.join(""))
    await new Promise((resolve) => setTimeout(resolve, 100))
    assert.deepEqual(fixture.interrupts, [])
  } finally {
    await fixture.hooks.dispose()
    await rm(directory, { recursive: true, force: true })
  }
})

test("log fallback handles rotation and truncation, recovers missed creation, and stops on dispose", async () => {
  const directory = await makeTempDir("rtrt-provenance-log-lifecycle-")
  const logPath = path.join(directory, "opencode.log")
  await writeFile(logPath, "pre-start evidence is ignored\n")
  const sessions = new Map([
    ["recovered-child", { id: "recovered-child", parentID: "manager", agent: "glm" }],
    ["rotated-child", { id: "rotated-child", parentID: "manager", agent: "glm" }],
    ["truncated-child", { id: "truncated-child", parentID: "manager", agent: "glm" }],
    ["manager", { id: "manager", agent: "rtrt-manager" }],
  ])
  const fixture = await circuitFixture({
    logPath,
    watchInterval: 20,
    rememberManager: false,
    get: async ({ sessionID }) => ({ data: { data: sessions.get(sessionID) } }),
  })
  try {
    await appendFile(logPath, liveLogLine({ sessionID: "recovered-child" }))
    await pollUntil(() => fixture.interrupts.length === 1)

    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "rotated-child", parentID: "manager", agent: "glm" },
    }))
    await rename(logPath, `${logPath}.1`)
    await writeFile(logPath, liveLogLine({ sessionID: "rotated-child" }))
    await pollUntil(() => fixture.interrupts.length === 2)

    await fixture.hooks.event(sessionEvent("session.created", {
      info: { id: "truncated-child", parentID: "manager", agent: "glm" },
    }))
    await truncate(logPath, 0)
    await new Promise((resolve) => setTimeout(resolve, 50))
    await appendFile(logPath, liveLogLine({ sessionID: "truncated-child" }))
    await pollUntil(() => fixture.interrupts.length === 3)

    await fixture.hooks.event(sessionEvent("session.idle", { sessionID: "truncated-child" }))
    await fixture.hooks.dispose()
    await appendFile(logPath, liveLogLine({ sessionID: "disposed-child" }))
    await new Promise((resolve) => setTimeout(resolve, 60))
    assert.deepEqual(fixture.interrupts, [
      { sessionID: "recovered-child" },
      { sessionID: "rotated-child" },
      { sessionID: "truncated-child" },
    ])
  } finally {
    await fixture.hooks.dispose()
    await rm(directory, { recursive: true, force: true })
  }
})
*/
