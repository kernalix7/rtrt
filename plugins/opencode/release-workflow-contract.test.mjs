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

const WORKFLOW_URL = new URL('../../.github/workflows/release.yml', import.meta.url)

function jobsSection(workflow) {
  const marker = workflow.startsWith('jobs:\n') ? 'jobs:\n' : '\njobs:\n'
  const at = workflow.indexOf(marker)
  if (at === -1) return null
  return { marker, head: workflow.slice(0, at), body: workflow.slice(at + marker.length) }
}

function jobsOf(workflow) {
  const section = jobsSection(workflow)
  if (!section) throw new Error('missing jobs section')
  const headers = [...section.body.matchAll(/^  ([\w-]+):\n/gm)]
  const jobs = new Map()
  for (const [index, header] of headers.entries()) {
    const start = header.index
    const end = headers[index + 1]?.index ?? section.body.length
    jobs.set(header[1], section.body.slice(start, end))
  }
  return jobs
}

function permissionEntries(block) {
  const match = /^( *)permissions:\s*\n/m.exec(block)
  if (!match) return null
  const indent = match[1].length
  const entries = []
  for (const line of block.slice(match.index + match[0].length).split('\n')) {
    if (!line.trim()) continue
    const entry = /^( *)([\w-]+):\s*(.*)$/.exec(line)
    if (!entry || entry[1].length <= indent) break
    entries.push([entry[2], entry[3].trim()])
  }
  return entries
}

function topPermissions(workflow) {
  const section = jobsSection(workflow)
  return permissionEntries(section ? section.head : workflow)
}

function withJob(workflow, name, mutate) {
  const section = jobsSection(workflow)
  if (!section) throw new Error('missing jobs section')
  const headers = [...section.body.matchAll(/^  ([\w-]+):\n/gm)]
  const index = headers.findIndex((header) => header[1] === name)
  if (index === -1) throw new Error(`missing ${name} job`)
  const start = headers[index].index
  const end = headers[index + 1]?.index ?? section.body.length
  return section.head + section.marker + section.body.slice(0, start) + mutate(section.body.slice(start, end)) + section.body.slice(end)
}

function publicationGate(workflow) {
  const jobs = jobsOf(workflow)
  const assets = jobs.get('validate-github-assets')
  const platform = jobs.get('publish-platform-npm')
  if (!assets || !platform) throw new Error('missing publication gate job')
  const dependencies = /^    needs: \[([^\]]+)\]/m.exec(platform)?.[1]?.split(/,\s*/)
  if (!dependencies?.includes('validate-github-assets')) throw new Error('npm publication bypasses asset gate')
  if (!/^    needs: \[([^\]]*\bbuild\b[^\]]*)\]/m.test(assets)) throw new Error('asset gate precedes build')
  if (!/node scripts\/release-assets\.mjs check\b/.test(assets)) throw new Error('asset gate does not check remote')
}

function validateGithubAssetsContract(workflow) {
  const assets = jobsOf(workflow).get('validate-github-assets')
  if (!assets) throw new Error('missing validate-github-assets job')
  // Draft visibility requires push access, but the validator must remain behind
  // the reviewer gate without gaining OIDC scopes or widening other jobs.
  if (!/^    environment: npm-publish\s*$/m.test(assets)) {
    throw new Error('validate-github-assets must use the protected npm-publish environment')
  }
  const permissions = permissionEntries(assets)
  if (!permissions) throw new Error('validate-github-assets must declare permissions')
  const scopes = new Map(permissions)
  const names = [...scopes.keys()].sort()
  if (names.join(',') !== 'contents') throw new Error('validate-github-assets must scope permissions to contents only')
  if (scopes.get('contents') !== 'write') throw new Error('validate-github-assets must request contents: write')
  if (/secrets\./.test(assets)) throw new Error('validate-github-assets must not reference repository secrets')
  if (!/^    if: needs\.preflight\.outputs\.publish == 'true'$/m.test(assets)) throw new Error('validate-github-assets must stay publish-gated')
  if (!/^    needs: \[preflight, license-inventory, build\]$/m.test(assets)) throw new Error('validate-github-assets must depend on build')
  if (!/GH_TOKEN: \$\{\{ github\.token \}\}/.test(assets)) throw new Error('validate-github-assets must authenticate its check with GH_TOKEN')
  if (!/node scripts\/release-assets\.mjs check\b/.test(assets)) throw new Error('validate-github-assets must run the check mode')
  if (!/sha256sum -c/.test(assets)) throw new Error('validate-github-assets must verify checksums')
  if (!/RELEASE_SOURCE_SHA/.test(assets)) throw new Error('validate-github-assets must pin the source SHA')
  if (!/persist-credentials: false/.test(assets)) throw new Error('validate-github-assets must disable persisted credentials')
}

