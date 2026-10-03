import { execFile } from "node:child_process"
import { readFileSync } from "node:fs"
import path from "node:path"
import { promisify } from "node:util"

const execute = promisify(execFile)
const script = readFileSync(new URL("./dashboard-acl.ps1", import.meta.url), "utf8")

export async function windowsAcl(target, action, { run = execute, systemRoot = process.env.SystemRoot } = {}) {
  if (!path.win32.isAbsolute(target) || !systemRoot || !path.win32.isAbsolute(systemRoot)) throw new Error("Dashboard unavailable.")
  const binary = path.win32.join(systemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
  try {
    await run(binary, ["-NoProfile", "-NonInteractive", "-Command", script], {
      windowsHide: true,
      timeout: 45_000,
      maxBuffer: 4096,
      env: { SystemRoot: systemRoot, RTRT_ACL_PATH: target, RTRT_ACL_ACTION: action },
    })
  } catch {
    throw new Error("Dashboard unavailable.")
  }
}
