import { spawnSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expectedNames } from '../../scripts/release-assets.mjs'
import { SEED_ASSET_ID, contentTypeFor, digest, dispatch, sha, stateShape, tag, version } from './release-assets-gh-api.mjs'

const root = fileURLToPath(new URL('../../.rtrt/tmp/', import.meta.url))

export async function ghFixture(releases = [], pageOne = []) {
  await mkdir(root, { recursive: true })
  const dir = await mkdtemp(join(root, 'release-gh-'))
  const bin = join(dir, 'bin')
  const assets = join(dir, 'assets')
  const config = join(dir, 'config')
  await Promise.all([mkdir(bin), mkdir(assets), mkdir(config)])
  const names = expectedNames(version)
  const content = (name) => Buffer.from(`\0binary\xff${name}\n`, 'latin1')
  await Promise.all(names.map((name) => writeFile(join(assets, name), content(name))))
  const notesPath = join(dir, 'notes.md')
  await writeFile(notesPath, 'Test-only release notes\n')
  const stateFile = join(dir, 'state.json')
  let assetSeq = SEED_ASSET_ID
  const release = (id, draft = true, included = names, overrides = {}) => ({
    id, tag_name: tag, draft, prerelease: false, target_commitish: sha,
    assets: included.map((name) => {
      const bytes = overrides[name] ?? content(name)
      return {
        id: assetSeq++, name, size: bytes.length, digest: digest(bytes), state: 'uploaded',
        content_type: contentTypeFor(name), bytes: bytes.toString('base64'),
      }
    }),
  })
  await writeFile(stateFile, JSON.stringify(stateShape(releases, pageOne)))
  const moduleUrl = new URL('./release-assets-gh-fixture.mjs', import.meta.url).href
  await writeFile(join(bin, 'package.json'), '{"type":"commonjs"}\n')
  await writeFile(join(bin, 'gh'), `#!${process.execPath}\nimport(${JSON.stringify(moduleUrl)}).then(({ fakeGh }) => fakeGh(process.argv.slice(2))).catch((error) => { console.error(error.message); process.exitCode = 1 })\n`, { mode: 0o700 })
  const env = {
    PATH: bin, HOME: dir, XDG_CONFIG_HOME: config, GH_CONFIG_DIR: config,
    GH_TOKEN: 'fixture-dummy-token-no-network', GH_HOST: 'fixture.invalid',
    RELEASE_GH_FIXTURE_STATE: stateFile, RELEASE_GH_FIXTURE_ASSETS: assets,
    RELEASE_GH_FIXTURE_NOTES: notesPath,
  }
  return {
    release, names, assetsDir: assets,
    probe: (...args) => spawnSync(join(bin, 'gh'), args, { env, encoding: 'utf8' }),
    seed(nextReleases, nextPageOne = [], extra = {}) {
      writeFileSync(stateFile, JSON.stringify({ ...stateShape(nextReleases, nextPageOne), ...extra }))
    },
    run(mode) {
      return spawnSync(process.execPath, [
        fileURLToPath(new URL('../../scripts/release-assets.mjs', import.meta.url)),
        mode, version, sha, assets, ...(mode === 'publish' ? [join(dir, 'notes.md')] : []),
      ], { env, encoding: 'utf8', timeout: 10_000 })
    },
    state: () => JSON.parse(readFileSync(stateFile, 'utf8')),
    close: () => rm(dir, { recursive: true, force: true }),
  }
}

export function fakeGh(argv) {
  const file = process.env.RELEASE_GH_FIXTURE_STATE
  const state = JSON.parse(readFileSync(file, 'utf8'))
  state.calls.push(argv)
  try {
    dispatch(argv, state)
  } finally {
    writeFileSync(file, JSON.stringify(state))
  }
}
