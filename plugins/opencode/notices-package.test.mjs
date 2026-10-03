import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import { cp, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { assertGraphCoverage, assertSupplementCoverage, registryArchiveFor } from "../../packaging/licenses/notice-inventory.mjs"

const root = fileURLToPath(new URL("../../", import.meta.url))
async function filesIn(directory, prefix = "") {
  const files = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const relative = path.posix.join(prefix, entry.name)
    if (entry.isDirectory()) files.push(...await filesIn(path.join(directory, entry.name), relative))
    else if (entry.isFile()) files.push(relative)
  }
  return files.sort()
}
const noticeFiles = (await filesIn(path.join(root, "THIRD_PARTY_NOTICES")))
  .map((file) => `THIRD_PARTY_NOTICES/${file}`)
const targets = [
  ["x86_64-unknown-linux-gnu", "linux-x64", "rtrt-dashboard"],
  ["aarch64-unknown-linux-gnu", "linux-arm64", "rtrt-dashboard"],
  ["x86_64-apple-darwin", "darwin-x64", "rtrt-dashboard"],
  ["aarch64-apple-darwin", "darwin-arm64", "rtrt-dashboard"],
  ["x86_64-pc-windows-msvc", "win32-x64", "rtrt-dashboard.exe"],
]

test("matchit binary notice carries both independently licensed upstream texts", async () => {
  // Given: matchit in the release graph and its distinct upstream LICENSE files.
  const manifest = JSON.parse(await readFile(path.join(root, "plugins/opencode/package.json"), "utf8"))
  // When: consumers inspect the source package inventory.
  const entries = manifest.files
  // Then: both MIT and BSD terms, including their respective owners, are available.
  assert.ok(entries.includes("THIRD_PARTY_NOTICES"))
  for (const name of ["LICENSE", "LICENSE.httprouter"])
    assert.ok(noticeFiles.includes(`THIRD_PARTY_NOTICES/matchit@0.8.4/${name}`))
})

test("new release graph nodes must have a source and text mapping", async () => {
  // Given: tokio is a default/release graph node on every target.
  const graph = execFileSync("cargo", ["tree", "--locked", "--offline", "--target", targets[0][0],
    "-e", "normal,build", "-p", "rtrt-cli", "--prefix", "none", "--format", "{p}"], { cwd: root, encoding: "utf8" })
  // When: we compare its graph identity to the versioned shipped mapping.
  assert.match(graph, /^tokio v1\.52\.3(?:\s|$)/m)
  const inventory = JSON.parse(await readFile(path.join(root, "THIRD_PARTY_NOTICES/INVENTORY.json"), "utf8"))
  // Then: it has pinned provenance and an actual delivered notice file.
  const tokio = inventory.components.find((component) => component.name === "tokio" && component.version === "1.52.3")
  assert.ok(tokio?.checksum && tokio?.files?.length > 0)
})

test("graph coverage gate rejects a newly resolved, unmapped node", () => {
  // Given: an additional normal/build graph identity absent from the notice inventory.
  const graph = { "x86_64-unknown-linux-gnu": { "rtrt-cli": ["new-crate@9.9.9"] } }
  // When / Then: no newly resolved dependency can pass without a notice mapping.
  assert.throws(() => assertGraphCoverage(graph, []), /missing notice mapping.*new-crate@9\.9\.9/)
})

test("graph coverage gate rejects a mapped component with missing notice entries", () => {
  // Given: a real graph identity mapped without any distributable file.
  const graph = { "x86_64-unknown-linux-gnu": { "rtrt-cli": ["matchit@0.8.4"] } }
  // When / Then: a metadata-only empty record cannot satisfy license delivery.
  assert.throws(() => assertGraphCoverage(graph, [{ name: "matchit", version: "0.8.4", files: [] }]),
    /missing notice entries.*matchit@0\.8\.4/)
})

test("L2 coverage rejects a Cargo.toml-only component without a full-text supplement", async () => {
  // Given: a checksum-pinned crate archive containing only metadata as its notice.
  const components = [{ name: "example", version: "1.0.0", checksum: "a".repeat(64),
    spdx: "MIT", upstreamNoticeFiles: 0, files: [{ path: "Cargo.toml" }] }]
  // When / Then: manifest metadata cannot substitute for full license terms.
  await assert.rejects(assertSupplementCoverage(components, [], new Map(), root),
    /missing full-text supplement.*example@1\.0\.0/)
})

