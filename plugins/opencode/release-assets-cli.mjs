import assert from 'node:assert/strict'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import { ghFixture } from './release-assets-gh-fixture.mjs'

const root = fileURLToPath(new URL('../../.rtrt/tmp/', import.meta.url))

const flag = (args, ...names) => {
  for (const name of names) {
    const index = args.indexOf(name)
    if (index >= 0 && index + 1 < args.length) return args[index + 1]
  }
  return undefined
}
const verb = (args) => (flag(args, '--method', '-X') ?? 'GET').toUpperCase()
const endpoint = (args) => String(args.at(-1) ?? '').replace(/^https:\/\/api\.github\.com\//, '')
const apiCalls = (calls) => calls.filter((args) => args[0] === 'api')
const mutations = (calls) => apiCalls(calls).filter((args) => ['POST', 'PATCH', 'DELETE'].includes(verb(args)))
const deletes = (calls) => apiCalls(calls).filter((args) => verb(args) === 'DELETE')
const creates = (calls) => apiCalls(calls).filter((args) =>
  verb(args) === 'POST' && endpoint(args) === 'repos/kernalix7/rtrt/releases')
const uploads = (calls) => apiCalls(calls).filter((args) =>
  verb(args) === 'POST' && endpoint(args).includes('/assets?name='))
const payloadInputs = (calls) => apiCalls(calls)
  .map((args) => flag(args, '--input'))
  .filter((input) => typeof input === 'string' && input.endsWith('.json'))

function assertNoDestructive(calls) {
  assert.deepEqual(deletes(calls), [])
  const names = uploads(calls).map((args) => new URL(String(args.at(-1))).searchParams.get('name'))
  assert.equal(new Set(names).size, names.length, `duplicate uploads: ${names.join(',')}`)
}

const fullPage = (f) => Array.from({ length: 100 }, (_, index) => ({
  ...f.release(index + 1, true, []), tag_name: `v0.1.${index}`,
}))
const fullPageWithDraft = (f, id) => fullPage(f).map((item, index) =>
  index === 50 ? f.release(id, true, []) : item
)

test('CLI check finds exact draft on later releases page despite by-tag 404', async () => {
  // Given: the by-tag endpoint hides an existing exact-tag draft on page two.
  const f = await ghFixture()
  f.seed([f.release(412)], fullPage(f))
  try {
    // When: the actual CLI invokes gh to check the full inventory.
    const result = f.run('check')
    // Then: it verifies bytes and reports no missing assets without writes.
    assert.equal(result.status, 0, result.stderr)
    assert.match(result.stdout, /GitHub asset check: 0 missing;/)
    assert.deepEqual(mutations(f.state().calls), [])
    assert.ok(f.state().calls.some((args) => args.some((item) => typeof item === 'string' && item.endsWith('per_page=100&page=2'))))
  } finally { await f.close() }
})

test('CLI publish creates one draft then publishes that release ID', async () => {
  // Given: no matching release exists and local binary/checksum assets are complete.
  const f = await ghFixture()
  try {
    // When: the actual CLI publishes a fresh release.
    const result = f.run('publish')
    // Then: exactly one draft was created and that ID was published.
    assert.equal(result.status, 0, result.stderr)
    const state = f.state()
    assert.equal(state.releases.length, 1)
    assert.equal(creates(state.calls).length, 1)
    assert.equal(uploads(state.calls).length, f.names.length)
    assert.equal(state.editedId, state.releases[0].id)
    assert.equal(state.releases[0].draft, false)
    assertNoDestructive(state.calls)
  } finally { await f.close() }
})

test('CLI interrupted publish reuses draft ID and uploads only missing bytes', async () => {
  // Given: previous publication created draft 412 but stopped before its first asset upload.
  const f = await ghFixture()
  f.seed([f.release(412, true, f.names.slice(1))])
  try {
    // When: the actual CLI reruns publication against the same tag.
    const result = f.run('publish')
    // Then: draft 412 is completed without creating another release.
    assert.equal(result.status, 0, result.stderr)
    const state = f.state()
    assert.deepEqual(creates(state.calls), [])
    assert.equal(state.editedId, 412)
    assert.equal(uploads(state.calls).length, 1)
    assert.deepEqual(state.releases[0].assets.map((asset) => asset.name).sort(), [...f.names].sort())
    assertNoDestructive(state.calls)
  } finally { await f.close() }
})

test('CLI refuses duplicate exact-tag drafts on later page without mutation', async () => {
  // Given: two exact-tag drafts appear on page two after a full unrelated first page.
  const f = await ghFixture()
  f.seed([f.release(412), f.release(413)], fullPage(f))
  try {
    // When: both read-only check and publication scan all pages.
    for (const mode of ['check', 'publish']) {
      const result = f.run(mode)
      // Then: ambiguity fails closed before any mutation.
      assert.equal(result.status, 1, `${mode}: ${result.stdout} ${result.stderr}`)
      assert.match(result.stderr, /duplicate|ambiguous|conflict/i)
    }
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI keeps scanning full pages past a match and fails on a later-page duplicate', async () => {
  // Given: an exact-tag draft sits mid-way through a full first page; page two holds a duplicate.
  const f = await ghFixture()
  f.seed([f.release(413)], fullPageWithDraft(f, 412))
  try {
    // When: the read-only gate lists every page instead of stopping at the first match.
    const result = f.run('check')
    // Then: the later-page duplicate fails closed before any mutation.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /duplicate|ambiguous|conflict/i)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI refuses a draft plus published duplicate for the same tag', async () => {
  // Given: one draft and one published release share the exact tag.
  const f = await ghFixture()
  f.seed([f.release(412, true), f.release(413, false)])
  try {
    // When: both modes inspect the ambiguous inventory.
    for (const mode of ['check', 'publish']) {
      const result = f.run(mode)
      // Then: ambiguity fails closed without mutation.
      assert.equal(result.status, 1, `${mode}: ${result.stdout} ${result.stderr}`)
      assert.match(result.stderr, /duplicate|ambiguous|conflict/i)
    }
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI leaves a complete published release untouched', async () => {
  // Given: an already-published release with all ten matching assets.
  const f = await ghFixture()
  f.seed([f.release(414, false)])
  try {
    // When: publication reconciles against it.
    const result = f.run('publish')
    // Then: it is a no-op that keeps the release public.
    assert.equal(result.status, 0, result.stderr)
    assert.equal(f.state().releases[0].draft, false)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI fails closed when a later release page errors', async () => {
  // Given: a full first page followed by a page-two server error.
  const f = await ghFixture()
  f.seed([f.release(412)], fullPage(f), { failPage: 2 })
  try {
    // When: the read-only gate pages past the first result.
    const result = f.run('check')
    // Then: the transport error is not mistaken for an empty tail.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /failed|error|500/i)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI fails closed on a malformed release id', async () => {
  // Given: a release list entry whose id is not a safe positive integer.
  const f = await ghFixture()
  f.seed([f.release('not-a-number', true, [])])
  try {
    // When: the inventory is selected for reconciliation.
    const result = f.run('check')
    // Then: malformed metadata fails closed before any mutation.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /malformed/i)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI fails closed when the pinned release id changes on reread', async () => {
  // Given: a listed match whose GET-by-id returns a different release id.
  const f = await ghFixture()
  f.seed([f.release(412)], [], { shiftId: true })
  try {
    // When: reconciliation re-reads the pinned id before trusting it.
    const result = f.run('check')
    // Then: the id change fails closed without mutation.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /id|changed/i)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI rejects malformed tag metadata instead of treating it as an absent release', async () => {
  // Given: a successful list response contains a non-string tag.
  const f = await ghFixture()
  f.seed([{ ...f.release(412, true, []), tag_name: 42 }])
  try {
    // When: the actual CLI checks the release inventory.
    const result = f.run('check')
    // Then: malformed metadata is refused without writes.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /malformed/i)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})

test('CLI fails closed when draft creation reports 201 but exits nonzero', async () => {
  // Given: a fresh publish whose create prints a valid 201 yet exits nonzero.
  const f = await ghFixture()
  f.seed([], [], { createExitNonzero: true })
  try {
    // When: the adapter inspects both status and process exit.
    const result = f.run('publish')
    // Then: no upload or publication follows the failed create.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /write failed/i)
    const state = f.state()
    assert.equal(uploads(state.calls).length, 0)
    assert.equal(state.editedId, undefined)
    assert.equal(state.releases.length, 1)
    assert.equal(state.releases[0].draft, true)
  } finally { await f.close() }
})

test('CLI writes JSON payloads only under the repository .rtrt/tmp', async () => {
  // Given: a fresh publish that needs draft-create and publish JSON bodies.
  const f = await ghFixture()
  try {
    // When: the CLI runs end to end.
    const result = f.run('publish')
    // Then: every JSON payload lives under the repo-local private tmp root.
    assert.equal(result.status, 0, result.stderr)
    const payloads = payloadInputs(f.state().calls)
    assert.ok(payloads.length >= 2, JSON.stringify(payloads))
    for (const input of payloads) assert.ok(input.startsWith(root), input)
  } finally { await f.close() }
})

test('CLI rejects a signal-terminated create even after a complete 201 response', async () => {
  // Given: gh flushes a valid create response then terminates with SIGTERM.
  const f = await ghFixture()
  f.seed([], [], { createSignal: true })
  try {
    // When: the actual CLI attempts publication.
    const result = f.run('publish')
    // Then: no upload or publication follows the uncertain transport result.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /write failed/i)
    const state = f.state()
    assert.equal(uploads(state.calls).length, 0)
    assert.equal(state.editedId, undefined)
    assert.equal(state.releases[0].draft, true)
  } finally { await f.close() }
})

test('CLI rejects a published release whose asset bytes conflict', async () => {
  // Given: a published release whose first archive bytes differ from the rebuild.
  const f = await ghFixture()
  f.seed([f.release(415, false, f.names, { [f.names[0]]: Buffer.from('tampered bytes') })])
  try {
    // When: the pre-npm guard compares downloaded bytes.
    const result = f.run('check')
    // Then: the byte conflict fails closed without mutation.
    assert.equal(result.status, 1, `${result.stdout} ${result.stderr}`)
    assert.match(result.stderr, /conflict/i)
    assert.deepEqual(mutations(f.state().calls), [])
  } finally { await f.close() }
})
