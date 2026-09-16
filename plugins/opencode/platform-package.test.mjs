import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

const root = fileURLToPath(new URL("../../", import.meta.url))
const manifest = JSON.parse(await readFile(new URL("./package.json", import.meta.url), "utf8"))
const targets = [
  ["x86_64-unknown-linux-gnu", "linux", "x64"],
  ["aarch64-unknown-linux-gnu", "linux", "arm64"],
  ["x86_64-apple-darwin", "darwin", "x64"],
  ["aarch64-apple-darwin", "darwin", "arm64"],
  ["x86_64-pc-windows-msvc", "win32", "x64"],
]
const stageScript = path.join(root, "packaging/npm/stage-dashboard.mjs")

async function workspace(t) {
  const parent = path.join(root, ".rtrt/tmp")
  await mkdir(parent, { recursive: true })
  const directory = await mkdtemp(path.join(parent, "npm-contract-"))
  t.after(() => rm(directory, { recursive: true, force: true }))
  return directory
}

test("agent pins every platform to its own exact version without install hooks", () => {
  // Given: the source package version, independent of generated platform metadata.
  const expected = Object.fromEntries(targets.map(([, os, cpu]) => [
    `rtrt-dashboard-${os}-${cpu}`, manifest.version,
  ]))
  // When: npm reads the platform dependency contract.
  const actual = manifest.optionalDependencies
  // Then: every platform is exact; installation never requires scripts.
  assert.deepEqual(actual, expected)
  assert.deepEqual(manifest.bin, { "rtrt-dashboard-open": "./bin/rtrt-dashboard-open.js" })
  for (const hook of ["preinstall", "install", "postinstall", "prepare", "prepack"])
    assert.equal(manifest.scripts[hook], undefined)
})

for (const [target, os, cpu] of targets) {
  test(`${os}-${cpu} packs only executable and metadata without native artifacts`, async (t) => {
    // Given: a non-executable fixture, not a downloaded or built release binary.
    const directory = await workspace(t)
    const binary = os === "win32" ? "rtrt-dashboard.exe" : "rtrt-dashboard"
    const source = path.join(directory, binary)
    const bytes = Buffer.from(`fixture:${target}\n`)
    await writeFile(source, bytes, { mode: 0o644 })
    const output = path.join(directory, "staged")
    const env = { ...process.env, RELEASE_VERSION: manifest.version, npm_config_cache: path.join(directory, "cache") }
    // When: the release generator and real npm pack consume that fixture.
    execFileSync(process.execPath, [stageScript, target, source, output], { cwd: root, env })
    const cwd = path.join(output, `rtrt-dashboard-${os}-${cpu}`)
    const dry = JSON.parse(execFileSync("npm", ["pack", "--dry-run", "--ignore-scripts", "--json"], { cwd, env }))
    const packed = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--json"], { cwd, env }))
    const archive = path.join(cwd, packed[0].filename)
    const metadata = JSON.parse(execFileSync("tar", ["-xOf", archive, "package/package.json"]))
    // Then: filters, bytes and POSIX execute bits survive the actual tarball.
    const files = ["LICENSE", "package.json", `bin/${binary}`].sort()
    assert.deepEqual(dry[0].files.map((file) => file.path).sort(), files)
    assert.deepEqual(packed[0].files.map((file) => file.path).sort(), files)
    assert.equal(metadata.name, `rtrt-dashboard-${os}-${cpu}`)
    assert.equal(metadata.version, manifest.version)
    assert.deepEqual(metadata.os, [os])
    assert.deepEqual(metadata.cpu, [cpu])
    assert.equal(metadata.scripts, undefined)
    assert.equal(metadata.dependencies, undefined)
    assert.deepEqual(execFileSync("tar", ["-xOf", archive, `package/bin/${binary}`]), bytes)
    if (os !== "win32") {
      const listing = execFileSync("tar", ["-tvf", archive, `package/bin/${binary}`], { encoding: "utf8" })
      assert.match(listing, /^-rwxr-xr-x\s/)
    }
  })
}

test("generator rejects a release version different from source before staging", async (t) => {
  // Given: valid source bytes but a mismatched release version.
  const directory = await workspace(t)
  const source = path.join(directory, "rtrt-dashboard")
  await writeFile(source, "fixture")
  // When / Then: mismatched tags cannot label the binary with another package version.
  assert.throws(() => execFileSync(process.execPath, [stageScript, targets[0][0], source, directory], {
    cwd: root, env: { ...process.env, RELEASE_VERSION: "999.0.0" }, stdio: "pipe",
  }), /release version/)
})

test("generator rejects unsupported targets", async (t) => {
  // Given: a target outside the native build matrix.
  const directory = await workspace(t)
  // When / Then: no guessed platform name or architecture is published.
  assert.throws(() => execFileSync(process.execPath, [stageScript, "linux-riscv64", "missing", directory], {
    cwd: root, stdio: "pipe",
  }), /unsupported target/)
})

