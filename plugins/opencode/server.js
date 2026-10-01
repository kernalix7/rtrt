import { randomUUID } from "node:crypto"
import path from "node:path"

import { createDashboardSupervisor } from "./runtime/dashboard-supervisor.js"
import { createPermissionEvaluator } from "./v2/permissions.js"

const RTRT_TOOLS = new Set(["rtrt_agent_call", "rtrt_agent_route"])
const DASHBOARD_INPUT = { type: "object", properties: {}, additionalProperties: false }

function scheduleStartup(start) {
  const timer = setTimeout(() => { void start() }, 0)
  timer.unref?.()
  return { cancel: () => clearTimeout(timer) }
}

// The native server contract is @opencode/plugin 2.0.20. The v1 root remains
// separate: v2 has no permission.create or shell call identity, so it cannot
// provide the legacy Claude permission broker or per-call shell provenance.
export function createNativeServer({ supervisor, schedule = scheduleStartup, managedAgentStatePath, report = (message) => process.emitWarning(message, "RTRT_NATIVE_PLUGIN") } = {}) {
  return {
    id: "rtrt-agent",
    async setup(ctx) {
      const dashboard = supervisor ?? createDashboardSupervisor()
      const invocations = new Map()
      const registrations = []
      let disposed = false
      let cleanupPromise
      const notify = (message) => {
        try { report(message) } catch {
          process.emitWarning("RTRT native diagnostic unavailable.", "RTRT_NATIVE_PLUGIN")
        }
      }
      const startup = schedule(() => Promise.resolve()
        .then(() => { if (!disposed) return dashboard.ensure() })
        .catch(() => { if (!disposed) notify("RTRT dashboard unavailable.") }))
      const dispose = () => {
        if (cleanupPromise) return cleanupPromise
        disposed = true
        invocations.clear()
        cleanupPromise = (async () => {
          const failures = []
          try { startup.cancel() } catch { failures.push(new Error("RTRT startup cancellation failed.")) }
          const outcomes = await Promise.allSettled(registrations.reverse().map((registration) =>
            Promise.resolve().then(() => registration.dispose())))
          for (const [index, outcome] of outcomes.entries()) {
            if (outcome.status === "rejected") failures.push(new Error(`RTRT registration ${index} disposal failed.`))
          }
          if (failures.length) throw new AggregateError(failures, "RTRT native cleanup failed.")
        })()
        return cleanupPromise
      }
      try {
        registrations.push(await ctx.tool.hook("execute.before", (event) => {
          if (disposed || !RTRT_TOOLS.has(event.tool) || !event.input || typeof event.input !== "object" || Array.isArray(event.input)) return
          const key = `${event.sessionID}\0${event.id}`
          let invocationID = invocations.get(key)
          if (!invocationID) {
            invocationID = randomUUID()
            invocations.set(key, invocationID)
          }
          Object.assign(event.input, {
            invocation_id: invocationID,
            parent_project: path.basename(ctx.location.project.canonical),
            parent_session_id: event.sessionID,
            parent_call_id: event.id,
            caller_agent: event.agent,
            parent_cwd: ctx.location.directory,
            parent_worktree: ctx.location.project.canonical,
          })
        }))
        registrations.push(await ctx.tool.hook("execute.after", (event) => {
          if (!disposed && RTRT_TOOLS.has(event.tool)) invocations.delete(`${event.sessionID}\0${event.id}`)
        }))
        registrations.push(await ctx.tool.transform((editor) => {
          if (disposed) return
          editor.add({
            name: "rtrt_dashboard_open",
            description: "Open the RTRT dashboard in the operator's browser when explicitly requested.",
            input: DASHBOARD_INPUT,
            async execute() {
              if (disposed) return { content: "Dashboard unavailable." }
              const status = await dashboard.open().catch(() => "Dashboard unavailable.")
              return { content: status === "Dashboard open requested." ? status : "Dashboard unavailable." }
            },
          })
        }))
        const evaluate = await createPermissionEvaluator(ctx, managedAgentStatePath, () => !disposed)
        registrations.push(await ctx.permission.hook("evaluate", evaluate))
        return dispose
      } catch (error) {
        try { await dispose() } catch { notify("RTRT native cleanup failed.") }
        throw error
      }
    },
  }
}

export default createNativeServer()
