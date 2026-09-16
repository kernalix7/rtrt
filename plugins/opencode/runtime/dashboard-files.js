import { constants } from "node:fs"
import { lstat, mkdir, open, rmdir } from "node:fs/promises"
import path from "node:path"

export const unavailable = () => new Error("Dashboard unavailable.")

export async function realPath(candidate) {
  if (typeof candidate !== "string" || !path.isAbsolute(candidate) || candidate.includes("\0")) throw unavailable()
  const normalized = path.resolve(candidate)
  const parts = normalized.slice(path.parse(normalized).root.length).split(path.sep).filter(Boolean)
  let current = path.parse(normalized).root
  for (const part of parts) {
    current = path.join(current, part)
    if ((await lstat(current)).isSymbolicLink()) throw unavailable()
  }
  return normalized
}

export function privateMetadata(stat, { uid, platform }, mode) {
  if (stat.isSymbolicLink()) throw unavailable()
  if (platform !== "win32" && (stat.uid !== uid || (stat.mode & 0o7777) !== mode)) throw unavailable()
}

export async function readRegular(file, validate, limit = 16_384) {
  const before = await lstat(file)
  validate(before)
  if (!before.isFile() || before.nlink !== 1 || before.size > limit) throw unavailable()
  const handle = await open(file, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0))
  try {
    const opened = await handle.stat()
    validate(opened)
    if (!opened.isFile() || opened.dev !== before.dev || opened.ino !== before.ino) throw unavailable()
    const buffer = Buffer.alloc(limit + 1)
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0)
    if (bytesRead > limit) throw unavailable()
    return buffer.subarray(0, bytesRead).toString("utf8")
  } finally {
    await handle.close()
  }
}

export function dashboardFiles({ home, uid, platform }) {
  const identity = { uid, platform }
  const state = path.join(home, ".rtrt", "dashboard")
  const credential = path.join(state, "dashboard.env")
  const lock = path.join(state, "startup.lock")
  const directory = async (candidate) => {
    const stat = await lstat(candidate)
    privateMetadata(stat, identity, 0o700)
    if (!stat.isDirectory()) throw unavailable()
  }
  const checkLock = async () => {
    try { await directory(lock) } catch (error) {
      if (error.code !== "ENOENT") throw error
    }
  }
  const prepare = async () => {
    await realPath(home)
    const stat = await lstat(home)
    if (!stat.isDirectory() || (platform !== "win32" && (stat.uid !== uid || (stat.mode & 0o022) !== 0))) throw unavailable()
    for (const candidate of [path.dirname(state), state]) {
      try { await mkdir(candidate, { mode: 0o700 }) } catch (error) {
        if (error.code !== "EEXIST") throw error
      }
      await directory(candidate)
    }
    await checkLock()
  }
  const readToken = async () => {
    await realPath(state)
    let raw
    try { raw = await readRegular(credential, (stat) => privateMetadata(stat, identity, 0o600)) } catch (error) {
      if (error.code === "ENOENT") return undefined
      throw error
    }
    const entries = raw.split(/\r?\n/).filter((line) => /^\s*RTRT_DASHBOARD_TOKEN\s*=/.test(line))
    if (entries.length !== 1) throw unavailable()
    const match = entries[0].match(/^\s*RTRT_DASHBOARD_TOKEN\s*=\s*(?:"([a-fA-F0-9]{64})"|'([a-fA-F0-9]{64})'|([a-fA-F0-9]{64}))\s*$/)
    if (!match) throw unavailable()
    return match[1] ?? match[2] ?? match[3]
  }
  const createToken = async (token) => {
    if (!/^[a-fA-F0-9]{64}$/.test(token)) throw unavailable()
    const handle = await open(credential, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | (constants.O_NOFOLLOW ?? 0), 0o600)
    try {
      privateMetadata(await handle.stat(), identity, 0o600)
      await handle.writeFile(`RTRT_DASHBOARD_TOKEN=${token}\n`)
      await handle.sync()
    } finally {
      await handle.close()
    }
  }
  const acquire = async () => {
    try { await mkdir(lock, { mode: 0o700 }) } catch (error) {
      if (error.code !== "EEXIST") throw error
      await directory(lock)
      return undefined
    }
    await directory(lock)
    // Never steal a lock on age/PID guesses, or remove another starter's lock.
    return () => rmdir(lock)
  }
  return { state, prepare, readToken, createToken, acquire }
}
