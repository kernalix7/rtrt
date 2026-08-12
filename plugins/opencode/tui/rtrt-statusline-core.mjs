import { spawn as nodeSpawn } from "node:child_process"
import { performance } from "node:perf_hooks"

export const STATUSLINE_VERSION = 1
export const STATUSLINE_SEPARATOR = " · "
export const STATUSLINE_FALLBACK = "rtrt · n/a"
export const STATUSLINE_GROUP_SEPARATOR = " | "
export const STATUSLINE_WIDE_WIDTH = 100
export const STATUSLINE_MEDIUM_WIDTH = 70
export const DEFAULT_DEBOUNCE_MS = 750
export const DEFAULT_INTERVAL_MS = 15_000
export const DEFAULT_TIMEOUT_MS = 1_500

const MAX_OUTPUT_BYTES = 64 * 1024
const MAX_SEGMENTS = 32
const MAX_SEGMENT_LENGTH = 256
const CONTROL_CHAR = /[\u0000-\u001f\u007f-\u009f]/u
const CLI_TONES = new Set(["accent", "good", "warn", "bad", "muted"])
const monotonicNow = () => performance.now()

const fallbackPayload = Object.freeze({
  version: STATUSLINE_VERSION,
  stale: true,
  degraded: Object.freeze(["statusline"]),
  segments: Object.freeze([
    Object.freeze({
      id: "fallback",
      text: STATUSLINE_FALLBACK,
      compact: STATUSLINE_FALLBACK,
      tone: "muted",
      priority: 0,
    }),
  ]),
})

const isRecord = (value) => !!value && typeof value === "object" && !Array.isArray(value)

const validText = (value, allowEmpty = false) => {
  if (typeof value !== "string" || value.length > MAX_SEGMENT_LENGTH || CONTROL_CHAR.test(value)) return undefined
  const text = value.trim()
  if (!allowEmpty && !text) return undefined
  return text
}

const hasControlChars = (value) => {
  const pending = [value]
  const seen = new Set()
  while (pending.length) {
    const item = pending.pop()
    if (typeof item === "string") {
      if (CONTROL_CHAR.test(item)) return true
      continue
    }
    if (!item || typeof item !== "object" || seen.has(item)) continue
    seen.add(item)
    if (Array.isArray(item)) {
      pending.push(...item)
      continue
    }
    for (const [key, child] of Object.entries(item)) {
      if (CONTROL_CHAR.test(key)) return true
      pending.push(child)
    }
  }
  return false
}

const normalizeSegment = (segment) => {
  if (!isRecord(segment)) return undefined

  const id = validText(segment.id)
  const text = validText(segment.text)
  const sourceTone = validText(segment.tone)
  const tone = sourceTone && CLI_TONES.has(sourceTone) ? sourceTone : "muted"
  const priority = segment.pri
  if (!id || !text || !sourceTone || !Number.isSafeInteger(priority) || priority < 0 || priority > 255) {
    return undefined
  }
  return { id, text, compact: text, tone, priority }
}

export function parseStatuslineJson(raw) {
  if (typeof raw !== "string" || !raw.trim() || Buffer.byteLength(raw) > MAX_OUTPUT_BYTES) return undefined
  try {
    const value = JSON.parse(raw)
    if (!isRecord(value) || value.v !== STATUSLINE_VERSION || hasControlChars(value)) return undefined
    if (value.stale !== undefined && typeof value.stale !== "boolean") return undefined
    if (
      value.degraded !== undefined &&
      (!Array.isArray(value.degraded) ||
        value.degraded.length > MAX_SEGMENTS ||
        value.degraded.some((item) => !validText(item)))
    ) {
      return undefined
    }
    if (!Array.isArray(value.segments) || value.segments.length < 1 || value.segments.length > MAX_SEGMENTS) {
      return undefined
    }
    const segments = value.segments.map(normalizeSegment)
    if (segments.some((segment) => !segment)) return undefined
    return {
      version: STATUSLINE_VERSION,
      stale: value.stale === true,
      degraded: value.degraded?.map((item) => item.trim()) ?? [],
      segments,
    }
  } catch {
    return undefined
  }
}

const normalizedWidth = (width) => {
  if (!Number.isFinite(width)) return 80
  return Math.max(1, Math.min(4_096, Math.floor(width)))
}

export function statuslineCommandArgs({ cwd, session, width, model }) {
  const args = [
    "statusline",
    "--opencode",
    "--cwd",
    String(cwd ?? ""),
    "--session",
    String(session ?? ""),
    "--width",
    String(normalizedWidth(width)),
  ]
  const selectedModel = validText(model)
  if (selectedModel) args.push("--model", selectedModel)
  return args
}

