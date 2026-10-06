// GitHub release boundary for scripts/release-assets.mjs.
//
// Every remote read/write goes through the authenticated `gh` CLI. The adapter
// pins a single numeric release id for all rerreads and mutations, lists every
// release page (drafts included) instead of the published-only by-tag endpoint,
// streams asset bytes by asset id, and uploads to the fixed uploads.github.com
// URL. No response-provided URL is ever followed.
import { execFile, spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { basename, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'

const exec = promisify(execFile)
const repo = 'kernalix7/rtrt'
const timeout = 120_000
const maxBytes = 512 * 1024 * 1024
const maxBuffer = 4 * 1024 * 1024

function toText(value) {
  if (value == null) return ''
  return Buffer.isBuffer(value) ? value.toString() : String(value)
}

function parseStatus(text) {
  return /^HTTP\/\S+\s+(\d{3})/m.exec(text)?.[1]
}

function parseBody(text) {
  const separator = /\r?\n\r?\n/.exec(text)
  if (!separator) throw new Error('GitHub API response has no JSON body')
  const body = text.slice(separator.index + separator[0].length).trim()
  if (!body) return null
  return JSON.parse(body)
}

async function runGh(args) {
  try {
    return await exec('gh', args, { timeout, maxBuffer })
  } catch (error) {
    return error
  }
}

async function apiGet(endpoint) {
  const result = await runGh(['api', '--include', endpoint])
  const text = toText(result.stdout)
  const status = parseStatus(text)
  if (result instanceof Error || status !== '200') {
    throw new Error(`GitHub API read failed: ${endpoint} (${status ?? result.code ?? 'unknown'})`)
  }
  return parseBody(text)
}

async function apiWrite(method, endpoint, { input, expect = [200], headers = [] } = {}) {
  const args = ['api', '--include', '--method', method, ...headers]
  if (input) args.push('--input', input)
  args.push(endpoint)
  const result = await runGh(args)
  const text = toText(result.stdout)
  const status = parseStatus(text)
  // A nonzero gh process exit is a failure even when it printed a 2xx status.
  if (result instanceof Error || !expect.includes(Number(status))) {
    throw new Error(`GitHub API write failed: ${method} ${endpoint} (${status ?? result.code ?? 'unknown'})`)
  }
  return parseBody(text)
}

const payloadRoot = fileURLToPath(new URL('../.rtrt/tmp/', import.meta.url))

async function withJsonInput(body, run) {
  await mkdir(payloadRoot, { recursive: true })
  const dir = await mkdtemp(join(payloadRoot, 'rtrt-release-'))
  const input = join(dir, 'payload.json')
  try {
    await writeFile(input, JSON.stringify(body), { mode: 0o600 })
    return await run(input)
  } finally {
    await rm(dir, { recursive: true, force: true })
  }
}

async function tagCommit(tag) {
  let ref = await apiGet(`repos/${repo}/git/ref/tags/${tag}`)
  for (let depth = 0; depth < 4 && ref.object?.type === 'tag'; depth++) {
    ref = await apiGet(`repos/${repo}/git/tags/${ref.object.sha}`)
  }
  if (ref.object?.type !== 'commit') throw new Error(`invalid remote tag target: ${tag}`)
  return ref.object.sha
}

async function listReleases() {
  const releases = []
  for (let page = 1; ; page++) {
    const batch = await apiGet(`repos/${repo}/releases?per_page=100&page=${page}`)
    if (!Array.isArray(batch)) throw new Error('GitHub release list is malformed')
    releases.push(...batch)
    if (batch.length < 100) return releases
  }
}

function getRelease(id) {
  return apiGet(`repos/${repo}/releases/${id}`)
}

function downloadAsset(assetId) {
  const endpoint = `repos/${repo}/releases/assets/${assetId}`
  const child = spawn('gh', ['api', '-H', 'Accept: application/octet-stream', endpoint], {
    stdio: ['ignore', 'pipe', 'ignore'],
  })
  const exit = new Promise((resolve) => {
    let spawnError
    child.once('error', (error) => { spawnError = error })
    child.once('close', (code) => resolve({ code, spawnError }))
  })
  let timedOut = false
  const timer = setTimeout(() => {
    timedOut = true
    child.kill('SIGKILL')
  }, timeout)
  const hash = createHash('sha256')
  let size = 0
  return (async () => {
    try {
      for await (const chunk of child.stdout) {
        size += chunk.length
        if (size > maxBytes) {
          child.kill('SIGKILL')
          break
        }
        hash.update(chunk)
      }
      const { code, spawnError } = await exit
      if (spawnError || timedOut || code !== 0 || size > maxBytes) {
        throw new Error(`GitHub asset download failed: ${assetId}${timedOut ? ' (timeout)' : ''}`, { cause: spawnError })
      }
      return { size, digest: `sha256:${hash.digest('hex')}` }
    } finally {
      clearTimeout(timer)
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
      await exit
    }
  })()
}

async function createDraft({ tag, sourceSha, title, notesFile }) {
  const notes = await readFile(notesFile, 'utf8')
  return withJsonInput({
    tag_name: tag,
    target_commitish: sourceSha,
    name: title,
    body: notes,
    draft: true,
    prerelease: false,
  }, (input) => apiWrite('POST', `repos/${repo}/releases`, { input, expect: [201] }))
}

function uploadAsset(releaseId, filePath) {
  const name = basename(filePath)
  const endpoint = `https://uploads.github.com/repos/${repo}/releases/${releaseId}/assets?name=${encodeURIComponent(name)}`
  return apiWrite('POST', endpoint, {
    input: filePath,
    expect: [201],
    headers: ['-H', 'Content-Type: application/octet-stream'],
  })
}

function publishRelease(id) {
  return withJsonInput({ draft: false }, (input) =>
    apiWrite('PATCH', `repos/${repo}/releases/${id}`, { input, expect: [200] }))
}

export function createGithubAdapter() {
  return {
    tagCommit,
    listReleases,
    getRelease,
    download: downloadAsset,
    createDraft,
    uploadAsset,
    publishRelease,
  }
}
