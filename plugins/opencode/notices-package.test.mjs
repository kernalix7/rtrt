import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

const root = fileURLToPath(new URL("../../", import.meta.url))
const noticeFiles = [
  "INDEX.md",
  ...["cytoscape-fcose@2.2.0", "cytoscape-cola@2.5.1", "cose-base@2.2.0",
    "layout-base@2.0.1", "webcola@3.4.0", "cytoscape@3.30.2",
    "subtle@2.6.1", "webpki-roots@0.26.11", "webpki-roots@1.0.7"].map((name) => `${name}/LICENSE`),
  "ring@0.17.14/LICENSE",
  "ring@0.17.14/LICENSE-BoringSSL",
  "ring@0.17.14/LICENSE-other-bits",
  "ring@0.17.14/src/polyfill/once_cell/LICENSE-APACHE",
  "ring@0.17.14/src/polyfill/once_cell/LICENSE-MIT",
  "ring@0.17.14/third_party/fiat/LICENSE",
  "option-ext@0.2.0/LICENSE.txt",
].map((file) => `THIRD_PARTY_NOTICES/${file}`).sort()
const targets = [
  ["x86_64-unknown-linux-gnu", "linux-x64", "rtrt-dashboard"],
  ["aarch64-unknown-linux-gnu", "linux-arm64", "rtrt-dashboard"],
  ["x86_64-apple-darwin", "darwin-x64", "rtrt-dashboard"],
  ["aarch64-apple-darwin", "darwin-arm64", "rtrt-dashboard"],
  ["x86_64-pc-windows-msvc", "win32-x64", "rtrt-dashboard.exe"],
]

test("indexed notice digests match the distributed source files", async () => {
  const index = await readFile(path.join(root, "THIRD_PARTY_NOTICES/INDEX.md"), "utf8")
  const digests = new Map()
  for (const line of index.split(/\r?\n/)) {
    const entry = line.match(/^\| `([^`]+)` \| `([0-9a-f]{64})` \|$/)
    if (entry) digests.set(entry[1], entry[2])
  }
  const files = noticeFiles.filter((file) => !file.endsWith("/INDEX.md"))
  assert.deepEqual([...digests.keys()].sort(), files.map((file) => file.slice("THIRD_PARTY_NOTICES/".length)))
  for (const file of files) {
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
  assert.deepEqual(manifest.files.filter((file) => file.startsWith("THIRD_PARTY_NOTICES/")).sort(), noticeFiles)

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
  assert.deepEqual(packed.files.map(({ path: file }) => file).sort(), ["package.json", ...manifest.files].sort())
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
  const manifest = JSON.parse(await readFile(path.join(root, "plugins/opencode/package.json"), "utf8"))
  const notices = manifest.files.filter((file) => file.startsWith("THIRD_PARTY_NOTICES/"))
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
