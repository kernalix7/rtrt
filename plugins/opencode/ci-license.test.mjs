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

// A step whose `run` is a bare `cargo fetch --locked` (no `--target`). This hydrates
// the full locked dependency closure; the five-target loop below only fetches archives
// for explicit release triples, which omits transitively-referenced crates such as
// `android_system_properties v0.1.5` that `cargo metadata --locked --offline` still
// needs because the inventory graph is target-agnostic.
function completeFetchIndex(steps) {
  return steps.findIndex(({ run }) =>
    typeof run === 'string' && run.split('\n').some((line) => line.trim() === 'cargo fetch --locked'))
}

function assertFetchBefore(job, following) {
  const steps = job.steps
  const rust = steps.findIndex(({ uses }) => uses?.startsWith('dtolnay/rust-toolchain@'))
  const cache = steps.findIndex(({ uses }) => uses?.startsWith('Swatinem/rust-cache@'))
  const fetch = steps.findIndex(({ run }) => run?.includes('cargo fetch --locked --target "$target"'))
  const complete = completeFetchIndex(steps)
  const after = steps.findIndex(({ run }) => run?.includes(following))
  assert.ok(rust >= 0 && rust < cache && cache < fetch && fetch < after,
    'pinned Rust, Cargo cache and locked fetch must precede the offline consumer')
  for (const target of targets) assert.ok(steps[fetch].run.includes(target), `fetch misses ${target}`)
  // The five-target loop only fetches archives for explicit release triples; it
  // omits transitively-referenced platform-conditional crates (e.g. the Android
  // archive) that the target-agnostic `cargo metadata --locked --offline` consumer
  // still resolves. Require a standalone `cargo fetch --locked` step (no --target)
  // to hydrate the full locked closure before the consumer.
  assert.ok(complete >= 0, 'a complete `cargo fetch --locked` step (no --target) must hydrate the full closure')
  assert.ok(complete < after, 'complete fetch must precede the offline consumer')
  assert.ok(complete > cache, 'complete fetch must follow the Cargo cache restore')
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

function assertCiFetch(job, following) {
  const steps = job.steps
  const rust = steps.findIndex(({ uses }) => uses?.startsWith('dtolnay/rust-toolchain@'))
  const cache = steps.findIndex(({ uses }) => uses?.startsWith('Swatinem/rust-cache@'))
  const fetch = steps.findIndex(({ run }) => run?.includes('cargo fetch --locked --target "$target"'))
  const complete = completeFetchIndex(steps)
  const after = steps.findIndex(({ run }) => run?.includes(following))
  assert.ok(rust >= 0 && rust < cache && cache < fetch && fetch < after,
    'pinned Rust, Cargo cache and locked fetch must precede the offline CI consumer')
  for (const target of targets) assert.ok(steps[fetch].run.includes(target), `fetch misses ${target}`)
  assert.ok(complete >= 0, 'CI needs a complete `cargo fetch --locked` step (no --target) for the offline consumer')
  assert.ok(complete < after, 'complete fetch must precede the offline consumer')
  assert.ok(complete > cache, 'complete fetch must follow the Cargo cache restore')
}

test('release inventory is unconditional and upstream of every publication and staging job', () => {
  // Given: the actual parsed Actions job DAG.
  // When: publication, archive and npm-package prerequisites are inspected.
  // Then: none can bypass a successful inventory of validated source.
  assertLicenseGate(release)
})

test('fresh CI Node test jobs hydrate the full closure and the five locked target registries before offline tests', () => {
  // Given: the real CI jobs and the release build target matrix.
  // When: CI's offline notice test and npm test setup are inspected.
  // Then: the cache is populated by a complete fetch AND the five-target loop
  // before each offline consumer; CI independently checks inventory.
  assertCiFetch(ci.jobs.opencode, 'npm test')
  assert.ok(ci.jobs['license-inventory'] && !ci.jobs['license-inventory'].if)
  assertCiFetch(ci.jobs['license-inventory'], 'node packaging/licenses/notice-inventory.mjs')
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
    // Strip every `cargo fetch --locked` step (loop + complete). The five-target
    // loop alone is what the OLD workflow had; the test must keep rejecting it.
    (jobs) => { jobs['license-inventory'].steps = jobs['license-inventory'].steps.filter(({ run }) => !run?.includes('cargo fetch --locked')) },
    (jobs) => { jobs['package-npm'].steps = jobs['package-npm'].steps.filter(({ run }) => !run?.includes('cargo fetch --locked')) },
    // Strip ONLY the standalone `cargo fetch --locked` step (no --target), leaving
    // the five-target loop. This is the "only-five-target hydration" regression
    // that broke the v0.2.1 fresh-cache license-inventory job (CI #111159539081).
    (jobs) => { jobs['license-inventory'].steps = jobs['license-inventory'].steps.filter(({ run }) => !run?.split('\n').some((line) => line.trim() === 'cargo fetch --locked')) },
    (jobs) => { jobs['package-npm'].steps = jobs['package-npm'].steps.filter(({ run }) => !run?.split('\n').some((line) => line.trim() === 'cargo fetch --locked')) },
  ]
  // When / Then: each bypass or skipped prerequisite fails the structural contract.
  for (const edit of edits) {
    const fixture = structuredClone(release)
    edit(fixture.jobs)
    assert.throws(() => assertLicenseGate(fixture), undefined, `edit ${edit.toString().slice(0, 80)} accepted`)
  }
})

test('CI fetch fixtures reject the five-target loop as the sole hydration', () => {
  // Given: a valid CI workflow with one offline-hydration prerequisite removed.
  // Each edit represents a real regression mode that surfaced in v0.2.1.
  const probe = (workflow) => {
    assertCiFetch(workflow.jobs['license-inventory'], 'node packaging/licenses/notice-inventory.mjs')
    assertCiFetch(workflow.jobs.opencode, 'npm test')
  }
  assert.doesNotThrow(() => probe(ci))
  const edits = [
    // Strip every `cargo fetch --locked` step (loop + complete) from the inventory
    // and npm-test jobs. This is what the OLD CI workflow was doing.
    (jobs) => { jobs['license-inventory'].steps = jobs['license-inventory'].steps.filter(({ run }) => !run?.includes('cargo fetch --locked')) },
    (jobs) => { jobs.opencode.steps = jobs.opencode.steps.filter(({ run }) => !run?.includes('cargo fetch --locked')) },
    // Strip ONLY the standalone `cargo fetch --locked` step (no --target), leaving
    // the five-target loop. The OLD v0.2.1 CI shipped exactly this and broke on a
    // fresh cache because `android_system_properties v0.1.5` is not an explicit
    // release triple but the target-agnostic `cargo metadata --locked --offline`
    // still resolves it.
    (jobs) => { jobs['license-inventory'].steps = jobs['license-inventory'].steps.filter(({ run }) => !run?.split('\n').some((line) => line.trim() === 'cargo fetch --locked')) },
    (jobs) => { jobs.opencode.steps = jobs.opencode.steps.filter(({ run }) => !run?.split('\n').some((line) => line.trim() === 'cargo fetch --locked')) },
  ]
  // When / Then: each bypass fails the structural consumer check.
  for (const edit of edits) {
    const fixture = structuredClone(ci)
    edit(fixture.jobs)
    assert.throws(() => probe(fixture), undefined,
      `CI edit ${edit.toString().slice(0, 80)} accepted only-five-target hydration`)
  }
})
