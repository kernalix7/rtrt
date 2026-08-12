// BEGIN rtrt-managed provenance plugin
import { constants } from "node:fs"
import { chmod, lstat, mkdir, open, readFile, realpath } from "node:fs/promises"
import { createHash, randomBytes, randomUUID, timingSafeEqual } from "node:crypto"
import { createServer } from "node:http"
import { fileURLToPath } from "node:url"
import path from "node:path"

const BROKER_PATH = "/rtrt/permission/v1"
const BODY_LIMIT = 32_768
const MAX_PENDING = 8
const MAX_DEPTH = 12
const MAX_FIELD = 4_096
const MAX_REPLAYS = 128
const MAX_PERMISSION_EVENT_REPLIES = 128
const MAX_TRACKED_CHILDREN = 128
const MAX_APPROVAL_SESSIONS = 128
const MAX_APPROVALS_PER_SESSION = 128
const MAX_COMMAND = 8_192
const MANAGED_AGENT_STATE_LIMIT = 65_536
const MANAGED_AGENT_STATE_DEPTH = 8
const MANAGED_AGENT_STATE_NODES = 512
const MAX_MANAGED_AGENTS = 128
const MANAGED_AGENT_STATE_OWNER = "rtrt-opencode-task-agents"
const MANAGED_AGENT_STATE_VERSION = 1
const MANAGER_AGENT = "rtrt-manager"
const DEFAULT_MANAGED_AGENT_STATE_PATH = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "agents",
  ".rtrt-managed-state.json",
)

