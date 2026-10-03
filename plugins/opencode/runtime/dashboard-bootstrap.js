import { randomBytes } from "node:crypto"
import { constants } from "node:fs"
import { lstat, open, rename, unlink } from "node:fs/promises"
import path from "node:path"
import { privateMetadata, realPath, unavailable } from "./dashboard-files.js"
import { windowsAcl } from "./dashboard-acl.js"

export async function writeDashboardBootstrap(state, credential, { platform = process.platform, uid = process.geteuid?.() } = {}) {
  if (!/^[A-Za-z0-9_-]{87}$/.test(credential)) throw unavailable()
  await realPath(state)
  const stateMetadata = await lstat(state)
  privateMetadata(stateMetadata, { platform, uid }, 0o700)
  if (!stateMetadata.isDirectory()) throw unavailable()
  if (platform === "win32") await windowsAcl(state, "private-check")
  const destination = path.join(state, "bootstrap.html")
  try {
    const existing = await lstat(destination)
    privateMetadata(existing, { platform, uid }, 0o600)
    if (!existing.isFile() || existing.nlink !== 1 || existing.size > 512) throw unavailable()
    if (platform === "win32") await windowsAcl(destination, "private-check")
  } catch (error) {
    if (error.code !== "ENOENT") throw error
  }
  const temporary = path.join(state, `.bootstrap-${randomBytes(16).toString("hex")}.tmp`)
  const handle = await open(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | (constants.O_NOFOLLOW ?? 0), 0o600)
  try {
    if (platform === "win32") await windowsAcl(temporary, "private-create")
    privateMetadata(await handle.stat(), { platform, uid }, 0o600)
    const body = `<!doctype html><meta http-equiv="refresh" content="0;url=http://127.0.0.1:7311/#bootstrap=${credential}">`
    await handle.writeFile(body)
    await handle.sync()
  } catch (error) {
    await handle.close()
    await unlink(temporary)
    throw error
  }
  await handle.close()
  try {
    await rename(temporary, destination)
  } catch (error) {
    await unlink(temporary)
    throw error
  }
  return destination
}
