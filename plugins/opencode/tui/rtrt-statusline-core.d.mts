export type StatuslineTone =
  | "accent"
  | "good"
  | "warn"
  | "bad"
  | "muted"

export type StatuslineSegment = {
  id: string
  text: string
  compact: string | null
  tone: StatuslineTone
  priority: number
}

export type StatuslinePayload = {
  version: 1
  stale: boolean
  degraded: string[]
  segments: StatuslineSegment[]
}
export type StatuslineViewRole = "badge" | "label" | "value" | "separator"
export type StatuslineViewPart = {
  id: string
  text: string
  role: StatuslineViewRole
  tone: StatuslineTone
}
export type StatuslineViewRow = { id: string; parts: StatuslineViewPart[] }
export type StatuslineViewModel = {
  layout: "wide" | "medium" | "narrow"
  width: number
  rows: StatuslineViewRow[]
}
export type StatuslineEconomicsValue = {
  text: string
  tone: StatuslineTone
  used?: number
  limit?: number
  percent?: number
  headroom?: number
}
export type StatuslineEconomics = {
  cost: StatuslineEconomicsValue
  context: StatuslineEconomicsValue
  model: StatuslineEconomicsValue
  status: StatuslineEconomicsValue
  week: StatuslineEconomicsValue
  modelRef?: string
}
export type StatuslineViewOptions = {
  width?: number
  economics?: StatuslineEconomics
  promptRight?: boolean
}
export type StatuslineState = {
  payload: StatuslinePayload
  lastGood: StatuslinePayload | undefined
  stale: boolean
  unavailable: boolean
}
export type StatuslineContext = { cwd: string; session: string; width: number; model?: string }

export const STATUSLINE_VERSION: 1
export const STATUSLINE_SEPARATOR: string
export const STATUSLINE_FALLBACK: string
export const STATUSLINE_GROUP_SEPARATOR: string
export const STATUSLINE_WIDE_WIDTH: number
export const STATUSLINE_MEDIUM_WIDTH: number
export const DEFAULT_DEBOUNCE_MS: number
export const DEFAULT_INTERVAL_MS: number
export const DEFAULT_TIMEOUT_MS: number

export function parseStatuslineJson(raw: string): StatuslinePayload | undefined
export function statuslineCommandArgs(context: StatuslineContext): string[]
export function statuslineBinaryCandidates(bin?: unknown, env?: Record<string, string | undefined>): string[]
export function runStatusline(input: StatuslineContext & {
  bin?: unknown
  env?: Record<string, string | undefined>
  spawnImpl?: (...args: any[]) => any
  timeoutMs?: number
  signal?: AbortSignal
  nowFn?: () => number
  setTimeoutFn?: typeof setTimeout
  clearTimeoutFn?: typeof clearTimeout
}): Promise<StatuslinePayload | undefined>
export function createSpawnLimiter(input?: { concurrency?: number }): {
  run<Value>(
    task: (signal: AbortSignal) => Value | Promise<Value>,
    options?: { signal?: AbortSignal; priority?: number },
  ): Promise<Value | undefined>
  activeCount(): number
  queuedCount(): number
  dispose(): void
}
export function cellWidth(text: string): number
export function deriveSessionEconomics(input?: {
  session?: {
    cost?: number
    model?: { id: string; providerID: string }
  }
  messages?: ReadonlyArray<{
    role: string
    providerID?: string
    modelID?: string
    time?: { completed?: number }
    error?: unknown
    tokens?: {
      input?: number
      output?: number
      reasoning?: number
      cache?: { read?: number; write?: number }
    }
  }>
  providers?: ReadonlyArray<{
    id: string
    models: Readonly<Record<string, {
      id: string
      providerID?: string
      name?: string
      limit?: { context?: number }
    }>>
  }>
  status?: { type?: string }
}): StatuslineEconomics
export function selectSegments(
  payload: StatuslinePayload,
  options?: { width?: number; compact?: boolean },
): StatuslineSegment[]
export function initialStatuslineState(): StatuslineState
export function nextStatuslineState(previous: StatuslineState, payload?: StatuslinePayload): StatuslineState
export function renderSegments(
  state: StatuslineState,
  options?: { width?: number; compact?: boolean },
): StatuslineSegment[]
export function renderStatuslineText(
  state: StatuslineState,
  options?: StatuslineViewOptions & { compact?: boolean },
): string
export function createStatuslineViewModel(
  state: StatuslineState,
  options?: StatuslineViewOptions,
): StatuslineViewModel
export function renderStatuslineViewText(
  state: StatuslineState,
  options?: StatuslineViewOptions,
): string

export function createRefreshScheduler(input: {
  run: () => void | Promise<void>
  debounceMs?: number
  intervalMs?: number
  setTimeoutFn?: typeof setTimeout
  clearTimeoutFn?: typeof clearTimeout
  setIntervalFn?: typeof setInterval
  clearIntervalFn?: typeof clearInterval
}): {
  start(): void
  request(immediate?: boolean): void
  flush(): Promise<void>
  dispose(): void
}

export function createStatuslineController(input: {
  load: (
    context: StatuslineContext,
    signal: AbortSignal,
  ) => StatuslinePayload | undefined | Promise<StatuslinePayload | undefined>
  onState?: (state: StatuslineState) => void
  debounceMs?: number
  intervalMs?: number
  timers?: Record<string, unknown>
}): {
  start(): void
  request(immediate?: boolean): void
  refreshNow(): Promise<void>
  dispose(): void
  setContext(context: Partial<StatuslineContext>): void
  getState(): StatuslineState
  getContext(): StatuslineContext
}

export function createKeyedControllerPool<Key, Value extends { dispose(): void }>(input: {
  create: (key: Key) => Value
}): {
  acquire(key: Key): { value: Value; release(): void }
  get(key: Key): Value | undefined
  forEach(callback: (value: Value, key: Key) => void): void
  size(): number
  dispose(): void
}
