import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import { mkdir, readFile, writeFile } from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"
import { ringSourceOverlay } from "./ring-source-headers.mjs"

export { assertRingHeaderCoverage } from "./ring-source-headers.mjs"

const root = fileURLToPath(new URL("../../", import.meta.url))
const notices = path.join(root, "THIRD_PARTY_NOTICES")
const roots = ["rtrt-cli", "rtrt-mcp", "rtrt-dashboard"]
const targets = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
  "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"]
const registry = "registry+https://github.com/rust-lang/crates.io-index"
const noticeName = /^(?:LICEN[CS]E|COPYING|COPYRIGHT|NOTICE)(?:[._-].*)?$/i
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex")
const cargo = (args) => execFileSync("cargo", ["--color", "never", ...args], {
  cwd: root, encoding: "utf8", maxBuffer: 32 * 1024 * 1024,
  env: { ...process.env, CARGO_NET_OFFLINE: "true" },
})

function noticePaths(archive, id) {
  const prefix = `${id.replace("@", "-")}/`
  const entries = execFileSync("tar", ["-tzf", archive], { encoding: "utf8" }).split("\n")
  return entries.filter((entry) => entry.startsWith(prefix) && !entry.endsWith("/"))
    .map((entry) => entry.slice(prefix.length))
    .filter((entry) => !entry.split("/").includes("..") && noticeName.test(path.posix.basename(entry)))
    .sort()
}

function releaseGraph(metadata) {
  const packages = new Map(metadata.packages.map((item) => [`${item.name}@${item.version}`, item]))
  const graph = {}
  for (const target of targets) {
    graph[target] = {}
    for (const binary of roots) {
      const lines = cargo(["tree", "--locked", "--offline", "--target", target,
        "-e", "normal,build", "-p", binary, "--prefix", "none", "--format", "{p}"])
      const nodes = new Set()
      for (const line of lines.split("\n")) {
        if (!line) continue
        const match = /^([^\s]+) v([^\s]+)/.exec(line)
        assert.ok(match, `unparsed Cargo graph line: ${line}`)
        const id = `${match[1]}@${match[2]}`
        assert.ok(packages.has(id), `unresolved Cargo graph node ${id}`)
        if (packages.get(id).source === registry) nodes.add(id)
        else assert.equal(packages.get(id).source, null, `unknown source for ${id}`)
      }
      graph[target][binary] = [...nodes].sort()
    }
  }
  return graph
}

export function assertGraphCoverage(graph, components) {
  const ids = new Set(components.map((component) => {
    const id = `${component.name}@${component.version}`
    assert.ok(component.files?.length > 0, `missing notice entries: ${id}`)
    return id
  }))
  assert.equal(ids.size, components.length, "duplicate component identity")
  for (const [target, matrix] of Object.entries(graph))
    for (const [binary, nodes] of Object.entries(matrix))
      for (const id of nodes) assert.ok(ids.has(id), `missing notice mapping: ${target}/${binary}/${id}`)
}

export async function assertSupplementCoverage(components, supplements, vcsById, directory = notices) {
  const needed = components.filter((component) => component.upstreamNoticeFiles === 0)
  const byId = new Map(supplements.map((item) => [`${item.name}@${item.version}`, item]))
  assert.equal(byId.size, supplements.length, "duplicate supplement identity")
  for (const component of needed) {
    const id = `${component.name}@${component.version}`
    const entry = byId.get(id)
    assert.ok(entry, `missing full-text supplement: ${id}`)
    assert.equal(entry.crateChecksum, component.checksum, `supplement checksum mismatch: ${id}`)
    assert.equal(entry.componentCommit, vcsById.get(id), `supplement VCS mismatch: ${id}`)
    assert.match(entry.componentCommit, /^[0-9a-f]{40}$/, `missing crate VCS pin: ${id}`)
    assert.ok(component.spdx.split(" OR ").includes(entry.selectedSpdx), `invalid selected SPDX: ${id}`)
    assert.ok(["same-commit", "later-same-author", "declared-standard"].includes(entry.correspondence),
      `invalid correspondence: ${id}`)
    if (entry.correspondence === "declared-standard") {
      assert.equal(entry.licenseCommit, null, `standard text has no upstream commit: ${id}`)
      assert.equal(entry.selectedSpdx, "Apache-2.0", `standard choice must be Apache: ${id}`)
    } else {
      assert.match(entry.licenseCommit, /^[0-9a-f]{40}$/, `missing license commit pin: ${id}`)
      if (entry.correspondence === "same-commit")
        assert.equal(entry.licenseCommit, entry.componentCommit, `wrong same-commit pin: ${id}`)
      else assert.equal(entry.selectedSpdx, "Apache-2.0", `later choice must be Apache: ${id}`)
    }
    assert.ok(entry.files?.length > 0, `missing full-text supplement: ${id}`)
    for (const file of entry.files) {
      assert.ok(file.path.startsWith(`${id}/`) && !file.path.includes("\\") &&
        !file.path.split("/").includes("..") &&
        file.path !== `${id}/Cargo.toml`, `invalid supplement path: ${id}`)
      assert.match(file.sha256, /^[0-9a-f]{64}$/, `missing supplement digest: ${id}`)
      assert.ok(file.sourcePath && (entry.correspondence === "declared-standard"
        ? file.url === "https://www.apache.org/licenses/LICENSE-2.0.txt" &&
          file.sha256 === "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
        : file.url.startsWith("https://raw.githubusercontent.com/") &&
          file.url.includes(`/${entry.licenseCommit}/`) && file.url.endsWith(`/${file.sourcePath}`)),
      `unversioned supplement source: ${id}`)
      let bytes
      try { bytes = await readFile(path.join(directory, file.path)) }
      catch (error) {
        if (error.code !== "ENOENT") throw error
        assert.fail(`missing supplement text: ${file.path}`)
      }
      assert.equal(hash(bytes), file.sha256, `supplement digest mismatch: ${file.path}`)
      assert.ok(bytes.length > 100, `incomplete supplement text: ${file.path}`)
    }
  }
  for (const entry of supplements)
    assert.ok(needed.some((item) => item.name === entry.name && item.version === entry.version),
      `unexpected supplement: ${entry.name}@${entry.version}`)
  assert.equal(byId.size, needed.length, "supplement count differs from metadata-only crates")
}