export function statuslineBinaryCandidates(bin, env = process.env) {
  const values = [bin, env?.RTRT_BIN, "rtrt"]
  const seen = new Set()
  return values.flatMap((value) => {
    const candidate = validText(value)
    if (!candidate || seen.has(candidate)) return []
    seen.add(candidate)
    return [candidate]
  })
}

const runCandidate = ({
  binary,
  args,
  cwd,
  spawnImpl,
  timeoutMs,
  signal,
  setTimeoutFn,
  clearTimeoutFn,
}) =>
  new Promise((resolve) => {
    let child
    let settled = false
    let bytes = 0
    const chunks = []
    let timeout
    let abort

    const finish = (payload) => {
      if (settled) return
      settled = true
      clearTimeoutFn(timeout)
      if (abort) signal?.removeEventListener("abort", abort)
      resolve(payload)
    }

    if (signal?.aborted) {
      finish(undefined)
      return
    }

    try {
      child = spawnImpl(binary, args, {
        cwd: cwd || undefined,
        shell: false,
        stdio: ["ignore", "pipe", "ignore"],
        windowsHide: true,
      })
    } catch {
      finish(undefined)
      return
    }

    if (!child?.stdout || typeof child.once !== "function") {
      try {
        child?.kill?.()
      } catch {}
      finish(undefined)
      return
    }

    child.stdout.on("data", (chunk) => {
      const data = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
      bytes += data.length
      if (bytes > MAX_OUTPUT_BYTES) {
        try {
          child.kill("SIGKILL")
        } catch {}
        finish(undefined)
        return
      }
      chunks.push(data)
    })
    child.once("error", () => finish(undefined))
    child.once("close", (code) => {
      if (code !== 0) {
        finish(undefined)
        return
      }
      finish(parseStatuslineJson(Buffer.concat(chunks).toString("utf8")))
    })

    abort = () => {
      try {
        child.kill("SIGKILL")
      } catch {}
      finish(undefined)
    }
    signal?.addEventListener("abort", abort, { once: true })

    timeout = setTimeoutFn(() => {
      try {
        child.kill("SIGKILL")
      } catch {}
      finish(undefined)
    }, Math.max(1, timeoutMs))
    timeout.unref?.()
  })

export async function runStatusline({
  bin,
  env = process.env,
  cwd,
  session,
  width,
  model,
  spawnImpl = nodeSpawn,
  timeoutMs = DEFAULT_TIMEOUT_MS,
  signal,
  nowFn = monotonicNow,
  setTimeoutFn = setTimeout,
  clearTimeoutFn = clearTimeout,
}) {
  const args = statuslineCommandArgs({ cwd, session, width, model })
  const duration = Number.isFinite(timeoutMs) ? Math.max(1, timeoutMs) : DEFAULT_TIMEOUT_MS
  const deadline = nowFn() + duration
  for (const binary of statuslineBinaryCandidates(bin, env)) {
    if (signal?.aborted) return undefined
    const remaining = deadline - nowFn()
    if (remaining <= 0) return undefined
    try {
      const payload = await runCandidate({
        binary,
        args,
        cwd,
        spawnImpl,
        timeoutMs: Math.ceil(remaining),
        signal,
        setTimeoutFn,
        clearTimeoutFn,
      })
      if (payload) return payload
    } catch {}
  }
  return undefined
}

export function createSpawnLimiter({ concurrency = 2 } = {}) {
  const limit = Number.isSafeInteger(concurrency) ? Math.max(1, Math.min(16, concurrency)) : 2
  const queue = []
  const running = new Set()
  let active = 0
  let disposed = false

  const cleanup = (entry) => entry.signal?.removeEventListener("abort", entry.abort)

  const finish = (entry, value) => {
    if (entry.finished) return
    entry.finished = true
    cleanup(entry)
    if (entry.started) {
      active -= 1
      running.delete(entry)
    }
    entry.resolve(value)
    pump()
  }

  const start = (entry) => {
    entry.started = true
    active += 1
    running.add(entry)
    void (async () => {
      let value
      try {
        value = await entry.task(entry.controller.signal)
      } catch {}
      finish(entry, value)
    })()
  }

  const pump = () => {
    while (!disposed && active < limit && queue.length > 0) {
      let selected = 0
      for (let index = 1; index < queue.length; index += 1) {
        if (queue[index].priority > queue[selected].priority) selected = index
      }
      const [entry] = queue.splice(selected, 1)
      if (!entry.finished) start(entry)
    }
  }

  return {
    run(task, { signal, priority = 0 } = {}) {
      if (disposed || signal?.aborted) return Promise.resolve(undefined)
      return new Promise((resolve) => {
        const entry = {
          task,
          signal,
          priority: Number.isFinite(priority) ? priority : 0,
          controller: new AbortController(),
          resolve,
          started: false,
          finished: false,
          abort: undefined,
        }
        entry.abort = () => {
          entry.controller.abort()
          if (entry.started) return
          const index = queue.indexOf(entry)
          if (index >= 0) queue.splice(index, 1)
          finish(entry, undefined)
        }
        signal?.addEventListener("abort", entry.abort, { once: true })
        queue.push(entry)
        pump()
      })
    },
    activeCount: () => active,
    queuedCount: () => queue.length,
    dispose() {
      if (disposed) return
      disposed = true
      for (const entry of queue.splice(0)) {
        entry.controller.abort()
        finish(entry, undefined)
      }
      for (const entry of running) entry.controller.abort()
    },
  }
}

