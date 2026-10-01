import { deriveSessionEconomics } from "../rtrt-statusline-core.mjs"

/** @typedef {import("@opencode/plugin/tui").Plugin.Context} Context */

/** @param {never} value @returns {never} */
function assertNever(value) {
  throw new TypeError(`Unsupported native route: ${String(value)}`)
}

/**
 * @param {Context} ctx
 * @param {string} [slotSessionID]
 */
export function activeSessionID(ctx, slotSessionID) {
  if (slotSessionID !== undefined) return slotSessionID
  const route = ctx.ui.router.current()
  switch (route.type) {
    case "session": return route.sessionID
    case "home":
    case "plugin": return ""
    default: return assertNever(route)
  }
}

/**
 * Translate only documented 2.0.20 values; never use aggregate session tokens
 * as context usage, message costs as session spend, or pricing as quota.
 * @param {Context} ctx
 * @param {string} sessionID
 * @param {ReturnType<Context["ui"]["model"]["current"]>} [selected]
 * @returns {{ readonly cwd: string, readonly economics: import("../rtrt-statusline-core.mjs").StatuslineEconomics }}
 */
export function nativeSnapshot(ctx, sessionID, selected = ctx.ui.model.current()) {
  const session = sessionID ? ctx.data.session.get(sessionID) : undefined
  const location = session?.location ?? ctx.location ?? ctx.data.location.default()
  const models = ctx.data.location.model.list(location) ?? []
  const providers = Array.from(new Set(models.map((model) => model.providerID)), (id) => ({
    id,
    models: Object.fromEntries(models.filter((model) => model.providerID === id).map((model) => [model.id, model])),
  }))
  const model = selected ? { id: selected.modelID, providerID: selected.providerID } : session?.model
  const messages = session ? ctx.data.session.message.list(sessionID) : []
  const economics = deriveSessionEconomics({
    session: {
      ...(session ? { cost: session.cost } : {}),
      ...(model ? { model } : {}),
    },
    messages: messages.filter((message) => message.type === "assistant").map((message) => ({
      role: "assistant",
      providerID: message.model.providerID,
      modelID: message.model.id,
      time: message.time,
      ...(message.tokens ? { tokens: message.tokens } : {}),
      ...(message.error ? { error: message.error } : {}),
    })),
    providers,
    status: { type: session && ctx.data.session.status(sessionID) === "running" ? "busy" : "idle" },
  })
  return {
    cwd: location.directory,
    economics: {
      ...economics,
      status: session ? economics.status : { text: "UNKNOWN", tone: "muted" },
    },
  }
}
