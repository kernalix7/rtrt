import assert from "node:assert/strict"
import { execFile, spawn } from "node:child_process"
import { constants } from "node:fs"
import { open, readFile, readdir, stat } from "node:fs/promises"
import { createServer } from "node:net"
import { once } from "node:events"
import path from "node:path"
import test from "node:test"
import { promisify } from "node:util"
import { fileURLToPath } from "node:url"
import { windowsAcl } from "./runtime/dashboard-acl.js"
import { writeDashboardBootstrap } from "./runtime/dashboard-bootstrap.js"
import { dashboardFiles } from "./runtime/dashboard-files.js"
import { fixture, TOKEN } from "./test-fixtures/dashboard.mjs"

const run = promisify(execFile)

async function availablePort() {
  const server = createServer()
  server.listen(0, "127.0.0.1")
  await once(server, "listening")
  const { port } = server.address()
  server.close()
  await once(server, "close")
  return port
}

function machineEnv(home, port) {
  return {
    SystemRoot: process.env.SystemRoot, PATH: process.env.PATH, TEMP: process.env.TEMP,
    TMP: process.env.TMP, USERPROFILE: process.env.USERPROFILE, HOME: home,
    RTRT_CONFIG: path.join(home, ".rtrt", "config.toml"),
    // On Windows `dirs::home_dir()` resolves the OS known-folder profile and
    // ignores HOME, so without this pin the machine executable would create
    // the prompt registry under the real USERPROFILE instead of the fixture.
    RTRT_PROMPTS_DIR: path.join(home, ".rtrt", "prompts"),
    RTRT_DASHBOARD_BIND: `127.0.0.1:${port}`, RTRT_AUTO_CAPTURE: "0",
  }
}

async function profileRtrtSnapshot() {
  const profileRoot = process.env.USERPROFILE
  if (!profileRoot) return "no-userprofile"
  try {
    return (await readdir(path.win32.join(profileRoot, ".rtrt"))).sort().join(",")
  } catch (error) {
    if (error.code === "ENOENT") return "absent"
    throw error
  }
}

async function machineFixture(t) {
  const f = await fixture(t)
  await windowsAcl(path.join(f.home, ".rtrt"), "private-create")
  await windowsAcl(f.state, "private-create")
  await dashboardFiles({ home: f.home, platform: "win32" }).createToken(TOKEN)
  return f
}