const isWideCodePoint = (code) =>
  code >= 0x1100 &&
  (code <= 0x115f ||
    code === 0x2329 ||
    code === 0x232a ||
    (code >= 0x2e80 && code <= 0xa4cf && code !== 0x303f) ||
    (code >= 0xac00 && code <= 0xd7a3) ||
    (code >= 0xf900 && code <= 0xfaff) ||
    (code >= 0xfe10 && code <= 0xfe19) ||
    (code >= 0xfe30 && code <= 0xfe6f) ||
    (code >= 0xff00 && code <= 0xff60) ||
    (code >= 0xffe0 && code <= 0xffe6) ||
    (code >= 0x1f300 && code <= 0x1faff) ||
    (code >= 0x20000 && code <= 0x3fffd))

export function cellWidth(text) {
  let width = 0
  for (const character of text) {
    if (/\p{Mark}/u.test(character) || character === "\u200d" || character === "\ufe0f") continue
    width += isWideCodePoint(character.codePointAt(0)) ? 2 : 1
  }
  return width
}

const truncateToWidth = (text, width) => {
  if (cellWidth(text) <= width) return text
  if (width <= 1) return "…".slice(0, width)
  let output = ""
  let used = 0
  for (const character of text) {
    const size = cellWidth(character)
    if (used + size > width - 1) break
    output += character
    used += size
  }
  return `${output}…`
}

const findCatalogModel = (providers, providerID, modelID) => {
  if (!Array.isArray(providers) || typeof providerID !== "string" || typeof modelID !== "string") {
    return undefined
  }
  const provider = providers.find((item) => item?.id === providerID)
  if (!isRecord(provider?.models)) return undefined
  const direct = provider.models[modelID]
  if (isRecord(direct)) return direct
  return Object.values(provider.models).find(
    (model) => isRecord(model) && model.id === modelID && (!model.providerID || model.providerID === providerID),
  )
}

const tokenCount = (value) => (Number.isFinite(value) && value > 0 ? value : 0)

const contextTone = (percent) => {
  if (percent > 100) return "warn"
  if (percent >= 90) return "bad"
  if (percent >= 75) return "warn"
  return "good"
}

export function deriveSessionEconomics({ session, messages = [], providers = [], status } = {}) {
  let cost = { text: "N/A", tone: "muted" }
  if (session?.cost === 0) {
    cost = { text: "$0?", tone: "muted" }
  } else if (Number.isFinite(session?.cost) && session.cost > 0) {
    cost = { text: `~$${session.cost.toFixed(2)}`, tone: "accent" }
  }

  const selected = session?.model
  const selectedModel = findCatalogModel(providers, selected?.providerID, selected?.id)
  const selectedText = validText(selectedModel?.name) ?? validText(selected?.id)
  const model = selectedText
    ? { text: truncateToWidth(selectedText.split("/").at(-1), 22), tone: "accent" }
    : { text: "N/A", tone: "muted" }
  const selectedProviderID = validText(selected?.providerID)
  const selectedModelID = validText(selected?.id)
  const modelRef =
    selectedProviderID && selectedModelID ? validText(`${selectedProviderID}/${selectedModelID}`) : undefined

  let context = { text: "N/A", tone: "muted" }
  if (Array.isArray(messages)) {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
      const message = messages[index]
      if (message?.role !== "assistant") continue
      const tokens = message.tokens
      if (message.error || !Number.isFinite(message.time?.completed) || tokenCount(tokens?.output) <= 0) {
        continue
      }
      const used =
        tokenCount(tokens?.input) +
        tokenCount(tokens?.output) +
        tokenCount(tokens?.reasoning) +
        tokenCount(tokens?.cache?.read) +
        tokenCount(tokens?.cache?.write)
      if (used <= 0) continue
      const exactModel = findCatalogModel(providers, message.providerID, message.modelID)
      const limit = exactModel?.limit?.context
      if (Number.isFinite(limit) && limit > 0) {
        const percent = Math.max(0, Math.round((used / limit) * 100))
        const headroom = Math.max(0, 100 - percent)
        context = { text: `${percent}%`, tone: contextTone(percent), used, limit, percent, headroom }
      } else {
        context = { text: "N/A", tone: "muted", used }
      }
      break
    }
  }

  // OpenCode omits idle sessions from the status map.
  const sessionStatus = session
    ? ({
        busy: { text: "BUSY", tone: "accent" },
        retry: { text: "RETRY", tone: "bad" },
        idle: { text: "IDLE", tone: "muted" },
      }[status?.type] ?? { text: "IDLE", tone: "muted" })
    : { text: "UNKNOWN", tone: "muted" }

  return {
    cost,
    context,
    model,
    status: sessionStatus,
    week: { text: "N/A/not exposed", tone: "muted" },
    modelRef,
  }
}

