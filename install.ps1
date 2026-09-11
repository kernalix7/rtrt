# RTRT installer — Windows PowerShell.
#
# One-liner install:
#   irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
#
# Flags:
#   -Version vX.Y.Z       Pin a specific release tarball (skip source build).
#   -Main                 Build from git main HEAD. Same as -Ref main.
#                         (env: RTRT_REF=main)
#   -Ref <tag>            Build from a specific tag / branch / commit.
#                         (env: RTRT_REF=<ref>)
#   -Source <path>        Build from a local copy instead of git clone.
#                         (env: RTRT_SOURCE)
#   -InstallDir <path>    Install dir (default: $env:LOCALAPPDATA\Programs\rtrt).
#   -SkipDeps             Skip toolchain check (fail early if missing).
#                         (env: RTRT_SKIP_DEPS=1)
#   -Uninstall            Compat shim — removes only an owned task + binaries.
#   -NoSetup              Disable agent setup. Linux strict OpenCode bootstrap
#                         is unsupported on native Windows. (env: RTRT_NO_SETUP=1)
#   -NoService            Don't register the rtrt-dashboard logon task.
#   -DryRun               Print intended actions without writing anything.

[CmdletBinding()]
param(
    [string]   $Version    = "",
    [switch]   $Main,
    [string]   $Ref        = "",
    [string]   $Source     = "",
    [string]   $InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\rtrt"),
    [switch]   $SkipDeps,
    [switch]   $Uninstall,
    [switch]   $NoSetup,
    [switch]   $NoService,
    [switch]   $DryRun
)

$ErrorActionPreference = "Stop"

# Windows PowerShell 5.1 may default to TLS 1.0 — force TLS 1.2+ so the
# GitHub API / release downloads don't fail the handshake.
try {
    [Net.ServicePointManager]::SecurityProtocol = `
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # PowerShell 7+ negotiates modern TLS on its own.
}

$Repo = "kernalix7/rtrt"
$Bins = @("rtrt.exe", "rtrt-mcp.exe", "rtrt-dashboard.exe")
$DashboardTaskName = "rtrt-dashboard"
$DashboardTaskMarker = "rtrt-managed-dashboard-service"
$CurrentIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$CurrentSid = $CurrentIdentity.User

# Env-var fallbacks. Flag values above already take precedence.
if (-not $Ref    -and $env:RTRT_REF)        { $Ref    = $env:RTRT_REF }
if (-not $Source -and $env:RTRT_SOURCE)     { $Source = $env:RTRT_SOURCE }
if (-not $SkipDeps -and $env:RTRT_SKIP_DEPS) { $SkipDeps = $true }
if (-not $NoSetup -and $env:RTRT_NO_SETUP -eq "1") { $NoSetup = $true }
if (-not $NoService -and $env:RTRT_NO_SERVICE -eq "1") { $NoService = $true }
if ($Main) { $Ref = "main" }

function Write-Log($Message)  { Write-Host "[rtrt] $Message"  -ForegroundColor Green }
function Write-Warn($Message) { Write-Host "[warn] $Message"  -ForegroundColor Yellow }
function Write-Err($Message)  { Write-Host "[error] $Message" -ForegroundColor Red }

function Invoke-Step($Action, $Script) {
    if ($DryRun) {
        Write-Host "[dry-run] $Action"
    } else {
        Write-Host ">> $Action"
        & $Script
    }
}

function Require-Cmd($Name) {
    if ($SkipDeps) { return }
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "required command not found: $Name (use -SkipDeps to bypass)"
    }
}

function Test-OwnedDashboardTask($Task) {
    if ($Task.Description -ne $DashboardTaskMarker) { return $false }
    if (@($Task.Actions).Count -ne 1) { return $false }
    try {
        if ($Task.Principal.UserId -match '^S-1-') {
            $principalSid = New-Object Security.Principal.SecurityIdentifier($Task.Principal.UserId)
        } else {
            $principalSid = (New-Object Security.Principal.NTAccount($Task.Principal.UserId)).Translate([Security.Principal.SecurityIdentifier])
        }
    } catch { return $false }
    return ($principalSid.Value -eq $CurrentSid.Value)
}

# ---------- uninstall (compat shim) ----------
if ($Uninstall) {
    Write-Log "== rtrt uninstall (compat shim) =="
    Write-Log "For interactive / purge flow, use uninstall.ps1:"
    Write-Log "  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/$Repo/main/uninstall.ps1))) -Confirm"
    Write-Host ""
    $task = Get-ScheduledTask -TaskName $DashboardTaskName -ErrorAction SilentlyContinue
    if ($task) {
        if (-not (Test-OwnedDashboardTask $task)) {
            throw "refusing foreign scheduled task: $DashboardTaskName"
        }
        Invoke-Step "remove owned dashboard task" {
            Stop-ScheduledTask -TaskName $DashboardTaskName -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $DashboardTaskName -Confirm:`$false
        }
    }
    foreach ($bin in $Bins) {
        $target = Join-Path $InstallDir $bin
        if (Test-Path $target) {
            Invoke-Step "remove $target" { Remove-Item -Force $target }
        } else {
            Write-Warn "  skip $target (not present)"
        }
    }
    Write-Log "rtrt uninstalled. Local state under `$env:USERPROFILE\.rtrt is untouched."
    return
}

