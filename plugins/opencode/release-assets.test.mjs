import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { resolve, join } from 'node:path'
import { test } from 'node:test'
import { expectedNames, reconcile } from '../../scripts/release-assets.mjs'

const version = '0.2.0'
const tag = `v${version}`
const sha = 'a'.repeat(40)
const digest = (bytes) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`
const assetRoot = resolve('.rtrt/tmp')

async function fixture(state) {
  const dir = await mkdtemp(join(assetRoot, 'release-assets-'))
  const names = expectedNames(version)
  const bytes = new Map()
  for (const name of names) {
    const content = Buffer.from(`new-${name}\n`)
    bytes.set(name, content)
    await writeFile(join(dir, name), content)
  }
  const remote = new Map(names.map((name) => [name, bytes.get(name)]))
  if (state === 'none' || state === 'create-drops') remote.clear()
  if (state.includes('partial') || state === 'draft-upload-fails' || state === 'draft-upload-drops') remote.delete(names[0])
  if (state === 'archive-conflict' || state === 'draft-conflict') remote.set(names[0], Buffer.from('old archive'))
  if (state === 'checksum-conflict') remote.set(names[1], Buffer.from('old checksum'))
  if (state === 'unexpected') remote.set('surprise.zip', Buffer.from('surprise'))
  const calls = []
  let release = state === 'none' || state === 'create-drops' ? null : {
    tag_name: tag,
    target_commitish: state === 'release-retargeted' ? 'b'.repeat(40) : sha,
    draft: state.startsWith('draft'),
    prerelease: state === 'prerelease',
  }
  if (state === 'bad-metadata') delete release.draft
  const api = {
    async tagCommit(name) {
      calls.push(`tag:${name}`)
      return state === 'retargeted' && name === tag ? 'b'.repeat(40) : sha
    },
    async release() {
      calls.push('release')
      return release && {
        ...release,
        assets: [...remote].map(([name, data]) => ({ name, size: data.length, digest: digest(data) })),
      }
    },
    async download(_tag, name) {
      calls.push(`download:${name}`)
      return { size: remote.get(name).length, digest: digest(remote.get(name)) }
    },
    async create(_tag, _notes, files) {
      calls.push(`create:${files.map((file) => file.split('/').at(-1)).join(',')}`)
      release = { tag_name: tag, target_commitish: sha, draft: true, prerelease: false }
      for (const file of files) {
        const name = file.split('/').at(-1)
        if (state !== 'create-drops' || name !== names[0]) remote.set(name, bytes.get(name))
      }
    },
    async upload(_tag, files) {
      calls.push(`upload:${files.map((file) => file.split('/').at(-1)).join(',')}`)
      if (state === 'draft-upload-fails') throw new Error('upload interrupted')
      if (state === 'draft-upload-drops') return
      for (const file of files) remote.set(file.split('/').at(-1), bytes.get(file.split('/').at(-1)))
    },
    async publishDraft() {
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

for (const state of ['none', 'exact', 'draft-exact', 'draft-partial']) {
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
      if (state === 'none') assert.deepEqual(writes(f), [`create:${f.names.join(',')}`, 'undraft'])
      if (state === 'exact') assert.deepEqual(writes(f), [])
      if (state === 'draft-exact') assert.deepEqual(writes(f), ['undraft'])
      if (state === 'draft-partial') assert.deepEqual(writes(f), [`upload:${f.names[0]}`, 'undraft'])
    } finally { await f.close() }
  })
}

for (const state of [
  'archive-conflict', 'checksum-conflict', 'draft-conflict', 'unexpected', 'retargeted',
  'release-retargeted', 'bad-metadata', 'prerelease', 'published-partial',
]) {
  test(`release assets ${state} when validating before npm and before upload`, async () => {
    // Given: an unrecoverable remote release/tag state.
    const f = await fixture(state)
    try {
      // When: both the prepublication gate and publish path inspect it.
      await assert.rejects(reconcile(f.api, plan(f), 'check'), /conflict|unexpected|tag|metadata|prerelease|published/i)
      await assert.rejects(reconcile(f.api, plan(f), 'publish'), /conflict|unexpected|tag|metadata|prerelease|published/i)
      // Then: neither path mutates the remote.
      assert.deepEqual(writes(f), [])
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
  // Given: no release and an adapter that loses one asset on draft creation.
  const f = await fixture('create-drops')
  try {
    // When: publication checks the created draft before changing visibility.
    await assert.rejects(reconcile(f.api, plan(f), 'publish'), /incomplete|missing|postcondition/i)
    // Then: the release is not published with nine assets.
    assert.equal(f.releaseState().draft, true)
    assert.deepEqual(writes(f), [`create:${f.names.join(',')}`])
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