test("L2 coverage rejects a supplement pinned to another archive or source commit", async () => {
  // Given: a crate whose archive checksum and VCS pin differ from its supplement.
  const components = [{ name: "example", version: "1.0.0", checksum: "a".repeat(64),
    spdx: "MIT", upstreamNoticeFiles: 0, files: [{ path: "Cargo.toml" }] }]
  const supplement = { name: "example", version: "1.0.0", crateChecksum: "b".repeat(64),
    componentCommit: "c".repeat(40), licenseCommit: "c".repeat(40),
    correspondence: "same-commit", selectedSpdx: "MIT", files: [] }
  // When / Then: neither a stale archive checksum nor a stale VCS pin passes.
  await assert.rejects(assertSupplementCoverage(components, [supplement],
    new Map([["example@1.0.0", "d".repeat(40)]]), root), /supplement checksum mismatch/)
  supplement.crateChecksum = components[0].checksum
  await assert.rejects(assertSupplementCoverage(components, [supplement],
    new Map([["example@1.0.0", "d".repeat(40)]]), root), /supplement VCS mismatch/)
})

test("L2 coverage rejects missing or altered full text despite a matching manifest", async (t) => {
  // Given: the correct locked archive and VCS identity, but a missing/altered text.
  const directory = await mkdtemp(path.join(root, ".rtrt/tmp/l2-test-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const components = [{ name: "example", version: "1.0.0", checksum: "a".repeat(64),
    spdx: "MIT", upstreamNoticeFiles: 0, files: [{ path: "Cargo.toml" }] }]
  const supplement = { name: "example", version: "1.0.0", crateChecksum: components[0].checksum,
    componentCommit: "c".repeat(40), licenseCommit: "c".repeat(40),
    correspondence: "same-commit", selectedSpdx: "MIT",
    files: [{ path: "example@1.0.0/LICENSE-SUPPLEMENT", sourcePath: "LICENSE",
      url: `https://raw.githubusercontent.com/example/example/${"c".repeat(40)}/LICENSE`,
      sha256: createHash("sha256").update("full original license text").digest("hex") }] }
  const vcs = new Map([["example@1.0.0", "c".repeat(40)]])
  // When / Then: a mere Cargo.toml or a changed local file cannot pass.
  await assert.rejects(assertSupplementCoverage(components, [supplement], vcs, directory),
    /missing supplement text/)
  await mkdir(path.join(directory, "example@1.0.0"))
  await writeFile(path.join(directory, "example@1.0.0/LICENSE-SUPPLEMENT"), "wrong text")
  await assert.rejects(assertSupplementCoverage(components, [supplement], vcs, directory),
    /supplement digest mismatch/)
})

test("registry archive location follows Cargo metadata rather than a fixed cache key", () => {
  // Given: a different official registry source directory from Cargo metadata.
  const pkg = { name: "example", version: "1.0.0",
    manifest_path: "/isolated/cargo/registry/src/index.crates.io-different/example-1.0.0/Cargo.toml" }
  // When / Then: the candidate comes from that source tree's matching cache namespace.
  assert.equal(registryArchiveFor(pkg),
    "/isolated/cargo/registry/cache/index.crates.io-different/example-1.0.0.crate")
})

