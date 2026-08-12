import assert from "node:assert/strict"
import { EventEmitter } from "node:events"
import test from "node:test"

import {
  cellWidth,
  createKeyedControllerPool,
  createRefreshScheduler,
  createSpawnLimiter,
  createStatuslineController,
  createStatuslineViewModel,
  deriveSessionEconomics,
  initialStatuslineState,
  nextStatuslineState,
  parseStatuslineJson,
  renderSegments,
  renderStatuslineText,
  renderStatuslineViewText,
  runStatusline,
  statuslineBinaryCandidates,
  statuslineCommandArgs,
} from "./rtrt-statusline-core.mjs"

const payload = (segments, stale = false, degraded = []) => ({ version: 1, stale, degraded, segments })

const RICH_SEGMENTS = [
  { id: "project", text: "00G_rtrt", compact: "00G_rtrt", tone: "accent", priority: 100 },
  { id: "style", text: "opt:full", compact: "opt:full", tone: "accent", priority: 90 },
  { id: "savings", text: "save:87% cmd:26%", compact: "save:87% cmd:26%", tone: "good", priority: 80 },
  { id: "headroom", text: "room:opencode:73%", compact: "room:opencode:73%", tone: "good", priority: 70 },
  { id: "git", text: "main*", compact: "main*", tone: "warn", priority: 65 },
  { id: "session", text: "sess:contract-ses", compact: "sess:contract-ses", tone: "muted", priority: 50 },
  { id: "memory", text: "mem:12 40%", compact: "mem:12 40%", tone: "good", priority: 40 },
]

const OFFICIAL_LIMIT_SEGMENTS = [
  { id: "limit_5h", text: "5h:72% reset:1h 20m", compact: null, tone: "warn", priority: 75 },
  { id: "limit_week", text: "week:91% reset:2d 4h", compact: null, tone: "bad", priority: 74 },
]

const PROVIDERS = [
  {
    id: "openai",
    models: {
      "model-a": { id: "model-a", providerID: "openai", name: "Model Alpha", limit: { context: 800 } },
      "model-b": { id: "model-b", providerID: "openai", name: "Model Beta", limit: { context: 2_000 } },
    },
  },
]

const assistantMessage = (sessionID, modelID, tokens) => ({
  id: `${sessionID}-${modelID}`,
  sessionID,
  role: "assistant",
  providerID: "openai",
  modelID,
  cost: 999,
  time: { created: 1, completed: 2 },
  tokens,
})

const ECONOMICS = deriveSessionEconomics({
  session: { id: "economics", cost: 1.234, model: { id: "model-b", providerID: "openai" } },
  messages: [
    assistantMessage("economics", "model-a", {
      input: 100,
      output: 20,
      reasoning: 30,
      cache: { read: 200, write: 50 },
    }),
  ],
  providers: PROVIDERS,
  status: { type: "busy" },
})

const RETRY_ECONOMICS = { ...ECONOMICS, status: { text: "RETRY", tone: "bad" } }

// Shape captured from `rtrt statusline --opencode --session contract-session --width 120`.
const CLI_CONTRACT_FIXTURE = JSON.stringify({
  cwd: "/repo",
  data: {
    git: { branch: "main", cached: false, dirty: true },
    headroom: {
      opencode: { tokens_estimated: true, used_requests: 109, used_tokens: 325112 },
    },
    savings: {
      base_chars: 219788,
      cached: false,
      command_pct: 26,
      pct: 87,
      recall_chars: 182404,
      saved_chars: 192076,
    },
    session: "contract-session",
    style: "full",
    width: 120,
  },
  degraded: ["headroom", "memory", "budget"],
  project: "00G_rtrt",
  segments: [
    { id: "project", pri: 100, text: "00G_rtrt", tone: "accent" },
    { id: "style", pri: 90, text: "opt:full", tone: "accent" },
    { id: "savings", pri: 80, text: "save:87% cmd:26%", tone: "good" },
    { id: "git", pri: 65, text: "main*", tone: "warn" },
    { id: "session", pri: 50, text: "sess:contract-ses", tone: "muted" },
  ],
  stale: true,
  took_ms: 158,
  ts: 1785829346,
  v: 1,
})

const fakeSpawn = (steps, calls = []) => (binary, args, options) => {
  calls.push({ binary, args, options })
  const child = new EventEmitter()
  child.stdout = new EventEmitter()
  child.kill = () => true
  const step = steps.shift() ?? { error: Object.assign(new Error("missing"), { code: "ENOENT" }) }
  queueMicrotask(() => {
    if (step.error) {
      child.emit("error", step.error)
      return
    }
    if (step.stdout !== undefined) child.stdout.emit("data", Buffer.from(step.stdout))
    child.emit("close", step.code ?? 0)
  })
  return child
}

const wait = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds))

const flushMicrotasks = async () => {
  for (let index = 0; index < 8; index += 1) await Promise.resolve()
}

const createFakeClock = () => {
  let now = 0
  let nextID = 0
  const timers = new Map()
  const setTimeoutFn = (callback, delay) => {
    const handle = {
      id: nextID,
      at: now + Math.max(0, delay),
      callback,
      unref() {},
    }
    nextID += 1
    timers.set(handle.id, handle)
    return handle
  }
  const clearTimeoutFn = (handle) => timers.delete(handle?.id)
  return {
    nowFn: () => now,
    setTimeoutFn,
    clearTimeoutFn,
    timerCount: () => timers.size,
    advance(milliseconds) {
      const target = now + milliseconds
      while (true) {
        const ready = [...timers.values()]
          .filter((timer) => timer.at <= target)
          .sort((left, right) => left.at - right.at || left.id - right.id)[0]
        if (!ready) break
        timers.delete(ready.id)
        now = ready.at
        ready.callback()
      }
      now = target
    },
  }
}

