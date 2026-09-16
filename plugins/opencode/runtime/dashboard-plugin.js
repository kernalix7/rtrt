import { RtrtProvenance } from "../rtrt-provenance.js"
import { createDashboardSupervisor } from "./dashboard-supervisor.js"

const scheduleStartup = (callback) => setTimeout(callback, 0).unref()

export function createDashboardPlugin({
  provenance = RtrtProvenance, supervisor = createDashboardSupervisor(), schedule = scheduleStartup,
} = {}) {
  return async (input) => {
    const hooks = await provenance(input)
    // Startup is best-effort and never blocks plugin initialization or opens a browser.
    schedule(() => { void supervisor.ensure().catch(() => {}) })
    return {
      ...hooks,
      tool: {
        ...hooks.tool,
        rtrt_dashboard_open: {
          description: "Open the RTRT dashboard in the operator's browser when explicitly requested.",
          args: {},
          async execute() {
            try {
              const status = await supervisor.open()
              return status === "Dashboard open requested." ? status : "Dashboard unavailable."
            } catch {
              return "Dashboard unavailable."
            }
          },
        },
      },
    }
  }
}
