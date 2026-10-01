import { spawn } from "node:child_process"
import { isAbsolute } from "node:path"
import { createStatuslineController, runStatusline } from "../rtrt-statusline-core.mjs"

/** @typedef {import("@opencode/plugin/tui").Plugin.Context} Context */
/** @typedef {import("@opencode/plugin/tui/context").SlotMap["prompt.footer.status"]} FooterInput */

/** @type {readonly import("@opencode/client").OpenCodeEvent["type"][]} */
export const NATIVE_REFRESH_EVENTS = Object.freeze([
  "session.created", "session.deleted", "session.renamed", "session.moved",
  "session.model.selected", "session.status", "session.idle",
  "session.step.ended", "session.step.failed",
  "session.usage.updated",
  "session.compaction.ended", "session.compaction.failed",
  "model.updated", "provider.updated",
])

/**
 * @param {Context} ctx
 * @param {{readonly sessionID: () => string, readonly refresh: () => void, readonly dispose: () => void}} entry
 * @param {(input: FooterInput) => import("@opentui/solid").JSX.Element} render
 */
export function registerNativeStatusline(ctx, entry, render) {
  /** @type {Array<() => void>} */
  const disposers = []
  let disposed = false
  const cleanup = () => {
    if (disposed) return
    disposed = true
    /** @type {unknown[]} */
    const errors = []
    for (const dispose of [entry.dispose, ...disposers.reverse()]) {
      try { dispose() } catch (error) { errors.push(error) }
    }
    if (errors.length) throw new AggregateError(errors, "Native statusline cleanup failed")
  }
  try {
    for (const name of NATIVE_REFRESH_EVENTS) {
      disposers.push(ctx.data.on(name, (event) => {
        if (disposed) return
        if ("sessionID" in event.data && event.data.sessionID !== entry.sessionID()) return
        entry.refresh()
      }))
    }
    disposers.push(ctx.ui.slot({ append: "prompt.footer.status", render }))
    return cleanup
  } catch (error) {
    try { cleanup() } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "Native statusline registration failed")
    }
    throw error
  }
}

/**
 * The existing controller serializes refreshes and owns subprocess aborts.
 * The runner keeps its 1.5s total deadline, 64KiB cap and shell:false argv.
 * An explicit bin must be absolute and never falls back after failure.
 * When omitted, the optional CLI uses RTRT_BIN then PATH's rtrt; if neither
 * works, the shared view shows unavailable rather than inventing CLI data.
 * @param {Readonly<Record<string, unknown>>} options
 * @param {(state: import("../rtrt-statusline-core.mjs").StatuslineState) => void} onState
 * @param {typeof spawn} [spawnImpl]
 */
export function createNativeController(options, onState, spawnImpl = spawn) {
  const explicit = Object.hasOwn(options, "bin")
  const bin = typeof options["bin"] === "string" ? options["bin"] : undefined
  /** @type {(binary: string, args: readonly string[], settings: import("node:child_process").SpawnOptions) => import("node:child_process").ChildProcess | undefined} */
  const spawnCandidate = (binary, args, settings) => {
    // Keep the shared legacy runner unchanged, but deny its fallback candidates.
    if (explicit && binary !== bin) return undefined
    return spawnImpl(binary, args, settings)
  }
  return createStatuslineController({
    load: (context, signal) => {
      if (explicit && (bin === undefined || !isAbsolute(bin))) return undefined
      return runStatusline({ ...context, bin, signal, spawnImpl: spawnCandidate })
    },
    onState,
  })
}