const renderedWidth = (segments) =>
  segments.reduce((sum, segment) => sum + cellWidth(segment.text), 0) +
  Math.max(0, segments.length - 1) * cellWidth(STATUSLINE_SEPARATOR)

export function selectSegments(payload, { width = 80, compact = false } = {}) {
  const limit = normalizedWidth(width)
  let selected = payload.segments.flatMap((segment, index) => {
    const text = compact ? segment.compact : segment.text
    return text ? [{ ...segment, text, index }] : []
  })
  while (selected.length > 1 && renderedWidth(selected) > limit) {
    let remove = 0
    for (let index = 1; index < selected.length; index += 1) {
      if (
        selected[index].priority < selected[remove].priority ||
        (selected[index].priority === selected[remove].priority && selected[index].index > selected[remove].index)
      ) {
        remove = index
      }
    }
    selected.splice(remove, 1)
  }
  if (selected.length === 1) selected[0] = { ...selected[0], text: truncateToWidth(selected[0].text, limit) }
  return selected.map(({ index: _index, ...segment }) => segment)
}

export function initialStatuslineState() {
  return { payload: fallbackPayload, lastGood: undefined, stale: true, unavailable: true }
}

export function nextStatuslineState(previous, payload) {
  if (payload) return { payload, lastGood: payload, stale: payload.stale === true, unavailable: false }
  if (previous.lastGood) {
    return { payload: previous.lastGood, lastGood: previous.lastGood, stale: true, unavailable: false }
  }
  return initialStatuslineState()
}

export function renderSegments(state, options = {}) {
  const payload =
    state.stale && !state.unavailable
      ? {
          version: STATUSLINE_VERSION,
          stale: true,
          segments: [
            ...state.payload.segments,
            { id: "stale", text: "stale", compact: "stale", tone: "muted", priority: 1_000 },
          ],
        }
      : state.payload
  const segments = selectSegments(payload, options)
  return state.stale ? segments.map((segment) => ({ ...segment, tone: "muted" })) : segments
}

const viewPart = (text, role, tone, id) => ({ text, role, tone, id })

const valueGroup = (id, text, tone = "muted", role = "value") => ({
  id,
  parts: [viewPart(text, role, tone, id)],
})

const metricGroup = (id, label, text, tone) => ({
  id,
  parts: [
    viewPart(`${label} `, "label", "muted", id),
    viewPart(text, "value", tone, id),
  ],
})

const groupText = (group) => group.parts.map((part) => part.text).join("")
const groupsWidth = (groups, separator) =>
  groups.reduce((sum, group) => sum + cellWidth(groupText(group)), 0) +
  Math.max(0, groups.length - 1) * cellWidth(separator)

const rowFromGroups = (id, groups, separator) => {
  const parts = []
  for (const [index, group] of groups.entries()) {
    if (index > 0) parts.push(viewPart(separator, "separator", "muted", "separator"))
    parts.push(...group.parts)
  }
  return { id, parts }
}

const sourceTone = (state, segment) => (state.stale ? "muted" : segment?.tone ?? "muted")

const segmentValue = (segments, id, prefix = "") => {
  const segment = segments.get(id)
  if (!segment) return undefined
  const text = prefix && segment.text.startsWith(prefix) ? segment.text.slice(prefix.length) : segment.text
  return text ? { text, segment } : undefined
}

const savingsValues = (segments) => {
  const savings = segments.get("savings")
  if (savings) {
    const save = /(?:^|\s)save:([^\s]+)/u.exec(savings.text)?.[1]
    const command = /(?:^|\s)cmd:([^\s]+)/u.exec(savings.text)?.[1]
    return {
      save: save ? { text: save, segment: savings } : undefined,
      command: command ? { text: command, segment: savings } : undefined,
    }
  }
  const sigma = segments.get("sigma")
  if (!sigma) return {}
  const text = sigma.text.replace(/^(?:Σ|sigma):?/iu, "")
  return text ? { save: { text, segment: sigma } } : {}
}

const headroomValues = (segments) => {
  const value = segmentValue(segments, "headroom", "room:")
  if (!value) return undefined
  const boundary = value.text.lastIndexOf(":")
  if (boundary < 0) return { full: value.text, compact: value.text, segment: value.segment }
  const target = value.text.slice(0, boundary)
  const amount = value.text.slice(boundary + 1)
  return {
    full: target && amount ? `${target} ${amount}` : value.text,
    compact: amount || value.text,
    segment: value.segment,
  }
}

