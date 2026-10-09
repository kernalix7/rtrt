import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { registryArchiveFor } from "../../packaging/licenses/notice-inventory.mjs"
import { ringSourceOverlay } from "../../packaging/licenses/ring-source-headers.mjs"

async function ringCoverage() {
  const module = await import("../../packaging/licenses/notice-inventory.mjs")
  assert.equal(typeof module.assertRingHeaderCoverage, "function")
  return module.assertRingHeaderCoverage
}

const root = fileURLToPath(new URL("../../", import.meta.url))
const archiveChecksum = "a4689e6c2294d81e88dc6261c768b63bc4fcdb852be6d1352498b114f61383b7"
const grant = /(?:permission to use, copy|licensed under the apache license|permission is hereby granted)/i
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex")

function metadata() {
  return JSON.parse(execFileSync("cargo", ["metadata", "--locked", "--offline", "--format-version", "1"], {
    cwd: root, encoding: "utf8", maxBuffer: 32 * 1024 * 1024,
    env: { ...process.env, CARGO_NET_OFFLINE: "true" },
  }))
}

async function archive() {
  const pkg = metadata().packages.find((item) => item.name === "ring" && item.version === "0.17.14")
  const file = registryArchiveFor(pkg)
  assert.equal(hash(await readFile(file)), archiveChecksum)
  return file
}

function members(file) {
  return execFileSync("tar", ["-tzf", file], { encoding: "utf8" }).trim().split("\n")
    .map((entry) => entry.slice("ring-0.17.14/".length))
}

function headerPaths(file) {
  return members(file).filter((relative) => {
    const bytes = execFileSync("tar", ["-xOzf", file, `ring-0.17.14/${relative}`], { maxBuffer: 16 * 1024 * 1024 })
    return /copyright/i.test(bytes.subarray(0, 3000).toString("utf8")) && grant.test(bytes.subarray(0, 3000).toString("utf8"))
  }).sort()
}

test("real ring archive baseline copies every leading grant header byte-identically", async () => {
  // Given: the checksum-pinned ring 0.17.14 registry archive and its current notice mapping.
  const file = await archive()
  const inventory = JSON.parse(await readFile(path.join(root, "THIRD_PARTY_NOTICES/INVENTORY.json"), "utf8"))
  const ring = inventory.components.find((item) => item.name === "ring" && item.version === "0.17.14")
  const required = headerPaths(file)
  // When: comparing discovered source-header carriers with delivered files.
  const delivered = new Map(ring.files.map((item) => [item.path, item.sha256]))
  // Then: every original carrier is present and byte-identical, including the six existing licenses.
  assert.ok(required.length > 6)
  for (const relative of required) {
    const bytes = execFileSync("tar", ["-xOzf", file, `ring-0.17.14/${relative}`], { maxBuffer: 16 * 1024 * 1024 })
    assert.equal(delivered.get(relative), hash(bytes), relative)
    assert.deepEqual(await readFile(path.join(root, "THIRD_PARTY_NOTICES/ring@0.17.14", relative)), bytes)
  }
})

test("ring coverage rejects an omitted mapped source path", async () => {
  // Given: a discovered header carrier absent from the explicit overlay.
  const assertRingHeaderCoverage = await ringCoverage()
  const discovered = ["crypto/poly1305/poly1305.c", "src/limb.rs"]
  // When / Then: the pinned mapping cannot silently omit it.
  assert.throws(() => assertRingHeaderCoverage(discovered, ["src/limb.rs"]), /missing ring source header/)
})

test("ring coverage rejects a newly discovered unmapped header", async () => {
  // Given: a current archive member whose leading grant is absent from the pinned manifest.
  const assertRingHeaderCoverage = await ringCoverage()
  const discovered = ["crypto/poly1305/poly1305.c", "src/new_header.rs"]
  // When / Then: adding a header-bearing file without updating the manifest fails.
  assert.throws(() => assertRingHeaderCoverage(discovered, ["crypto/poly1305/poly1305.c"]), /unmapped ring source header/)
})

test("ring source overlay rejects a version outside the pinned manifest", async () => {
  // Given: the current checksum-pinned archive and a future ring package version.
  const file = await archive()
  // When / Then: a new version must fail even when the archive still matches the current pin.
  assert.throws(() => ringSourceOverlay(file, "0.17.15"), /unsupported ring version: 0\.17\.15/)
})

test("ring coverage rejects an altered grant despite a matching path", async (t) => {
  // Given: the correct source path whose copied grant bytes differ from the archive.
  const directory = await mkdtemp(path.join(root, ".rtrt/tmp/ring-header-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const relative = "src/limb.rs"
  await mkdir(path.dirname(path.join(directory, relative)), { recursive: true })
  await writeFile(path.join(directory, relative), "Copyright changed\nPermission to use, copy\n")
  // When / Then: path presence cannot hide changed original terms.
  const assertRingHeaderCoverage = await ringCoverage()
  assert.throws(() => assertRingHeaderCoverage([relative], [relative], {
    directory, archiveBytes: new Map([[relative, Buffer.from("Copyright 2016 David Judd.\nPermission to use, copy\n")]]),
  }), /altered ring source grant/)
})

test("ring archive validation rejects unsafe members before reading them", async () => {
  // Given: archive metadata containing an absolute path, traversal, link, or duplicate.
  const assertRingHeaderCoverage = await ringCoverage()
  const cases = [
    [{ path: "/etc/passwd", type: "file" }],
    [{ path: "../escape.rs", type: "file" }],
    [{ path: "src/link.rs", type: "symlink" }],
    [{ path: "src/limb.rs", type: "file" }, { path: "src/limb.rs", type: "file" }],
  ]
  // When / Then: each unsafe member is rejected before content inspection.
  for (const entries of cases)
    assert.throws(() => assertRingHeaderCoverage(["src/limb.rs"], ["src/limb.rs"], { entries }),
      /unsafe ring archive member/)
})
