import assert from "node:assert/strict"
import { chmod, mkdir, readFile, symlink, writeFile } from "node:fs/promises"
import { execFile } from "node:child_process"
import { promisify } from "node:util"
import path from "node:path"
import test from "node:test"
import { resolveDashboardBinary } from "./runtime/dashboard-binary.js"
import { fixture, packageFixture, PACKAGE, VERSION } from "./test-fixtures/dashboard.mjs"
import { windowsAcl } from "./runtime/dashboard-acl.js"

// Observe one real, unmodified binary-check before each positive resolver assertion.
// Only fixed policy labels and fixture-relative location metadata leave this process.
async function diagnoseBinaryPolicy(t, manifest) {
  const policy = await readFile(new URL("./runtime/dashboard-acl.ps1", import.meta.url), "utf8")
  const powershell = path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
  const harness = `
$script:fixture = [IO.Path]::GetDirectoryName([IO.Path]::GetDirectoryName([IO.Path]::GetDirectoryName($env:RTRT_ACL_PATH)))
$script:lastAclRelation = 'none'
$script:ancestorCount = 0
function Get-Acl {
  param([string] $LiteralPath)
  if ($LiteralPath.Equals($script:fixture, [StringComparison]::OrdinalIgnoreCase) -or
      $LiteralPath.StartsWith($script:fixture + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    $script:lastAclRelation = 'inside-fixture'
  } else {
    $script:lastAclRelation = 'ancestor'
    $script:ancestorCount++
  }
  Microsoft.PowerShell.Security\\Get-Acl -LiteralPath $LiteralPath
}
try {
${policy}
  $decision = 'accept'
} catch {
  $decision = switch -Exact ($_.Exception.Message) {
    'Untrusted binary owner' { 'Untrusted binary owner' }
    'Untrusted binary writer' { 'Untrusted binary writer' }
    'Reparse point refused' { 'Reparse point refused' }
    'Invalid ACL path' { 'Invalid ACL path' }
    'Invalid ACL operation' { 'Invalid ACL operation' }
    default { 'other' }
  }
}
[pscustomobject]@{ message = $decision; relation = $script:lastAclRelation; ancestors = $script:ancestorCount } | ConvertTo-Json -Compress
`
  const started = performance.now()
  let error
  let stdout
  try {
    ({ stdout } = await promisify(execFile)(powershell, ["-NoProfile", "-NonInteractive", "-Command", harness], {
      windowsHide: true, timeout: 45_000, maxBuffer: 4096,
      env: { SystemRoot: process.env.SystemRoot, RTRT_ACL_PATH: manifest, RTRT_ACL_ACTION: "binary-check" },
    }))
  } catch (caught) {
    error = caught
  }
  let decision
  let outputState = "missing"
  if (typeof stdout === "string") {
    try { decision = JSON.parse(stdout); outputState = "parsed" } catch (cause) {
      if (!(cause instanceof SyntaxError)) throw cause
      outputState = "malformed"
    }
  }
  const messages = ["accept", "Untrusted binary owner", "Untrusted binary writer", "Reparse point refused", "Invalid ACL path", "Invalid ACL operation", "other"]
  const relations = ["none", "inside-fixture", "ancestor"]
  const message = messages.includes(decision?.message) ? decision.message : "unknown"
  const relation = relations.includes(decision?.relation) ? decision.relation : "unknown"
  t.diagnostic(JSON.stringify({ probe: "binary-check-package-manifest", killed: Boolean(error?.killed),
    signal: ["SIGTERM", "SIGKILL"].includes(error?.signal) ? error.signal : null,
    code: Number.isInteger(error?.code) ? error.code : (error?.code === "ETIMEDOUT" ? "ETIMEDOUT" : error ? "other" : 0),
    elapsedMs: Math.round(performance.now() - started), outputState, message, lastAclRelation: relation,
    ancestorCount: Number.isSafeInteger(decision?.ancestors) && decision.ancestors >= 0 ? decision.ancestors : null,
    environment: ["Untrusted binary owner", "Untrusted binary writer"].includes(message) && relation === "ancestor"
      ? "unsafe-ancestor" : "undetermined",
  }))
}

test("resolver selects exact-version platform npm package", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  const expected = await packageFixture(root)
  // When
  const binary = await resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] })
  // Then
  assert.equal(binary, expected)
})

