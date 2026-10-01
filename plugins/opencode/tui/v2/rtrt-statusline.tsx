/** @jsxImportSource @opentui/solid */
import type { Plugin } from "@opencode/plugin/tui"
import type { SlotMap } from "@opencode/plugin/tui/context"
import type { BoxRenderable } from "@opentui/core"
import { useTerminalDimensions } from "@opentui/solid"
import { createEffect, createMemo, createSignal, For, onMount, untrack } from "solid-js"
import {
  createStatuslineViewModel,
  initialStatuslineState,
  type StatuslineState,
  type StatuslineViewPart,
} from "../rtrt-statusline-core.mjs"
import { activeSessionID, nativeSnapshot } from "./native-state.mjs"
import { createNativeController, registerNativeStatusline } from "./native-runtime.mjs"

// Preserve the legacy footer's two-cell gutters, inherited terminal typography,
// truncation and responsive view model. All colors come from the native theme.
const GUTTER = 2

function partColor(theme: Plugin.Context["theme"], part: StatuslineViewPart) {
  switch (part.role) {
    case "separator": return theme.border.base
    case "label": return theme.text.muted
    case "badge":
    case "value": return {
      accent: theme.hue.accent[600],
      good: theme.text.feedback.success.base,
      warn: theme.text.feedback.warning.base,
      bad: theme.text.feedback.error.base,
      muted: theme.text.muted,
    }[part.tone]
    default: {
      const exhaustive: never = part.role
      return exhaustive
    }
  }
}

type Entry = {
  readonly state: () => StatuslineState
  readonly revision: () => number
  readonly controller: ReturnType<typeof createNativeController>
}

function NativeLine(props: {
  readonly ctx: Plugin.Context
  readonly input: SlotMap["prompt.footer.status"]
  readonly entry: Entry
}) {
  const dimensions = useTerminalDimensions()
  const [measuredWidth, setMeasuredWidth] = createSignal<number>()
  const width = createMemo(() => Math.max(1, (measuredWidth() ?? dimensions().width) - GUTTER * 2))
  const sessionID = () => activeSessionID(props.ctx, props.input.sessionID)
  const snapshot = createMemo(() => {
    props.entry.revision()
    const session = sessionID()
    const selected = props.ctx.ui.model.current()
    // Stream deltas never subscribe the footer to every token.
    return untrack(() => nativeSnapshot(props.ctx, session, selected))
  })
  const view = createMemo(() => createStatuslineViewModel(props.entry.state(), {
    width: width(), economics: snapshot().economics,
  }))
  createEffect(() => {
    props.entry.controller.setContext({
      cwd: snapshot().cwd,
      session: sessionID(),
      width: width(),
      model: snapshot().economics.modelRef ?? "",
    })
  })
  onMount(() => props.entry.controller.start())

  return (
    <box
      width="100%"
      height={view().rows.length}
      flexDirection="column"
      flexShrink={0}
      paddingLeft={GUTTER}
      paddingRight={GUTTER}
      overflow="hidden"
      onSizeChange={function (this: BoxRenderable) { setMeasuredWidth(this.width) }}
    >
      <For each={view().rows}>
        {(row) => (
          <text fg={props.ctx.theme.text.muted} wrapMode="none" truncate>
            <For each={row.parts}>
              {(part) => <span style={{ fg: partColor(props.ctx.theme, part) }}>{part.text}</span>}
            </For>
          </text>
        )}
      </For>
    </box>
  )
}

export default {
  id: "rtrt-statusline",
  setup(ctx) {
    if (ctx.options["enabled"] === false) return
    const [state, setState] = createSignal(initialStatuslineState())
    const [revision, setRevision] = createSignal(0)
    const refreshEconomics = () => { setRevision((value) => value + 1) }
    const controller = createNativeController(ctx.options, (next) => {
      setState(next)
      refreshEconomics()
    })
    const entry: Entry = { state, revision, controller }
    return registerNativeStatusline(ctx, {
      sessionID: () => controller.getContext().session,
      refresh: () => { refreshEconomics(); controller.request() },
      dispose: controller.dispose,
    }, (input) => <NativeLine ctx={ctx} input={input} entry={entry} />)
  },
} satisfies Plugin.Definition
