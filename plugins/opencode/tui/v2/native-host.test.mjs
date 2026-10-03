// Opt-in, offline native host test. Run inside a private network namespace.
// Set RTRT_NATIVE_OPENCODE_BIN, RTRT_NATIVE_RTRT_BIN and RTRT_NATIVE_BUN_BIN.
import assert from "node:assert/strict"
import { spawn, spawnSync } from "node:child_process"
import { copyFile, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { stripVTControlCharacters } from "node:util"
import { createNativeController } from "./native-runtime.mjs"

const source = fileURLToPath(new URL("../../", import.meta.url))
const root = fileURLToPath(new URL("../../../../", import.meta.url))
const host = process.env.RTRT_NATIVE_OPENCODE_BIN
const cli = process.env.RTRT_NATIVE_RTRT_BIN
const bun = process.env.RTRT_NATIVE_BUN_BIN

if (process.argv[2] === "--capture") {
  const [project, binary, widthText, mode] = process.argv.slice(3)
  assert.ok(host)
  // Given: real CLI plus the same adapter used by the native component.
  const updates = []
  const controller = createNativeController({ bin: binary }, (state) => updates.push(state))
  controller.setContext({ cwd: project, session: "", width: Number(widthText) })
  try {
    // When
    await controller.refreshNow()
    // Then: parsed CLI identity, not a synthetic metrics payload.
    assert.equal(updates.at(-1)?.unavailable, mode === "missing")
    if (mode === "working") {
      const segments = updates.at(-1).payload.segments
      if (Number(widthText) === 120) assert.equal(segments.find((segment) => segment.id === "project")?.text, "qv1")
      assert.equal(segments.find((segment) => segment.id === "style")?.text, "opt:off")
      assert.equal(segments.some((segment) => ["savings", "headroom", "limit_5h", "limit_week"].includes(segment.id)), false)
    }
  } finally {
    controller.dispose()
  }

  const { Terminal } = await import("bun")
  const chunks = []
  let child
  let timer
  let completed = false
  const ready = Promise.withResolvers()
  const terminal = new Terminal({ cols: Number(widthText), rows: 28, data: (_terminal, data) => {
    chunks.push(Buffer.from(data))
    const text = stripVTControlCharacters(Buffer.concat(chunks).toString("utf8"))
    const visible = mode === "working"
      ? text.includes("OPT off") || /off\s*N\/A DEGRADED/u.test(text)
      : /N\/A.*DEGRADED/u.test(text)
    if (visible) ready.resolve(text)
  } })
  try {
    // When: the actual native executable discovers, compiles and mounts the plugin.
    child = Bun.spawn([host, "--standalone", "--log-level", "none", project], {
      cwd: project, env: process.env, terminal, stdin: "ignore", stdout: "ignore", stderr: "ignore",
    })
    timer = setTimeout(() => ready.reject(new Error("Native footer did not consume configured CLI status: " +
      stripVTControlCharacters(Buffer.concat(chunks).toString("utf8")).slice(-4000))), 12000)
    child.exited.then((code) => {
      if (!completed) ready.reject(new Error(`Native host exited before footer assertion: ${code}`))
    })
    const text = await ready.promise
    // Then: observable terminal values from the real native component.
    // Incremental terminal writes may put qv1 directly before OPT after stripping cursor motion.
    if (mode === "working" && Number(widthText) === 120) assert.ok(text.includes("qv1"))
    if (mode === "missing") assert.ok(!text.includes("OPT off") && !/off\s*N\/A DEGRADED/u.test(text))
    console.log(JSON.stringify({ controller: updates.at(-1), host, columns: Number(widthText), mode, footerObserved: true }))
  } finally {
    completed = true
    clearTimeout(timer)
    if (child) {
      child.kill("SIGTERM")
      const killTimer = setTimeout(() => child.kill("SIGKILL"), 3000)
      await child.exited.finally(() => clearTimeout(killTimer))
    }
    terminal.close()
  }
} else {
  for (const width of [120, 40]) {
    for (const mode of ["working", "missing"]) {
      test(`native host consumes ${mode} absolute CLI status at ${width} columns`, {
        skip: !(host && cli && bun) && "set the three RTRT_NATIVE_*_BIN paths to verified private fixtures",
        timeout: 25000,
      }, async () => {
        // Given: canonical git root, scrubbed HOME/XDG/TMP, no paid credentials or external discovery.
        for (const binary of [host, cli, bun]) assert.ok(path.isAbsolute(binary))
        const tmp = path.join(root, ".rtrt/tmp")
        await mkdir(tmp, { recursive: true })
        const base = await mkdtemp(path.join(tmp, "native-footer-"))
        const project = path.join(base, "qv1")
        const packagePath = path.join(base, "package")
        const home = path.join(base, "home")
        const configDir = path.join(base, "config/opencode")
        const env = {
          PATH: "/usr/bin:/bin", HOME: home, OPENCODE_TEST_HOME: home,
          XDG_CONFIG_HOME: path.join(base, "config"), XDG_DATA_HOME: path.join(base, "data"),
          XDG_CACHE_HOME: path.join(base, "cache"), XDG_STATE_HOME: path.join(base, "state"),
          XDG_RUNTIME_DIR: path.join(base, "run"), TMPDIR: path.join(base, "tmp"),
          BUN_INSTALL_CACHE_DIR: path.join(base, "bun-cache"), OPENCODE_CONFIG_DIR: configDir,
          OPENCODE_CONFIG: path.join(project, "opencode.json"), OPENCODE_CONFIG_PROJECT_DISABLE: "true",
          OPENCODE_DISABLE_FILEWATCHER: "true", OPENCODE_DISABLE_MODELS_FETCH: "true",
          OPENCODE_DISABLE_AUTOUPDATE: "true", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null",
          GIT_CEILING_DIRECTORIES: base, GIT_OPTIONAL_LOCKS: "0", DO_NOT_TRACK: "1",
          TERM: "xterm-256color", COLORTERM: "truecolor", LANG: "C.UTF-8", SHELL: "/bin/bash",
          RTRT_NATIVE_OPENCODE_BIN: host,
        }
        try {
          for (const directory of [project, packagePath, home, configDir, env.XDG_RUNTIME_DIR, env.TMPDIR]) {
            await mkdir(directory, { recursive: true, mode: 0o700 })
          }
          const init = spawnSync("/usr/bin/git", ["init", "--quiet", "--initial-branch=main", project], {
            cwd: base, env: { ...env, GIT_MASTER: "1" }, encoding: "utf8", timeout: 5000,
          })
          assert.equal(init.status, 0, init.stderr || String(init.error))
          const manifest = JSON.parse(await readFile(path.join(source, "package.json"), "utf8"))
          for (const file of ["package.json", ...manifest.files.filter((file) => /\.(?:js|mjs|tsx|mts|ps1)$/u.test(file))]) {
            await mkdir(path.dirname(path.join(packagePath, file)), { recursive: true })
            await copyFile(path.join(source, file), path.join(packagePath, file))
          }
          await import(path.join(packagePath, "runtime/dashboard-supervisor.js"))
          await symlink(path.resolve(path.dirname(host), "../../.."), path.join(base, "node_modules"))
          const binary = mode === "working" ? cli : path.join(base, "absent-rtrt")
          const plugins = [{ package: packagePath, options: { bin: binary } }]
          await writeFile(env.OPENCODE_CONFIG, JSON.stringify({ plugins, update: "disable", warming: false, share: "disabled" }))
          // Server discovery exposes the TUI entry, not its options; the native CLI owns those.
          await writeFile(path.join(configDir, "cli.json"), JSON.stringify({ plugins }))

          // When: execute the behavioral driver in Bun's real PTY runtime.
          const child = spawn(bun, [fileURLToPath(import.meta.url), "--capture", project, binary, String(width), mode], {
            cwd: project, env, stdio: ["ignore", "pipe", "pipe"], timeout: 20000,
          })
          const output = []
          for (const stream of [child.stdout, child.stderr]) stream.on("data", (chunk) => output.push(chunk))
          const code = await new Promise((resolve, reject) => { child.once("close", resolve); child.once("error", reject) })
          // Then: both controller data and native terminal behavior must pass.
          assert.equal(code, 0, Buffer.concat(output).toString("utf8"))
        } finally {
          await rm(base, { recursive: true, force: true })
        }
      })
    }
  }
}