test("eight metadata-only crates keep pinned full texts and the declared Apache choice", async () => {
  // Given: the checksum-pinned supplement manifest and the shipped whatlang text.
  const { components } = JSON.parse(await readFile(path.join(root, "THIRD_PARTY_NOTICES/SUPPLEMENTS.json"), "utf8"))
  const byName = new Map(components.map((item) => [item.name, item]))
  const whatlang = await readFile(path.join(root, "THIRD_PARTY_NOTICES/whatlang@0.16.4/LICENSE"), "utf8")
  // When / Then: exact VCS correspondence survives, including five copyright lines and no invented MIT text.
  assert.equal(byName.size, 8)
  assert.equal([...whatlang.matchAll(/^Copyright /gm)].length, 5)
  assert.equal(byName.get("sse-stream").componentCommit, "70fd1a9da602060069e9da6e337a17be4f496455")
  assert.equal(byName.get("sse-stream").licenseCommit, "9b95874ca02cc337b9f86b54915ba6c6b27821d0")
  assert.equal(byName.get("sse-stream").files.length, 2)
  assert.equal(byName.get("eventsource-stream").correspondence, "declared-standard")
  assert.equal(byName.get("eventsource-stream").selectedSpdx, "Apache-2.0")
  assert.equal(byName.get("instant-distance").selectedSpdx, "Apache-2.0")
  assert.equal(byName.get("rmcp").files[0].sha256, byName.get("rmcp-macros").files[0].sha256)
})

test("declared-standard Apache text cannot be mislabeled as an original upstream LICENSE", async () => {
  // Given: a crate with an Apache OR MIT declaration but no archived license file.
  const components = [{ name: "example", version: "1.0.0", checksum: "a".repeat(64),
    spdx: "MIT OR Apache-2.0", upstreamNoticeFiles: 0, files: [{ path: "Cargo.toml" }] }]
  const supplement = { name: "example", version: "1.0.0", crateChecksum: components[0].checksum,
    componentCommit: "c".repeat(40), licenseCommit: "c".repeat(40),
    correspondence: "declared-standard", selectedSpdx: "Apache-2.0", files: [] }
  // When / Then: no fictitious upstream license commit is accepted for canonical terms.
  await assert.rejects(assertSupplementCoverage(components, [supplement],
    new Map([["example@1.0.0", "c".repeat(40)]]), root), /standard text has no upstream commit/)
})

test("inventory normalizes legacy slash-separated license metadata to SPDX OR", async () => {
  // Given: a registry crate's legacy MIT/Apache-2.0 metadata.
  const inventory = JSON.parse(await readFile(path.join(root, "THIRD_PARTY_NOTICES/INVENTORY.json")))
  const component = inventory.components.find((item) => item.name === "sse-stream")
  // When / Then: both the exact upstream declaration and the parsed SPDX choice survive.
  assert.equal(component.declaredLicense, "MIT/Apache-2.0")
  assert.equal(component.spdx, "MIT OR Apache-2.0")
})

test("readable JamaJS attribution retains the byte-exact embedded Apache section", async () => {
  // Given: the versioned dashboard bundle and a separate distribution notice.
  const source = await readFile(path.join(root, "crates/rtrt-dashboard/ui/vendor/layout-base.js"), "utf8")
  const notice = await readFile(path.join(root, "THIRD_PARTY_NOTICES/layout-base@2.0.1/JamaJS-APACHE.txt"), "utf8")
  // When: extracting the SVD attribution and its Apache terms from the bundled JS.
  const start = source.indexOf("/* Below singular value decomposition")
  const end = source.indexOf("*/", source.indexOf("END OF TERMS AND CONDITIONS", start)) + 2
  // Then: recipients get those exact bytes in a readable standalone file.
  assert.ok(start > 0 && end > start)
  assert.equal(createHash("sha256").update(source).digest("hex"),
    "ec15ab5df9af3f20708f4faab994accf91cda71848cd5bb10a23432cc50b6745")
  assert.equal(notice, `${source.slice(start, end)}\n`)
})

test("option-ext source archive contains the pinned complete upstream crate", async () => {
  // Given: the locally bundled crate and the resolved dependency lock.
  const file = path.join(root, "THIRD_PARTY_NOTICES/option-ext@0.2.0/SOURCE.crate")
  const lock = await readFile(path.join(root, "Cargo.lock"), "utf8")

  // When: a recipient verifies the source archive and enumerates its members.
  const digest = createHash("sha256").update(await readFile(file)).digest("hex")
  const members = execFileSync("tar", ["-tzf", file], { encoding: "utf8" }).trim().split("\n")

  // Then: exact upstream bytes include package metadata, license, and all source files.
  assert.equal(digest, "04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d")
  assert.match(lock, /name = "option-ext"\nversion = "0\.2\.0"\nsource = "registry\+https:\/\/github\.com\/rust-lang\/crates\.io-index"\nchecksum = "04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d"/)
  assert.deepEqual(members.sort(), [
    ".cargo_vcs_info.json", ".gitignore", "Cargo.toml", "Cargo.toml.orig",
    "LICENSE.txt", "README.md", "src/impl.rs", "src/lib.rs",
  ].map((member) => `option-ext-0.2.0/${member}`).sort())
  assert.deepEqual(execFileSync("tar", ["-xOzf", file, "option-ext-0.2.0/LICENSE.txt"]),
    await readFile(path.join(root, "THIRD_PARTY_NOTICES/option-ext@0.2.0/LICENSE.txt")))
})

