import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { expectedNames } from '../../scripts/release-assets.mjs'

export const version = '0.2.0'
export const tag = `v${version}`
export const sha = 'a'.repeat(40)
export const repo = 'kernalix7/rtrt'
export const SEED_ASSET_ID = 1001
export const NEXT_ASSET_ID = 5001
export const NEXT_RELEASE_ID = 9001

export const digest = (bytes) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`
// Real gh infers a content type per extension; the fake also accepts the generic
// binary type GitHub stores, so an adapter may declare either one.
export const contentTypeFor = (name) =>
  name.endsWith('.tar.gz') ? 'application/gzip'
    : name.endsWith('.zip') ? 'application/zip'
      : name.endsWith('.sha256') ? 'text/plain'
        : 'application/octet-stream'
export const stateShape = (releases, pageOne) => ({
  releases, pageOne, calls: [], nextId: NEXT_RELEASE_ID, nextAssetId: NEXT_ASSET_ID,
})

const statusText = (status) => ({ 200: 'OK', 201: 'Created', 404: 'Not Found', 422: 'Unprocessable Entity' }[status] ?? 'OK')
const asBool = (value) => (value === false || value === 'false' ? false : value === true || value === 'true' ? true : value)
const allReleases = (state) => [...state.pageOne, ...state.releases]
const findRelease = (state, id) => allReleases(state).find((item) => item.id === id)
const findAsset = (state, id) => allReleases(state).flatMap((item) => item.assets).find((asset) => asset.id === id)
const uploadedAsset = (state, name, bytes, contentType) => ({
  id: state.nextAssetId++, name, size: bytes.length, digest: digest(bytes), state: 'uploaded',
  content_type: contentType ?? contentTypeFor(name), bytes: bytes.toString('base64'),
})

const argValue = (argv, ...flags) => {
  for (const flag of flags) {
    const index = argv.indexOf(flag)
    if (index >= 0 && index + 1 < argv.length) return argv[index + 1]
  }
  for (const flag of flags) {
    const match = argv.find((item) => item.startsWith(`${flag}=`))
    if (match) return match.slice(flag.length + 1)
  }
  return undefined
}
const apiMethod = (argv) => (argValue(argv, '--method', '-X') ?? 'GET').toUpperCase()
const headers = (argv) => {
  const found = {}
  for (let index = 0; index < argv.length; index++) {
    if (argv[index] === '-H' || argv[index] === '--header') {
      const [key, ...rest] = String(argv[index + 1] ?? '').split(':')
      found[key.trim().toLowerCase()] = rest.join(':').trim()
    }
  }
  return found
}
const requestBody = (argv) => {
  const input = argValue(argv, '--input')
  if (input && input !== '-') return JSON.parse(readFileSync(input, 'utf8'))
  const fields = {}
  for (let index = 0; index < argv.length; index++) {
    if (['-f', '--field', '--raw-field'].includes(argv[index])) {
      const [key, ...rest] = String(argv[index + 1] ?? '').split('=')
      fields[key] = rest.join('=')
    }
  }
  return fields
}

export function dispatch(argv, state) {
  const include = argv.includes('--include')
  const respond = (status, body) => {
    if (include) {
      process.stdout.write(`HTTP/2.0 ${status} ${statusText(status)}\r\ncontent-type: application/json\r\n\r\n`)
      if (body !== undefined) process.stdout.write(JSON.stringify(body))
    } else if (status < 400 && body !== undefined) {
      process.stdout.write(JSON.stringify(body))
    }
    // Real gh exits nonzero on 4xx while --include still prints the status line.
    if (status >= 400) throw new Error(`gh: ${statusText(status)} (HTTP ${status})`)
  }
  const respondRaw = (bytes) => {
    if (include) process.stdout.write('HTTP/2.0 200 OK\r\ncontent-type: application/octet-stream\r\n\r\n')
    process.stdout.write(bytes)
  }
  const publicAsset = ({ bytes: _bytes, ...rest }) => rest
  const publicRelease = ({ assets, ...rest }) => ({ ...rest, assets: assets.map(publicAsset) })

  if (argv[0] === 'api') {
    // Absolute api.github.com URLs normalize to repo-relative endpoints; the
    // upload host stays absolute so the upload matcher can require it exactly.
    const rawEndpoint = String(argv.at(-1) ?? '')
    const endpoint = rawEndpoint.replace(/^https:\/\/api\.github\.com\//, '')
    const verb = apiMethod(argv)
    const head = headers(argv)
    if (verb === 'GET' && endpoint.startsWith(`repos/${repo}/git/ref/tags/`)) {
      respond(200, { object: { type: 'commit', sha } })
      return
    }
    const byTag = /^repos\/kernalix7\/rtrt\/releases\/tags\/(.+)$/.exec(endpoint)
    if (verb === 'GET' && byTag) {
      // Published-only: an exact-tag draft must stay hidden, proving the production bug.
      const found = allReleases(state).find((item) => item.tag_name === byTag[1] && !item.draft)
      respond(found ? 200 : 404, found ? publicRelease(found) : { message: 'Not Found' })
      return
    }
    const list = /^repos\/kernalix7\/rtrt\/releases\?per_page=100(?:&page=(\d+))?$/.exec(endpoint)
    if (verb === 'GET' && list) {
      const all = state.pageOne.length ? [...state.pageOne, ...state.releases] : [...state.releases]
      const page = Number(list[1] ?? 1)
      if (state.failPage === page) {
        respond(500, { message: 'fixture page failure' })
        return
      }
      respond(200, all.slice((page - 1) * 100, page * 100).map(publicRelease))
      return
    }
    if (verb === 'POST' && endpoint === `repos/${repo}/releases`) {
      const meta = requestBody(argv)
      const notesPath = process.env.RELEASE_GH_FIXTURE_NOTES
      if (meta.tag_name !== tag || meta.target_commitish !== sha || meta.name !== tag
        || meta.draft !== true || meta.prerelease !== false
        || (notesPath && meta.body !== readFileSync(notesPath, 'utf8'))) {
        throw new Error('create payload mismatch')
      }
      if (allReleases(state).some((item) => item.tag_name === meta.tag_name)) {
        throw new Error('duplicate release creation refused')
      }
      const created = {
        id: state.nextId++, tag_name: meta.tag_name, draft: true,
        prerelease: false, target_commitish: meta.target_commitish, assets: [],
      }
      state.releases.push(created)
      respond(201, publicRelease(created))
      if (state.createSignal) process.stdout.write('', () => process.kill(process.pid, 'SIGTERM'))
      // Simulate gh printing a valid 201 while still exiting nonzero.
      if (state.createExitNonzero) throw new Error('gh: simulated nonzero exit after 201')
      return
    }
    const byId = /^repos\/kernalix7\/rtrt\/releases\/(\d+)$/.exec(endpoint)
    if (byId && (verb === 'GET' || verb === 'PATCH')) {
      const found = findRelease(state, Number(byId[1]))
      if (!found) {
        respond(404, { message: 'Not Found' })
        return
      }
      if (verb === 'GET') {
        const shifted = state.shiftId ? { ...found, id: found.id + 1 } : found
        respond(200, publicRelease(state.retagOnRead ? { ...shifted, tag_name: 'v9.9.9' } : shifted))
        return
      }
      const patch = requestBody(argv)
      if ('draft' in patch) found.draft = asBool(patch.draft)
      if ('target_commitish' in patch) found.target_commitish = patch.target_commitish
      state.editedId = found.id
      respond(200, publicRelease(found))
      return
    }
    const relAssets = /^repos\/kernalix7\/rtrt\/releases\/(\d+)\/assets$/.exec(endpoint)
    if (verb === 'GET' && relAssets) {
      const found = findRelease(state, Number(relAssets[1]))
      respond(found ? 200 : 404, found ? found.assets.map(publicAsset) : { message: 'Not Found' })
      return
    }
    // Uploads must target the fixed absolute upload-host URL; a bare relative
    // repo path is not accepted as an upload endpoint.
    const upload = /^https:\/\/uploads\.github\.com\/repos\/kernalix7\/rtrt\/releases\/(\d+)\/assets\?name=(.+)$/.exec(rawEndpoint)
    if (verb === 'POST' && upload) {
      const found = findRelease(state, Number(upload[1]))
      if (!found) {
        respond(404, { message: 'Not Found' })
        return
      }
      const name = decodeURIComponent(upload[2])
      if (!expectedNames(version).includes(name)) throw new Error(`unexpected upload name: ${name}`)
      const input = argValue(argv, '--input')
      const expectedPath = join(process.env.RELEASE_GH_FIXTURE_ASSETS, name)
      if (input !== expectedPath) throw new Error(`upload --input must be ${expectedPath}, got ${input}`)
      const declared = head['content-type']
      if (declared !== contentTypeFor(name) && declared !== 'application/octet-stream') {
        throw new Error(`unexpected upload content type: ${declared}`)
      }
      if (found.assets.some((item) => item.name === name)) throw new Error(`duplicate asset upload: ${name}`)
      const bytes = readFileSync(expectedPath)
      const uploaded = uploadedAsset(state, name, bytes, declared)
      found.assets.push(uploaded)
      respond(201, publicAsset(uploaded))
      return
    }
    const assetById = /^repos\/kernalix7\/rtrt\/releases\/assets\/(\d+)$/.exec(endpoint)
    if (verb === 'GET' && assetById) {
      const found = findAsset(state, Number(assetById[1]))
      if (!found) {
        respond(404, { message: 'Not Found' })
        return
      }
      if ((head.accept ?? '').includes('application/octet-stream')) {
        respondRaw(Buffer.from(found.bytes, 'base64'))
        return
      }
      respond(200, publicAsset(found))
      return
    }
  }

  throw new Error(`unexpected fake gh argv: ${JSON.stringify(argv)}`)
}