test("resolver accepts only version-scoped private cache package", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.state, "packages", VERSION, "node_modules")
  const expected = await packageFixture(root)
  // When
  const binary = await resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [] })
  // Then
  assert.equal(binary, expected)
})

test("resolver rejects mismatched version without PATH or target fallback", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  await packageFixture(root, "99.0.0")
  // When / Then
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] }))
})

test("resolver rejects symlinked package directory", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const realRoot = path.join(f.home, "real")
  await packageFixture(realRoot)
  const root = path.join(f.home, "node_modules")
  await mkdir(root)
  await symlink(path.join(realRoot, PACKAGE), path.join(root, PACKAGE))
  // When / Then
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] }))
})

test("resolver rejects group-writable executable", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  const binary = await packageFixture(root)
  await chmod(binary, 0o775)
  // When / Then
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] }))
})

test("Windows binary trust permits public read but rejects untrusted write", { skip: process.platform !== "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  const directory = path.join(root, "rtrt-dashboard-win32-x64")
  const bin = path.join(directory, "bin")
  await mkdir(bin, { recursive: true })
  await writeFile(path.join(directory, "package.json"), JSON.stringify({ name: "rtrt-dashboard-win32-x64", version: VERSION }))
  const binary = path.join(bin, "rtrt-dashboard.exe")
  await writeFile(binary, "fixture only")
  const powershell = path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
  const grant = async (rights) => promisify(execFile)(powershell, ["-NoProfile", "-NonInteractive", "-Command",
    "$a=Get-Acl -LiteralPath $env:RTRT_ACL_PATH; $sid=New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-545'; $r=New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @($sid,$env:RTRT_ACL_RIGHTS,'Allow'); $a.AddAccessRule($r); Set-Acl -LiteralPath $env:RTRT_ACL_PATH -AclObject $a"],
  { env: { SystemRoot: process.env.SystemRoot, RTRT_ACL_PATH: binary, RTRT_ACL_RIGHTS: rights } })
  await grant("ReadAndExecute")
  await diagnoseBinaryPolicy(t, path.join(directory, "package.json"))
  // When
  const accepted = await resolveDashboardBinary({ home: f.home, platform: "win32", arch: "x64", version: VERSION, roots: [root] })
  // Then
  assert.equal(accepted, binary)
  await grant("WriteData")
  await assert.rejects(windowsAcl(binary, "binary-check"))
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "win32", arch: "x64", version: VERSION, roots: [root] }))
})

test("Windows binary trust ignores inherit-only and creator-owner placeholders, but not effective parent delete", { skip: process.platform !== "win32" }, async (t) => {
  // Given: a real-volume package fixture under the project scratch root.
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  const directory = path.join(root, "rtrt-dashboard-win32-x64")
  const bin = path.join(directory, "bin")
  await mkdir(bin, { recursive: true })
  await writeFile(path.join(directory, "package.json"), JSON.stringify({ name: "rtrt-dashboard-win32-x64", version: VERSION }))
  const binary = path.join(bin, "rtrt-dashboard.exe")
  await writeFile(binary, "fixture only")
  const powershell = path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
  const script = "$a=Get-Acl -LiteralPath $env:RTRT_ACL_PATH; $sid=New-Object Security.Principal.SecurityIdentifier $env:RTRT_ACL_SID; $r=New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @($sid,$env:RTRT_ACL_RIGHTS,$env:RTRT_ACL_FLAGS,$env:RTRT_ACL_PROP,'Allow'); $a.AddAccessRule($r); Set-Acl -LiteralPath $env:RTRT_ACL_PATH -AclObject $a"
  const grant = (target, sid, rights, flags = "None", propagation = "None") => promisify(execFile)(powershell,
    ["-NoProfile", "-NonInteractive", "-Command", script],
    { env: { SystemRoot: process.env.SystemRoot, RTRT_ACL_PATH: target, RTRT_ACL_SID: sid, RTRT_ACL_RIGHTS: rights, RTRT_ACL_FLAGS: flags, RTRT_ACL_PROP: propagation } })
  await grant(root, "S-1-5-32-545", "WriteData,AppendData") // parent create only; no delete-child
  await grant(root, "S-1-3-0", "FullControl", "ContainerInherit,ObjectInherit", "InheritOnly")
  await diagnoseBinaryPolicy(t, path.join(directory, "package.json"))
  // When: effective ACL is checked for the executable and its ancestors.
  const accepted = await resolveDashboardBinary({ home: f.home, platform: "win32", arch: "x64", version: VERSION, roots: [root] })
  // Then: non-effective ACEs and ancestor creation alone do not block normal startup.
  assert.equal(accepted, binary)
  await grant(root, "S-1-5-32-545", "DeleteSubdirectoriesAndFiles")
  await assert.rejects(windowsAcl(binary, "binary-check"))
})

