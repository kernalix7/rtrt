/** @jsxImportSource @opentui/solid */
import { spawn } from "node:child_process"
import { useTerminalDimensions } from "@opentui/solid"
import type { TuiPlugin, TuiPluginApi, TuiSlotContext } from "@opencode-ai/plugin/tui"
import { createEffect, createMemo, createSignal, For, onCleanup, onMount } from "solid-js"

import {
  createKeyedControllerPool,
  createSpawnLimiter,
  createStatuslineController,
  createStatuslineViewModel,
  deriveSessionEconomics,
  initialStatuslineState,
  runStatusline,
  type StatuslineState,
  type StatuslineTone,
  type StatuslineViewModel,
  type StatuslineViewPart,
} from "./rtrt-statusline-core.mjs"

const refreshEvents = [
  "server.connected",
  "project.updated",
  "file.edited",
  "file.watcher.updated",
  "session.created",
  "session.updated",
  "session.compacted",
  "session.deleted",
  "session.diff",
  "session.error",
  "session.idle",
  "session.status",
  "message.updated",
  "message.removed",
  "message.part.updated",
  "message.part.removed",
] as const

const compactWidth = (terminalWidth: number) => Math.max(12, Math.min(40, Math.floor(terminalWidth / 3)))

function themeColor(theme: TuiSlotContext["theme"]["current"], tone: StatuslineTone) {
  return {
    accent: theme.accent,
    good: theme.success,
    warn: theme.warning,
    bad: theme.error,
    muted: theme.textMuted,
  }[tone] ?? theme.textMuted
}

function partColor(theme: TuiSlotContext["theme"]["current"], part: StatuslineViewPart) {
  if (part.role === "separator") return theme.borderSubtle
  if (part.role === "label") return theme.textMuted
  return themeColor(theme, part.tone)
}

type StatuslineController = ReturnType<typeof createStatuslineController>
type StatuslineEntry = {
  state: () => StatuslineState
  controller: StatuslineController
  dispose: () => void
}
type SessionControllers = {
  acquire: (sessionID: string) => { value: StatuslineEntry; release: () => void }
}

function activeSessionID(api: TuiPluginApi) {
  const route = api.route.current
  const params = "params" in route ? route.params : undefined
  const sessionID = params?.sessionID
  return route.name === "session" && typeof sessionID === "string" ? sessionID : ""
}

function eventSessionID(event: unknown) {
  if (!event || typeof event !== "object") return undefined
  const value = event as { type?: unknown; properties?: unknown }
  if (!value.properties || typeof value.properties !== "object") return undefined
  const properties = value.properties as Record<string, unknown>
  if (typeof properties.sessionID === "string") return properties.sessionID
  for (const key of ["part", "message", "info"]) {
    const child = properties[key]
    if (!child || typeof child !== "object") continue
    const record = child as Record<string, unknown>
    if (typeof record.sessionID === "string") return record.sessionID
    if (key === "info" && typeof value.type === "string" && value.type.startsWith("session.")) {
      if (typeof record.id === "string") return record.id
    }
  }
  return undefined
}

function sessionEconomics(api: TuiPluginApi, sessionID: string) {
  return deriveSessionEconomics({
    session: sessionID ? api.state.session.get(sessionID) : undefined,
    messages: sessionID ? api.state.session.messages(sessionID) : [],
    providers: api.state.provider,
    status: sessionID ? api.state.session.status(sessionID) : undefined,
  })
}

function StatusRows(props: {
  view: () => StatuslineViewModel
  theme: TuiSlotContext["theme"]
}) {
  return (
    <For each={props.view().rows}>
      {(row) => (
        <text fg={props.theme.current.textMuted} wrapMode="none" truncate>
          <For each={row.parts}>
            {(part) => (
              <span style={{ fg: partColor(props.theme.current, part) }}>{part.text}</span>
            )}
          </For>
        </text>
      )}
    </For>
  )
}