test("Windows private fixture is accepted only after owner-only ACL is installed before content", { skip: process.platform !== "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const files = dashboardFiles({ home: f.home, platform: "win32" })
  await assert.rejects(files.prepare())
  await windowsAcl(path.join(f.home, ".rtrt"), "private-create")
  await windowsAcl(f.state, "private-create")
  // When
  await files.prepare()
  await files.createToken(TOKEN)
  // Then
  assert.equal(await files.readToken(), TOKEN)
  const html = await writeDashboardBootstrap(f.state, "A".repeat(87), { platform: "win32" })
  assert.equal(path.basename(html), "bootstrap.html")
  assert.match(await readFile(html, "utf8"), /#bootstrap=A{87}/)
})

test("Windows inherited permissive fixture is refused without rewriting it", { skip: process.platform !== "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  await windowsAcl(path.join(f.home, ".rtrt"), "private-create")
  await windowsAcl(f.state, "private-create")
  const handle = await open(f.envFile, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600)
  await handle.close()
  await windowsAcl(f.envFile, "private-create")
  const script = "$p=$env:RTRT_ACL_PATH; $parent=Split-Path -LiteralPath $p -Parent; $a=Get-Acl -LiteralPath $parent; $sid=New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-545'; $r=New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @($sid,'ReadAndExecute','ContainerInherit,ObjectInherit','None','Allow'); $a.AddAccessRule($r); Set-Acl -LiteralPath $parent -AclObject $a; $a=Get-Acl -LiteralPath $p; $a.SetAccessRuleProtection($false,$true); Set-Acl -LiteralPath $p -AclObject $a"
  await run(path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe"),
    ["-NoProfile", "-NonInteractive", "-Command", script], { env: { SystemRoot: process.env.SystemRoot, RTRT_ACL_PATH: f.envFile } })
  const files = dashboardFiles({ home: f.home, platform: "win32" })
  // When / Then
  await assert.rejects(files.prepare())
  await assert.rejects(files.readToken())
  await assert.rejects(windowsAcl(f.envFile, "private-check"))
})

test("Windows installer leaves existing unrelated parent untouched and creates only validated managed directories", { skip: process.platform !== "win32" }, async (t) => {
  // Given: an inherited existing .rtrt parent in the private project fixture.
  const f = await fixture(t)
  const parent = path.join(f.home, ".rtrt")
  const leaf = path.join(parent, "managed-fixture")
  const powershell = path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
  const load = "$parseTokens=$null; $parseErrors=$null; $ast=[System.Management.Automation.Language.Parser]::ParseFile($env:RTRT_INSTALL_PATH,[ref]$parseTokens,[ref]$parseErrors); if($parseErrors.Count) { throw ('installer parse error: ' + (($parseErrors | ForEach-Object { '{0}:{1}:{2}:{3}' -f $_.ErrorId,$_.Extent.StartLineNumber,$_.Extent.StartColumnNumber,$_.Message }) -join '; ')) }; $CurrentSid=[Security.Principal.WindowsIdentity]::GetCurrent().User; $names=@('Set-PrivateDirectoryAcl','Assert-PrivateAcl','Assert-SafeDirectory'); foreach($fn in $ast.FindAll({param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $names -contains $node.Name},$true)) { Invoke-Expression $fn.Extent.Text }; "
  const env = { SystemRoot: process.env.SystemRoot, RTRT_INSTALL_PATH: fileURLToPath(new URL("../../install.ps1", import.meta.url)), RTRT_ACL_PATH: parent, RTRT_ACL_LEAF: leaf }
  const refused = "$before=(Get-Acl -LiteralPath $env:RTRT_ACL_PATH).Sddl; $rejected=$false; try { Assert-SafeDirectory $env:RTRT_ACL_PATH } catch { $rejected=$true }; if(-not $rejected -or $before -cne (Get-Acl -LiteralPath $env:RTRT_ACL_PATH).Sddl) { throw 'existing parent was changed' }"
  await run(powershell, ["-NoProfile", "-NonInteractive", "-Command", load + refused], { env })
  await windowsAcl(parent, "private-create")
  // When: the installer validates the owned managed parent and creates a new leaf.
  const created = "$before=(Get-Acl -LiteralPath $env:RTRT_ACL_PATH).Sddl; Assert-SafeDirectory $env:RTRT_ACL_PATH; Assert-SafeDirectory $env:RTRT_ACL_LEAF; if($before -cne (Get-Acl -LiteralPath $env:RTRT_ACL_PATH).Sddl) { throw 'existing managed parent was rewritten' }; Assert-PrivateAcl $env:RTRT_ACL_LEAF"
  await run(powershell, ["-NoProfile", "-NonInteractive", "-Command", load + created], { env })
  // Then: the new directory satisfies the same private ACL policy as Node and Rust.
  await windowsAcl(leaf, "private-check")
})

test("Windows machine executable accepts plain Node state path and serves loopback health", { skip: process.platform !== "win32" }, async (t) => {
  // Given: an empty private HOME distinct from USERPROFILE and a private token created before writing content.
  const f = await machineFixture(t)
  const executable = process.env.RTRT_TEST_DASHBOARD_EXE
  assert.ok(executable && path.win32.isAbsolute(executable), "CI must supply the built dashboard executable")
  const port = await availablePort()
  const profileBefore = await profileRtrtSnapshot()
  // When: the real executable starts with Node's normal path, never a verbatim path or token argv.
  const child = spawn(executable, ["--machine", "--state-dir", f.state], {
    env: machineEnv(f.home, port), windowsHide: true, stdio: ["ignore", "ignore", "pipe"],
  })
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) { child.kill(); await once(child, "exit") }
  })
  assert.deepEqual(child.spawnargs.slice(1), ["--machine", "--state-dir", f.state])
  let healthy = false
  for (let attempt = 0; attempt < 80 && !healthy && child.exitCode === null; attempt++) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/healthz`, { signal: AbortSignal.timeout(500) })
      healthy = response.status === 200 && await response.text() === "ok"
    } catch { await new Promise((resolve) => setTimeout(resolve, 100)) }
  }
  // Then: health is served on the chosen loopback port; no real profile was used.
  assert.equal(healthy, true)
  // And: the pinned prompt registry lives inside the fixture; the operator profile is untouched.
  assert.equal(await profileRtrtSnapshot(), profileBefore)
  assert.equal((await stat(path.join(f.home, ".rtrt", "prompts"))).isDirectory(), true)
})

test("Windows machine executable refuses inherited token before binding", { skip: process.platform !== "win32" }, async (t) => {
  // Given: valid private directories and a token whose ACL is changed to inherited.
  const f = await machineFixture(t)
  const executable = process.env.RTRT_TEST_DASHBOARD_EXE
  assert.ok(executable && path.win32.isAbsolute(executable), "CI must supply the built dashboard executable")
  const powershell = path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
  await run(powershell, ["-NoProfile", "-NonInteractive", "-Command",
    "$a=Get-Acl -LiteralPath $env:RTRT_ACL_PATH; $a.SetAccessRuleProtection($false,$true); Set-Acl -LiteralPath $env:RTRT_ACL_PATH -AclObject $a"],
  { env: { SystemRoot: process.env.SystemRoot, RTRT_ACL_PATH: f.envFile } })
  const port = await availablePort()
  // When: the actual executable attempts startup with the same plain state path.
  const child = spawn(executable, ["--machine", "--state-dir", f.state], {
    env: machineEnv(f.home, port), windowsHide: true, stdio: ["ignore", "ignore", "pipe"],
  })
  const stderr = []
  child.stderr.on("data", (part) => stderr.push(part))
  const [code] = await once(child, "exit")
  // Then: the private ACL guard, not a bind conflict, terminates startup.
  assert.notEqual(code, 0)
  assert.match(Buffer.concat(stderr).toString("utf8"), /dashboard Windows ACL is not private/)
  // And: refusal happened before bind — the chosen port is still immediately bindable.
  const probe = createServer()
  const bindable = await new Promise((resolve) => {
    probe.once("error", () => resolve(false))
    probe.listen(port, "127.0.0.1", () => resolve(true))
  })
  await new Promise((resolve) => probe.close(resolve))
  assert.equal(bindable, true)
})
