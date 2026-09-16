import { createHmac, randomBytes as systemRandomBytes } from "node:crypto"
import { homedir } from "node:os"
import { setTimeout as delay } from "node:timers/promises"
import { dashboardFiles, unavailable } from "./dashboard-files.js"
import { resolveDashboardBinary } from "./dashboard-binary.js"
import { launchDetached, openDashboardBrowser, probeDashboard } from "./dashboard-process.js"

export function createDashboardSupervisor({
  env = process.env, platform = process.platform, arch = process.arch, uid = process.geteuid?.(),
  probe = probeDashboard, resolveBinary = resolveDashboardBinary, launch = launchDetached,
  openBrowser = openDashboardBrowser, wait = delay, now = Date.now, randomBytes = systemRandomBytes,
} = {}) {
  const home = env.HOME || env.USERPROFILE || homedir()
  const files = dashboardFiles({ home, uid, platform })
  let pending
  const check = async () => {
    const status = await probe()
    switch (status) {
      case "healthy": return true
      case "offline": return false
      case "conflict": throw unavailable()
      default: throw unavailable()
    }
  }
  const start = async () => {
    await files.prepare()
    const token = await files.readToken()
    if (await check()) {
      if (!token) throw unavailable()
      return
    }
    let release
    for (let attempt = 0; attempt < 20; attempt++) {
      release = await files.acquire()
      if (release) break
      await wait(100)
      await files.prepare()
      if (await check()) {
        if (!await files.readToken()) throw unavailable()
        return
      }
    }
    if (!release) throw unavailable()
    try {
      // A different process may have completed startup between first probe and lock.
      if (await check()) {
        if (!await files.readToken()) throw unavailable()
        return
      }
      const binary = await resolveBinary({ home, platform, arch, uid })
      if (!await files.readToken()) await files.createToken(randomBytes(32).toString("hex"))
      // Keep operator environment, but never inherit loader injection or dashboard credentials.
      const childEnv = Object.fromEntries(Object.entries(env).filter(([key]) =>
        !/^(?:LD_|DYLD_|RTRT_DASHBOARD_|NODE_OPTIONS$)/i.test(key)))
      await launch(binary, ["--machine", "--state-dir", files.state], {
        cwd: home,
        env: { ...childEnv, HOME: home, RTRT_DASHBOARD_BIND: "127.0.0.1:7311" },
      })
      for (let attempt = 0; attempt < 20; attempt++) {
        if (await check()) return
        await wait(100)
      }
      throw unavailable()
    } finally {
      await release()
    }
  }
  const ensure = () => {
    if (!pending) {
      pending = start().catch(() => { throw unavailable() }).finally(() => { pending = undefined })
    }
    return pending
  }
  const open = async () => {
    try {
      await ensure()
      const token = await files.readToken()
      if (!token) throw unavailable()
      const issued = BigInt(Math.floor(now() / 1000))
      const wire = Buffer.alloc(65)
      wire[0] = 1
      wire.writeBigUInt64BE(issued, 1)
      wire.writeBigUInt64BE(issued + 60n, 9)
      randomBytes(16).copy(wire, 17)
      createHmac("sha256", Buffer.from(token, "hex"))
        .update("rtrt-dashboard-browser-bootstrap\0v1\0")
        .update(wire.subarray(0, 33)).digest().copy(wire, 33)
      await openBrowser(`http://127.0.0.1:7311/#bootstrap=${wire.toString("base64url")}`, { platform })
      return "Dashboard open requested."
    } catch {
      // Never expose dependency errors: process errors can contain the bootstrap URL.
      return "Dashboard unavailable."
    }
  }
  return { ensure, open }
}