test("accepts the live CLI contract and maps every emitted tone", () => {
  assert.deepEqual(parseStatuslineJson(CLI_CONTRACT_FIXTURE), {
    version: 1,
    stale: true,
    degraded: ["headroom", "memory", "budget"],
    segments: [
      { id: "project", text: "00G_rtrt", compact: "00G_rtrt", tone: "accent", priority: 100 },
      { id: "style", text: "opt:full", compact: "opt:full", tone: "accent", priority: 90 },
      {
        id: "savings",
        text: "save:87% cmd:26%",
        compact: "save:87% cmd:26%",
        tone: "good",
        priority: 80,
      },
      { id: "git", text: "main*", compact: "main*", tone: "warn", priority: 65 },
      {
        id: "session",
        text: "sess:contract-ses",
        compact: "sess:contract-ses",
        tone: "muted",
        priority: 50,
      },
    ],
  })
  assert.equal(
    parseStatuslineJson('{"v":1,"segments":[{"id":"headroom","text":"room:codex:8%","tone":"bad","pri":70}]}')
      .segments[0].tone,
    "bad",
  )
  assert.equal(
    parseStatuslineJson('{"v":1,"segments":[{"id":"future","text":"future","tone":"future-tone","pri":1}]}')
      .segments[0].tone,
    "muted",
  )
})

test("rejects incompatible versions, malformed segments, and control characters", () => {
  for (const raw of [
    "not json",
    '{"v":2,"segments":[{"id":"style","text":"opt:full","tone":"accent","pri":90}]}',
    '{"version":1,"segments":[{"id":"style","text":"opt:full","tone":"accent","pri":90}]}',
    '{"v":1,"segments":[]}',
    '{"v":1,"segments":[{"id":"style","text":4,"tone":"accent","pri":90}]}',
    '{"v":1,"segments":[{"id":"style","text":"bad\\u001b[31m","tone":"accent","pri":90}]}',
    '{"v":1,"degraded":"memory","segments":[{"id":"style","text":"opt:full","tone":"accent","pri":90}]}',
  ]) {
    assert.equal(parseStatuslineJson(raw), undefined)
  }
})

test("reported zero may be included, free, or unpriced and stays ambiguous and muted", () => {
  assert.equal(deriveSessionEconomics().cost.text, "N/A")
  const zero = deriveSessionEconomics({
    session: { cost: 0 },
    messages: [assistantMessage("zero", "model-a", { input: 0, output: 1, reasoning: 0, cache: { read: 0, write: 0 } })],
    providers: PROVIDERS,
  })
  assert.deepEqual(zero.cost, { text: "$0?", tone: "muted" })
  assert.equal(
    deriveSessionEconomics({
      session: { cost: 12.345 },
      messages: [
        { ...assistantMessage("positive", "model-a", { input: 1, output: 0, reasoning: 0, cache: { read: 0, write: 0 } }), cost: 9_999 },
      ],
      providers: PROVIDERS,
    }).cost.text,
    "~$12.35",
  )
})

test("derives exact-model context from latest completed output turn including reasoning and cache", () => {
  const economics = deriveSessionEconomics({
    session: { cost: 1, model: { id: "model-b", providerID: "openai" } },
    messages: [
      assistantMessage("switch", "model-a", {
        input: 100,
        output: 20,
        reasoning: 30,
        cache: { read: 200, write: 50 },
        total: 1,
      }),
      assistantMessage("switch", "model-b", {
        input: 0,
        output: 0,
        reasoning: 0,
        cache: { read: 0, write: 0 },
        total: 999_999,
      }),
      {
        ...assistantMessage("switch", "model-b", {
          input: 1_900,
          output: 100,
          reasoning: 0,
          cache: { read: 0, write: 0 },
        }),
        time: { created: 3 },
      },
      {
        ...assistantMessage("switch", "model-b", {
          input: 1_900,
          output: 100,
          reasoning: 0,
          cache: { read: 0, write: 0 },
        }),
        error: { name: "APIError" },
      },
    ],
    providers: PROVIDERS,
    status: { type: "idle" },
  })
  assert.deepEqual(economics.context, {
    text: "50%",
    tone: "good",
    used: 400,
    limit: 800,
    percent: 50,
    headroom: 50,
  })
  assert.equal(economics.model.text, "Model Beta")
  assert.equal(economics.modelRef, "openai/model-b")
  assert.equal(economics.status.text, "IDLE")
  assert.equal(economics.week.text, "N/A/not exposed")

  const missingExactLimit = deriveSessionEconomics({
    session: { model: { id: "model-b", providerID: "openai" } },
    messages: [assistantMessage("switch", "model-a", { input: 399, output: 1, reasoning: 0, cache: { read: 0, write: 0 } })],
    providers: [{ id: "openai", models: { "model-b": PROVIDERS[0].models["model-b"] } }],
  })
  assert.equal(missingExactLimit.model.text, "Model Beta")
  assert.equal(missingExactLimit.context.text, "N/A")

  const overflow = deriveSessionEconomics({
    messages: [assistantMessage("clamp", "model-a", { input: 900, output: 1, reasoning: 0, cache: { read: 0, write: 0 } })],
    providers: PROVIDERS,
  })
  assert.deepEqual(overflow.context, {
    text: "113%",
    tone: "warn",
    used: 901,
    limit: 800,
    percent: 113,
    headroom: 0,
  })
})