const reportedPercentToken = (text) => {
  const match = /\d+(?:\.\d+)?%/u.exec(text)
  if (!match) return undefined
  const before = match.index > 0 ? text[match.index - 1] : ""
  const after = text[match.index + match[0].length] ?? ""
  if (
    (before && /[\p{Letter}\p{Number}_.+-]/u.test(before)) ||
    (after && /[\p{Letter}\p{Number}_.%]/u.test(after))
  ) {
    return undefined
  }
  const value = Number(match[0].slice(0, -1))
  if (!Number.isFinite(value) || value < 0 || value > 100) return undefined
  return { text: match[0], index: match.index }
}

const officialLimitValue = (segments, id) => {
  const segment = segments.get(id)
  if (!segment) return undefined
  const percentToken = reportedPercentToken(segment.text)
  if (!percentToken) return undefined
  const percent = percentToken.text
  let reset = segment.text.slice(percentToken.index + percent.length).trim()
  reset = reset
    .replace(/^[|,;/-]\s*/u, "")
    .replace(/^(?:reset(?:s|_at)?|r)\s*[:=]?\s*/iu, "")
    .replace(/^(?:in\s+|↻\s*|@\s*)/iu, "")
    .replace(/\s+/gu, "")
  return {
    full: reset ? `${percent}/${reset}` : percent,
    percent,
    segment,
  }
}

const statusGroups = (state, missing) => {
  const groups = []
  if (state.stale && !state.unavailable) groups.push(valueGroup("stale", "STALE", "warn"))
  if (state.unavailable || missing || (state.payload.degraded?.length ?? 0) > 0) {
    groups.push(valueGroup("degraded", "DEGRADED", "bad"))
  }
  if (groups.length === 0) groups.push(valueGroup("live", "LIVE", "good"))
  return groups
}

const replaceGroupText = (groups, id, text) =>
  groups.map((group) => {
    if (group.id !== id) return group
    const last = group.parts.length - 1
    return {
      ...group,
      parts: group.parts.map((part, index) => (index === last ? { ...part, text } : part)),
    }
  })

const removeGroup = (groups, id) => groups.filter((group) => group.id !== id)

const capGroupValues = (source, limits) => {
  let groups = source
  for (const [id, limit] of limits) {
    const group = groups.find((item) => item.id === id)
    if (!group) continue
    const value = group.parts.at(-1)?.text ?? ""
    groups = replaceGroupText(groups, id, truncateToWidth(value, limit))
  }
  return groups
}

const fitWideIdentityGroups = (source, width) => {
  let groups = capGroupValues(source, [
    ["project", 36],
    ["git", 24],
    ["model", 22],
  ])
  if (groupsWidth(groups, "  ") > width) groups = removeGroup(groups, "git")
  const project = groups.find((group) => group.id === "project")
  if (project && groupsWidth(groups, "  ") > width) {
    const overflow = groupsWidth(groups, "  ") - width
    const current = groupText(project)
    groups = replaceGroupText(groups, "project", truncateToWidth(current, Math.max(4, cellWidth(current) - overflow)))
  }
  if (groupsWidth(groups, "  ") > width) groups = removeGroup(groups, "project")
  return groups
}

const fitWideMetricGroups = (source, width) => {
  let groups = capGroupValues(source, [
    ["style", 12],
    ["savings", 8],
    ["command", 8],
    ["memory", 18],
    ["headroom", 18],
    ["cost", 12],
    ["context", 5],
    ["week", 15],
    ["limit-5h", 12],
    ["limit-week", 12],
  ])
  for (const id of ["headroom", "memory", "command", "savings", "style"]) {
    if (groupsWidth(groups, STATUSLINE_GROUP_SEPARATOR) <= width) break
    groups = removeGroup(groups, id)
  }
  return groups
}

