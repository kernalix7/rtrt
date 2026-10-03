import assert from "node:assert/strict"
import { chmod, mkdir, readFile, symlink, writeFile } from "node:fs/promises"
import { execFile } from "node:child_process"
import { promisify } from "node:util"
import path from "node:path"
import test from "node:test"
import { resolveDashboardBinary } from "./runtime/dashboard-binary.js"
import { fixture, packageFixture, PACKAGE, VERSION } from "./test-fixtures/dashboard.mjs"
import { windowsAcl } from "./runtime/dashboard-acl.js"

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
  $owner = if ($env:RTRT_TEST_OWNER -eq 'untrusted' -and $LiteralPath -eq $root -or
               $env:RTRT_TEST_OWNER -eq 'untrusted-file' -and $isFile) { $untrusted }
           elseif ($LiteralPath -eq $root) { $trusted }
           else { [Security.Principal.WindowsIdentity]::GetCurrent().User.Value }
  $acl.SetOwner((New-Object Security.Principal.SecurityIdentifier $owner))
  if ($env:RTRT_TEST_WHERE -eq 'root' -and $LiteralPath -eq $root -or
      $env:RTRT_TEST_WHERE -eq 'file' -and $isFile) {
    $sid = switch ($env:RTRT_TEST_SID) {
      'trusted' { $trusted }
      'creator' { 'S-1-3-0' }
      default { $untrusted }
    }
    $rights = if ($env:RTRT_TEST_RIGHTS -like '0x*') {
      [Security.AccessControl.FileSystemRights][Convert]::ToInt32($env:RTRT_TEST_RIGHTS.Substring(2), 16)
    } else { [Security.AccessControl.FileSystemRights][Enum]::Parse([Security.AccessControl.FileSystemRights], $env:RTRT_TEST_RIGHTS) }
    $flags = if ($env:RTRT_TEST_PROP -eq 'InheritOnly') { 'ContainerInherit,ObjectInherit' } else { 'None' }
    $rule = New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @((New-Object Security.Principal.SecurityIdentifier $sid), $rights, $flags, $env:RTRT_TEST_PROP, 'Allow')
    if ($env:RTRT_TEST_RIGHTS -like '0x*' -and
        (([int]$rule.FileSystemRights -band [int]$rights) -eq 0)) { throw 'Fixture lost generic mask' }
    $acl.AddAccessRule($rule)
  }
  $acl
}
try {
${policy}
  $result = 'accept'
} catch { $result = $_.Exception.Message }
[pscustomobject]@{ Result = $result; VisitedRoot = $script:visitedRoot } | ConvertTo-Json -Compress
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
    assert.equal(decision.Result, scenario.result)
  })
}