test("derives busy, retry, idle, and unknown session status explicitly", () => {
  assert.equal(deriveSessionEconomics({ session: {}, status: { type: "busy" } }).status.text, "BUSY")
  assert.equal(deriveSessionEconomics({ session: {}, status: { type: "retry", attempt: 2 } }).status.text, "RETRY")
  assert.equal(deriveSessionEconomics({ session: {}, status: { type: "idle" } }).status.text, "IDLE")
  assert.equal(deriveSessionEconomics({ session: {} }).status.text, "IDLE")
  assert.equal(deriveSessionEconomics().status.text, "UNKNOWN")
})

test("two sessions retain distinct economics and prompt-right snapshots", () => {
  const alpha = deriveSessionEconomics({
    session: { cost: 1.25, model: { id: "model-a", providerID: "openai" } },
    messages: [assistantMessage("alpha", "model-a", { input: 399, output: 1, reasoning: 0, cache: { read: 0, write: 0 } })],
    providers: PROVIDERS,
    status: { type: "busy" },
  })
  const beta = deriveSessionEconomics({
    session: { cost: 0, model: { id: "model-b", providerID: "openai" } },
    messages: [assistantMessage("beta", "model-b", { input: 99, output: 1, reasoning: 0, cache: { read: 50, write: 50 } })],
    providers: PROVIDERS,
    status: { type: "retry", attempt: 3 },
  })
  const state = nextStatuslineState(initialStatuslineState(), payload(RICH_SEGMENTS))

  assert.equal(renderStatuslineViewText(state, { width: 40, economics: alpha, promptRight: true }), "87% ~$1.25 50%")
  assert.equal(renderStatuslineViewText(state, { width: 40, economics: beta, promptRight: true }), "87% $0? 10%")
  assert.equal(alpha.modelRef, "openai/model-a")
  assert.equal(beta.modelRef, "openai/model-b")
  assert.match(renderStatuslineViewText(state, { width: 80, economics: beta }), /STATE RETRY/)
})

test("missing or malformed binaries fail closed without throwing", async () => {
  const spawnImpl = fakeSpawn([
    { stdout: "not json" },
    { error: Object.assign(new Error("missing env binary"), { code: "ENOENT" }) },
    { error: Object.assign(new Error("missing PATH binary"), { code: "ENOENT" }) },
  ])
  const result = await runStatusline({
    bin: "/custom/rtrt",
    env: { RTRT_BIN: "/env/rtrt" },
    cwd: "/repo",
    session: "ses-1",
    width: 90,
    spawnImpl,
    timeoutMs: 20,
  })
  assert.equal(result, undefined)
})

test("uses binary precedence and passes an uninterpolated argv", async () => {
  const calls = []
  const spawnImpl = fakeSpawn(
    [{ stdout: '{"v":1,"segments":[{"id":"style","text":"opt:full","tone":"accent","pri":90}]}' }],
    calls,
  )
  await runStatusline({
    bin: "/opt/rtrt custom",
    env: { RTRT_BIN: "/env/rtrt" },
    cwd: "/repo with spaces; touch nope",
    session: "ses;$(nope)",
    width: 101.8,
    spawnImpl,
  })

  assert.deepEqual(statuslineBinaryCandidates("/opt/rtrt custom", { RTRT_BIN: "/env/rtrt" }), [
    "/opt/rtrt custom",
    "/env/rtrt",
    "rtrt",
  ])
  assert.deepEqual(calls[0].args, [
    "statusline",
    "--opencode",
    "--cwd",
    "/repo with spaces; touch nope",
    "--session",
    "ses;$(nope)",
    "--width",
    "101",
  ])
  assert.equal(calls[0].binary, "/opt/rtrt custom")
  assert.equal(calls[0].options.shell, false)
  assert.equal(calls[0].args.includes("--model"), false)
  assert.deepEqual(
    statuslineCommandArgs({ cwd: "/x", session: "s", width: 44 }),
    ["statusline", "--opencode", "--cwd", "/x", "--session", "s", "--width", "44"],
  )
})

test("binary fallbacks share one total deadline", async () => {
  const clock = createFakeClock()
  const calls = []
  const killed = []
  const spawnImpl = (binary, args, options) => {
    calls.push({ binary, args, options })
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    child.kill = () => {
      killed.push(binary)
      return true
    }
    if (calls.length === 1) clock.setTimeoutFn(() => child.emit("close", 1), 600)
    return child
  }
  let settled = false
  const result = runStatusline({
    bin: "/first/rtrt",
    env: { RTRT_BIN: "/second/rtrt" },
    cwd: "/repo",
    session: "deadline",
    width: 80,
    spawnImpl,
    timeoutMs: 1_500,
    nowFn: clock.nowFn,
    setTimeoutFn: clock.setTimeoutFn,
    clearTimeoutFn: clock.clearTimeoutFn,
  })
  void result.then(() => {
    settled = true
  })

  await flushMicrotasks()
  assert.deepEqual(calls.map((call) => call.binary), ["/first/rtrt"])
  clock.advance(600)
  await flushMicrotasks()
  assert.deepEqual(calls.map((call) => call.binary), ["/first/rtrt", "/second/rtrt"])
  clock.advance(899)
  await flushMicrotasks()
  assert.equal(settled, false)
  clock.advance(1)
  assert.equal(await result, undefined)
  assert.equal(clock.nowFn(), 1_500)
  assert.deepEqual(killed, ["/second/rtrt"])
  assert.equal(clock.timerCount(), 0)
})

