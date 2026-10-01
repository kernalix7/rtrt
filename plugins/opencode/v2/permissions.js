import { constants } from "node:fs"
import { lstat, open } from "node:fs/promises"
import { homedir } from "node:os"
import path from "node:path"

const SESSION_ID = /^[A-Za-z0-9_-]{1,256}$/
const OWNER = "rtrt-opencode-task-agents"
const MANAGER = "rtrt-manager"
const STATE_LIMIT = 65_536
const configuredPath = (value) => typeof value === "string" && value.length > 0 && !value.includes("\0")

function boundedState(value, depth = 8, count = { value: 0 }) {
  count.value++
  if (count.value > 512 || depth < 0) return false
  if (!value || typeof value !== "object") return true
  const items = Object.values(value)
  if (items.length > 128) return false
  return items.every((item) => boundedState(item, depth - 1, count))
}

function secureStateFile(stat, uid) {
  return stat.isFile() && !stat.isSymbolicLink() && stat.uid === uid &&
    stat.nlink === 1n && (stat.mode & 0o777n) === 0o600n && stat.size <= BigInt(STATE_LIMIT)
}

function sameFile(left, right) {
  return ["dev", "ino", "uid", "mode", "nlink", "size", "mtimeNs", "ctimeNs"]
    .every((field) => left[field] === right[field])
}

async function secureAncestors(file, uid, statPath = lstat) {
  let directory = path.dirname(file)
  while (true) {
    const stat = await statPath(directory, { bigint: true })
    if (!stat.isDirectory() || stat.isSymbolicLink() ||
      (stat.uid !== uid && stat.uid !== 0n) || (stat.mode & 0o022n) !== 0n) return false
    const parent = path.dirname(directory)
    if (parent === directory) return true
    directory = parent
  }
}

export function managedAgentStatePath(env = process.env) {
  const home = configuredPath(env.HOME) ? env.HOME : homedir()
  const root = configuredPath(env.OPENCODE_CONFIG_DIR)
    ? path.resolve(env.OPENCODE_CONFIG_DIR)
    : configuredPath(env.XDG_CONFIG_HOME)
      ? path.resolve(env.XDG_CONFIG_HOME, "opencode")
      : path.resolve(home, ".config", "opencode")
  return path.join(root, "agents", ".rtrt-managed-state.json")
}

export async function loadAgents(file, io = { lstat, open }) {
  let handle
  let result
  try {
    const currentUser = process.getuid?.()
    if (currentUser === undefined || typeof file !== "string" || !path.isAbsolute(file) || file.includes("\0")) return undefined
    const uid = BigInt(currentUser)
    if (!(await secureAncestors(file, uid, io.lstat))) return undefined
    const stat = await io.lstat(file, { bigint: true })
    if (!secureStateFile(stat, uid)) return undefined
    handle = await io.open(file, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0))
    const opened = await handle.stat({ bigint: true })
    if (!secureStateFile(opened, uid) || !sameFile(stat, opened)) return undefined
    const raw = await handle.readFile("utf8")
    if (Buffer.byteLength(raw) > STATE_LIMIT) return undefined
    const after = await handle.stat({ bigint: true })
    const finalPath = await io.lstat(file, { bigint: true })
    if (!sameFile(opened, after) || !sameFile(opened, finalPath) ||
      !(await secureAncestors(file, uid, io.lstat))) return undefined
    const state = JSON.parse(raw)
    if (!state || typeof state !== "object" || Array.isArray(state) || !boundedState(state)) return undefined
    if (state.owner !== OWNER || state.version !== 1 || !Array.isArray(state.agents)) return undefined
    if (state.agents.length < 1 || state.agents.length > 128 || !state.agents.every((id) =>
      typeof id === "string" && SESSION_ID.test(id))) return undefined
    const agents = new Set(state.agents)
    result = agents.size === state.agents.length && agents.has(MANAGER) ? agents : undefined
  } catch {
    // State is an authorization boundary; unreadable or malformed state fails closed.
    return undefined
  } finally {
    if (handle) {
      try { await handle.close() } catch { result = undefined }
    }
  }
  return result
}

export async function createPermissionEvaluator(ctx, statePath = managedAgentStatePath(), active = () => true) {
  const managed = await loadAgents(statePath)
  const project = ctx.location.project
  const directory = ctx.location.directory
  return async (request) => {
    if (
      !active() || !managed || request.effect !== "ask" || request.action !== "shell" ||
      request.resources?.length !== 1 || request.resources[0] !== "pwd" ||
      !SESSION_ID.test(request.sessionID ?? "")
    ) return
    try {
      const session = await ctx.session.get({ sessionID: request.sessionID })
      if (
        session?.id !== request.sessionID || !managed.has(session.agent) ||
        session.agent === MANAGER || request.agent && request.agent !== session.agent ||
        session.projectID !== project.id || session.location?.directory !== directory ||
        !SESSION_ID.test(session.parentID ?? "")
      ) return
      const parent = await ctx.session.get({ sessionID: session.parentID })
      if (
        parent?.id !== session.parentID || parent.agent !== MANAGER ||
        parent.projectID !== project.id || parent.location?.directory !== directory
      ) return
      if (active() && request.effect === "ask") request.effect = "allow"
    } catch {
      // Unknown session or unavailable host API must preserve the host's decision.
      return
    }
  }
}