const fitMediumGroups = (source, width) => {
  let groups = capGroupValues(source, [
    ["project", 18],
    ["model", 16],
    ["style", 12],
    ["savings", 8],
    ["command", 8],
    ["headroom", 10],
    ["cost", 12],
    ["context", 5],
    ["week", 15],
    ["limit-5h", 12],
    ["limit-week", 12],
  ])
  for (const id of ["week", "headroom", "command"]) {
    if (groupsWidth(groups, STATUSLINE_GROUP_SEPARATOR) <= width) break
    groups = removeGroup(groups, id)
  }
  const project = groups.find((group) => group.id === "project")
  if (project && groupsWidth(groups, STATUSLINE_GROUP_SEPARATOR) > width) {
    const overflow = groupsWidth(groups, STATUSLINE_GROUP_SEPARATOR) - width
    const current = groupText(project)
    groups = replaceGroupText(groups, "project", truncateToWidth(current, Math.max(4, cellWidth(current) - overflow)))
  }
  for (const id of ["model", "project", "style", "savings", "badge"]) {
    if (groupsWidth(groups, STATUSLINE_GROUP_SEPARATOR) <= width) break
    groups = removeGroup(groups, id)
  }
  if (groupsWidth(groups, STATUSLINE_GROUP_SEPARATOR) <= width) {
    return { groups, separator: STATUSLINE_GROUP_SEPARATOR }
  }

  const fallback = []
  for (const id of ["savings", "cost", "context", "session-status"]) {
    const group = groups.find((item) => item.id === id)
    const value = group?.parts.at(-1)
    if (value) fallback.push(valueGroup(id, truncateToWidth(value.text, id === "cost" ? 12 : 8), value.tone))
  }
  for (const [id, label] of [
    ["limit-5h", "5H"],
    ["limit-week", "WEEK"],
  ]) {
    const group = groups.find((item) => item.id === id)
    const value = group?.parts.at(-1)
    if (!value) continue
    fallback.push(metricGroup(id, label, truncateToWidth(value.text, 12), value.tone))
  }
  fallback.push(...groups.filter((group) => ["stale", "degraded", "live"].includes(group.id)))
  return {
    groups: fitNarrowGroups(fallback, width, ["context", "cost", "savings", "session-status", "limit-5h", "limit-week"]),
    separator: " ",
  }
}

const fitNarrowGroups = (source, width, removalOrder = ["cost", "context", "style", "savings"]) => {
  let groups = source
  for (const id of removalOrder) {
    if (groupsWidth(groups, " ") <= width) break
    groups = removeGroup(groups, id)
  }
  if (groups.length === 0) groups = [valueGroup("missing", "N/A", "bad")]
  if (groupsWidth(groups, " ") <= width) return groups
  const text = groups.map(groupText).join(" ")
  return [valueGroup("status", truncateToWidth(text, width), "bad")]
}

