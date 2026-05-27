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
#   -Uninstall            Compat shim — defers to uninstall.ps1.
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
    [switch]   $NoService,
    [switch]   $DryRun
)

$ErrorActionPreference = "Stop"
$Repo = "kernalix7/rtrt"
$Bins = @("rtrt.exe", "rtrt-mcp.exe", "rtrt-dashboard.exe")

# Env-var fallbacks. Flag values above already take precedence.
if (-not $Ref    -and $env:RTRT_REF)        { $Ref    = $env:RTRT_REF }
if (-not $Source -and $env:RTRT_SOURCE)     { $Source = $env:RTRT_SOURCE }
if (-not $SkipDeps -and $env:RTRT_SKIP_DEPS) { $SkipDeps = $true }
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

# ---------- uninstall (compat shim) ----------
if ($Uninstall) {
    Write-Log "== rtrt uninstall (compat shim) =="
    Write-Log "For interactive / purge flow, use uninstall.ps1:"
    Write-Log "  irm https://raw.githubusercontent.com/$Repo/main/uninstall.ps1 | iex -Args '-Confirm'"
    Write-Host ""
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
        Write-Warn "  Add it via:  setx PATH `"$InstallDir;$env:PATH`""
        Write-Host ""
    }
    Write-Log "rtrt installed:"
    foreach ($bin in $Bins) {
        Write-Log "  $(Join-Path $InstallDir $bin)"
    }
    Install-DashboardTask
    Write-Host ""
    Write-Log "Next:"
    Write-Log "  rtrt --version"
    Write-Log "  rtrt info"
    Write-Log "  rtrt templates"
}

# Register a logon scheduled task that starts rtrt-dashboard in the background
# (Windows has no `rtrt service` path; this is the equivalent auto-start).
# Default-on; `-NoService` disables. Best-effort — never fails the install.
function Install-DashboardTask {
    if ($NoService -or $DryRun) { return }
    $dash = Join-Path $InstallDir "rtrt-dashboard.exe"
    if (-not (Test-Path $dash)) { return }
    Write-Host ""
    Write-Log "registering rtrt-dashboard logon task"
    try {
        $action  = New-ScheduledTaskAction -Execute $dash
        $trigger = New-ScheduledTaskTrigger -AtLogOn
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
            -DontStopIfGoingOnBatteries -StartWhenAvailable
        Register-ScheduledTask -TaskName "rtrt-dashboard" -Action $action `
            -Trigger $trigger -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName "rtrt-dashboard"
        Write-Log "  dashboard task registered + started — http://127.0.0.1:7311"
        Write-Log "  remove: Unregister-ScheduledTask -TaskName rtrt-dashboard -Confirm:`$false"
    } catch {
        Write-Warn "  task registration skipped: $($_.Exception.Message)"
        Write-Warn "  run rtrt-dashboard manually if you want the web UI"
    }
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

$work = Join-Path $env:TEMP "rtrt-install-$(Get-Random)"
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $archivePath = Join-Path $work $asset
    Invoke-Step "downloading $url" { Invoke-WebRequest -Uri $url -OutFile $archivePath }
    if (-not $DryRun) {
        try {
            $expected = (Invoke-WebRequest -Uri $checksumUrl -UseBasicParsing).Content.Trim().Split(' ')[0]
            if ($expected) {
                $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLower()
                if ($actual -ne $expected.ToLower()) {
                    throw "checksum mismatch: expected $expected actual $actual"
                }
                Write-Log "  checksum: ok"
            } else {
                Write-Warn "  checksum: no SHA256 file at release; skipping verification"
            }
        } catch {
            Write-Warn "  checksum: SHA256 file not yet attached; skipping verification"
        }
    }
    $extract = Join-Path $work "extracted"
    Invoke-Step "extract" { Expand-Archive -Path $archivePath -DestinationPath $extract -Force }
    if (-not (Test-Path $InstallDir)) {
        Invoke-Step "mkdir $InstallDir" { New-Item -ItemType Directory -Path $InstallDir | Out-Null }
    }
    foreach ($bin in $Bins) {
        $src = Get-ChildItem -Path $extract -Recurse -Filter $bin | Select-Object -First 1
        if (-not $src) { throw "binary missing from zip: $bin" }
        $dst = Join-Path $InstallDir $bin
        Invoke-Step "install $bin" { Copy-Item -Force $src.FullName $dst }
    }
    Show-InstallCheck
} finally {
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
}