function AppLine(props: {
  api: TuiPluginApi
  context: TuiSlotContext
  entry: StatuslineEntry
}) {
  const dimensions = useTerminalDimensions()
  const width = createMemo(() => Math.max(1, dimensions().width - 4))
  const economics = createMemo(() => sessionEconomics(props.api, activeSessionID(props.api)))
  const view = createMemo(() =>
    createStatuslineViewModel(props.entry.state(), { width: width(), economics: economics() }),
  )

  createEffect(() => {
    const sessionID = activeSessionID(props.api)
    const cwd =
      (sessionID && props.api.state.session.get(sessionID)?.directory) || props.api.state.path.directory
    props.entry.controller.setContext({
      cwd,
      session: sessionID,
      width: width(),
      model: economics().modelRef ?? "",
    })
  })
  onMount(() => props.entry.controller.start())

  return (
    <box
      width="100%"
      height={view().rows.length}
      flexDirection="column"
      flexShrink={0}
      paddingLeft={2}
      paddingRight={2}
      overflow="hidden"
    >
      <StatusRows view={view} theme={props.context.theme} />
    </box>
  )
}

function SessionLine(props: {
  api: TuiPluginApi
  sessionID: string
  context: TuiSlotContext
  sessions: SessionControllers
}) {
  const dimensions = useTerminalDimensions()
  const width = createMemo(() => compactWidth(dimensions().width))
  const lease = props.sessions.acquire(props.sessionID)
  const entry = lease.value
  const economics = createMemo(() => sessionEconomics(props.api, props.sessionID))
  const view = createMemo(() =>
    createStatuslineViewModel(entry.state(), {
      width: width(),
      economics: economics(),
      promptRight: true,
    }),
  )

  createEffect(() => {
    const cwd = props.api.state.session.get(props.sessionID)?.directory ?? props.api.state.path.directory
    entry.controller.setContext({
      cwd,
      session: props.sessionID,
      width: width(),
      model: economics().modelRef ?? "",
    })
  })
  onMount(() => entry.controller.start())
  onCleanup(() => lease.release())

  return <StatusRows view={view} theme={props.context.theme} />
}

const tui: TuiPlugin = async (api, options) => {
  if (options?.enabled === false) return

  const bin = typeof options?.bin === "string" ? options.bin : undefined
  const spawnLimiter = createSpawnLimiter({ concurrency: 2 })
  const createEntry = (): StatuslineEntry => {
    const [state, setState] = createSignal(initialStatuslineState())
    const controller = createStatuslineController({
      // Session.model is current selection. Forward it only as argv to the
      // local statusline command; quota economics never depend on CLI output.
      load: (context, signal) =>
        spawnLimiter.run(
          (limitedSignal) =>
            runStatusline({ ...context, bin, spawnImpl: spawn, signal: limitedSignal }),
          {
            signal,
            priority: context.session === activeSessionID(api) ? 1 : 0,
          },
        ),
      onState: setState,
    })
    return { state, controller, dispose: controller.dispose }
  }
  const app = createEntry()
  const sessions = createKeyedControllerPool<string, StatuslineEntry>({ create: createEntry })

  const disposers = refreshEvents.map((name) =>
    api.event.on(name, (event) => {
      app.controller.request()
      const sessionID = eventSessionID(event)
      if (sessionID) {
        sessions.get(sessionID)?.controller.request()
        return
      }
      sessions.forEach((entry) => entry.controller.request())
    }),
  )
  api.lifecycle.onDispose(() => {
    app.dispose()
    sessions.dispose()
    spawnLimiter.dispose()
    for (const dispose of disposers) dispose()
  })

  api.slots.register({
    slots: {
      app_bottom(context) {
        return <AppLine api={api} context={context} entry={app} />
      },
      session_prompt_right(context, value) {
        return (
          <SessionLine
            api={api}
            sessionID={value.session_id}
            context={context}
            sessions={sessions}
          />
        )
      },
    },
  })
}

export default { id: "rtrt-statusline", tui }
