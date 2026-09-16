#!/usr/bin/env node
import { realpath } from "node:fs/promises"
import { fileURLToPath } from "node:url"
import { createDashboardSupervisor } from "../runtime/dashboard-supervisor.js"

export async function runDashboardOpen({
  open = () => createDashboardSupervisor().open(), write = (text) => process.stdout.write(text),
} = {}) {
  let success = false
  try { success = await open() === "Dashboard open requested." } catch {
    // Process/browser failures may contain credentials; emit only a fixed status.
  }
  write(success ? "Dashboard open requested.\n" : "Dashboard unavailable.\n")
  return success ? 0 : 1
}

if (process.argv[1] && await realpath(process.argv[1]).catch(() => "") === fileURLToPath(import.meta.url)) {
  process.exitCode = await runDashboardOpen()
}