// Policy-unit proof: intercept only Get-Item/Get-Acl with in-memory, typed Windows
// security descriptors. The real filesystem/resolver integration above remains separate.
// C:\ here is synthetic: it exercises the production ancestor walk without changing a drive ACL.
const binaryPolicyCases = [
  { name: "accepts TrustedInstaller-owned system root", result: "accept" },
  { name: "accepts TrustedInstaller as effective root writer", sid: "trusted", rights: "FullControl", where: "root", result: "accept" },
  { name: "refuses an untrusted root owner", owner: "untrusted", result: "Untrusted binary owner" },
  { name: "refuses an untrusted file owner", owner: "untrusted-file", result: "Untrusted binary owner", rootVisited: false },
  { name: "accepts public read on the root", rights: "ReadAndExecute", where: "root", result: "accept" },
  { name: "accepts directory create-only rights", rights: "WriteData,AppendData", where: "root", result: "accept" },
  { name: "accepts GenericWrite on a directory without delete-child", rights: "0x40000000", where: "root", result: "accept" },
  { name: "refuses GenericAll on the root", rights: "0x10000000", where: "root", result: "Untrusted binary writer" },
  { name: "refuses GenericAll on the file", rights: "0x10000000", where: "file", result: "Untrusted binary writer", rootVisited: false },
  { name: "refuses GenericWrite on the file", rights: "0x40000000", where: "file", result: "Untrusted binary writer", rootVisited: false },
  { name: "refuses file WriteData", rights: "WriteData", where: "file", result: "Untrusted binary writer", rootVisited: false },
  { name: "refuses directory delete-child", rights: "DeleteSubdirectoriesAndFiles", where: "root", result: "Untrusted binary writer" },
  { name: "refuses directory permission changes", rights: "ChangePermissions", where: "root", result: "Untrusted binary writer" },
  { name: "refuses directory ownership changes", rights: "TakeOwnership", where: "root", result: "Untrusted binary writer" },
  { name: "ignores inherit-only untrusted rights", rights: "FullControl", where: "root", propagation: "InheritOnly", result: "accept" },
  { name: "ignores creator-owner placeholder rights", sid: "creator", rights: "FullControl", where: "root", result: "accept" },
]