test("forwards only a safe current selected model argv", () => {
  assert.equal(
    statuslineCommandArgs({ cwd: "/repo", session: "session-1", width: 100 }).includes("--model"),
    false,
  )
  assert.deepEqual(
    statuslineCommandArgs({
      cwd: "/repo",
      session: "session-1",
      width: 100,
      model: "openai/model-b",
    }).slice(-2),
    ["--model", "openai/model-b"],
  )
  assert.equal(
    statuslineCommandArgs({ cwd: "/repo", session: "session-1", width: 100, model: "bad\nmodel" }).includes("--model"),
    false,
  )
})

test("responsive view model matches width-tier goldens", () => {
  const state = nextStatuslineState(initialStatuslineState(), payload(RICH_SEGMENTS))
  const golden = {
    120: "[RTRT]  00G_rtrt  main*  MODEL Model Beta  STATE BUSY  LIVE\nOPT full | SAVE 87% | CMD 26% | MEM 12 40% | ROOM opencode 73% | COST ~$1.23 | CTX 50% | WEEK N/A/not exposed",
    100: "[RTRT]  00G_rtrt  main*  MODEL Model Beta  STATE BUSY  LIVE\nOPT full | SAVE 87% | CMD 26% | MEM 12 40% | COST ~$1.23 | CTX 50% | WEEK N/A/not exposed",
    99: "[RTRT] | 00G_… | MODEL Model Beta | STATE BUSY | OPT full | SAVE 87% | COST ~$1.23 | CTX 50% | LIVE",
    70: "[RTRT] | STATE BUSY | SAVE 87% | COST ~$1.23 | CTX 50% | LIVE",
    69: "full 87% ~$1.23",
    12: "full 87%",
  }

  for (const [width, expected] of Object.entries(golden)) {
    assert.equal(renderStatuslineViewText(state, { width: Number(width), economics: ECONOMICS }), expected)
    assert.equal(renderStatuslineText(state, { width: Number(width), economics: ECONOMICS }), expected)
  }
  assert.equal(
    renderStatuslineViewText(state, { width: 40, economics: ECONOMICS, promptRight: true }),
    "87% ~$1.23 50%",
  )
  assert.equal(
    renderStatuslineViewText(state, { width: 12, economics: ECONOMICS, promptRight: true }),
    "87% ~$1.23",
  )
  assert.equal(createStatuslineViewModel(state, { width: 100, economics: ECONOMICS }).layout, "wide")
  assert.equal(createStatuslineViewModel(state, { width: 99, economics: ECONOMICS }).layout, "medium")
  assert.equal(createStatuslineViewModel(state, { width: 69, economics: ECONOMICS }).layout, "narrow")
  for (let width = 1; width <= 140; width += 1) {
    for (const row of createStatuslineViewModel(state, { width, economics: ECONOMICS }).rows) {
      assert.ok(row.parts.reduce((sum, part) => sum + cellWidth(part.text), 0) <= width)
    }
  }
})

test("fresh official limits render compactly with retry and preserve warning and error tones", () => {
  const state = nextStatuslineState(
    initialStatuslineState(),
    payload([...RICH_SEGMENTS, ...OFFICIAL_LIMIT_SEGMENTS]),
  )
  assert.equal(
    renderStatuslineViewText(state, { width: 120, economics: RETRY_ECONOMICS }),
    "[RTRT]  00G_rtrt  main*  MODEL Model Beta  STATE RETRY  LIVE\nOPT full | SAVE 87% | CMD 26% | MEM 12 40% | ROOM opencode 73% | COST ~$1.23 | CTX 50% | 5H 72%/1h20m | WEEK 91%/2d4h",
  )
  assert.equal(
    renderStatuslineViewText(state, { width: 99, economics: RETRY_ECONOMICS }),
    "[RTRT] | STATE RETRY | SAVE 87% | COST ~$1.23 | CTX 50% | 5H 72%/1h20m | WEEK 91%/2d4h | LIVE",
  )
  assert.equal(
    renderStatuslineViewText(state, { width: 70, economics: RETRY_ECONOMICS }),
    "~$1.23 50% RETRY 5H 72%/1h20m WEEK 91%/2d4h LIVE",
  )
  const limitParts = createStatuslineViewModel(state, { width: 120, economics: RETRY_ECONOMICS }).rows
    .flatMap((row) => row.parts)
    .filter((part) => part.role === "value" && ["limit-5h", "limit-week"].includes(part.id))
  assert.deepEqual(
    limitParts.map((part) => ({ id: part.id, text: part.text, tone: part.tone })),
    [
      { id: "limit-5h", text: "72%/1h20m", tone: "warn" },
      { id: "limit-week", text: "91%/2d4h", tone: "bad" },
    ],
  )
})

