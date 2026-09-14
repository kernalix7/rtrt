/** @jsxImportSource @opentui/solid */
import { spawn } from "node:child_process"
import { useTerminalDimensions } from "@opentui/solid"
import type { TuiPlugin, TuiPluginApi, TuiSlotContext } from "@opencode-ai/plugin/tui"
import { createEffect, createMemo, createSignal, For, onMount, untrack } from "solid-js"

import {
  createSpawnLimiter,
  createStatuslineController,
  createStatuslineViewModel,
  deriveSessionEconomics,
  initialStatuslineState,
  runStatusline,
  STATUSLINE_REFRESH_EVENTS,
  statuslineRefreshTarget,
  type StatuslineState,
  type StatuslineTone,
  type StatuslineViewModel,
  type StatuslineViewPart,
} from "./rtrt-statusline-core.mjs"

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
  economicsRevision: () => number
  refreshEconomics: () => void
  controller: StatuslineController
  dispose: () => void
}

function activeSessionID(api: TuiPluginApi) {
  const route = api.route.current
  const params = "params" in route ? route.params : undefined
  const sessionID = params?.sessionID
  return route.name === "session" && typeof sessionID === "string" ? sessionID : ""
}

function sessionEconomicsSnapshot(api: TuiPluginApi, sessionID: string) {
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
  const economics = createMemo(() => {
    props.entry.economicsRevision()
    const sessionID = activeSessionID(props.api)
    return untrack(() => sessionEconomicsSnapshot(props.api, sessionID))
  })
  const view = createMemo(() =>
    createStatuslineViewModel(props.entry.state(), { width: width(), economics: economics() }),
  )

  createEffect(() => {
    const sessionID = activeSessionID(props.api)
    const cwd = untrack(
      () =>
        (sessionID && props.api.state.session.get(sessionID)?.directory) ||
        props.api.state.path.directory,
    )
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

const tui: TuiPlugin = async (api, options) => {
  if (options?.enabled === false) return

  const bin = typeof options?.bin === "string" ? options.bin : undefined
  const spawnLimiter = createSpawnLimiter({ concurrency: 2 })
  const createEntry = (): StatuslineEntry => {
    const [state, setState] = createSignal(initialStatuslineState())
    const [economicsRevision, setEconomicsRevision] = createSignal(0)
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
    return {
      state,
      economicsRevision,
      refreshEconomics: () => setEconomicsRevision((revision) => revision + 1),
      controller,
      dispose: controller.dispose,
    }
  }
  const app = createEntry()

  const disposers = STATUSLINE_REFRESH_EVENTS.map((name) =>
    api.event.on(name, (event) => {
      const target = statuslineRefreshTarget(event, activeSessionID(api))
      if (target.app) {
        app.refreshEconomics()
        app.controller.request()
      }
    }),
  )
  api.lifecycle.onDispose(() => {
    app.dispose()
    spawnLimiter.dispose()
    for (const dispose of disposers) dispose()
  })

  api.slots.register({
    slots: {
      app_bottom(context) {
        return <AppLine api={api} context={context} entry={app} />
      },
    },
  })
}

export default { id: "rtrt-statusline", tui }