export function registryArchiveFor(pkg) {
  const sourceRoot = path.dirname(path.dirname(pkg.manifest_path))
  return path.join(path.dirname(path.dirname(sourceRoot)), "cache", path.basename(sourceRoot),
    `${pkg.name}-${pkg.version}.crate`)
}

export async function inventory(write = false) {
  const metadata = JSON.parse(cargo(["metadata", "--locked", "--offline", "--format-version", "1"]))
  const lock = await readFile(path.join(root, "Cargo.lock"), "utf8")
  const checksums = new Map([...lock.matchAll(/\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"\nsource = "registry\+https:\/\/github\.com\/rust-lang\/crates\.io-index"\nchecksum = "([0-9a-f]{64})"/g)]
    .map((match) => [`${match[1]}@${match[2]}`, match[3]]))
  const graph = releaseGraph(metadata)
  const packageById = new Map(metadata.packages.map((item) => [`${item.name}@${item.version}`, item]))
  const ids = [...new Set(Object.values(graph).flatMap((matrix) => Object.values(matrix).flat()))].sort()
  const supplements = JSON.parse(await readFile(path.join(notices, "SUPPLEMENTS.json"), "utf8"))
  assert.equal(supplements.schema, 1, "unsupported supplement schema")
  const vcsById = new Map()
  const components = []
  for (const id of ids) {
    const pkg = packageById.get(id)
    assert.equal(pkg.source, registry)
    const checksum = checksums.get(id)
    assert.match(checksum, /^[0-9a-f]{64}$/, `missing locked checksum: ${id}`)
    assert.ok(pkg.license, `missing SPDX metadata for ${id}`)
    const archive = registryArchiveFor(pkg)
    assert.equal(hash(await readFile(archive)), checksum, `registry archive mismatch: ${id}`)
    const overlayBytes = new Map((pkg.name === "ring" ? ringSourceOverlay(archive, pkg.version) : [])
      .map((file) => [file.path, file.bytes]))
    const upstreamNotices = noticePaths(archive, id)
    if (!upstreamNotices.length) {
      const vcs = execFileSync("tar", ["-xOzf", archive, `${pkg.name}-${pkg.version}/.cargo_vcs_info.json`])
      vcsById.set(id, JSON.parse(vcs).git.sha1)
    }
    // Some registry archives contain SPDX metadata but no license file. Preserve
    // their original manifest as evidence; do not manufacture an upstream text.
    const paths = [...new Set([...upstreamNotices.length ? upstreamNotices : ["Cargo.toml"],
      ...overlayBytes.keys()])].sort()
    const files = []
    for (const relative of paths) {
      const upstream = `${pkg.name}-${pkg.version}/${relative}`
      const bytes = overlayBytes.get(relative) ??
        execFileSync("tar", ["-xOzf", archive, upstream], { maxBuffer: 16 * 1024 * 1024 })
      const destination = path.join(notices, id, relative)
      if (write) {
        await mkdir(path.dirname(destination), { recursive: true })
        await writeFile(destination, bytes)
      }
      assert.equal(hash(await readFile(destination)), hash(bytes), `missing/altered notice: ${id}/${relative}`)
      files.push({ path: relative, sha256: hash(bytes) })
    }
    components.push({ name: pkg.name, version: pkg.version, checksum,
      declaredLicense: pkg.license, spdx: pkg.license.replace(/\s*\/\s*/g, " OR "),
      source: `https://static.crates.io/crates/${pkg.name}/${pkg.name}-${pkg.version}.crate`,
      upstreamNoticeFiles: upstreamNotices.length, files })
  }
  const result = { schema: 1, scope: "default release normal+build conservative union, not retained-code SBOM",
    roots, targets, graph, components }
  assertGraphCoverage(graph, components)
  await assertSupplementCoverage(components, supplements.components, vcsById)
  const serialized = `${JSON.stringify(result, null, 2)}\n`
  if (write) await writeFile(path.join(notices, "INVENTORY.json"), serialized)
  else assert.equal(await readFile(path.join(notices, "INVENTORY.json"), "utf8"), serialized, "stale release graph mapping")
  return result
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  assert.ok(process.argv.length === 2 || (process.argv.length === 3 && process.argv[2] === "--write"),
    "usage: node packaging/licenses/notice-inventory.mjs [--write]")
  const result = await inventory(process.argv[2] === "--write")
  console.log(`notice coverage: ${result.components.length} registry components; ${targets.length} targets × ${roots.length} binaries`)
}