test("official limit percentage token rejects signed, out-of-range, and malformed values", () => {
  for (const text of [
    "week:-1% reset:2d",
    "week:+1% reset:2d",
    "week:101% reset:2d",
    "week:NaN% reset:2d",
    "week:1e2% reset:2d",
    "week:.5% reset:2d",
    "week:1.% reset:2d",
    "week:50%% reset:2d",
  ]) {
    const state = nextStatuslineState(
      initialStatuslineState(),
      payload([...RICH_SEGMENTS, { ...OFFICIAL_LIMIT_SEGMENTS[1], text }]),
    )
    const rendered = renderStatuslineViewText(state, { width: 140, economics: RETRY_ECONOMICS })
    assert.match(rendered, /WEEK N\/A\/not exposed/)
    assert.equal(
      createStatuslineViewModel(state, { width: 140, economics: RETRY_ECONOMICS }).rows
        .flatMap((row) => row.parts)
        .some((part) => part.id === "limit-week"),
      false,
      text,
    )
  }
})

test("official limit percentage token accepts decimals, rounded integers, and bounds", () => {
  for (const [text, expected] of [
    ["week:12.5% reset:2d 4h", "WEEK 12.5%/2d4h"],
    ["week:73% reset:5h", "WEEK 73%/5h"],
    ["week:0% reset:7d", "WEEK 0%/7d"],
    ["week:100.0% reset:1m", "WEEK 100.0%/1m"],
  ]) {
    const state = nextStatuslineState(
      initialStatuslineState(),
      payload([...RICH_SEGMENTS, { ...OFFICIAL_LIMIT_SEGMENTS[1], text }]),
    )
    assert.match(
      renderStatuslineViewText(state, { width: 140, economics: RETRY_ECONOMICS }),
      new RegExp(expected.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&")),
    )
  }
})

test("partial and stale official limits keep truthful weekly fallback", () => {
  const fiveHourOnly = nextStatuslineState(
    initialStatuslineState(),
    payload([...RICH_SEGMENTS, OFFICIAL_LIMIT_SEGMENTS[0]]),
  )
  assert.equal(
    renderStatuslineViewText(fiveHourOnly, { width: 120, economics: RETRY_ECONOMICS }).split("\n")[1],
    "OPT full | SAVE 87% | CMD 26% | MEM 12 40% | COST ~$1.23 | CTX 50% | 5H 72%/1h20m | WEEK N/A/not exposed",
  )

  const weekOnly = nextStatuslineState(
    initialStatuslineState(),
    payload([...RICH_SEGMENTS, OFFICIAL_LIMIT_SEGMENTS[1]]),
  )
  assert.equal(
    renderStatuslineViewText(weekOnly, { width: 80, economics: RETRY_ECONOMICS }),
    "[RTRT] | STATE RETRY | SAVE 87% | COST ~$1.23 | CTX 50% | WEEK 91%/2d4h | LIVE",
  )

  const stale = nextStatuslineState(
    initialStatuslineState(),
    payload([...RICH_SEGMENTS, ...OFFICIAL_LIMIT_SEGMENTS], true, ["official_cache"]),
  )
  const staleText = renderStatuslineViewText(stale, { width: 120, economics: RETRY_ECONOMICS })
  assert.equal(
    staleText,
    "[RTRT]  00G_rtrt  main*  MODEL Model Beta  STATE RETRY  STALE  DEGRADED\nOPT full | SAVE 87% | CMD 26% | MEM 12 40% | ROOM opencode 73% | COST ~$1.23 | CTX 50% | WEEK N/A/not exposed",
  )
  assert.equal(staleText.includes("5H "), false)
  assert.equal(staleText.includes("91%"), false)
})

test("prompt-right includes fresh weekly percentage only when it fits", () => {
  const state = nextStatuslineState(
    initialStatuslineState(),
    payload([...RICH_SEGMENTS, ...OFFICIAL_LIMIT_SEGMENTS]),
  )
  const golden = {
    40: "87% ~$1.23 50% 91%",
    16: "87% ~$1.23 50%",
    12: "87% ~$1.23",
  }
  for (const [width, expected] of Object.entries(golden)) {
    const view = createStatuslineViewModel(state, {
      width: Number(width),
      economics: RETRY_ECONOMICS,
      promptRight: true,
    })
    assert.equal(view.rows[0].parts.map((part) => part.text).join(""), expected)
    assert.ok(view.rows[0].parts.reduce((sum, part) => sum + cellWidth(part.text), 0) <= Number(width))
  }
})

test("hostile official resets stay bounded without displacing provider or freshness status", () => {
  const state = nextStatuslineState(
    initialStatuslineState(),
    payload(
      [
        ...RICH_SEGMENTS,
        { ...OFFICIAL_LIMIT_SEGMENTS[0], text: `72% reset:${"x".repeat(246)}` },
        { ...OFFICIAL_LIMIT_SEGMENTS[1], text: `91% reset:${"y".repeat(246)}` },
      ],
      false,
      ["other"],
    ),
  )
  assert.equal(
    renderStatuslineViewText(state, { width: 70, economics: RETRY_ECONOMICS }),
    "~$1.23 50% RETRY 5H 72%/xxxxxxx… WEEK 91%/yyyyyyy… DEGRADED",
  )
  for (let width = 70; width <= 140; width += 1) {
    for (const row of createStatuslineViewModel(state, { width, economics: RETRY_ECONOMICS }).rows) {
      assert.ok(row.parts.reduce((sum, part) => sum + cellWidth(part.text), 0) <= width)
    }
  }
})

test("view model separates theme-aware labels and values", () => {
  const state = nextStatuslineState(initialStatuslineState(), payload(RICH_SEGMENTS))
  const view = createStatuslineViewModel(state, { width: 120, economics: ECONOMICS })
  assert.deepEqual(
    view.rows[1].parts.filter((part) => part.role === "label").map((part) => part.text.trim()),
    ["OPT", "SAVE", "CMD", "MEM", "ROOM", "COST", "CTX", "WEEK"],
  )
  assert.deepEqual(
    view.rows[1].parts.filter((part) => part.role === "value").map((part) => part.tone),
    ["accent", "good", "good", "good", "good", "accent", "good", "muted"],
  )
  assert.equal(view.rows[0].parts[0].role, "badge")
})

test("stale, degraded, missing, and long-project goldens stay explicit and bounded", () => {
  const stale = nextStatuslineState(
    initialStatuslineState(),
    payload(RICH_SEGMENTS, true, ["headroom", "memory"]),
  )
  assert.equal(
    renderStatuslineViewText(stale, { width: 120, economics: ECONOMICS }),
    "[RTRT]  00G_rtrt  main*  MODEL Model Beta  STATE BUSY  STALE  DEGRADED\nOPT full | SAVE 87% | CMD 26% | MEM 12 40% | ROOM opencode 73% | COST ~$1.23 | CTX 50% | WEEK N/A/not exposed",
  )
  assert.ok(
    createStatuslineViewModel(stale, { width: 120, economics: ECONOMICS }).rows
      .flatMap((row) => row.parts)
      .filter((part) => ["project", "git", "style", "savings", "command", "memory", "headroom"].includes(part.id))
      .every((part) => part.role === "label" || part.tone === "muted"),
  )

  assert.equal(
    renderStatuslineViewText(initialStatuslineState(), { width: 120, economics: ECONOMICS }),
    "[RTRT]  N/A  MODEL Model Beta  STATE BUSY  DEGRADED\nOPT N/A | SAVE N/A | CMD N/A | MEM N/A | ROOM N/A | COST ~$1.23 | CTX 50% | WEEK N/A/not exposed",
  )
  assert.equal(
    renderStatuslineViewText(initialStatuslineState(), { width: 80, economics: ECONOMICS }),
    "[RTRT] | N/A | MODEL Model Beta | STATE BUSY | COST ~$1.23 | CTX 50% | DEGRADED",
  )
  assert.equal(
    renderStatuslineViewText(initialStatuslineState(), { width: 40, economics: ECONOMICS }),
    "~$1.23 DEGRADED",
  )

  const long = nextStatuslineState(
    initialStatuslineState(),
    payload([
      {
        id: "project",
        text: "an-extraordinarily-long-project-name-that-must-not-overflow-the-statusline",
        compact: null,
        tone: "accent",
        priority: 100,
      },
      ...RICH_SEGMENTS.filter((segment) => segment.id !== "project"),
    ]),
  )
  assert.equal(
    renderStatuslineViewText(long, { width: 100, economics: ECONOMICS }).split("\n")[0],
    "[RTRT]  an-extraordinarily-long-project-nam…  main*  MODEL Model Beta  STATE BUSY  LIVE",
  )
  for (const row of createStatuslineViewModel(long, { width: 100, economics: ECONOMICS }).rows) {
    assert.ok(row.parts.reduce((sum, part) => sum + cellWidth(part.text), 0) <= 100)
  }
})

test("medium adversarial maximums preserve terminal stale and degraded status", () => {
  const segments = [
    { id: "project", text: "p".repeat(256), compact: null, tone: "accent", priority: 100 },
    { id: "style", text: `opt:${"x".repeat(252)}`, compact: null, tone: "accent", priority: 90 },
    {
      id: "savings",
      text: `save:${"y".repeat(120)} cmd:${"z".repeat(126)}`,
      compact: null,
      tone: "good",
      priority: 80,
    },
    { id: "headroom", text: `room:${"h".repeat(251)}`, compact: null, tone: "warn", priority: 70 },
  ]
  assert.ok(segments.every((segment) => segment.text.length === 256))
  const state = nextStatuslineState(
    initialStatuslineState(),
    payload(segments, true, ["headroom", "budget"]),
  )
  const economics = { ...ECONOMICS, status: { text: "RETRY", tone: "bad" } }
  const expected = "[RTRT] | STATE RETRY | COST ~$1.23 | CTX 50% | STALE | DEGRADED"
  assert.equal(renderStatuslineViewText(state, { width: 70, economics }), expected)
  assert.ok(cellWidth(expected) <= 70)
  assert.ok(expected.endsWith("STALE | DEGRADED"))
})

test("debounces bursts and never overlaps refreshes", async () => {
  let runs = 0
  let active = 0
  let maxActive = 0
  let release
  const first = new Promise((resolve) => {
    release = resolve
  })
  const scheduler = createRefreshScheduler({
    debounceMs: 15,
    intervalMs: 0,
    run: async () => {
      runs += 1
      active += 1
      maxActive = Math.max(maxActive, active)
      if (runs === 1) await first
      active -= 1
    },
  })

  scheduler.request()
  scheduler.request()
  scheduler.request()
  await wait(25)
  assert.equal(runs, 1)
  scheduler.request()
  scheduler.request()
  await wait(25)
  assert.equal(runs, 1)
  release()
  await scheduler.flush()
  assert.equal(runs, 2)
  assert.equal(maxActive, 1)
  scheduler.dispose()
})

test("app controller rejects an old-session result after a coalesced context switch", async () => {
  const pending = []
  const committed = []
  const controller = createStatuslineController({
    intervalMs: 0,
    load: (context, signal) =>
      new Promise((resolve) => {
        pending.push({ context, signal, resolve })
      }),
    onState: (state) => committed.push(state),
  })
  controller.setContext({ cwd: "/repo/alpha", session: "alpha", width: 80, model: "openai/model-a" })
  const first = controller.refreshNow()
  await flushMicrotasks()
  assert.equal(pending.length, 1)
  assert.equal(pending[0].context.session, "alpha")

  controller.setContext({ cwd: "/repo/beta", session: "beta", width: 90, model: "openai/model-b" })
  const switched = controller.refreshNow()
  const coalesced = controller.refreshNow()
  assert.equal(pending[0].signal.aborted, true)
  pending[0].resolve(
    payload([{ id: "style", text: "opt:alpha", compact: null, tone: "accent", priority: 90 }]),
  )
  await flushMicrotasks()

  assert.equal(committed.length, 0)
  assert.equal(pending.length, 2)
  assert.deepEqual(pending[1].context, {
    cwd: "/repo/beta",
    session: "beta",
    width: 90,
    model: "openai/model-b",
  })
  pending[1].resolve(
    payload([{ id: "style", text: "opt:beta", compact: null, tone: "accent", priority: 90 }]),
  )
  await Promise.all([first, switched, coalesced])

  assert.equal(committed.length, 1)
  assert.equal(controller.getState().payload.segments[0].text, "opt:beta")
  assert.equal(controller.getContext().session, "beta")
  controller.dispose()
})

test("two concurrent session controllers keep distinct argv and snapshots", async () => {
  const calls = []
  const spawnImpl = (binary, args, options) => {
    const session = args[args.indexOf("--session") + 1]
    const sessionSavings = session === "alpha" ? 11 : 22
    return fakeSpawn(
      [
        {
          stdout: JSON.stringify({
            v: 1,
            stale: false,
            segments: [
              { id: "style", text: `opt:${session}`, tone: "accent", pri: 90 },
              { id: "savings", text: `save:${sessionSavings}%`, tone: "good", pri: 80 },
            ],
          }),
        },
      ],
      calls,
    )(binary, args, options)
  }
  const pool = createKeyedControllerPool({
    create() {
      let state = initialStatuslineState()
      const controller = createStatuslineController({
        intervalMs: 0,
        load: (context, signal) =>
          runStatusline({ ...context, env: {}, spawnImpl, signal, timeoutMs: 100 }),
        onState: (value) => {
          state = value
        },
      })
      return { controller, state: () => state, dispose: controller.dispose }
    },
  })
  const alpha = pool.acquire("alpha")
  const beta = pool.acquire("beta")
  alpha.value.controller.setContext({
    cwd: "/repo/alpha",
    session: "alpha",
    width: 40,
    model: "openai/model-a",
  })
  beta.value.controller.setContext({
    cwd: "/repo/beta",
    session: "beta",
    width: 40,
    model: "openai/model-b",
  })

  await Promise.all([alpha.value.controller.refreshNow(), beta.value.controller.refreshNow()])

  assert.deepEqual(
    calls.map((call) => call.args[call.args.indexOf("--session") + 1]).sort(),
    ["alpha", "beta"],
  )
  assert.deepEqual(
    calls.map((call) => call.args[call.args.indexOf("--model") + 1]).sort(),
    ["openai/model-a", "openai/model-b"],
  )
  assert.equal(renderStatuslineViewText(alpha.value.state(), { width: 40 }), "alpha 11% N/A")
  assert.equal(renderStatuslineViewText(beta.value.state(), { width: 40 }), "beta 22% N/A")
  alpha.release()
  assert.equal(pool.size(), 1)
  assert.equal(pool.get("beta"), beta.value)
  beta.release()
  assert.equal(pool.size(), 0)
})

test("shared limiter caps a 12-session broadcast, prioritizes active session, and cancels cleanly", async () => {
  const limiter = createSpawnLimiter({ concurrency: 2 })
  const activeProcesses = new Map()
  const killed = []
  const started = []
  let active = 0
  let maxActive = 0
  const spawnImpl = (_binary, args) => {
    const session = args[args.indexOf("--session") + 1]
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    let closed = false
    const finishProcess = () => {
      if (closed) return false
      closed = true
      active -= 1
      activeProcesses.delete(session)
      return true
    }
    child.kill = () => {
      if (finishProcess()) killed.push(session)
      return true
    }
    child.succeed = (saving) => {
      if (!finishProcess()) return
      child.stdout.emit(
        "data",
        Buffer.from(
          JSON.stringify({
            v: 1,
            segments: [
              { id: "style", text: "opt:full", tone: "accent", pri: 90 },
              { id: "savings", text: `save:${saving}%`, tone: "good", pri: 80 },
            ],
          }),
        ),
      )
      child.emit("close", 0)
    }
    active += 1
    maxActive = Math.max(maxActive, active)
    started.push(session)
    activeProcesses.set(session, child)
    return child
  }

  const controllers = Array.from({ length: 12 }, (_, index) => {
    const session = `session-${index}`
    let state = initialStatuslineState()
    const controller = createStatuslineController({
      intervalMs: 0,
      load: (context, signal) =>
        limiter.run(
          (limitedSignal) =>
            runStatusline({
              ...context,
              bin: "/rtrt",
              env: {},
              signal: limitedSignal,
              spawnImpl,
              timeoutMs: 60_000,
            }),
          { signal, priority: context.session === "session-11" ? 1 : 0 },
        ),
      onState: (value) => {
        state = value
      },
    })
    controller.setContext({ cwd: `/repo/${session}`, session, width: 40 })
    return { controller, state: () => state }
  })

  const refreshes = controllers.map((entry) => entry.controller.refreshNow())
  await flushMicrotasks()
  assert.deepEqual(started, ["session-0", "session-1"])
  assert.equal(maxActive, 2)
  assert.equal(limiter.activeCount(), 2)
  assert.equal(limiter.queuedCount(), 10)

  activeProcesses.get("session-0").succeed(10)
  await flushMicrotasks()
  assert.deepEqual(started.slice(0, 3), ["session-0", "session-1", "session-11"])
  activeProcesses.get("session-11").succeed(99)
  await flushMicrotasks()
  assert.equal(started[3], "session-2")
  assert.equal(renderStatuslineViewText(controllers[0].state(), { width: 40 }), "full 10% N/A")
  assert.equal(renderStatuslineViewText(controllers[11].state(), { width: 40 }), "full 99% N/A")

  for (let index = 0; index < 6; index += 1) controllers[10].controller.request(true)
  for (const entry of controllers) entry.controller.dispose()
  limiter.dispose()
  await Promise.all(refreshes)
  await flushMicrotasks()

  assert.equal(started.length, 4)
  assert.equal(started.includes("session-10"), false)
  assert.deepEqual(killed.sort(), ["session-1", "session-2"])
  assert.equal(activeProcesses.size, 0)
  assert.equal(limiter.activeCount(), 0)
  assert.equal(limiter.queuedCount(), 0)
  assert.equal(maxActive, 2)
})

test("closing one session cancels only its timer and process without leaks", async () => {
  const timeouts = new Set()
  const intervals = new Set()
  const timer = (collection, fn) => {
    const handle = { fn, unref() {} }
    collection.add(handle)
    return handle
  }
  const timers = {
    setTimeoutFn: (fn) => timer(timeouts, fn),
    clearTimeoutFn: (handle) => timeouts.delete(handle),
    setIntervalFn: (fn) => timer(intervals, fn),
    clearIntervalFn: (handle) => intervals.delete(handle),
  }
  const active = new Map()
  const killed = []
  let spawned = 0
  const spawnImpl = (_binary, args) => {
    const session = args[args.indexOf("--session") + 1]
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    child.kill = () => {
      if (active.delete(session)) killed.push(session)
      return true
    }
    active.set(session, child)
    spawned += 1
    return child
  }
  const pool = createKeyedControllerPool({
    create() {
      const controller = createStatuslineController({
        intervalMs: 15_000,
        timers,
        load: (context, signal) =>
          runStatusline({
            ...context,
            bin: "/rtrt",
            env: {},
            signal,
            spawnImpl,
            timeoutMs: 60_000,
          }),
      })
      return { controller, dispose: controller.dispose }
    },
  })
  const alpha = pool.acquire("alpha")
  const beta = pool.acquire("beta")
  alpha.value.controller.setContext({ cwd: "/repo/alpha", session: "alpha", width: 40 })
  beta.value.controller.setContext({ cwd: "/repo/beta", session: "beta", width: 40 })
  alpha.value.controller.start()
  beta.value.controller.start()

  assert.equal(active.size, 2)
  assert.equal(intervals.size, 2)
  alpha.release()
  assert.deepEqual(killed, ["alpha"])
  assert.equal(active.has("beta"), true)
  assert.equal(intervals.size, 1)
  assert.equal(pool.get("beta"), beta.value)

  beta.release()
  await Promise.resolve()
  await Promise.resolve()
  assert.deepEqual(killed, ["alpha", "beta"])
  assert.equal(active.size, 0)
  assert.equal(intervals.size, 0)
  assert.equal(timeouts.size, 0)
  assert.equal(spawned, 2)
  assert.equal(pool.size(), 0)
})

test("keeps last-good output dim and stale, then falls back to explicit degraded output", async () => {
  const good = payload([
    { id: "project", text: "repo", compact: "repo", tone: "accent", priority: 100 },
    { id: "style", text: "opt:full", compact: "full", tone: "accent", priority: 90 },
    { id: "savings", text: "save:42%", compact: "42%", tone: "good", priority: 80 },
  ])
  const results = [good, undefined]
  const controller = createStatuslineController({
    intervalMs: 0,
    load: async () => results.shift(),
  })

  await controller.refreshNow()
  assert.equal(
    renderStatuslineText(controller.getState(), { width: 80 }),
    "[RTRT] | repo | STATE UNKNOWN | OPT full | SAVE 42% | COST N/A | CTX N/A | LIVE",
  )
  await controller.refreshNow()
  assert.equal(
    renderStatuslineText(controller.getState(), { width: 80 }),
    "[RTRT] | repo | STATE UNKNOWN | OPT full | SAVE 42% | COST N/A | CTX N/A | STALE",
  )
  assert.equal(controller.getState().stale, true)
  assert.ok(renderSegments(controller.getState()).every((segment) => segment.tone === "muted"))
  assert.deepEqual(
    controller.getState().payload.segments.map((segment) => segment.tone),
    ["accent", "accent", "good"],
  )

  const unavailable = createStatuslineController({ intervalMs: 0, load: async () => undefined })
  await unavailable.refreshNow()
  assert.equal(
    renderStatuslineText(unavailable.getState(), { width: 80 }),
    "[RTRT] | N/A | MODEL N/A | STATE UNKNOWN | COST N/A | CTX N/A | DEGRADED",
  )
  assert.deepEqual(
    unavailable.getState().payload.segments.map((segment) => segment.tone),
    ["muted"],
  )
  controller.dispose()
  unavailable.dispose()
})