# ---------- detect arch ----------
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$TargetTriple = "$arch-pc-windows-msvc"

Write-Log "== rtrt install =="
Write-Log "  target: $TargetTriple"
Write-Log "  prefix: $InstallDir"

function Show-InstallCheck {
    $pathSep = ';'
    $current = "$env:PATH"
    if (-not ($current.Split($pathSep) -contains $InstallDir)) {
        Write-Host ""
        Write-Warn "$InstallDir is not on `$env:PATH."
        # NOT `setx PATH` — that flattens machine+user PATH into the user value
        # and truncates it at 1024 characters.
        Write-Warn "  Add it (user scope, new shells) via:"
        Write-Warn "    [Environment]::SetEnvironmentVariable('Path', ([Environment]::GetEnvironmentVariable('Path','User') + ';$InstallDir'), 'User')"
        Write-Host ""
    }
    Write-Log "rtrt installed:"
    foreach ($bin in $Bins) {
        Write-Log "  $(Join-Path $InstallDir $bin)"
    }
    if (-not $NoSetup) {
        Write-Log "native Windows: skipping Linux-only strict OpenCode bootstrap/session migration (bubblewrap unsupported)"
    }
    Install-DashboardTask
    Write-Host ""
    Write-Log "Next:"
    Write-Log "  rtrt --version"
    Write-Log "  rtrt info"
    Write-Log "  rtrt templates"
}

function Set-PrivateDirectoryAcl([string] $Path) {
    $security = New-Object Security.AccessControl.DirectorySecurity
    $security.SetOwner($CurrentSid)
    $security.SetAccessRuleProtection($true, $false)
    $rule = New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @(
        $CurrentSid, "FullControl", "ContainerInherit,ObjectInherit", "None", "Allow"
    )
    $security.AddAccessRule($rule)
    Set-Acl -LiteralPath $Path -AclObject $security
}

function Assert-SafeDirectory([string] $Path) {
    $parent = Split-Path -LiteralPath $Path -Parent
    while ($parent) {
        $parentItem = Get-Item -LiteralPath $parent -Force -ErrorAction SilentlyContinue
        if ($parentItem -and (($parentItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "unsafe dashboard state ancestor: $parent"
        }
        $next = Split-Path -LiteralPath $parent -Parent
        if ($next -eq $parent) { break }
        $parent = $next
    }
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($item) {
        if (-not $item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "unsafe dashboard state path: $Path"
        }
    } else {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
        $item = Get-Item -LiteralPath $Path -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "unsafe dashboard state path: $Path" }
    }
    Set-PrivateDirectoryAcl $Path
    $owner = (Get-Acl -LiteralPath $Path).GetOwner([Security.Principal.SecurityIdentifier])
    if ($owner -ne $CurrentSid) { throw "dashboard state owner mismatch: $Path" }
}

function Protect-PrivateFile([string] $Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw "unsafe dashboard token file: $Path" }
    $security = New-Object Security.AccessControl.FileSecurity
    $security.SetOwner($CurrentSid)
    $security.SetAccessRuleProtection($true, $false)
    $security.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @($CurrentSid, "FullControl", "Allow")))
    Set-Acl -LiteralPath $Path -AclObject $security
    $owner = (Get-Acl -LiteralPath $Path).GetOwner([Security.Principal.SecurityIdentifier])
    if ($owner -ne $CurrentSid) { throw "dashboard token owner mismatch: $Path" }
}

