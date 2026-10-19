import { randomUUID } from "node:crypto"
import { spawn } from "node:child_process"

const MAX_IO = 256 * 1024
const DEADLINE_MS = 1500

const invocationKey = (sessionId, toolCallId) => `${sessionId}\0${toolCallId}`

const singleText = (content) => {
  if (!Array.isArray(content) || content.length !== 1) return undefined
  const block = content[0]
  if (!block || block.type !== "text" || typeof block.text !== "string") return undefined
  return block.text
}

export const isEligibleBashToolResult = (event) => {
  if (event?.toolName !== "bash" || event.isError) return false
  const input = event.input
  if (!input || typeof input !== "object" || Array.isArray(input) || input.run_in_background) return false
  const command = input.command
  if (typeof command !== "string" || !command.trim() || Buffer.byteLength(command) > MAX_IO) return false
  // Prefix-based CLI filters cannot safely classify combined shell output.
  if (/[;&|<>`$\\\r\n(){}]/u.test(command)) return false
  return singleText(event.content) !== undefined
}

export async function filterCommandOutput({ binary, command, text, cwd, signal }, spawnImpl = spawn) {
  if (typeof binary !== "string" || !binary || typeof command !== "string" || !command.trim()) return undefined
  if (Buffer.byteLength(command) > MAX_IO) return undefined
  const input = typeof text === "string" ? text : ""
  if (Buffer.byteLength(input) > MAX_IO) return undefined
  if (signal?.aborted) return undefined

  return await new Promise((resolve) => {
    let settled = false
    const chunks = []
    let stdoutBytes = 0
    let child
    const finish = (value) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      signal?.removeEventListener("abort", onAbort)
      if (child && child.exitCode === null) child.kill("SIGKILL")
      resolve(value)
    }
    const onAbort = () => finish(undefined)
    signal?.addEventListener("abort", onAbort, { once: true })
    const timer = setTimeout(() => finish(undefined), DEADLINE_MS)
    try {
      child = spawnImpl(binary, ["proxy", "--", command], {
        cwd, shell: false, stdio: ["pipe", "pipe", "ignore"],
      })
    } catch (error) {
      if (!(error instanceof Error)) throw error
      finish(undefined)
      return
    }
    child.on("error", () => finish(undefined))
    child.stdout.on("data", (chunk) => {
      stdoutBytes += chunk.length
      if (stdoutBytes > MAX_IO) finish(undefined)
      else chunks.push(chunk)
    })
    child.on("close", (code) => {
      if (signal?.aborted || code !== 0) finish(undefined)
      else finish(Buffer.concat(chunks).toString("utf8"))
    })
    child.stdin.on("error", () => finish(undefined))
    child.stdin.end(input)
  })
}

export function createRtrtExtension({ binary, mcpBinary, toolPrefix = "mcp" }) {
  const agentTools = new Set([
    `${toolPrefix}_rtrt_agent_call`,
    `${toolPrefix}_rtrt_agent_route`,
  ])

  return (pi) => {
    if (mcpBinary) {
      pi.registerMcpServer("rtrt", { type: "stdio", command: mcpBinary, args: [] })
    }
    const invocations = new Map()
    const pending = new Set()

    const resetSession = () => {
      for (const controller of pending) controller.abort()
      pending.clear()
      invocations.clear()
    }

    pi.on("session_start", resetSession)
    pi.on("session_shutdown", resetSession)

    pi.on("tool_call", async (event, ctx) => {
      if (!agentTools.has(event.toolName)) return
      const sessionId = ctx.sessionManager.getSessionId()
      const key = invocationKey(sessionId, event.toolCallId)
      let invocationId = invocations.get(key)
      if (!invocationId) {
        invocationId = randomUUID()
        invocations.set(key, invocationId)
      }
      event.input.invocation_id = invocationId
      event.input.parent_session_id = sessionId
      event.input.parent_call_id = event.toolCallId
      event.input.caller_agent = "omo-native"
      event.input.parent_cwd = ctx.cwd
    })

    pi.on("tool_result", async (event, ctx) => {
      const sessionId = ctx.sessionManager.getSessionId()
      invocations.delete(invocationKey(sessionId, event.toolCallId))
      if (!isEligibleBashToolResult(event)) return

      const original = singleText(event.content)
      const command = event.input.command
      const controller = new AbortController()
      pending.add(controller)
      const onAbort = () => controller.abort()
      ctx.signal?.addEventListener("abort", onAbort, { once: true })
      if (ctx.signal?.aborted) controller.abort()
      let filtered
      try {
        filtered = await filterCommandOutput({
          binary, command, text: original, cwd: ctx.cwd, signal: controller.signal,
        })
      } finally {
        ctx.signal?.removeEventListener("abort", onAbort)
        pending.delete(controller)
      }
      if (filtered === undefined || filtered === original) return
      return { content: [{ ...event.content[0], text: filtered }] }
    })
  }
}

export default createRtrtExtension({ binary: process.env.RTRT_BIN || "rtrt" })