export function createStatuslineViewModel(state, { width = 80, economics, promptRight = false } = {}) {
  const limit = normalizedWidth(width)
  const layout = limit >= STATUSLINE_WIDE_WIDTH ? "wide" : limit >= STATUSLINE_MEDIUM_WIDTH ? "medium" : "narrow"
  const sessionEconomics = economics ?? deriveSessionEconomics()
  const segments = new Map(state.payload.segments.map((segment) => [segment.id, segment]))
  const project = segmentValue(segments, "project")
  const git = segmentValue(segments, "git")
  const style = segmentValue(segments, "style", "opt:")
  const savings = savingsValues(segments)
  const memory = segmentValue(segments, "memory", "mem:")
  const headroom = headroomValues(segments)
  const limitsFresh = !state.stale && !state.unavailable
  const limit5h = limitsFresh ? officialLimitValue(segments, "limit_5h") : undefined
  const limitWeek = limitsFresh ? officialLimitValue(segments, "limit_week") : undefined
  const missing = state.unavailable || (!style && !savings.save)
  const statuses = statusGroups(state, missing)

  if (layout === "wide") {
    const identity = [valueGroup("badge", "[RTRT]", "accent", "badge")]
    if (state.unavailable) {
      identity.push(valueGroup("missing", "N/A", "bad"))
    } else {
      if (project) identity.push(valueGroup("project", truncateToWidth(project.text, 36), sourceTone(state, project.segment)))
      if (git) identity.push(valueGroup("git", truncateToWidth(git.text, 24), sourceTone(state, git.segment)))
    }
    identity.push(
      metricGroup("model", "MODEL", sessionEconomics.model.text, sessionEconomics.model.tone),
      metricGroup("session-status", "STATE", sessionEconomics.status.text, sessionEconomics.status.tone),
      ...statuses,
    )

    const metrics = []
    if (state.unavailable) {
      for (const label of ["OPT", "SAVE", "CMD", "MEM", "ROOM"]) {
        metrics.push(metricGroup(label.toLowerCase(), label, "N/A", "bad"))
      }
    } else {
      if (style) metrics.push(metricGroup("style", "OPT", style.text, sourceTone(state, style.segment)))
      if (savings.save) metrics.push(metricGroup("savings", "SAVE", savings.save.text, sourceTone(state, savings.save.segment)))
      if (savings.command) metrics.push(metricGroup("command", "CMD", savings.command.text, sourceTone(state, savings.command.segment)))
      if (memory) metrics.push(metricGroup("memory", "MEM", truncateToWidth(memory.text, 18), sourceTone(state, memory.segment)))
      if (headroom) metrics.push(metricGroup("headroom", "ROOM", truncateToWidth(headroom.full, 28), sourceTone(state, headroom.segment)))
      if (metrics.length === 0) metrics.push(metricGroup("metrics", "METRICS", "N/A", "bad"))
    }
    metrics.push(
      metricGroup("cost", "COST", sessionEconomics.cost.text, sessionEconomics.cost.tone),
      metricGroup("context", "CTX", sessionEconomics.context.text, sessionEconomics.context.tone),
    )
    if (limit5h) {
      metrics.push(metricGroup("limit-5h", "5H", limit5h.full, sourceTone(state, limit5h.segment)))
    }
    metrics.push(
      limitWeek
        ? metricGroup("limit-week", "WEEK", limitWeek.full, sourceTone(state, limitWeek.segment))
        : metricGroup("week", "WEEK", sessionEconomics.week.text, sessionEconomics.week.tone),
    )
    const fittedIdentity = fitWideIdentityGroups(identity, limit)
    const fittedMetrics = fitWideMetricGroups(metrics, limit)
    return {
      layout,
      width: limit,
      rows: [
        rowFromGroups("identity", fittedIdentity, "  "),
        rowFromGroups("metrics", fittedMetrics, STATUSLINE_GROUP_SEPARATOR),
      ],
    }
  }

  if (layout === "medium") {
    let groups = [valueGroup("badge", "[RTRT]", "accent", "badge")]
    if (state.unavailable) {
      groups.push(valueGroup("missing", "N/A", "bad"))
    } else {
      if (project) groups.push(valueGroup("project", truncateToWidth(project.text, 18), sourceTone(state, project.segment)))
      groups.push(metricGroup("model", "MODEL", sessionEconomics.model.text, sessionEconomics.model.tone))
      groups.push(metricGroup("session-status", "STATE", sessionEconomics.status.text, sessionEconomics.status.tone))
      if (style) groups.push(metricGroup("style", "OPT", style.text, sourceTone(state, style.segment)))
      if (savings.save) groups.push(metricGroup("savings", "SAVE", savings.save.text, sourceTone(state, savings.save.segment)))
      groups.push(metricGroup("cost", "COST", sessionEconomics.cost.text, sessionEconomics.cost.tone))
      groups.push(metricGroup("context", "CTX", sessionEconomics.context.text, sessionEconomics.context.tone))
      if (savings.command) groups.push(metricGroup("command", "CMD", savings.command.text, sourceTone(state, savings.command.segment)))
      if (headroom) groups.push(metricGroup("headroom", "ROOM", headroom.compact, sourceTone(state, headroom.segment)))
    }
    if (state.unavailable) {
      groups.push(
        metricGroup("model", "MODEL", sessionEconomics.model.text, sessionEconomics.model.tone),
        metricGroup("session-status", "STATE", sessionEconomics.status.text, sessionEconomics.status.tone),
        metricGroup("cost", "COST", sessionEconomics.cost.text, sessionEconomics.cost.tone),
        metricGroup("context", "CTX", sessionEconomics.context.text, sessionEconomics.context.tone),
      )
    }
    if (limit5h) {
      groups.push(metricGroup("limit-5h", "5H", limit5h.full, sourceTone(state, limit5h.segment)))
    }
    groups.push(
      limitWeek
        ? metricGroup("limit-week", "WEEK", limitWeek.full, sourceTone(state, limitWeek.segment))
        : metricGroup("week", "WEEK", sessionEconomics.week.text, sessionEconomics.week.tone),
      ...statuses,
    )
    const fitted = fitMediumGroups(groups, limit)
    return { layout, width: limit, rows: [rowFromGroups("summary", fitted.groups, fitted.separator)] }
  }

  let groups = []
  if (!state.unavailable && savings.save) {
    groups.push(valueGroup("savings", savings.save.text, sourceTone(state, savings.save.segment)))
  }
  if (!promptRight && !state.unavailable && style) {
    groups.unshift(valueGroup("style", style.text, sourceTone(state, style.segment)))
  }
  groups.push(valueGroup("cost", sessionEconomics.cost.text, sessionEconomics.cost.tone))
  if (promptRight) groups.push(valueGroup("context", sessionEconomics.context.text, sessionEconomics.context.tone))
  if (promptRight && limitWeek) {
    groups.push(valueGroup("limit-week", limitWeek.percent, sourceTone(state, limitWeek.segment)))
  }
  if (statuses[0]?.id !== "live") groups.push(...statuses)
  groups = fitNarrowGroups(
    groups,
    limit,
    promptRight ? ["limit-week", "context", "cost", "savings"] : ["cost", "style", "savings"],
  )
  return { layout, width: limit, rows: [rowFromGroups("core", groups, " ")] }
}

export function renderStatuslineViewText(state, options = {}) {
  return createStatuslineViewModel(state, options)
    .rows.map((row) => row.parts.map((part) => part.text).join(""))
    .join("\n")
}

export function renderStatuslineText(state, options = {}) {
  return renderStatuslineViewText(state, options)
}