for (const scenario of binaryPolicyCases) {
  test(`Windows binary policy ${scenario.name} (in-memory ACL)`, { skip: process.platform !== "win32" }, async () => {
    // Given: typed ACLs for a synthetic C:\ tree, with a TrustedInstaller-owned root.
    const policy = await readFile(new URL("./runtime/dashboard-acl.ps1", import.meta.url), "utf8")
    const powershell = path.win32.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
    const harness = `
$script:visitedRoot = $false
$script:readRootAcl = $false
$script:injectedOwner = $false
$script:injectedAce = $false
$root = 'C:\\'
$file = 'C:\\Users\\fixture\\node_modules\\package\\bin\\rtrt-dashboard.exe'
$trusted = 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'
$untrusted = 'S-1-5-32-545'
function Get-Item {
  param([string] $LiteralPath, [switch] $Force)
  if ($LiteralPath -eq $root) { $script:visitedRoot = $true }
  [pscustomobject]@{ Attributes = [IO.FileAttributes]::Normal; PSIsContainer = ($LiteralPath -ne $file) }
}
function Get-Acl {
  param([string] $LiteralPath)
  $isFile = $LiteralPath -eq $file
  $acl = if ($isFile) { New-Object Security.AccessControl.FileSecurity } else { New-Object Security.AccessControl.DirectorySecurity }
  $owner = if (($env:RTRT_TEST_OWNER -eq 'untrusted' -and $LiteralPath -eq $root) -or
               ($env:RTRT_TEST_OWNER -eq 'untrusted-file' -and $isFile)) { $untrusted }
            elseif ($LiteralPath -eq $root) { $trusted }
            else { [Security.Principal.WindowsIdentity]::GetCurrent().User.Value }
  $acl.SetOwner((New-Object Security.Principal.SecurityIdentifier $owner))
  if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $owner) { throw 'Fixture owner injection failed' }
  if ($LiteralPath -eq $root) { $script:readRootAcl = $true }
  if ($owner -eq $untrusted) { $script:injectedOwner = $true }
  if (($env:RTRT_TEST_WHERE -eq 'root' -and $LiteralPath -eq $root) -or
      ($env:RTRT_TEST_WHERE -eq 'file' -and $isFile)) {
    $sid = switch ($env:RTRT_TEST_SID) {
      'trusted' { $trusted }
      'creator' { 'S-1-3-0' }
      default { $untrusted }
    }
    $mask = if ($env:RTRT_TEST_RIGHTS -like '0x*') {
      [Convert]::ToInt32($env:RTRT_TEST_RIGHTS.Substring(2), 16)
    } else { [Security.AccessControl.FileSystemRights][Enum]::Parse([Security.AccessControl.FileSystemRights], $env:RTRT_TEST_RIGHTS) }
    $flags = if ($env:RTRT_TEST_PROP -eq 'InheritOnly') { [Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit' } else { [Security.AccessControl.InheritanceFlags]::None }
    $propagation = [Security.AccessControl.PropagationFlags]$env:RTRT_TEST_PROP
    # Public factory accepts a raw Int32 access mask; the public constructor requires
    # FileSystemRights and rejects the generic bits (0x10000000 / 0x40000000).
    $rule = $acl.AccessRuleFactory((New-Object Security.Principal.SecurityIdentifier $sid), [int]$mask, $false, $flags, $propagation, [Security.AccessControl.AccessControlType]::Allow)
    $acl.AddAccessRule($rule)
    $readback = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | Where-Object {
      $_.IdentityReference.Value -eq $sid -and $_.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
      $_.InheritanceFlags -eq $flags -and $_.PropagationFlags -eq $propagation -and
      (([int]$_.FileSystemRights -band [int]$mask) -eq [int]$mask)
    })
    if ($readback.Count -ne 1) { throw 'Fixture lost generic mask or ACE on descriptor readback' }
    $script:injectedAce = $true
  }
  $acl
}
try {
${policy}
  $result = 'accept'
} catch { $result = $_.Exception.Message }
[pscustomobject]@{ Result = $result; VisitedRoot = $script:visitedRoot; ReadRootAcl = $script:readRootAcl; InjectedOwner = $script:injectedOwner; InjectedAce = $script:injectedAce } | ConvertTo-Json -Compress
`
    // When: run the unmodified production binary-check branch against the typed ACLs.
    const { stdout } = await promisify(execFile)(powershell, ["-NoProfile", "-NonInteractive", "-Command", harness], {
      env: {
        SystemRoot: process.env.SystemRoot, RTRT_ACL_PATH: "C:\\Users\\fixture\\node_modules\\package\\bin\\rtrt-dashboard.exe",
        RTRT_ACL_ACTION: "binary-check", RTRT_TEST_OWNER: scenario.owner ?? "trusted",
        RTRT_TEST_SID: scenario.sid ?? "untrusted", RTRT_TEST_WHERE: scenario.where ?? "none",
        RTRT_TEST_RIGHTS: scenario.rights ?? "ReadAndExecute", RTRT_TEST_PROP: scenario.propagation ?? "None",
      },
    })
    // Then: accepted paths reach C:\; early file refusals need not walk further.
    const decision = JSON.parse(stdout.trim())
    assert.equal(decision.VisitedRoot, scenario.rootVisited ?? true)
    assert.equal(decision.ReadRootAcl, scenario.rootVisited ?? true, "fixture must return the root descriptor to policy")
    assert.equal(decision.InjectedOwner, Boolean(scenario.owner?.startsWith("untrusted")), "fixture must inject the selected owner")
    assert.equal(decision.InjectedAce, Boolean(scenario.where), "fixture must retain the selected ACE before the decision")
    assert.equal(decision.Result, scenario.result)
  })
}
