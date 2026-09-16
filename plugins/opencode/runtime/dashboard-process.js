import { spawn } from "node:child_process"
import { get } from "node:http"
import path from "node:path"
import { unavailable } from "./dashboard-files.js"

export function probeDashboard({ request = get, timeout = 750 } = {}) {
  return new Promise((resolve) => {
    let settled = false
    let requestHandle
    const finish = (result) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      requestHandle?.destroy()
      resolve(result)
    }
    const timer = setTimeout(() => finish("conflict"), timeout)
    try {
      requestHandle = request({ hostname: "127.0.0.1", port: 7311, path: "/healthz", agent: false }, (response) => {
        let body = ""
        response.on("error", () => finish("conflict"))
        response.on("aborted", () => finish("conflict"))
        response.on("data", (chunk) => {
          body += chunk.toString("utf8")
          if (body.length > 16) finish("conflict")
        })
        response.on("end", () => finish(response.statusCode === 200 && body === "ok" ? "healthy" : "conflict"))
      })
      requestHandle.once("error", (error) => finish(error.code === "ECONNREFUSED" ? "offline" : "conflict"))
    } catch {
      finish("conflict")
    }
  })
}

export function launchDetached(binary, args, { spawnProcess = spawn, ...options } = {}) {
  return new Promise((resolve, reject) => {
    if (!path.isAbsolute(binary)) { reject(unavailable()); return }
    try {
      const child = spawnProcess(binary, args, {
        ...options, detached: true, windowsHide: true, stdio: "ignore", shell: false,
      })
      child.once("error", () => reject(unavailable()))
      child.once("spawn", () => { child.unref(); resolve() })
    } catch {
      reject(unavailable())
    }
  })
}

export async function openDashboardBrowser(url, { platform = process.platform, launch = launchDetached } = {}) {
  // Never consult BROWSER or PATH. URL is generated locally, not supplied by a tool argument.
  switch (platform) {
    case "linux": return launch("/usr/bin/xdg-open", [url])
    case "darwin": return launch("/usr/bin/open", [url])
    case "win32": return launch("C:\\Windows\\System32\\rundll32.exe", ["url.dll,FileProtocolHandler", url])
    default: throw unavailable()
  }
}
