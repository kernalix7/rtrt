import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { basename, join } from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import { expectedNames, reconcile } from '../../scripts/release-assets.mjs'
import './release-assets-cli.mjs'

const version = '0.2.0'
const tag = `v${version}`
const sha = 'a'.repeat(40)
const digest = (bytes) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`
const assetRoot = fileURLToPath(new URL('../../.rtrt/tmp/', import.meta.url))
const noRemote = new Set(['none', 'create-drops', 'create-retargeted', 'create-published'])

async function fixture(state) {
  await mkdir(assetRoot, { recursive: true })
  const dir = await mkdtemp(join(assetRoot, 'release-assets-'))
  const names = expectedNames(version)
  const bytes = new Map()
  for (const name of names) {
    const content = Buffer.from(`new-${name}\n`)
    bytes.set(name, content)
    await writeFile(join(dir, name), content)
  }
  const remote = new Map()
  if (!noRemote.has(state)) for (const name of names) remote.set(name, bytes.get(name))
  if (state.includes('partial') || state === 'draft-upload-fails' || state === 'draft-upload-drops') remote.delete(names[0])
  if (state === 'archive-conflict' || state === 'draft-conflict') remote.set(names[0], Buffer.from('old archive'))
  if (state === 'checksum-conflict') remote.set(names[1], Buffer.from('old checksum'))
  if (state === 'unexpected') remote.set('surprise.zip', Buffer.from('surprise'))

  const calls = []
  let assetSeq = 1000
  const assetIds = new Map()
  for (const name of names) assetIds.set(name, assetSeq++)
  const idFor = (name) => {
    if (!assetIds.has(name)) assetIds.set(name, assetSeq++)
    return assetIds.get(name)
  }
  const assetRows = () => [...remote].map(([name, data]) => ({
    id: idFor(name), name, size: data.length, digest: digest(data),
    state: 'uploaded', content_type: 'application/octet-stream',
  }))
  let release = noRemote.has(state) ? null : {
    id: 77,
    tag_name: tag,
    target_commitish: state === 'release-retargeted' ? 'b'.repeat(40) : sha,
    draft: state.startsWith('draft'),
    prerelease: state === 'prerelease',
  }
  if (state === 'bad-metadata') delete release.draft
  const snapshot = () => release && { ...release, assets: assetRows() }
  const api = {
    async tagCommit(name) {
      calls.push(`tag:${name}`)
      return state === 'retargeted' && name === tag ? 'b'.repeat(40) : sha
    },
    async listReleases() {
      calls.push('list')
      return release ? [snapshot()] : []
    },
    async getRelease(id) {
      calls.push(`get:${id}`)
      const current = snapshot()
      if (!current) throw new Error(`unknown release: ${id}`)
      return state === 'changed-id' ? { ...current, id: current.id + 1 } : current
    },
    async download(assetId) {
      const [name, data] = [...remote].find(([candidate]) => idFor(candidate) === assetId)
      calls.push(`download:${name}`)
      return { size: data.length, digest: digest(data) }
    },
    async createDraft({ tag: draftTag, sourceSha }) {
      calls.push(`create:${draftTag}`)
      release = {
        id: 900,
        tag_name: state === 'create-retargeted' ? 'v9.9.9' : draftTag,
        target_commitish: sourceSha,
        draft: state !== 'create-published',
        prerelease: false,
      }
      return snapshot()
    },
    async uploadAsset(_releaseId, filePath) {
      const name = basename(filePath)
      calls.push(`upload:${name}`)
      if (state === 'draft-upload-fails') throw new Error('upload interrupted')
      if ((state === 'draft-upload-drops' || state === 'create-drops') && name === names[0]) return
      remote.set(name, bytes.get(name))
    },
    async publishRelease() {
      calls.push('undraft')
      if (state !== 'draft-edit-drops') release = { ...release, draft: false }
    },
  }
  return {
    dir, names, calls, api,
    releaseState: () => release && { ...release, names: [...remote.keys()] },
    async close() { await rm(dir, { recursive: true, force: true }) },
  }
}

const plan = (f) => ({ version, sourceSha: sha, assetDir: f.dir, notes: 'body.md' })
const writes = (f) => f.calls.filter((call) => /^(create:|upload:|undraft)/.test(call))
const uploads = (names) => names.map((name) => `upload:${name}`)

const scenarios = {
  none: (f) => [`create:${tag}`, ...uploads(f.names), 'undraft'],
  exact: () => [],
  'draft-exact': () => ['undraft'],
  'draft-partial': (f) => [`upload:${f.names[0]}`, 'undraft'],
}

for (const [state, expectedWrites] of Object.entries(scenarios)) {
  test(`release assets ${state} when checking then publishing`, async () => {
    // Given: complete local artifacts and a controlled remote state.
    const f = await fixture(state)
    try {
      // When: the read-only gate runs, then publication reconciles.
      await reconcile(f.api, plan(f), 'check')
      assert.deepEqual(writes(f), [])
      await reconcile(f.api, plan(f), 'publish')
      // Then: only a draft is undrafted, after its ten matching assets exist.
      assert.deepEqual(f.releaseState().names.sort(), [...f.names].sort())
      assert.equal(f.releaseState().draft, false)
      assert.deepEqual(writes(f), expectedWrites(f))
    } finally { await f.close() }
  })
}

for (const state of [
  'archive-conflict', 'checksum-conflict', 'draft-conflict', 'unexpected', 'retargeted',
  'release-retargeted', 'bad-metadata', 'prerelease', 'published-partial', 'changed-id',
]) {
  test(`release assets ${state} when validating before npm and before upload`, async () => {
    // Given: an unrecoverable remote release/tag state.
    const f = await fixture(state)
    try {
      // When: both the prepublication gate and publish path inspect it.
      await assert.rejects(reconcile(f.api, plan(f), 'check'), /conflict|unexpected|tag|metadata|prerelease|published|id/i)
      await assert.rejects(reconcile(f.api, plan(f), 'publish'), /conflict|unexpected|tag|metadata|prerelease|published|id/i)
      // Then: neither path mutates the remote.
      assert.deepEqual(writes(f), [])
    } finally { await f.close() }
  })
}

for (const state of ['create-retargeted', 'create-published']) {
  test(`release assets ${state} rejects invalid created metadata`, async () => {
    // Given: a fresh create whose returned metadata is untrustworthy.
    const f = await fixture(state)
    try {
      // When: publication validates the created draft before any upload.
      await assert.rejects(reconcile(f.api, plan(f), 'publish'), /conflict|draft|metadata/i)
      // Then: no asset upload happens and no release is made public.
      assert.deepEqual(writes(f), [`create:${tag}`])
    } finally { await f.close() }
  })
}

test('draft stays draft when missing asset upload fails', async () => {
  // Given: a matching partial draft whose upload fails.
  const f = await fixture('draft-upload-fails')
  try {
    // When: publication attempts to append the missing archive.
    await assert.rejects(reconcile(f.api, plan(f), 'publish'), /upload interrupted/)
    // Then: release stays draft; no incomplete release becomes visible.
    assert.equal(f.releaseState().draft, true)
    assert.deepEqual(writes(f), [`upload:${f.names[0]}`])
  } finally { await f.close() }
})

test('draft stays draft when upload reports success but inventory is incomplete', async () => {
  // Given: a draft and an adapter that silently loses an upload.
  const f = await fixture('draft-upload-drops')
  try {
    // When: publishing verifies the post-upload remote inventory.
    await assert.rejects(reconcile(f.api, plan(f), 'publish'), /incomplete|missing|postcondition/i)
    // Then: no incomplete release is made public.
    assert.equal(f.releaseState().draft, true)
    assert.deepEqual(writes(f), [`upload:${f.names[0]}`])
  } finally { await f.close() }
})

test('publication fails if draft edit does not change release state', async () => {
  // Given: ten matching draft assets and an edit that has no effect.
  const f = await fixture('draft-edit-drops')
  try {
    // When: the publish operation verifies its postcondition.
    await assert.rejects(reconcile(f.api, plan(f), 'publish'), /draft|postcondition/i)
    // Then: success is not reported for a draft release.
    assert.equal(f.releaseState().draft, true)
    assert.deepEqual(writes(f), ['undraft'])
  } finally { await f.close() }
})

test('new release stays draft when creation loses an asset', async () => {
  // Given: no release and an adapter that loses the first upload.
  const f = await fixture('create-drops')
  try {
    // When: publication checks the created draft before changing visibility.
    await assert.rejects(reconcile(f.api, plan(f), 'publish'), /incomplete|missing|postcondition/i)
    // Then: the release is not published with nine assets.
    assert.equal(f.releaseState().draft, true)
    assert.deepEqual(writes(f), [`create:${tag}`, ...uploads(f.names)])
  } finally { await f.close() }
})

test('release assets reject a rebuilt archive even when the remote registry version is already published', async () => {
  // Given: a previously published release whose archive bytes differ from this rebuild.
  const f = await fixture('archive-conflict')
  try {
    // When: a rerun performs its pre-npm release guard.
    await assert.rejects(reconcile(f.api, plan(f), 'check'), /conflict/i)
    // Then: the remote release is untouched.
    assert.deepEqual(writes(f), [])
  } finally { await f.close() }
})