export function createRefreshScheduler({
  run,
  debounceMs = DEFAULT_DEBOUNCE_MS,
  intervalMs = DEFAULT_INTERVAL_MS,
  setTimeoutFn = setTimeout,
  clearTimeoutFn = clearTimeout,
  setIntervalFn = setInterval,
  clearIntervalFn = clearInterval,
}) {
  let started = false
  let disposed = false
  let queued = false
  let debounce
  let interval
  let running

  const execute = () => {
    if (disposed) return Promise.resolve()
    if (running) {
      queued = true
      return running
    }
    running = (async () => {
      do {
        queued = false
        try {
          await run()
        } catch {}
      } while (queued && !disposed)
    })().finally(() => {
      running = undefined
      if (queued && !disposed) void execute()
    })
    return running
  }

  const clearDebounce = () => {
    if (debounce === undefined) return
    clearTimeoutFn(debounce)
    debounce = undefined
  }

  const request = (immediate = false) => {
    if (disposed) return
    clearDebounce()
    if (immediate) {
      void execute()
      return
    }
    debounce = setTimeoutFn(() => {
      debounce = undefined
      void execute()
    }, Math.max(0, debounceMs))
    debounce?.unref?.()
  }

  return {
    start() {
      if (started || disposed) return
      started = true
      request(true)
      if (intervalMs > 0) {
        interval = setIntervalFn(() => request(true), intervalMs)
        interval?.unref?.()
      }
    },
    request,
    flush() {
      clearDebounce()
      return execute()
    },
    dispose() {
      disposed = true
      clearDebounce()
      if (interval !== undefined) clearIntervalFn(interval)
    },
  }
}

export function createStatuslineController({
  load,
  onState = () => {},
  debounceMs = DEFAULT_DEBOUNCE_MS,
  intervalMs = DEFAULT_INTERVAL_MS,
  timers = {},
}) {
  let context = { cwd: "", session: "", width: 80, model: "" }
  let state = initialStatuslineState()
  let generation = 0
  let activeRefresh
  const lifetime = new AbortController()
  const scheduler = createRefreshScheduler({
    run: async () => {
      const snapshot = { ...context }
      const startedGeneration = generation
      const refresh = new AbortController()
      const abortRefresh = () => refresh.abort()
      activeRefresh = refresh
      lifetime.signal.addEventListener("abort", abortRefresh, { once: true })
      let payload
      try {
        payload = await load(snapshot, refresh.signal)
      } catch {}
      lifetime.signal.removeEventListener("abort", abortRefresh)
      if (activeRefresh === refresh) activeRefresh = undefined
      if (
        lifetime.signal.aborted ||
        refresh.signal.aborted ||
        startedGeneration !== generation ||
        snapshot.cwd !== context.cwd ||
        snapshot.session !== context.session ||
        snapshot.width !== context.width ||
        snapshot.model !== context.model
      ) {
        return
      }
      state = nextStatuslineState(state, payload)
      try {
        onState(state)
      } catch {}
    },
    debounceMs,
    intervalMs,
    ...timers,
  })

  return {
    start: scheduler.start,
    request: scheduler.request,
    refreshNow: scheduler.flush,
    dispose() {
      activeRefresh?.abort()
      lifetime.abort()
      scheduler.dispose()
    },
    setContext(next) {
      const updated = {
        cwd: typeof next.cwd === "string" ? next.cwd : context.cwd,
        session: typeof next.session === "string" ? next.session : context.session,
        width: next.width === undefined ? context.width : normalizedWidth(next.width),
        model: typeof next.model === "string" ? next.model : context.model,
      }
      if (
        updated.cwd === context.cwd &&
        updated.session === context.session &&
        updated.width === context.width &&
        updated.model === context.model
      ) {
        return
      }
      context = updated
      generation += 1
      activeRefresh?.abort()
      scheduler.request()
    },
    getState: () => state,
    getContext: () => ({ ...context }),
  }
}

export function createKeyedControllerPool({ create }) {
  const entries = new Map()

  const dispose = (entry) => {
    try {
      entry.value.dispose()
    } catch {}
  }

  return {
    acquire(key) {
      let entry = entries.get(key)
      if (!entry) {
        entry = { value: create(key), refs: 0 }
        entries.set(key, entry)
      }
      entry.refs += 1
      let released = false
      return {
        value: entry.value,
        release() {
          if (released) return
          released = true
          entry.refs -= 1
          if (entry.refs > 0 || entries.get(key) !== entry) return
          entries.delete(key)
          dispose(entry)
        },
      }
    },
    get(key) {
      return entries.get(key)?.value
    },
    forEach(callback) {
      for (const [key, entry] of entries) callback(entry.value, key)
    },
    size: () => entries.size,
    dispose() {
      for (const entry of entries.values()) dispose(entry)
      entries.clear()
    },
  }
}
