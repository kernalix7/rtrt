import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import { readFileSync } from "node:fs"
import path from "node:path"
import { gunzipSync } from "node:zlib"

const manifest = JSON.parse(readFileSync(new URL("./ring-0.17.14-source-headers.json", import.meta.url)))
const prefix = "ring-0.17.14/"
const grant = /(?:permission to use, copy|licensed under the apache license|permission is hereby granted)/i
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex")

function rejectUnsafe(entry, seen) {
  const regular = entry.type === "file" || entry.type === "0"
  const parts = entry.path.split("/")
  if (!regular || entry.path.startsWith("/") || entry.path.includes("\\") ||
    parts.some((part) => part === "" || part === "." || part === "..") || seen.has(entry.path))
    assert.fail(`unsafe ring archive member: ${entry.path}`)
  seen.add(entry.path)
}

function carriesGrant(bytes) {
  const lead = bytes.subarray(0, 3000).toString("utf8")
  return /copyright/i.test(lead) && grant.test(lead)
}

function membersFromArchive(archive) {
  const compressed = readFileSync(archive)
  assert.equal(hash(compressed), manifest.archiveSha256, "ring archive checksum mismatch")
  const bytes = gunzipSync(compressed)
  const members = []
  const seen = new Set()
  let offset = 0
  while (offset + 512 <= bytes.length) {
    const header = bytes.subarray(offset, offset + 512)
    if (header.every((value) => value === 0)) break
    const name = header.toString("utf8", 0, 100).replace(/\0.*$/s, "")
    const extended = header.toString("utf8", 345, 500).replace(/\0.*$/s, "")
    const full = extended ? `${extended}/${name}` : name
    const size = Number.parseInt(header.toString("ascii", 124, 136).replace(/\0.*$/s, "").trim(), 8)
    const typeflag = header[156] === 0 ? "0" : header.toString("ascii", 156, 157)
    if (!full.startsWith(prefix) || !Number.isSafeInteger(size) || size < 0)
      assert.fail(`unsafe ring archive member: ${full}`)
    const relative = full.slice(prefix.length)
    const type = typeflag === "0" ? "file" : typeflag
    rejectUnsafe({ path: relative, type }, seen)
    members.push({ path: relative, bytes: bytes.subarray(offset + 512, offset + 512 + size) })
    offset += 512 + Math.ceil(size / 512) * 512
  }
  return members
}

export function assertRingHeaderCoverage(discovered, mapped, options = {}) {
  if (options.entries) {
    const seen = new Set()
    for (const entry of options.entries) rejectUnsafe(entry, seen)
  }
  const pinned = new Set(manifest.paths)
  const unmapped = discovered.find((item) => !pinned.has(item))
  if (unmapped) assert.fail(`unmapped ring source header: ${unmapped}`)
  if (options.archiveBytes) {
    for (const [relative, expected] of options.archiveBytes) {
      const actual = readFileSync(path.join(options.directory, relative))
      if (!actual.equals(Buffer.from(expected))) assert.fail(`altered ring source grant: ${relative}`)
    }
  }
  const missing = manifest.paths.find((item) => !mapped.includes(item) || !discovered.includes(item))
  if (missing) assert.fail(`missing ring source header: ${missing}`)
}

export function ringSourceOverlay(archive, version) {
  assert.equal(version, "0.17.14", `unsupported ring version: ${version}`)
  const members = membersFromArchive(archive)
  const discovered = members.filter((member) => carriesGrant(member.bytes)).map((member) => member.path).sort()
  assertRingHeaderCoverage(discovered, manifest.paths)
  const wanted = new Set(manifest.paths)
  return members.filter((member) => wanted.has(member.path))
}