const tokenizeCommand = (command) => {
  if (typeof command !== "string" || !command || Buffer.byteLength(command) > MAX_COMMAND) return
  if (/[\0\r\n`$~;|&<>*?\[]/.test(command)) return
  const tokens = []
  let token = ""
  let quote = ""
  let active = false
  for (let index = 0; index < command.length; index += 1) {
    const character = command[index]
    if (!quote && /\s/.test(character)) {
      if (active) tokens.push(token)
      token = ""
      active = false
      continue
    }
    if (character === "\\") {
      index += 1
      if (index >= command.length) return
      token += command[index]
      active = true
      continue
    }
    if (character === "'" || character === '"') {
      if (!quote) quote = character
      else if (quote === character) quote = ""
      else token += character
      active = true
      continue
    }
    token += character
    active = true
  }
  if (quote) return
  if (active) tokens.push(token)
  return tokens.length ? tokens : undefined
}

const URL_ARGUMENT = /^[A-Za-z][A-Za-z0-9+.-]*:\/\//
const BARE_COMMAND = /^[A-Za-z0-9][A-Za-z0-9_-]*$/
const SAFE_SESSION_FIELD = /^[A-Za-z0-9_-]{1,256}$/
const SAFE_AGENT_ID = /^[A-Za-z0-9_-]{1,256}$/

const boundedJson = (value, remaining = MANAGED_AGENT_STATE_DEPTH, count = { value: 0 }) => {
  count.value += 1
  if (count.value > MANAGED_AGENT_STATE_NODES || remaining < 0) return false
  if (!value || typeof value !== "object") return true
  if (Array.isArray(value)) {
    if (value.length > MAX_MANAGED_AGENTS) return false
    return value.every((item) => boundedJson(item, remaining - 1, count))
  }
  const entries = Object.entries(value)
  if (entries.length > MAX_MANAGED_AGENTS) return false
  return entries.every(([, item]) => boundedJson(item, remaining - 1, count))
}

const loadManagedAgents = async (statePath) => {
  if (typeof statePath !== "string" || !statePath || statePath.includes("\0")) return undefined
  let handle
  try {
    const before = await lstat(statePath)
    if (before.isSymbolicLink() || !before.isFile() || before.size > MANAGED_AGENT_STATE_LIMIT) return undefined
    handle = await open(statePath, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0))
    const opened = await handle.stat()
    if (
      !opened.isFile() ||
      opened.size > MANAGED_AGENT_STATE_LIMIT ||
      opened.dev !== before.dev ||
      opened.ino !== before.ino
    ) return undefined
    const raw = await handle.readFile("utf8")
    if (Buffer.byteLength(raw) > MANAGED_AGENT_STATE_LIMIT) return undefined
    const after = await handle.stat()
    if (after.size !== opened.size || after.dev !== opened.dev || after.ino !== opened.ino) return undefined
    const state = JSON.parse(raw)
    if (!state || typeof state !== "object" || Array.isArray(state) || !boundedJson(state)) return undefined
    if (state.owner !== MANAGED_AGENT_STATE_OWNER || state.version !== MANAGED_AGENT_STATE_VERSION) return undefined
    if (!Array.isArray(state.agents) || state.agents.length === 0 || state.agents.length > MAX_MANAGED_AGENTS) return undefined
    const agents = new Set()
    for (const agent of state.agents) {
      if (typeof agent !== "string" || !SAFE_AGENT_ID.test(agent) || agents.has(agent)) return undefined
      agents.add(agent)
    }
    return agents.has(MANAGER_AGENT) ? agents : undefined
  } catch {
    return undefined
  } finally {
    await handle?.close().catch(() => {})
  }
}

const realisticInside = async (root, operand) => {
  if (!operand || operand.includes("\0") || URL_ARGUMENT.test(operand)) return false
  if (!path.isAbsolute(operand) && operand.split(/[\\/]/).includes("..")) return false
  const context = typeof root === "string" ? { cwd: root, bounds: [root] } : root
  const target = path.resolve(context.cwd, operand)
  if (!context.bounds.some((bound) => isWithin(bound, target))) return false
  let probe = target
  while (true) {
    try {
      const resolved = await realpath(probe)
      return context.bounds.some((bound) => isWithin(bound, resolved))
    } catch (error) {
      if (error?.code !== "ENOENT" && error?.code !== "ENOTDIR") return false
      const parent = path.dirname(probe)
      if (parent === probe || !context.bounds.some((bound) => isWithin(bound, parent))) return false
      probe = parent
    }
  }
}

const splitOptions = (args, valueOptions = new Set()) => {
  const operands = []
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index]
    if (argument === "--") {
      operands.push(...args.slice(index + 1))
      break
    }
    if (!argument.startsWith("-") || argument === "-") {
      operands.push(argument)
      continue
    }
    const [option, attached] = argument.split(/=(.*)/s, 2)
    if (valueOptions.has(option)) {
      const value = attached ?? args[++index]
      if (!value) return
      operands.push(value)
    }
  }
  return operands
}

const allPathsInside = async (root, operands) => {
  if (!operands) return false
  for (const operand of operands) if (!(await realisticInside(root, operand))) return false
  return true
}

const READ_COMMANDS = new Set(["cat", "wc", "stat"])

const safeCommand = async (command, root) => {
  const tokens = tokenizeCommand(command)
  if (!tokens || !root || !BARE_COMMAND.test(tokens[0]) || tokens.some((item) => URL_ARGUMENT.test(item))) return false
  const [executable, ...args] = tokens
  for (const argument of args) {
    const value = argument.includes("=") ? argument.slice(argument.indexOf("=") + 1) : argument
    if ((path.isAbsolute(value) || value.includes("/") || value.includes("\\") || value.startsWith(".")) && !(await realisticInside(root, value))) return false
  }
  if (executable === "pwd") return args.every((item) => item === "-L" || item === "-P")
  if (executable === "ls") {
    if (args.some((item) => item.startsWith("-") && !/^-[AacdFfghikLlmnopqRrSstuUx1]+$/.test(item))) return false
    return allPathsInside(root, args.filter((item) => !item.startsWith("-")))
  }
  if (READ_COMMANDS.has(executable)) {
    if (args.some((item) => item.startsWith("-") && item !== "--")) return false
    const operands = splitOptions(args)
    return Boolean(operands?.length) && allPathsInside(root, operands)
  }
  if (["head", "tail"].includes(executable)) {
    const operands = []
    for (let index = 0; index < args.length; index += 1) {
      const argument = args[index]
      if (argument === "-q" || argument === "--quiet" || argument === "--silent" || argument === "-v" || argument === "--verbose") continue
      const attached = argument.match(/^(?:-n|--lines|-c|--bytes)=(.*)$/)
      if (attached) {
        if (!/^[-+]?\d+$/.test(attached[1])) return false
        continue
      }
      if (["-n", "--lines", "-c", "--bytes"].includes(argument)) {
        if (!/^[-+]?\d+$/.test(args[++index] ?? "")) return false
        continue
      }
      if (argument.startsWith("-")) return false
      operands.push(argument)
    }
    return operands.length > 0 && allPathsInside(root, operands)
  }
  return false
}

const defaultClientFactory = async (options) => {
  const { createOpencodeClient } = await import("@opencode-ai/sdk/v2")
  return createOpencodeClient(options)
}

const RTRT_AGENT_TOOLS = new Set([
  "rtrt_agent_call",
  "rtrt_agent_route",
  "rtrt_team_dispatch",
])

const sanitizeSessionID = (sessionID) => {
  const sanitized = String(sessionID ?? "")
    .replace(/[^A-Za-z0-9_-]/g, "_")
    .slice(0, 128)
  return sanitized || "session"
}

const isWithin = (parent, child) => {
  const relative = path.relative(parent, child)
  return (
    relative === "" ||
    (!path.isAbsolute(relative) && relative !== ".." && !relative.startsWith(`..${path.sep}`))
  )
}

const hasTraversal = (candidate) =>
  !path.isAbsolute(candidate) && candidate.split(path.sep).includes("..")

const canonicalProjectPath = async (rootDirectory, directory, candidate, allowMissing) => {
  if (
    typeof rootDirectory !== "string" ||
    !rootDirectory ||
    rootDirectory.includes("\0") ||
    typeof directory !== "string" ||
    !directory ||
    directory.includes("\0") ||
    typeof candidate !== "string" ||
    !candidate ||
    candidate.includes("\0")
  ) return undefined
  if (hasTraversal(candidate)) return undefined
  try {
    const root = await realpath(path.resolve(rootDirectory))
    const cwd = await realpath(path.resolve(directory))
    if (!isWithin(root, cwd)) return undefined
    const target = path.resolve(cwd, candidate)
    if (!isWithin(root, target)) return undefined
    try {
      const canonical = await realpath(target)
      return isWithin(root, canonical) ? canonical : undefined
    } catch (error) {
      if (!allowMissing || (error?.code !== "ENOENT" && error?.code !== "ENOTDIR")) return undefined
      let probe = path.dirname(target)
      while (isWithin(root, probe)) {
        try {
          const canonicalParent = await realpath(probe)
          return isWithin(root, canonicalParent) ? target : undefined
        } catch (parentError) {
          if (parentError?.code !== "ENOENT" && parentError?.code !== "ENOTDIR") return undefined
        }
        const parent = path.dirname(probe)
        if (parent === probe) return undefined
        probe = parent
      }
    }
  } catch {
    return undefined
  }
  return undefined
}

const readPathFile = async (file, prefix = "") => {
  const lines = (await readFile(file, "utf8")).split(/\r?\n/)
  if (lines.at(-1) === "") lines.pop()
  if (lines.length !== 1 || !lines[0].startsWith(prefix)) return undefined

  const value = lines[0].slice(prefix.length).trim()
  return value && !value.includes("\0") ? value : undefined
}

const resolveLinkedWorktree = async (worktree, gitFile) => {
  const rawGitDir = await readPathFile(gitFile, "gitdir:")
  if (!rawGitDir) return undefined

  const gitDirPath = path.resolve(worktree, rawGitDir)
  const gitDirStat = await lstat(gitDirPath)
  if (gitDirStat.isSymbolicLink() || !gitDirStat.isDirectory()) return undefined
  const gitDir = await realpath(gitDirPath)

  const rawCommonDir = await readPathFile(path.join(gitDir, "commondir"))
  const rawBacklink = await readPathFile(path.join(gitDir, "gitdir"))
  if (!rawCommonDir || !rawBacklink) return undefined

  const commonDirPath = path.resolve(gitDir, rawCommonDir)
  const commonDirStat = await lstat(commonDirPath)
  if (commonDirStat.isSymbolicLink() || !commonDirStat.isDirectory()) return undefined
  const commonDir = await realpath(commonDirPath)
  if (path.basename(commonDir) !== ".git") return undefined

  const mainWorktree = path.dirname(commonDir)
  if (mainWorktree === path.parse(mainWorktree).root) return undefined

  const mainGitStat = await lstat(path.join(mainWorktree, ".git"))
  if (mainGitStat.isSymbolicLink() || !mainGitStat.isDirectory()) return undefined
  if ((await realpath(path.join(mainWorktree, ".git"))) !== commonDir) return undefined

  const worktreesDir = await realpath(path.join(commonDir, "worktrees"))
  const adminRelative = path.relative(worktreesDir, gitDir)
  if (
    !adminRelative ||
    path.isAbsolute(adminRelative) ||
    adminRelative === ".." ||
    adminRelative.startsWith(`..${path.sep}`) ||
    adminRelative.includes(path.sep)
  ) {
    return undefined
  }

  const backlink = path.resolve(gitDir, rawBacklink)
  if ((await realpath(backlink)) !== (await realpath(gitFile))) return undefined
  return worktree
}

const resolveProjectWorktree = async (projectWorktree) => {
  if (!projectWorktree) return undefined

  try {
    const worktree = await realpath(path.resolve(projectWorktree))
    if (worktree === path.parse(worktree).root) return undefined

    const gitEntry = path.join(worktree, ".git")
    const gitStat = await lstat(gitEntry)
    if (gitStat.isSymbolicLink()) return undefined
    if (gitStat.isDirectory()) return worktree
    if (gitStat.isFile()) return await resolveLinkedWorktree(worktree, gitEntry)
  } catch {
    return undefined
  }

  return undefined
}

const ensureSessionTempDir = async (parentWorktree, sessionID) => {
  const root = await realpath(parentWorktree)
  const tempRoot = path.join(root, ".rtrt", "tmp", "opencode")
  const tempDir = path.join(tempRoot, sanitizeSessionID(sessionID))

  if (!isWithin(root, tempDir)) {
    throw new Error("OpenCode session temp directory escapes parent worktree")
  }

  // Create one level at a time so symlinks cannot redirect writes outside the repository.
  for (const directory of [
    path.join(root, ".rtrt"),
    path.join(root, ".rtrt", "tmp"),
    tempRoot,
    tempDir,
  ]) {
    try {
      await mkdir(directory, { mode: 0o700 })
    } catch (error) {
      if (error?.code !== "EEXIST") throw error
    }
    const stat = await lstat(directory)
    if (stat.isSymbolicLink() || !stat.isDirectory()) {
      throw new Error(`OpenCode session temp path is not a secure directory: ${directory}`)
    }
  }

  const realTempDir = await realpath(tempDir)
  if (!isWithin(root, realTempDir)) {
    throw new Error("OpenCode session temp directory escapes parent worktree")
  }
  await chmod(tempDir, 0o700)
  return tempDir
}

const boundedString = (value, maximum = MAX_FIELD) =>
  typeof value === "string" && value.length > 0 && value.length <= maximum ? value : undefined

const WEEKLY_USAGE_CODES = new Set([
  "weekly_usage_limit",
  "weekly-usage-limit",
  "weekly_limit",
  "usage_limit_weekly",
])

const weeklyUsageCode = (value) => {
  const normalized = boundedString(value, 128)?.trim().toLowerCase()
  return normalized && WEEKLY_USAGE_CODES.has(normalized)
}

const usageLimitPhrase = (value) => {
  const message = boundedString(value)
  if (!message) return undefined
  if (/\bweekly usage (?:limit|cap)\b/i.test(message)) return "weekly_usage_limit"
  if (/\b5[- ]hour usage (?:limit|cap)\b/i.test(message)) return "usage_limit_5h"
  return undefined
}

const providerLimit = (error) => {
  if (!error || typeof error !== "object" || Array.isArray(error)) return undefined
  if (error.statusCode === 429) return { kind: "rate_limit", status: 429 }
  if (error.statusCode === 529) return { kind: "capacity", status: 529 }
  return undefined
}

const sessionErrorProviderLimit = (error) => {
  if (!error || typeof error !== "object" || Array.isArray(error)) return undefined
  if (error.name !== "APIError" && error.name !== "UnknownError") return undefined

  const data = error.data
  if (!data || typeof data !== "object" || Array.isArray(data)) return undefined
  if (error.name === "APIError") {
    if (data.statusCode === 429) return { kind: "rate_limit", status: 429 }
    if (data.statusCode === 529) return { kind: "capacity", status: 529 }
    if (data.statusCode !== undefined) return undefined

    const metadata = data.metadata
    if (metadata && typeof metadata === "object" && !Array.isArray(metadata)) {
      for (const field of ["code", "type", "reason"]) {
        if (weeklyUsageCode(metadata[field])) {
          return { kind: "weekly_usage_limit", status: null }
        }
      }
    }
  }

  const phrase = usageLimitPhrase(data.message)
  if (phrase) return { kind: phrase, status: null }
  return undefined
}

const statusProviderLimit = (status) => {
  if (!status || typeof status !== "object" || Array.isArray(status) || status.type !== "retry") {
    return undefined
  }
  const action = status.action
  if (action && typeof action === "object" && !Array.isArray(action)) {
    if (action.reason === "account_rate_limit") return {
      kind: usageLimitPhrase(status.message) ?? "account_rate_limit",
      status: null,
    }
    if (action.reason === "free_tier_limit") return { kind: "free_tier_limit", status: null }
    if (action.reason !== undefined) return undefined
  }
  // Compatibility fallback for providers predating OpenCode Go structured actions.
  const phrase = usageLimitPhrase(status.message)
  if (phrase) return { kind: phrase, status: null }
  return undefined
}

const sdkSession = (response) => {
  if (!response || typeof response !== "object" || Array.isArray(response)) return undefined
  let data = response.data
  if (!data || typeof data !== "object" || Array.isArray(data)) return undefined
  if (Object.hasOwn(data, "data")) data = data.data
  return data && typeof data === "object" && !Array.isArray(data) ? data : undefined
}

const sdkPermissions = (response) => {
  if (!response || typeof response !== "object" || Array.isArray(response)) return undefined
  let data = response.data
  if (data && typeof data === "object" && !Array.isArray(data) && Object.hasOwn(data, "data")) data = data.data
  return Array.isArray(data) ? data : undefined
}

const sessionProjectIdentity = (session) => {
  for (const value of [
    session?.projectID,
    session?.projectId,
    session?.project_id,
    session?.project?.id,
  ]) {
    const identity = boundedString(value, 256)
    if (identity) return identity
  }
  return undefined
}

const sessionPathIdentity = (session) => {
  for (const value of [session?.worktree, session?.directory]) {
    const identity = boundedString(value)
    if (identity && !identity.includes("\0")) return path.resolve(identity)
  }
  return undefined
}

const sameAvailableIdentity = (child, parent, currentProject, currentPaths) => {
  const childProject = sessionProjectIdentity(child)
  const parentProject = sessionProjectIdentity(parent)
  if ((childProject || parentProject) && (!childProject || !parentProject || childProject !== parentProject)) return false
  if (childProject && currentProject && childProject !== currentProject) return false

  const childPath = sessionPathIdentity(child)
  const parentPath = sessionPathIdentity(parent)
  if ((childPath || parentPath) && (!childPath || !parentPath || childPath !== parentPath)) return false
  if (childPath && currentPaths.size > 0 && !currentPaths.has(childPath)) return false
  return true
}

const depthWithin = (value, remaining = MAX_DEPTH) => {
  if (remaining < 0) return false
  if (!value || typeof value !== "object") return true
  if (Array.isArray(value)) return value.every((item) => depthWithin(item, remaining - 1))
  return Object.values(value).every((item) => depthWithin(item, remaining - 1))
}

const safeEqual = (left, right) => {
  const leftDigest = createHash("sha256").update(String(left ?? "")).digest()
  const rightDigest = createHash("sha256").update(String(right ?? "")).digest()
  return timingSafeEqual(leftDigest, rightDigest) && typeof left === "string" && left === right
}

const directPermissionClaude = (command) =>
  typeof command === "string" &&
  /^claude -p(?:\s|$)/.test(command) &&
  /(?:^|\s)--permission-prompt-tool(?:=|\s+)mcp__rtrt__permission_prompt(?:\s|$)/.test(command)

const mappedPermission = async (body, projectRoot, pluginDirectory) => {
  const tool = boundedString(body.tool_name, 128)
  if (!tool) return undefined
  const input = body.input
  if (!input || typeof input !== "object" || Array.isArray(input) || !depthWithin(input)) return undefined

  const pick = (name) => boundedString(input[name])
  const mappedPath = async (resource, action, allowMissing = false) => {
    const classified = await canonicalProjectPath(
      projectRoot,
      pluginDirectory,
      resource,
      allowMissing,
    )
    return classified ? { action, resources: [classified] } : undefined
  }
  switch (tool) {
    case "Bash":
      return pick("command") && { action: "bash", resources: [pick("command")] }
    case "Read":
      return mappedPath(pick("file_path"), "read")
    case "Glob": {
      const pattern = pick("pattern")
      if (!pattern) return undefined
      return mappedPath(Object.hasOwn(input, "path") ? pick("path") : ".", "glob")
    }
    case "Grep": {
      const pattern = pick("pattern")
      return pattern && mappedPath(Object.hasOwn(input, "path") ? pick("path") : ".", "grep")
    }
    case "Edit":
    case "Write":
    case "MultiEdit":
      return mappedPath(pick("file_path"), "edit", true)
    case "NotebookEdit":
      return mappedPath(pick("notebook_path"), "edit", true)
    case "WebFetch":
      return pick("url") && { action: "webfetch", resources: [pick("url")] }
    case "WebSearch":
      return pick("query") && { action: "websearch", resources: [pick("query")] }
    default: {
      const action = tool.toLowerCase().replace(/[^a-z0-9_.-]/g, "-").slice(0, 64)
      return action && { action, resources: [`claude-tool:${action}`] }
    }
  }
}

const approvalDigest = ({ action, resources }) => createHash("sha256")
  .update(JSON.stringify([
    action.normalize("NFC"),
    resources.map((resource) => resource.normalize("NFC")),
  ]))
  .digest("base64url")

const listenLoopback = (server) =>
  new Promise((resolve, reject) => {
    server.once("error", reject)
    server.listen(0, "127.0.0.1", () => {
      server.off("error", reject)
      resolve()
    })
  })

const closeServer = (server) =>
  new Promise((resolve) => server.close(() => resolve()))

const createPlugin = async (
  { project, directory, serverUrl, client: legacyClient },
  {
    clientFactory = defaultClientFactory,
    bodyLimit = BODY_LIMIT,
    maxPending = MAX_PENDING,
    maxApprovalSessions = MAX_APPROVAL_SESSIONS,
    maxApprovalsPerSession = MAX_APPROVALS_PER_SESSION,
    maxTrackedChildren = MAX_TRACKED_CHILDREN,
    managedAgentStatePath = DEFAULT_MANAGED_AGENT_STATE_PATH,
  } = {},
) => {
  const agents = new Map()
  const invocations = new Map()
  const brokerInvocations = new Map()
  const permissionWaiters = new Map()
  const approvals = new Map()
  const trackedChildren = new Map()
  const cleanedChildren = new Set()
  const lifecycleOperations = new Map()
  const permissionEventReplies = new Set()
  const managedAgents = await loadManagedAgents(managedAgentStatePath)
  const parentWorktree = await resolveProjectWorktree(project?.worktree)
  const parentProject = parentWorktree ? path.basename(parentWorktree) : undefined
  const pluginDirectory = (
    typeof directory === "string" && directory && !directory.includes("\0")
      ? path.resolve(directory)
      : undefined
  )
  const projectRoot = parentWorktree ?? pluginDirectory
  const currentProjectIdentity = sessionProjectIdentity({
    projectID: project?.id ?? project?.projectID ?? project?.projectId ?? project?.project_id,
  })
  const currentPathIdentities = new Set([
    parentWorktree,
    pluginDirectory,
    boundedString(project?.worktree) && !project.worktree.includes("\0")
      ? path.resolve(project.worktree)
      : undefined,
  ].filter(Boolean))
  let approvalRoot
  try {
    const cwd = await realpath(path.resolve(directory ?? parentWorktree))
    const bounds = [...new Set([parentWorktree, cwd].filter(Boolean))]
    if (cwd !== path.parse(cwd).root && bounds.every((bound) => bound !== path.parse(bound).root)) {
      approvalRoot = { cwd, bounds }
    }
  } catch {}
  const v2Client = await clientFactory({
    baseUrl: serverUrl ? serverUrl.toString() : "http://127.0.0.1",
    directory,
  })
  const diagnostic = async (message, extra = {}) => {
    if (typeof v2Client.app?.log !== "function") return
    try {
      await v2Client.app.log({
        service: "rtrt-provenance",
        level: "debug",
        message,
        extra,
      })
    } catch {}
  }
  let pendingCount = 0
  let disposed = false

  const invocationFor = (callID) => {
    if (!callID) return randomUUID()
    let invocationID = invocations.get(callID)
    if (!invocationID) {
      invocationID = randomUUID()
      invocations.set(callID, invocationID)
    }
    return invocationID
  }

  const rememberAgent = (sessionID, agent) => {
    if (sessionID && agent) agents.set(sessionID, agent)
  }

  const confirmManagedSession = async (sessionID) => {
    if (
      !SAFE_SESSION_FIELD.test(sessionID ?? "") ||
      cleanedChildren.has(sessionID) ||
      typeof legacyClient?.session?.get !== "function"
    ) return
    try {
      const session = sdkSession(await legacyClient.session.get({ path: { id: sessionID } }))
      if (boundedString(session?.id, 256) !== sessionID) return
      if (session.agent === MANAGER_AGENT) return { manager: true, session }
      const parentID = boundedString(session.parentID, 256)
      if (!parentID || !SAFE_SESSION_FIELD.test(parentID)) return
      const parent = sdkSession(await legacyClient.session.get({ path: { id: parentID } }))
      if (boundedString(parent?.id, 256) !== parentID) return
      if (managedAgents) {
        if (!managedAgents.has(session.agent) || !sameAvailableIdentity(
          session,
          parent,
          currentProjectIdentity,
          currentPathIdentities,
        )) return
      } else if (parent?.agent !== MANAGER_AGENT) return
      return { manager: false, session, parentID }
    } catch {
      // Ownership is security-sensitive: SDK errors and malformed responses fail closed.
      return undefined
    }
  }

  const confirmPermissionRequest = async (requestID, sessionID, patterns) => {
    if (typeof v2Client.permission?.list !== "function") return false
    try {
      const permissions = sdkPermissions(await v2Client.permission.list())
      if (!permissions) return false
      return permissions.some((permission) =>
        permission &&
        typeof permission === "object" &&
        !Array.isArray(permission) &&
        permission.id === requestID &&
        permission.sessionID === sessionID &&
        permission.permission === "bash" &&
        Array.isArray(permission.patterns) &&
        permission.patterns.length === patterns.length &&
        permission.patterns.every((pattern, index) => pattern === patterns[index]))
    } catch {
      return false
    }
  }

  const rejectWaiters = (predicate) => {
    for (const waiter of permissionWaiters.values()) {
      if (!predicate(waiter)) continue
      waiter.resolve("reject")
    }
  }

  const hasApproval = (sessionID, digest) => approvals.get(sessionID)?.has(digest) === true

  const cacheApproval = (sessionID, digest) => {
    let sessionApprovals = approvals.get(sessionID)
    if (sessionApprovals?.has(digest)) return true
    if (!sessionApprovals) {
      if (approvals.size >= maxApprovalSessions) return false
      sessionApprovals = new Set()
    }
    if (sessionApprovals.size >= maxApprovalsPerSession) return false
    sessionApprovals.add(digest)
    approvals.set(sessionID, sessionApprovals)
    return true
  }

  const invalidate = (sessionID, callID) => {
    const key = `${sessionID ?? ""}\0${callID ?? ""}`
    const record = brokerInvocations.get(key)
    if (!record) return
    brokerInvocations.delete(key)
    record.valid = false
    rejectWaiters((waiter) => waiter.record === record)
  }

  const respond = (response, requestID, decision = "reject", status = 200) => {
    if (response.writableEnded || response.destroyed) return
    response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" })
    response.end(JSON.stringify({ version: 1, request_id: requestID ?? "", decision }))
  }

  const readBody = (request) =>
    new Promise((resolve, reject) => {
      let size = 0
      let oversized = false
      const chunks = []
      const cleanup = () => {
        request.off("data", onData)
        request.off("end", onEnd)
        request.off("error", onError)
      }
      const onData = (chunk) => {
        size += chunk.length
        if (size > bodyLimit) {
          oversized = true
          return
        }
        chunks.push(chunk)
      }
      const onEnd = () => {
        cleanup()
        if (oversized) reject(Object.assign(new Error("oversize"), { status: 413 }))
        else resolve(Buffer.concat(chunks).toString("utf8"))
      }
      const onError = (error) => {
        cleanup()
        reject(error)
      }
      request.on("data", onData)
      request.once("end", onEnd)
      request.once("error", onError)
    })

  const awaitReply = (record, sessionID, requestID, digest, request, response) =>
    new Promise((resolve) => {
      const key = `${sessionID}\0${requestID}`
      const finish = (decision) => {
        if (!permissionWaiters.has(key)) return
        permissionWaiters.delete(key)
        request.off("aborted", disconnect)
        response.off("close", disconnect)
        resolve(decision)
      }
      const disconnect = () => {
        if (response.writableFinished) return
        invalidate(record.sessionID, record.callID)
        finish("reject")
      }
      permissionWaiters.set(key, { record, resolve: finish, sessionID, requestID, digest })
      request.once("aborted", disconnect)
      response.once("close", disconnect)
      if (request.aborted || response.destroyed) disconnect()
    })

  const handlePermission = async (request, response) => {
    let requestID = ""
    let counted = false
    try {
      if (disposed || request.method !== "POST" || request.url !== BROKER_PATH) {
        respond(response, requestID, "reject", 404)
        return
      }
      if (pendingCount >= maxPending) {
        respond(response, requestID, "reject", 429)
        return
      }
      const contentLength = request.headers["content-length"]
      if (typeof contentLength !== "string" || !/^\d+$/.test(contentLength)) {
        respond(response, requestID, "reject", 411)
        return
      }
      const declared = Number(contentLength)
      if (!Number.isSafeInteger(declared) || declared > bodyLimit) {
        respond(response, requestID, "reject", 413)
        return
      }
      pendingCount += 1
      counted = true
      let body
      try {
        body = JSON.parse(await readBody(request))
      } catch (error) {
        if (!response.destroyed) respond(response, requestID, "reject", error?.status ?? 400)
        return
      }
      requestID = boundedString(body?.request_id, 256) ?? ""
      const allowedFields = new Set([
        "version",
        "request_id",
        "broker_nonce",
        "invocation_id",
        "parent_session_id",
        "parent_call_id",
        "child_session_id",
        "tool_use_id",
        "tool_name",
        "input",
      ])
      const required = [
        requestID,
        boundedString(body?.broker_nonce, 256),
        boundedString(body?.invocation_id, 256),
        boundedString(body?.parent_session_id, 256),
        boundedString(body?.parent_call_id, 256),
        boundedString(body?.tool_name, 128),
      ]
      if (
        body?.version !== 1 ||
        !body ||
        typeof body !== "object" ||
        Array.isArray(body) ||
        Object.keys(body).some((field) => !allowedFields.has(field)) ||
        required.some((value) => !value) ||
        !body.input ||
        typeof body.input !== "object" ||
        Array.isArray(body.input) ||
        !depthWithin(body) ||
        (body.child_session_id !== undefined && !boundedString(body.child_session_id, 256)) ||
        (body.tool_use_id !== undefined && !boundedString(body.tool_use_id, 256))
      ) {
        respond(response, requestID, "reject", 400)
        return
      }

      const key = `${body.parent_session_id}\0${body.parent_call_id}`
      const record = brokerInvocations.get(key)
      const authorization = request.headers.authorization ?? ""
      const suppliedToken = authorization.startsWith("Bearer ") ? authorization.slice(7) : ""
      const suppliedNonce = request.headers["x-rtrt-broker-nonce"]
      if (
        !record ||
        !record.valid ||
        !safeEqual(suppliedToken, record.token) ||
        !safeEqual(suppliedNonce, record.nonce) ||
        !safeEqual(body.broker_nonce, record.nonce) ||
        !safeEqual(body.invocation_id, record.invocationID) ||
        !safeEqual(body.parent_session_id, record.sessionID) ||
        !safeEqual(body.parent_call_id, record.callID)
      ) {
        respond(response, requestID, "reject", 401)
        return
      }

      const digest = createHash("sha256").update(JSON.stringify(body)).digest("base64url")
      const previous = record.replays.get(requestID)
      if (previous) {
        const identical = previous.digest === digest && previous.done
        respond(response, requestID, identical ? previous.decision : "reject", identical ? 200 : 409)
        return
      }
      if (record.replays.size >= MAX_REPLAYS) {
        respond(response, requestID, "reject", 429)
        return
      }
      record.replays.set(requestID, { digest, done: false, decision: "reject" })

      const mapped = await mappedPermission(body, projectRoot, pluginDirectory)
      if (!mapped) {
        record.replays.set(requestID, { digest, done: true, decision: "reject" })
        respond(response, requestID)
        return
      }
      const mappedDigest = approvalDigest(mapped)
      if (hasApproval(record.sessionID, mappedDigest)) {
        record.replays.set(requestID, { digest, done: true, decision: "always" })
        respond(response, requestID, "always")
        return
      }
      const permissionID = randomUUID()
      let result
      try {
        result = await v2Client.session.permission.create({
          sessionID: record.sessionID,
          id: permissionID,
          action: mapped.action,
          resources: mapped.resources,
          save: [],
          metadata: { source: "claude-cli", tool: body.tool_name },
          ...(record.agent ? { agent: record.agent } : {}),
        })
      } catch {
        result = { effect: "deny" }
      }
      const effect = result?.data?.data?.effect
      let decision = "reject"
      if (effect === "allow") decision = "once"
      else if (effect === "ask") {
        decision = await awaitReply(
          record,
          record.sessionID,
          permissionID,
          mappedDigest,
          request,
          response,
        )
      }
      record.replays.set(requestID, { digest, done: true, decision })
      respond(response, requestID, decision)
    } catch {
      respond(response, requestID, "reject", 500)
    } finally {
      if (counted) pendingCount -= 1
    }
  }

  const server = createServer((request, response) => void handlePermission(request, response))
  await listenLoopback(server)
  server.unref()
  const address = server.address()
  const brokerUrl = `http://127.0.0.1:${address.port}${BROKER_PATH}`

  const abortTracked = async (sessionID, tracked, limit) => {
    if (
      disposed ||
      tracked.aborted ||
      tracked.abortAttempted ||
      cleanedChildren.has(sessionID)
    ) return
    tracked.abortAttempted = true
    tracked.kind = limit.kind
    tracked.status = limit.status
    try {
      if (typeof legacyClient?.session?.abort !== "function") {
        await diagnostic("provider limit abort failed", { kind: limit.kind, reason: "unavailable" })
        return
      }
      const result = await legacyClient.session.abort({ path: { id: sessionID } })
      if (result?.error || result?.data !== true) {
        await diagnostic("provider limit abort rejected", { kind: limit.kind })
        return
      }
      tracked.aborted = true
      await diagnostic("provider limit abort accepted", { kind: limit.kind })
    } catch {
      await diagnostic("provider limit abort failed", { kind: limit.kind })
    }
  }

  const reserveChildCandidate = (sessionID, details = {}) => {
    if (
      disposed ||
      !SAFE_SESSION_FIELD.test(sessionID ?? "") ||
      cleanedChildren.has(sessionID)
    ) return undefined
    let tracked = trackedChildren.get(sessionID)
    if (!tracked) {
      if (trackedChildren.size >= Math.max(1, maxTrackedChildren)) return undefined
      tracked = { candidate: true, managed: undefined, aborted: false, ...details }
      trackedChildren.set(sessionID, tracked)
    }
    return tracked
  }

  const recoverAndAbort = async (sessionID, limit) => {
    if (disposed || cleanedChildren.has(sessionID)) return
    const tracked = reserveChildCandidate(sessionID)
    if (!tracked) return
    if (tracked.managed === undefined) {
      const ownership = await confirmManagedSession(sessionID)
      if (disposed || cleanedChildren.has(sessionID)) return
      if (!ownership || ownership.manager) {
        tracked.managed = false
        return
      }
      tracked.managed = true
      tracked.parentID = ownership.parentID
      tracked.agent = ownership.session.agent
    }
    if (tracked.managed) await abortTracked(sessionID, tracked, limit)
  }

  const serializeLifecycle = (sessionID, operation) => {
    const previous = lifecycleOperations.get(sessionID) ?? Promise.resolve()
    const current = previous.catch(() => {}).then(operation)
    lifecycleOperations.set(sessionID, current)
    return current.finally(() => {
      if (lifecycleOperations.get(sessionID) === current) lifecycleOperations.delete(sessionID)
    })
  }

  const tombstoneChild = (sessionID, confirmedChild = false) => {
    if (!sessionID || (!confirmedChild && !trackedChildren.has(sessionID))) return false
    trackedChildren.delete(sessionID)
    if (cleanedChildren.size >= Math.max(1, maxTrackedChildren)) {
      cleanedChildren.delete(cleanedChildren.values().next().value)
    }
    cleanedChildren.add(sessionID)
    return true
  }

  const dispose = async () => {
    if (disposed) return
    disposed = true
    for (const record of brokerInvocations.values()) record.valid = false
    brokerInvocations.clear()
    approvals.clear()
    trackedChildren.clear()
    lifecycleOperations.clear()
    cleanedChildren.clear()
    permissionEventReplies.clear()
    rejectWaiters(() => true)
    await closeServer(server)
  }

  return {
    "chat.message": async (input) => {
      rememberAgent(input.sessionID, input.agent)
    },
    "chat.params": async (input) => {
      rememberAgent(input.sessionID, input.agent)
    },
    "permission.ask": async (input, output) => {
      const permission = input?.type ?? input?.permission
      if (permission !== "bash" && permission !== "Bash") return
      if (output?.status !== "ask") return
      const sessionID = boundedString(input?.sessionID, 256)
      if (!sessionID || !SAFE_SESSION_FIELD.test(sessionID) || !(await confirmManagedSession(sessionID))) return
      const patterns = Array.isArray(input?.pattern) ? input.pattern : [input?.pattern]
      if (!patterns.length || patterns.some((pattern) => typeof pattern !== "string")) return
      for (const pattern of patterns) if (!(await safeCommand(pattern, approvalRoot))) return
      output.status = "allow"
    },
    "tool.execute.before": async (input, output) => {
      const invocationID = invocationFor(input.callID)
      const command = output?.args?.command
      if ((input.tool === "Bash" || input.tool === "bash") && directPermissionClaude(command)) {
        const key = `${input.sessionID ?? ""}\0${input.callID ?? ""}`
        invalidate(input.sessionID, input.callID)
        brokerInvocations.set(key, {
          token: randomBytes(32).toString("base64url"),
          nonce: randomBytes(32).toString("base64url"),
          invocationID,
          sessionID: String(input.sessionID ?? ""),
          callID: String(input.callID ?? ""),
          agent: agents.get(input.sessionID),
          replays: new Map(),
          valid: true,
        })
      }
      if (!RTRT_AGENT_TOOLS.has(input.tool)) return

      output.args.invocation_id = invocationID
      output.args.parent_project = parentProject
      output.args.parent_session_id = input.sessionID
      output.args.parent_call_id = input.callID
      output.args.caller_agent = agents.get(input.sessionID)
      output.args.parent_cwd = directory
      output.args.parent_worktree = parentWorktree
    },
    "shell.env": async (input, output) => {
      if (parentWorktree) {
        const sessionTempDir = await ensureSessionTempDir(parentWorktree, input.sessionID)
        output.env.TMPDIR = sessionTempDir
        output.env.TEMP = sessionTempDir
        output.env.TMP = sessionTempDir
      }
      output.env.RTRT_INVOCATION_ID = invocationFor(input.callID)
      output.env.RTRT_OPENCODE_PLUGIN_ACTIVE = "1"
      output.env.RTRT_PARENT_PROJECT = parentProject
      output.env.RTRT_PARENT_CWD = input.cwd
      output.env.RTRT_PARENT_WORKTREE = parentWorktree
      if (input.sessionID) {
        output.env.RTRT_PARENT_SESSION_ID = input.sessionID
        const agent = agents.get(input.sessionID)
        if (agent) output.env.RTRT_PARENT_AGENT = agent
      }
      if (input.callID) output.env.RTRT_PARENT_CALL_ID = input.callID
      const record = brokerInvocations.get(`${input.sessionID ?? ""}\0${input.callID ?? ""}`)
      if (record?.valid) {
        output.env.RTRT_PERMISSION_BROKER_URL = brokerUrl
        output.env.RTRT_PERMISSION_BROKER_TOKEN = record.token
        output.env.RTRT_PERMISSION_BROKER_NONCE = record.nonce
      }
    },
    "tool.execute.after": async (input) => {
      invalidate(input.sessionID, input.callID)
      invocations.delete(input.callID)
    },
    event: async (input) => {
      const event = input?.event ?? input
      const properties = event?.properties ?? {}
      if (event?.type === "session.created") {
        const sessionID = boundedString(properties.info?.id, 256)
        const parentID = boundedString(properties.info?.parentID, 256)
        const agent = boundedString(properties.info?.agent, 256)
        if (
          sessionID &&
          parentID &&
          SAFE_SESSION_FIELD.test(sessionID) &&
          SAFE_SESSION_FIELD.test(parentID) &&
          !cleanedChildren.has(sessionID) &&
          (!trackedChildren.has(sessionID) || trackedChildren.get(sessionID).managed === undefined) &&
          (trackedChildren.has(sessionID) || trackedChildren.size < maxTrackedChildren)
        ) {
          // Reserve synchronously: OpenCode does not await plugin event callbacks.
          const tracked = reserveChildCandidate(sessionID, { parentID, agent })
          if (!tracked) return
          await serializeLifecycle(sessionID, async () => {
            const ownership = await confirmManagedSession(sessionID)
            if (disposed || cleanedChildren.has(sessionID)) return
            tracked.managed = Boolean(
              ownership &&
              !ownership.manager &&
              ownership.parentID === parentID &&
              (!agent || ownership.session.agent === agent)
            )
            if (tracked.managed) tracked.agent = ownership.session.agent
          })
        }
      } else if (event?.type === "permission.asked") {
        const requestID = boundedString(properties.id, 256)
        const sessionID = boundedString(properties.sessionID, 256)
        const patterns = properties.patterns
        if (
          properties.permission !== "bash" ||
          !requestID ||
          !sessionID ||
          !SAFE_SESSION_FIELD.test(requestID) ||
          !SAFE_SESSION_FIELD.test(sessionID) ||
          !Array.isArray(patterns) ||
          patterns.length === 0 ||
          patterns.length > MAX_PERMISSION_EVENT_REPLIES ||
          patterns.some((pattern) => typeof pattern !== "string") ||
          !approvalRoot ||
          permissionEventReplies.size >= MAX_PERMISSION_EVENT_REPLIES
        ) return
        const key = `${sessionID}\0${requestID}`
        if (permissionEventReplies.has(key)) return
        // Reserve before asynchronous classification so duplicate events cannot race a reply.
        permissionEventReplies.add(key)
        const ownership = await confirmManagedSession(sessionID)
        if (!ownership || !(await confirmPermissionRequest(requestID, sessionID, patterns))) return
        for (const pattern of patterns) {
          if (!(await safeCommand(pattern, approvalRoot))) return
        }
        if (disposed) return
        try {
          await v2Client.permission.reply({ requestID, reply: "once" })
        } catch {
          // SDK/network failures fail closed; retained reservation prevents uncertain retries.
        }
      } else if (event?.type === "session.error") {
        const sessionID = boundedString(properties.sessionID, 256)
        const limit = sessionErrorProviderLimit(properties.error)
        if (sessionID && limit && reserveChildCandidate(sessionID)) {
          await serializeLifecycle(sessionID, () => recoverAndAbort(sessionID, limit))
        }
      } else if (event?.type === "session.next.retried" || event?.type === "session.status") {
        const sessionID = boundedString(properties.sessionID ?? properties.session_id, 256)
        const limit = event.type === "session.next.retried"
          ? providerLimit(properties.error)
          : statusProviderLimit(properties.status)
        if (sessionID && (!limit || reserveChildCandidate(sessionID))) {
          await serializeLifecycle(
            sessionID,
            () => limit ? recoverAndAbort(sessionID, limit) : undefined,
          )
        }
      } else if (event?.type === "permission.v2.replied") {
        const sessionID = properties.sessionID ?? properties.session_id
        const requestID = properties.requestID ?? properties.request_id ?? properties.id
        const key = `${sessionID}\0${requestID}`
        const waiter = permissionWaiters.get(key)
        if (waiter) {
          const reply = properties.reply ?? properties.response ?? properties.decision
          if (reply === "always" || reply === "allow_always") {
            waiter.resolve(
              waiter.record.valid && cacheApproval(waiter.sessionID, waiter.digest)
                ? "always"
                : "reject",
            )
          } else waiter.resolve(reply === "once" || reply === "allow_once" ? "once" : "reject")
        }
      } else if (event?.type === "session.idle" || event?.type === "session.deleted") {
        const sessionID = properties.sessionID ?? properties.session_id ?? properties.info?.id
        // Only known child candidates become terminal tombstones. Parent idle is ordinary.
        const deletedChild = event.type === "session.deleted" &&
          SAFE_SESSION_FIELD.test(properties.info?.parentID ?? "")
        tombstoneChild(sessionID, deletedChild)
        if (sessionID) void serializeLifecycle(sessionID, async () => {})
        if (event.type !== "session.deleted") return
        for (const key of permissionEventReplies) {
          if (key.startsWith(`${sessionID}\0`)) permissionEventReplies.delete(key)
        }
        approvals.delete(sessionID)
        for (const record of [...brokerInvocations.values()]) {
          if (record.sessionID === sessionID) invalidate(record.sessionID, record.callID)
        }
      }
    },
    dispose,
  }
}

export const RtrtProvenance = (input) => createPlugin(input)

export const __createRtrtProvenanceForTest = (input, options) => createPlugin(input, options)
// END rtrt-managed provenance plugin
