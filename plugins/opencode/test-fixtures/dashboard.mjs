import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"

export const TOKEN = "0123456789abcdef".repeat(4)
export const VERSION = "0.1.3"
export const PACKAGE = "rtrt-dashboard-linux-x64"

export async function fixture(t) {
  const parent = fileURLToPath(new URL("../../../.rtrt/tmp/", import.meta.url))
  await mkdir(parent, { recursive: true, mode: 0o700 })
  const home = await mkdtemp(path.join(parent, "dashboard-home-"))
  t.after(() => rm(home, { recursive: true, force: true }))
  const state = path.join(home, ".rtrt", "dashboard")
  await mkdir(path.dirname(state), { mode: 0o700 })
  await mkdir(state, { mode: 0o700 })
  const envFile = path.join(state, "dashboard.env")
  const saveToken = () => writeFile(envFile, `RTRT_DASHBOARD_TOKEN=${TOKEN}\n`, { mode: 0o600 })
  return { home, state, envFile, saveToken }
}

export async function packageFixture(root, version = VERSION) {
  const directory = path.join(root, PACKAGE)
  await mkdir(path.join(directory, "bin"), { recursive: true, mode: 0o755 })
  await writeFile(path.join(directory, "package.json"), JSON.stringify({ name: PACKAGE, version }))
  const binary = path.join(directory, "bin", "rtrt-dashboard")
  await writeFile(binary, "fixture only", { mode: 0o755 })
  return binary
}
