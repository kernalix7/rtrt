import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { test } from 'node:test'

// Job headers copied from v0.2.0:.github/workflows/release.yml (git show, read-only).
const originalV020 = `jobs:
  publish-platform-npm:
    name: Publish dashboard
    needs: [preflight, build, package-npm]
    if: needs.preflight.outputs.publish == 'true'
  publish-npm:
    name: Publish and verify npm
    needs: [preflight, build, package-npm, publish-platform-npm]
  release:
    name: Create or update GitHub Release
    needs: [preflight, build, publish-npm]
`

function publicationGate(workflow) {
  const jobs = new Map()
  const headers = [...workflow.matchAll(/^  ([\w-]+):\n/gm)]
  for (const [index, header] of headers.entries()) {
    jobs.set(header[1], workflow.slice(header.index, headers[index + 1]?.index))
  }
  const assets = jobs.get('validate-github-assets')
  const platform = jobs.get('publish-platform-npm')
  if (!assets || !platform) throw new Error('missing publication gate job')
  const dependencies = /^    needs: \[([^\]]+)\]/m.exec(platform)?.[1]?.split(/,\s*/)
  if (!dependencies?.includes('validate-github-assets')) throw new Error('npm publication bypasses asset gate')
  if (!/^    needs: \[([^\]]*\bbuild\b[^\]]*)\]/m.test(assets)) throw new Error('asset gate precedes build')
  if (!/node scripts\/release-assets\.mjs check\b/.test(assets)) throw new Error('asset gate does not check remote')
}

test('original v0.2.0 DAG cannot publish npm without a pre-npm asset gate', () => {
  // Given: the original release workflow job headers.
  // When: the machine-consumed DAG is validated.
  // Then: its missing pre-npm gate is rejected.
  assert.throws(() => publicationGate(originalV020), /gate/)
})

test('current release DAG checks remote assets after build and before npm', async () => {
  // Given: the checkout workflow that Actions executes.
  const workflow = await readFile(new URL('../../.github/workflows/release.yml', import.meta.url), 'utf8')
  // When: its job dependencies and asset guard are inspected.
  // Then: platform publication cannot bypass the validation job.
  assert.doesNotThrow(() => publicationGate(workflow))
})