function permissionsContract(workflow) {
  const jobs = jobsOf(workflow)
  assert.deepEqual(topPermissions(workflow), [['contents', 'read']], 'root permissions must stay contents: read')
  for (const name of ['publish-platform-npm', 'publish-npm']) {
    assert.deepEqual(
      permissionEntries(jobs.get(name) ?? ''),
      [['contents', 'read'], ['id-token', 'write']],
      `${name} permissions must stay contents:read + id-token:write`,
    )
  }
  assert.deepEqual(permissionEntries(jobs.get('release') ?? ''), [['contents', 'write']], 'release permissions must stay contents: write')
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

test('validate-github-assets reads draft assets with a protected job-scoped write', async () => {
  // Given: the checkout workflow that Actions executes.
  const workflow = await readFile(WORKFLOW_URL, 'utf8')
  // When: the draft-asset validator job is inspected.
  // Then: it carries the protected environment, a contents-only write scope, and
  // the check/checksum/source-SHA/persist-credentials gate it must keep.
  assert.doesNotThrow(() => validateGithubAssetsContract(workflow))
})

test('release workflow keeps root, npm, and release permission scopes unchanged', async () => {
  // Given: the checkout workflow that Actions executes.
  const workflow = await readFile(WORKFLOW_URL, 'utf8')
  // When: every non-validator permission block is inspected.
  // Then: the fix stays job-scoped and widens nothing else.
  assert.doesNotThrow(() => permissionsContract(workflow))
})

test('release workflow rejects permission and gate tampering fixtures', async () => {
  // Given: the fixed workflow plus one deliberate regression per fixture.
  const workflow = await readFile(WORKFLOW_URL, 'utf8')
  const fixtures = [
    ['validator loses the protected environment', withJob(workflow, 'validate-github-assets', (block) => block.replace('    environment: npm-publish\n', '')), /environment/],
    ['validator reverts to contents: read', withJob(workflow, 'validate-github-assets', (block) => block.replace('      contents: write', '      contents: read')), /contents/],
    ['root permissions widen to write', workflow.replace('\npermissions:\n  contents: read\n', '\npermissions:\n  contents: write\n'), /root permissions/],
    ['npm job widens contents to write', withJob(workflow, 'publish-npm', (block) => block.replace('      contents: read', '      contents: write')), /publish-npm permissions/],
    ['validator drops the check-mode call', withJob(workflow, 'validate-github-assets', (block) => block.replace('node scripts/release-assets.mjs check "$RELEASE_VERSION" "$RELEASE_SOURCE_SHA" artifacts', 'true')), /check/],
    ['validator drops the build dependency', withJob(workflow, 'validate-github-assets', (block) => block.replace('needs: [preflight, license-inventory, build]', 'needs: [preflight, license-inventory]')), /build/],
    ['platform npm bypasses validation', withJob(workflow, 'publish-platform-npm', (block) => block.replace(', validate-github-assets]', ']')), /bypasses asset gate/],
  ]
  // When: each regression is run through the machine-consumed contracts.
  // Then: every widened, reverted, or removed guard is rejected.
  for (const [label, fixture, pattern] of fixtures) {
    assert.throws(() => {
      publicationGate(fixture)
      validateGithubAssetsContract(fixture)
      permissionsContract(fixture)
    }, pattern, label)
  }
})
