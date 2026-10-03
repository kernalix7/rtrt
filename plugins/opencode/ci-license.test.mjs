import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('../../', import.meta.url))
const yaml = (name) => JSON.parse(execFileSync('ruby', ['-rpsych', '-rjson', '-e',
  'puts JSON.generate(Psych.safe_load_file(ARGV.fetch(0), permitted_classes: [], aliases: false))',
  `${root}.github/workflows/${name}.yml`], { encoding: 'utf8' }))
const ci = yaml('ci')
const release = yaml('release')
const targets = release.jobs.build.strategy.matrix.include.map(({ target }) => target)

function assertFetchBefore(job, following) {
  const steps = job.steps
  const rust = steps.findIndex(({ uses }) => uses?.startsWith('dtolnay/rust-toolchain@'))
  const cache = steps.findIndex(({ uses }) => uses?.startsWith('Swatinem/rust-cache@'))
  const fetch = steps.findIndex(({ run }) => run?.includes('cargo fetch --locked --target "$target"'))
  const after = steps.findIndex(({ run }) => run?.includes(following))
  assert.ok(rust >= 0 && rust < cache && cache < fetch && fetch < after,
    'pinned Rust, Cargo cache and locked fetch must precede the offline consumer')
  for (const target of targets) assert.ok(steps[fetch].run.includes(target), `fetch misses ${target}`)
}

function assertLicenseGate(workflow) {
  const jobs = workflow.jobs
  const gate = jobs['license-inventory']
  assert.ok(gate && !gate.if && !gate['continue-on-error'], 'inventory must be an unconditional failing job')
  assert.equal(gate['runs-on'], 'ubuntu-latest')
  const checkout = gate.steps.find(({ uses }) => uses?.startsWith('actions/checkout@'))
  assert.equal(checkout?.with?.ref, '${{ needs.preflight.outputs.source_sha }}')
  assert.equal(checkout?.with?.['persist-credentials'], false)
  assert.deepEqual(gate.needs, ['preflight'])
  const node = gate.steps.find(({ uses }) => uses?.startsWith('actions/setup-node@'))
  assert.equal(node?.with?.['node-version'], 24)
  assertFetchBefore(gate, 'node packaging/licenses/notice-inventory.mjs')
  const sources = ['build', 'package-npm', 'validate-github-assets']
  const publications = ['publish-platform-npm', 'publish-npm', 'release']
  for (const name of [...sources, ...publications]) {
    assert.ok(jobs[name].needs.includes('license-inventory'), `${name} bypasses inventory`)
    assert.equal(jobs[name]['continue-on-error'], undefined, `${name} ignores failure`)
  }
  for (const name of publications)
    assert.equal(jobs[name].if, "needs.preflight.outputs.publish == 'true'")
  assertFetchBefore(jobs['package-npm'], 'npm test')
}

test('release inventory is unconditional and upstream of every publication and staging job', () => {
  // Given: the actual parsed Actions job DAG.
  // When: publication, archive and npm-package prerequisites are inspected.
  // Then: none can bypass a successful inventory of validated source.
  assertLicenseGate(release)
})

test('fresh CI Node test jobs fetch the five locked target registries before offline tests', () => {
  // Given: the real CI jobs and the release build target matrix.
  // When: CI's offline notice test setup is inspected.
  // Then: the cache is populated before npm test; CI independently checks inventory.
  assertFetchBefore(ci.jobs.opencode, 'npm test')
  assert.ok(ci.jobs['license-inventory'] && !ci.jobs['license-inventory'].if)
  assertFetchBefore(ci.jobs['license-inventory'], 'node packaging/licenses/notice-inventory.mjs')
  const node = ci.jobs['license-inventory'].steps.find(({ uses }) => uses?.startsWith('actions/setup-node@'))
  assert.equal(node?.with?.['node-version'], 24)
})

test('DAG and fetch fixtures reject missing inventory prerequisites', () => {
  // Given: a valid workflow with one required edge or fetch removed in turn.
  const edits = [
    (jobs) => { delete jobs['license-inventory'] },
    (jobs) => { jobs.build.needs = ['preflight'] },
    (jobs) => { jobs['package-npm'].needs = ['preflight'] },
    (jobs) => { jobs['validate-github-assets'].needs = ['preflight', 'build'] },
    (jobs) => { jobs['publish-platform-npm'].needs = jobs['publish-platform-npm'].needs.filter((name) => name !== 'license-inventory') },
    (jobs) => { jobs['publish-npm'].needs = jobs['publish-npm'].needs.filter((name) => name !== 'license-inventory') },
    (jobs) => { jobs.release.needs = jobs.release.needs.filter((name) => name !== 'license-inventory') },
    (jobs) => { jobs['license-inventory'].if = 'false' },
    (jobs) => { jobs['license-inventory'].steps = jobs['license-inventory'].steps.filter(({ run }) => !run?.includes('cargo fetch --locked')) },
    (jobs) => { jobs['package-npm'].steps = jobs['package-npm'].steps.filter(({ run }) => !run?.includes('cargo fetch --locked')) },
  ]
  // When / Then: each bypass or skipped prerequisite fails the structural contract.
  for (const edit of edits) {
    const fixture = structuredClone(release)
    edit(fixture.jobs)
    assert.throws(() => assertLicenseGate(fixture))
  }
})