function Ensure-MachineDashboardToken([string] $StateDir) {
    $envPath = Join-Path $StateDir "dashboard.env"
    $existing = Get-Item -LiteralPath $envPath -Force -ErrorAction SilentlyContinue
    if ($existing) {
        if ($existing.PSIsContainer -or (($existing.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw "unsafe dashboard token file: $envPath" }
        Protect-PrivateFile $envPath
        $line = (Get-Content -LiteralPath $envPath -Raw).Trim()
        if ($line -notmatch '^RTRT_DASHBOARD_TOKEN=([0-9a-fA-F]{64})$') { throw "invalid dashboard token file: $envPath" }
        return
    }
    $bytes = New-Object byte[] 32
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
    $token = ([BitConverter]::ToString($bytes) -replace '-', '').ToLowerInvariant()
    $tmp = Join-Path $StateDir (".dashboard.env.{0}.tmp" -f ([Guid]::NewGuid().ToString('N')))
    try {
        $stream = New-Object IO.FileStream($tmp, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $stream.Dispose()
        Protect-PrivateFile $tmp
        [IO.File]::WriteAllText($tmp, "RTRT_DASHBOARD_TOKEN=$token`n", (New-Object Text.UTF8Encoding($false)))
        try { [IO.File]::Move($tmp, $envPath) }
        catch [IO.IOException] {
            # Another installer won the create race; validate and reuse its token.
            if (-not (Test-Path -LiteralPath $envPath -PathType Leaf)) { throw }
        }
    } finally { if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force } }
    Protect-PrivateFile $envPath
    $line = (Get-Content -LiteralPath $envPath -Raw).Trim()
    if ($line -notmatch '^RTRT_DASHBOARD_TOKEN=([0-9a-fA-F]{64})$') { throw "invalid dashboard token file: $envPath" }
    $token = $null
}

# Register a logon scheduled task that starts rtrt-dashboard in the background
# (Windows has no `rtrt service` path; this is the equivalent auto-start).
# Default-on; `-NoService` disables. Unsafe state or foreign tasks fail closed.
function Install-DashboardTask {
    if ($NoService -or $DryRun) { return }
    $dash = Join-Path $InstallDir "rtrt-dashboard.exe"
    $dashItem = Get-Item -LiteralPath $dash -Force -ErrorAction SilentlyContinue
    if (-not $dashItem) { return }
    if ($dashItem.PSIsContainer -or (($dashItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "unsafe dashboard executable: $dash"
    }
    $existing = Get-ScheduledTask -TaskName $DashboardTaskName -ErrorAction SilentlyContinue
    if ($existing -and -not (Test-OwnedDashboardTask $existing)) {
        throw "refusing foreign scheduled task: $DashboardTaskName"
    }
    $stateDir = Join-Path $env:USERPROFILE ".rtrt\dashboard"
    Assert-SafeDirectory $stateDir
    Ensure-MachineDashboardToken $stateDir
    Write-Host ""
    Write-Log "registering rtrt-dashboard logon task"
    $arguments = "--machine --state-dir `"$stateDir`""
    $action = New-ScheduledTaskAction -Execute $dash -Argument $arguments
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $CurrentIdentity.Name
    $principal = New-ScheduledTaskPrincipal -UserId $CurrentIdentity.Name -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries -StartWhenAvailable
    Register-ScheduledTask -TaskName $DashboardTaskName -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Description $DashboardTaskMarker -Force | Out-Null
    Start-ScheduledTask -TaskName $DashboardTaskName
    Write-Log "  dashboard task registered + started — http://127.0.0.1:7311"
}

function Build-FromSource($SrcDir) {
    Require-Cmd cargo
    Invoke-Step "cargo build --release" {
        Push-Location $SrcDir
        try { cargo build --release --workspace } finally { Pop-Location }
    }
    if (-not (Test-Path $InstallDir)) {
        Invoke-Step "mkdir $InstallDir" { New-Item -ItemType Directory -Path $InstallDir | Out-Null }
    }
    foreach ($bin in $Bins) {
        $src = Join-Path $SrcDir "target\release\$bin"
        $dst = Join-Path $InstallDir $bin
        Invoke-Step "install $bin" { Copy-Item -Force $src $dst }
    }
    Show-InstallCheck
}

# ---------- -Source PATH (local copy) ----------
if ($Source) {
    if (-not (Test-Path $Source -PathType Container)) {
        throw "-Source path is not a directory: $Source"
    }
    Write-Log "  source: $Source (local)"
    Build-FromSource $Source
    return
}

# ---------- -Ref / -Main (git clone) ----------
if ($Ref) {
    Require-Cmd git
    Require-Cmd cargo
    $work = Join-Path $env:TEMP "rtrt-install-$(Get-Random)"
    Invoke-Step "create $work" { New-Item -ItemType Directory -Path $work | Out-Null }
    Write-Log "  ref: $Ref (source build into $work)"
    try {
        Invoke-Step "git clone $Ref" {
            git clone --depth 1 --branch $Ref "https://github.com/$Repo" $work 2>$null
            if ($LASTEXITCODE -ne 0) {
                git clone "https://github.com/$Repo" $work
                Push-Location $work
                try { git checkout $Ref } finally { Pop-Location }
            }
        }
        Build-FromSource $work
    } finally {
        if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    }
    return
}

# ---------- release tarball ----------
if (-not $Version) {
    try {
        $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -ErrorAction Stop
        $Version = $latest.tag_name
    } catch {
        $Version = $null
    }
    if (-not $Version) {
        Write-Warn "no GitHub Release published yet — falling back to source build from main."
        Write-Warn "Pass -Version vX.Y.Z to pin a release once one is cut, or -Ref BRANCH to track a different branch."
        Write-Host ""
        Require-Cmd git
        Require-Cmd cargo
        $work = Join-Path $env:TEMP "rtrt-install-$(Get-Random)"
        Invoke-Step "create $work" { New-Item -ItemType Directory -Path $work | Out-Null }
        Write-Log "  ref: main (auto-fallback into $work)"
        try {
            Invoke-Step "git clone main" { git clone --depth 1 "https://github.com/$Repo" $work }
            Build-FromSource $work
        } finally {
            if (Test-Path $work) { Remove-Item -Recurse -Force $work }
        }
        return
    }
}
Write-Log "  version: $Version"

$versionBare = $Version.TrimStart('v')
$asset = "rtrt-$versionBare-$TargetTriple.zip"
$url = "https://github.com/$Repo/releases/download/$Version/$asset"
$checksumUrl = "$url.sha256"

$work = Join-Path $env:TEMP "rtrt-install-dry-run"
if (-not $DryRun) {
    $work = Join-Path $env:TEMP "rtrt-install-$([Guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $work | Out-Null
}
try {
    $archivePath = Join-Path $work $asset
    Invoke-Step "downloading $url" { Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing }
    if (-not $DryRun) {
        try {
            $checksumContent = (Invoke-WebRequest -Uri $checksumUrl -UseBasicParsing).Content
        } catch {
            throw "checksum file is missing for release asset $asset`: $checksumUrl"
        }
        $checksumLines = @($checksumContent.Trim() -split '\r?\n')
        $checksumParts = @($checksumLines[0] -split '\s+')
        if ($checksumLines.Count -ne 1 -or $checksumParts.Count -ne 2) {
            throw "checksum file must contain exactly one SHA256 record: $checksumUrl"
        }
        $expected = $checksumParts[0]
        if ($expected -notmatch '^[0-9a-fA-F]{64}$') {
            throw "checksum file must begin with exactly 64 hexadecimal characters: $checksumUrl"
        }
        if ($checksumParts[1] -cne $asset) {
            throw "checksum filename does not match release asset: expected $asset, found $($checksumParts[1])"
        }
        $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLower()
        if ($actual -ne $expected.ToLower()) {
            throw "checksum mismatch: expected $expected actual $actual"
        }
        Write-Log "  checksum: ok"
    }
    $extract = Join-Path $work "extracted"
    Invoke-Step "extract" { Expand-Archive -Path $archivePath -DestinationPath $extract -Force }
    if (-not (Test-Path $InstallDir)) {
        Invoke-Step "mkdir $InstallDir" { New-Item -ItemType Directory -Path $InstallDir | Out-Null }
    }
    foreach ($bin in $Bins) {
        $dst = Join-Path $InstallDir $bin
        if ($DryRun) {
            # Nothing was downloaded in dry-run mode, so skip the existence probe.
            Invoke-Step "install $bin -> $dst" { }
            continue
        }
        $src = Get-ChildItem -Path $extract -Recurse -Filter $bin | Select-Object -First 1
        if (-not $src) { throw "binary missing from zip: $bin" }
        Invoke-Step "install $bin" { Copy-Item -Force $src.FullName $dst }
    }
    Show-InstallCheck
} finally {
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
}
