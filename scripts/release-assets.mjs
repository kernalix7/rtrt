// Draft-aware GitHub release reconciliation.
//
// The release id discovered by scanning every release page (drafts included) is
// pinned: every subsequent reread, upload, and PATCH targets that same id. The
// read-only check never mutates. Existing bytes are re-downloaded by asset id
// and compared before any write, and publication only happens after a complete
// draft is re-verified.
import { createHash } from 'node:crypto'
import { createReadStream } from 'node:fs'
import { lstat, readdir } from 'node:fs/promises'
import { basename, join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { createGithubAdapter } from './release-github.mjs'

const targets = [
  'x86_64-unknown-linux-gnu.tar.gz',
  'aarch64-unknown-linux-gnu.tar.gz',
  'x86_64-apple-darwin.tar.gz',
  'aarch64-apple-darwin.tar.gz',
  'x86_64-pc-windows-msvc.zip',
]
const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/
const shaPattern = /^[0-9a-f]{40}$/
const digestPattern = /^sha256:[0-9a-f]{64}$/

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

async function readLocalAssets(assetDir, expected) {
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
  return local
}

function selectExactRelease(releases, tag) {
  if (!Array.isArray(releases)) throw new Error('GitHub release list is malformed')
  const seenIds = new Set()
  let match = null
  for (const release of releases) {
    if (!release || typeof release !== 'object'
      || !Number.isSafeInteger(release.id) || release.id <= 0
      || typeof release.tag_name !== 'string' || !release.tag_name) {
      throw new Error('GitHub release list is malformed')
    }
    if (seenIds.has(release.id)) throw new Error(`duplicate GitHub release id: ${release.id}`)
    seenIds.add(release.id)
    if (release.tag_name !== tag) continue
    if (match) throw new Error(`duplicate GitHub release for exact tag: ${tag}`)
    match = release
  }
  return match
}

function validateRelease(release, { tag, sourceSha, requireDraft = false }) {
  if (!release || typeof release !== 'object') throw new Error('invalid GitHub release metadata')
  if (!Number.isSafeInteger(release.id) || release.id <= 0) throw new Error('invalid GitHub release id')
  if (release.tag_name !== tag) throw new Error(`GitHub release tag conflict: ${release.tag_name}`)
  if (typeof release.draft !== 'boolean' || typeof release.prerelease !== 'boolean'
    || typeof release.target_commitish !== 'string' || !release.target_commitish
    || !Array.isArray(release.assets)) {
    throw new Error('invalid GitHub release metadata')
  }
  if (/^[0-9a-f]{40}$/i.test(release.target_commitish) && release.target_commitish.toLowerCase() !== sourceSha) {
    throw new Error('GitHub release target conflict')
  }
  if (release.prerelease) throw new Error('GitHub prerelease conflicts with stable publication')
  if (requireDraft && !release.draft) throw new Error('created GitHub release is not a draft')
}

function validateAsset(asset, expected, seenNames) {
  if (!asset || typeof asset !== 'object') throw new Error('unexpected GitHub asset')
  if (!Number.isSafeInteger(asset.id) || asset.id <= 0) throw new Error(`GitHub asset id is invalid: ${asset.name}`)
  if (typeof asset.name !== 'string' || !expected.includes(asset.name) || seenNames.has(asset.name)) {
    throw new Error(`unexpected or duplicate GitHub asset: ${asset.name}`)
  }
  if (!Number.isSafeInteger(asset.size) || asset.size < 0) throw new Error(`GitHub asset size is invalid: ${asset.name}`)
  if (asset.digest != null && !digestPattern.test(asset.digest)) throw new Error(`GitHub asset digest is invalid: ${asset.name}`)
  if (asset.state !== 'uploaded') throw new Error(`GitHub asset state is not uploaded: ${asset.name}`)
  if (typeof asset.content_type !== 'string' || !asset.content_type) throw new Error(`GitHub asset type is invalid: ${asset.name}`)
  seenNames.add(asset.name)
}

async function verifyRelease(adapter, release, { plan, local, expectedId }) {
  const tag = `v${plan.version}`
  validateRelease(release, { tag, sourceSha: plan.sourceSha })
  if (release.id !== expectedId) {
    throw new Error(`GitHub release id changed during reread: expected ${expectedId}`)
  }
  const expected = expectedNames(plan.version)
  const seenNames = new Set()
  for (const asset of release.assets) {
    validateAsset(asset, expected, seenNames)
    const downloaded = await adapter.download(asset.id)
    const wanted = local.get(asset.name)
    if (asset.size !== wanted.size || downloaded.size !== wanted.size || downloaded.digest !== wanted.digest
      || (asset.digest && asset.digest !== downloaded.digest)) {
      throw new Error(`GitHub asset conflict: ${asset.name}`)
    }
  }
  return expected.filter((name) => !seenNames.has(name))
}

export async function reconcile(adapter, plan, mode) {
  const { assetDir, version, sourceSha, notes } = plan
  if (!versionPattern.test(version) || !shaPattern.test(sourceSha)
    || !['check', 'publish'].includes(mode)
    || typeof assetDir !== 'string' || !assetDir || (mode === 'publish' && !notes)) {
    throw new Error('invalid release arguments')
  }
  const tag = `v${version}`
  const expected = expectedNames(version)
  const local = await readLocalAssets(assetDir, expected)
  for (const paired of [tag, `REL-${tag}`]) {
    if (await adapter.tagCommit(paired) !== sourceSha) throw new Error(`remote paired tag conflict: ${paired}`)
  }
  const listed = selectExactRelease(await adapter.listReleases(), tag)
  let release = null
  let missing = expected
  if (listed) {
    release = await adapter.getRelease(listed.id)
    missing = await verifyRelease(adapter, release, { plan, local, expectedId: listed.id })
    if (!release.draft && missing.length) throw new Error('published GitHub release is incomplete')
  }
  if (mode === 'check') return missing
  let releaseId = release?.id
  if (!release) {
    const created = await adapter.createDraft({ tag, sourceSha, title: tag, notesFile: notes })
    validateRelease(created, { tag, sourceSha, requireDraft: true })
    releaseId = created.id
  }
  if (missing.length) {
    for (const name of missing) await adapter.uploadAsset(releaseId, join(assetDir, name))
  }
  if (!release || release.draft) {
    const staged = await adapter.getRelease(releaseId)
    const stagedMissing = await verifyRelease(adapter, staged, { plan, local, expectedId: releaseId })
    if (!staged.draft || stagedMissing.length) throw new Error('draft release asset postcondition incomplete')
    const recheck = selectExactRelease(await adapter.listReleases(), tag)
    if (!recheck || recheck.id !== releaseId) throw new Error('exact release tag changed before publication')
    await adapter.publishRelease(releaseId)
    const published = await adapter.getRelease(releaseId)
    const publishedMissing = await verifyRelease(adapter, published, { plan, local, expectedId: releaseId })
    if (published.draft || published.prerelease) {
      throw new Error('published release postcondition failed: still draft or prerelease')
    }
    if (publishedMissing.length) throw new Error('published release postcondition failed: incomplete assets')
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
    const missing = await reconcile(createGithubAdapter(), { version, sourceSha: sha, assetDir: directory, notes }, mode)
    console.log(`GitHub asset ${mode}: ${missing.length} missing; existing bytes verified`)
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exitCode = 1
  }
}
