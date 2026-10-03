import { createHash } from 'node:crypto'
import { execFile, spawn } from 'node:child_process'
import { createReadStream } from 'node:fs'
import { lstat, readdir } from 'node:fs/promises'
import { basename, join } from 'node:path'
import { promisify } from 'node:util'
import { pathToFileURL } from 'node:url'

const exec = promisify(execFile)
const repo = 'kernalix7/rtrt'
const targets = [
  'x86_64-unknown-linux-gnu.tar.gz',
  'aarch64-unknown-linux-gnu.tar.gz',
  'x86_64-apple-darwin.tar.gz',
  'aarch64-apple-darwin.tar.gz',
  'x86_64-pc-windows-msvc.zip',
]
const timeout = 120_000

export function expectedNames(version) {
  return targets.flatMap((target) => {
    const archive = `rtrt-${version}-${target}`
    return [archive, `${archive}.sha256`]
  })
}

async function fingerprint(file) {
  const hash = createHash('sha256')
  let size = 0
  for await (const chunk of createReadStream(file)) {
    size += chunk.length
    hash.update(chunk)
  }
  return { size, digest: `sha256:${hash.digest('hex')}` }
}

async function gh(...args) {
  try {
    return (await exec('gh', args, { timeout, maxBuffer: 1024 * 1024 })).stdout
  } catch (error) {
    throw new Error(`GitHub CLI request failed: ${args.slice(0, 3).join(' ')} (${error.code ?? 'unknown'})`)
  }
}

async function apiGet(endpoint, allowMissing = false) {
  // --include preserves HTTP status even when gh exits nonzero for a 404.
  let output
  try {
    output = await exec('gh', ['api', '--include', endpoint], { timeout, maxBuffer: 1024 * 1024 })
  } catch (error) {
    output = error
  }
  const text = output.stdout?.toString() ?? ''
  const status = /^HTTP\/\S+\s+(\d{3})/m.exec(text)?.[1]
  if (status === '404' && allowMissing) return null
  if (status !== '200' || output.code) throw new Error(`GitHub API read failed (${status ?? output.code ?? 'unknown'})`)
  const separator = /\r?\n\r?\n/.exec(text)
  if (!separator) throw new Error('GitHub API response has no JSON body')
  return JSON.parse(text.slice(separator.index + separator[0].length))
}