test("indexed notice digests match the distributed source files", async () => {
  const index = await readFile(path.join(root, "THIRD_PARTY_NOTICES/INDEX.md"), "utf8")
  const digests = new Map()
  for (const line of index.split(/\r?\n/)) {
    const entry = line.match(/^\| `([^`]+)` \| `([0-9a-f]{64})` \|$/)
    if (entry) digests.set(entry[1], entry[2])
  }
  for (const file of [...digests.keys()].map((entry) => `THIRD_PARTY_NOTICES/${entry}`)) {
    const bytes = await readFile(path.join(root, file))
    const digest = createHash("sha256").update(bytes).digest("hex")
    assert.equal(digest, digests.get(file.slice("THIRD_PARTY_NOTICES/".length)), file)
  }
})

test("release manifest stages identical notice bytes into five platforms and agent", async (t) => {
  // Given: the explicit version-pinned source notice manifest and a fixture executable.
  const manifest = JSON.parse(await readFile(path.join(root, "plugins/opencode/package.json"), "utf8"))
  const tempRoot = path.join(root, ".rtrt/tmp/v016-notices")
  await mkdir(tempRoot, { recursive: true })
  const directory = await mkdtemp(path.join(tempRoot, "pack-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const source = path.join(directory, "dashboard")
  await writeFile(source, "fixture executable\n")
  const expected = new Map()
  for (const file of noticeFiles) expected.set(file, await readFile(path.join(root, file)))
  assert.ok(manifest.files.includes("THIRD_PARTY_NOTICES"))

  // When: release staging and npm pack run for every platform and the agent.
  const env = { ...process.env, RELEASE_VERSION: manifest.version, npm_config_cache: path.join(directory, "cache") }
  for (const [target, platform, binary] of targets) {
    const output = path.join(directory, platform)
    execFileSync(process.execPath, [path.join(root, "packaging/npm/stage-dashboard.mjs"), target, source, output], { env })
    const staged = path.join(output, `rtrt-dashboard-${platform}`)
    const [packed] = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", directory], { cwd: staged, env }))
    const archive = path.join(directory, packed.filename)
    // Then: exact allowlist and bytes survive each real npm tarball.
    assert.deepEqual(packed.files.map(({ path: file }) => file).sort(), ["LICENSE", "package.json", `bin/${binary}`, ...noticeFiles].sort())
    for (const [file, bytes] of expected)
      assert.deepEqual(execFileSync("tar", ["-xOf", archive, `package/${file}`]), bytes, `${platform}: ${file}`)
  }
  const agent = path.join(directory, "agent")
  execFileSync(process.execPath, [path.join(root, "packaging/npm/stage-agent.mjs"), agent], { env })
  const [packed] = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", directory], { cwd: agent, env }))
  assert.deepEqual(packed.files.map(({ path: file }) => file).sort(), ["package.json",
    ...manifest.files.filter((file) => file !== "THIRD_PARTY_NOTICES"), ...noticeFiles].sort())
  for (const [file, bytes] of expected)
    assert.deepEqual(execFileSync("tar", ["-xOf", path.join(directory, packed.filename), `package/${file}`]), bytes, `agent: ${file}`)

  // Then: the binary-archive stage copies the same canonical notice directory for all matrix targets.
  const workflow = await readFile(path.join(root, ".github/workflows/release.yml"), "utf8")
  assert.match(workflow, /cp -R THIRD_PARTY_NOTICES "\$stage\/"/)
  for (const [target] of targets) assert.ok(workflow.includes(`target: ${target}`))
})

test("release workflow builds five archives with the exact notice manifest", async (t) => {
  // Given: release's actual archive step, fixture executables, and canonical notice sources.
  const workflow = JSON.parse(execFileSync("ruby", ["-rpsych", "-rjson", "-e",
    "puts JSON.generate(Psych.safe_load_file(ARGV[0], permitted_classes: [], aliases: false))",
    path.join(root, ".github/workflows/release.yml"),
  ]))
  const stage = workflow.jobs.build.steps.find((step) => step.name === "Stage archive and checksum").run
  const tempRoot = path.join(root, ".rtrt/tmp/v016-notices")
  await mkdir(tempRoot, { recursive: true })
  const directory = await mkdtemp(path.join(tempRoot, "archive-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await cp(path.join(root, "THIRD_PARTY_NOTICES"), path.join(directory, "THIRD_PARTY_NOTICES"), { recursive: true })
  for (const file of ["LICENSE", "README.md", "CHANGELOG.md"])
    await cp(path.join(root, file), path.join(directory, file))
  await mkdir(path.join(directory, "scripts"))
  await cp(path.join(root, "scripts/release-helpers.sh"), path.join(directory, "scripts/release-helpers.sh"))
  await cp(path.join(root, "scripts/stage-release-archive.mjs"), path.join(directory, "scripts/stage-release-archive.mjs"))
  const manifest = JSON.parse(await readFile(path.join(root, "plugins/opencode/package.json"), "utf8"))
  const notices = noticeFiles
  for (const [target, , binary] of targets) {
    const output = path.join(directory, "target", target, "release")
    await mkdir(output, { recursive: true })
    for (const name of ["rtrt", "rtrt-mcp", "rtrt-dashboard"])
      await writeFile(path.join(output, `${name}${binary.endsWith(".exe") ? ".exe" : ""}`), `fixture:${target}:${name}\n`)
    const extension = binary.endsWith(".exe") ? "zip" : "tar.gz"
    const name = `rtrt-${manifest.version}-${target}`
    // When: the exact matrix packaging shell step produces the binary archive.
    execFileSync("bash", ["-e", "-c", stage], { cwd: directory, env: {
      ...process.env, RELEASE_VERSION: manifest.version, RELEASE_TARGET: target, RELEASE_EXT: extension,
      GITHUB_OUTPUT: path.join(directory, `${target}.output`),
    } })
    const archive = path.join(directory, "dist", `${name}.${extension}`)
    const entries = extension === "zip"
      ? execFileSync("7z", ["l", "-slt", archive], { encoding: "utf8" }).split(/\r?\n\r?\n/)
        .flatMap((block) => {
          const entry = block.match(/^Path = (.+)$/m)?.[1]
          const attributes = block.match(/^Attributes = (.+)$/m)?.[1]
          return entry?.startsWith(`${name}/`) && !attributes?.startsWith("D") ? [entry] : []
        })
      : execFileSync("tar", ["-tzf", archive], { encoding: "utf8" }).trim().split("\n")
        .filter((entry) => !entry.endsWith("/"))
    assert.deepEqual(entries.sort(), ["LICENSE", "README.md", "CHANGELOG.md", ...notices,
      ...["rtrt", "rtrt-mcp", "rtrt-dashboard"].map((file) => `${file}${binary.endsWith(".exe") ? ".exe" : ""}`),
    ].map((file) => `${name}/${file}`).sort(), target)
    const extract = (file) => extension === "zip"
      ? execFileSync("7z", ["x", "-so", archive, `${name}/${file}`])
      : execFileSync("tar", ["-xOf", archive, `${name}/${file}`])
    // Then: every archive contains identical notices and the expected binary bytes.
    for (const file of notices)
      assert.deepEqual(extract(file), await readFile(path.join(root, file)), `${target}: ${file}`)
    for (const file of ["rtrt", "rtrt-mcp", "rtrt-dashboard"])
      assert.equal(extract(`${file}${binary.endsWith(".exe") ? ".exe" : ""}`).toString(), `fixture:${target}:${file}\n`)
  }
})
