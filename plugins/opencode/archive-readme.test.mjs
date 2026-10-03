import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { stageArchiveReadme } from '../../scripts/stage-release-archive.mjs'

test('archive README links pinned documentation without changing source README', async () => {
  // Given: a source README with links to documentation absent from a binary archive.
  const dir = await mkdtemp(join(tmpdir(), 'rtrt-archive-readme-'))
  try {
    const source = join(dir, 'source.md')
    const target = join(dir, 'README.md')
    const body = '[Install](docs/INSTALL.md) [Security](SECURITY.md) [notices](THIRD_PARTY_LICENSES.md)'
    await writeFile(source, body)
    // When: an archive-specific README is staged for its version.
    await stageArchiveReadme('0.2.0', target)
    const staged = await readFile(target, 'utf8')
    // Then: only pinned remote links remain and the source stays unchanged.
    assert.equal(await readFile(source, 'utf8'), body)
    assert.match(staged, /https:\/\/github.com\/kernalix7\/rtrt\/blob\/v0\.2\.0\/docs\/INSTALL\.md/)
    assert.match(staged, /https:\/\/github.com\/kernalix7\/rtrt\/blob\/v0\.2\.0\/THIRD_PARTY_LICENSES\.md/)
    assert.equal(/\]\((?:docs\/|(?:DESIGN|SECURITY|THIRD_PARTY_LICENSES)\.md)/.test(staged), false)
  } finally { await rm(dir, { recursive: true, force: true }) }
})