async function downloadAsset(tag, name) {
  // gh streams exactly the named asset to stdout; no remote metadata URL is used.
  const child = spawn('gh', ['release', 'download', tag, '--repo', repo, '--pattern', name, '--output', '-'], {
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
  try {
    for await (const chunk of child.stdout) {
      size += chunk.length
      if (size > 512 * 1024 * 1024) {
        child.kill('SIGKILL')
        break
      }
      hash.update(chunk)
    }
    const { code, spawnError } = await exit
    if (spawnError || timedOut || code !== 0 || size > 512 * 1024 * 1024) {
      throw new Error(`GitHub asset download failed: ${name}${timedOut ? ' (timeout)' : ''}`, { cause: spawnError })
    }
    return { size, digest: `sha256:${hash.digest('hex')}` }
  } finally {
    clearTimeout(timer)
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
    await exit
  }
}

const github = {
  async tagCommit(tag) {
    let ref = await apiGet(`repos/${repo}/git/ref/tags/${tag}`)
    for (let depth = 0; depth < 4 && ref.object?.type === 'tag'; depth++) {
      ref = await apiGet(`repos/${repo}/git/tags/${ref.object.sha}`)
    }
    if (ref.object?.type !== 'commit') throw new Error(`invalid remote tag target: ${tag}`)
    return ref.object.sha
  },
  release: (tag) => apiGet(`repos/${repo}/releases/tags/${tag}`, true),
  download: downloadAsset,
  create: (tag, notes, paths) => gh('release', 'create', tag, ...paths, '--repo', repo, '--verify-tag', '--title', tag, '--notes-file', notes, '--draft'),
  upload: (tag, paths) => gh('release', 'upload', tag, ...paths, '--repo', repo),
  publishDraft: (tag) => gh('release', 'edit', tag, '--repo', repo, '--draft=false'),
}

async function readRelease(adapter, plan, local) {
  const tag = `v${plan.version}`
  const expected = expectedNames(plan.version)
  const release = await adapter.release(tag)
  if (release === null) return { release, missing: expected }
  if (!release || release.tag_name !== tag || typeof release.draft !== 'boolean'
    || typeof release.prerelease !== 'boolean' || typeof release.target_commitish !== 'string'
    || !release.target_commitish || !Array.isArray(release.assets)) {
    throw new Error('invalid GitHub release metadata')
  }
  if (release.prerelease) throw new Error('GitHub prerelease conflicts with stable publication')
  if (/^[0-9a-f]{40}$/i.test(release.target_commitish)
    && release.target_commitish.toLowerCase() !== plan.sourceSha) {
    throw new Error('GitHub release target conflict')
  }
  const remoteNames = new Set()
  for (const asset of release.assets) {
    if (!asset || !expected.includes(asset.name) || remoteNames.has(asset.name)
      || !Number.isSafeInteger(asset.size) || asset.size < 0
      || (asset.digest != null && !/^sha256:[0-9a-f]{64}$/.test(asset.digest))) {
      throw new Error(`unexpected or duplicate GitHub asset: ${asset?.name}`)
    }
    remoteNames.add(asset.name)
    const downloaded = await adapter.download(tag, asset.name)
    const wanted = local.get(asset.name)
    if (asset.size !== wanted.size || downloaded.size !== wanted.size || downloaded.digest !== wanted.digest
      || (asset.digest && asset.digest !== downloaded.digest)) {
      throw new Error(`GitHub asset conflict: ${asset.name}`)
    }
  }
  return { release, missing: expected.filter((name) => !remoteNames.has(name)) }
}

export async function reconcile(adapter, plan, mode) {
  const { assetDir, version, sourceSha, notes } = plan
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)
    || !/^[0-9a-f]{40}$/.test(sourceSha) || !['check', 'publish'].includes(mode)
    || typeof assetDir !== 'string' || !assetDir || (mode === 'publish' && !notes)) {
    throw new Error('invalid release arguments')
  }
  const tag = `v${version}`
  const expected = expectedNames(version)
  const present = await readdir(assetDir)
  if (present.length !== expected.length || present.some((name) => !expected.includes(name))) {
    throw new Error('local release asset inventory is incomplete or unexpected')
  }
  const local = new Map()
  for (const name of expected) {
    const path = join(assetDir, name)
    if (!(await lstat(path)).isFile() || basename(path) !== name) throw new Error(`invalid asset: ${name}`)
    local.set(name, await fingerprint(path))
  }
  for (const paired of [tag, `REL-${tag}`]) {
    if (await adapter.tagCommit(paired) !== sourceSha) throw new Error(`remote paired tag conflict: ${paired}`)
  }
  const { release, missing } = await readRelease(adapter, plan, local)
  if (release && !release.draft && missing.length) throw new Error('published GitHub release is incomplete')
  if (mode === 'check') return missing
  if (!release) await adapter.create(tag, notes, expected.map((name) => join(assetDir, name)))
  else if (missing.length) await adapter.upload(tag, missing.map((name) => join(assetDir, name)))
  if (!release || release.draft) {
    const staged = await readRelease(adapter, plan, local)
    if (!staged.release?.draft || staged.missing.length) {
      throw new Error('draft release asset postcondition incomplete')
    }
    await adapter.publishDraft(tag)
    const published = await readRelease(adapter, plan, local)
    if (!published.release || published.release.draft || published.missing.length) {
      throw new Error('published release postcondition failed: draft or incomplete assets')
    }
  }
  return missing
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    if (process.argv.length < 6 || process.argv.length > 7) {
      throw new Error('usage: release-assets.mjs <check|publish> <version> <source-sha> <asset-directory> [notes-file]')
    }
    const [mode, version, sha, directory, notes] = process.argv.slice(2)
    if (mode === 'publish' && !notes) throw new Error('release notes file required')
    const missing = await reconcile(github, { version, sourceSha: sha, assetDir: directory, notes }, mode)
    console.log(`GitHub asset ${mode}: ${missing.length} missing; existing bytes verified`)
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exitCode = 1
  }
}