test("agent pack includes all declared runtime files and an executable opener", async (t) => {
  // Given: source runtime files, without any platform release artifacts.
  const directory = await workspace(t)
  const cwd = path.join(root, "plugins/opencode")
  const env = { ...process.env, npm_config_cache: path.join(directory, "cache") }
  execFileSync(process.execPath, [path.join(root, "packaging/npm/agent-contract.mjs")], { cwd, env })
  // When: release staging and npm create the tarball with lifecycle scripts disabled.
  const staged = path.join(directory, "agent")
  execFileSync(process.execPath, [path.join(root, "packaging/npm/stage-agent.mjs"), staged], { cwd, env })
  const [packed] = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", directory], { cwd: staged, env }))
  const archive = path.join(directory, packed.filename)
  // Then: no declared runtime is silently dropped; the bin keeps executable mode.
  assert.deepEqual(packed.files.map((file) => file.path).sort(), ["package.json", ...manifest.files].sort())
  const opener = execFileSync("tar", ["-tvf", archive, "package/bin/rtrt-dashboard-open.js"], { encoding: "utf8" })
  assert.match(opener, /^-rwxr-xr-x\s/)
  const metadata = JSON.parse(execFileSync("tar", ["-xOf", archive, "package/package.json"]))
  assert.deepEqual(metadata.optionalDependencies, manifest.optionalDependencies)
})

test("test dependency staging uses the locked SDK without release package resolution", async (t) => {
  // Given: an isolated install destination and the checked-in dependency lock.
  const directory = await workspace(t)
  const lock = JSON.parse(await readFile(path.join(root, "plugins/opencode/package-lock.json"), "utf8"))
  // When: the release job stages its dependency-only npm ci inputs.
  execFileSync(process.execPath, [path.join(root, "packaging/npm/stage-test-dependencies.mjs"), directory])
  // Then: npm can install the original locked SDK, independent of platform publication.
  const staged = JSON.parse(await readFile(path.join(directory, "package.json"), "utf8"))
  assert.deepEqual(staged.dependencies, manifest.dependencies)
  assert.equal(staged.optionalDependencies, undefined)
  assert.equal(staged.scripts, undefined)
  assert.deepEqual(JSON.parse(await readFile(path.join(directory, "package-lock.json"), "utf8")), lock)
})

test("platform version sync follows a changed source version without manual edits", async (t) => {
  // Given: a future-version source fixture with stale platform pins.
  const directory = await workspace(t)
  const scriptDirectory = path.join(directory, "packaging/npm")
  const packageDirectory = path.join(directory, "plugins/opencode")
  await mkdir(scriptDirectory, { recursive: true })
  await mkdir(packageDirectory, { recursive: true })
  const script = path.join(scriptDirectory, "agent-contract.mjs")
  await copyFile(path.join(root, "packaging/npm/agent-contract.mjs"), script)
  await writeFile(path.join(packageDirectory, "package.json"), JSON.stringify({ ...manifest, version: "7.8.9" }))
  // When: the explicit version-sync command runs, never an install hook.
  execFileSync(process.execPath, [script, "--sync"])
  // Then: every optional dependency follows the new source version, not the old pins.
  const staged = JSON.parse(await readFile(path.join(packageDirectory, "package.json"), "utf8"))
  assert.deepEqual(staged.optionalDependencies, Object.fromEntries(targets.map(([, os, cpu]) => [
    `rtrt-dashboard-${os}-${cpu}`, "7.8.9",
  ])))
})

test("REL release waits for all provenance platform publishes before agent and GitHub", () => {
  // Given: the actual workflow, parsed as YAML rather than line-order heuristics.
  const workflow = JSON.parse(execFileSync("ruby", ["-rpsych", "-rjson", "-e",
    "puts JSON.generate(Psych.safe_load_file(ARGV[0], permitted_classes: [], aliases: false))",
    path.join(root, ".github/workflows/release.yml"),
  ]))
  // When: Actions constructs its dependency graph.
  const jobs = workflow.jobs
  const platforms = jobs["publish-platform-npm"]
  // Then: every native target publishes on REL only, with scoped OIDC permissions.
  assert.ok(platforms, "platform publication job exists")
  assert.deepEqual(platforms.needs, ["preflight", "build", "package-npm"])
  assert.equal(platforms.if, "needs.preflight.outputs.publish == 'true'")
  assert.deepEqual(platforms.permissions, { contents: "read", "id-token": "write" })
  assert.equal(platforms.environment, "npm-publish")
  assert.deepEqual(platforms.strategy.matrix.platform.sort(), targets.map(([, os, cpu]) => `${os}-${cpu}`).sort())
  assert.ok(jobs["publish-npm"].needs.includes("publish-platform-npm"))
  assert.ok(jobs.release.needs.includes("publish-npm"))
  assert.deepEqual(jobs.build.strategy.matrix.include.map(({ target, npm_platform }) => [target, npm_platform]),
    targets.map(([target, os, cpu]) => [target, `${os}-${cpu}`]))
  const buildCommands = jobs.build.steps.map((step) => step.run ?? "").join("\n")
  assert.match(buildCommands, /for bin in rtrt rtrt-mcp rtrt-dashboard/)
  assert.match(buildCommands, /stage-dashboard\.mjs/)
  assert.match(platforms.steps.map((step) => step.run ?? "").join("\n"), /publish-package\.sh/)
})
