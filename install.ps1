# RTRT installer — Windows PowerShell.
#
# One-liner install (latest release):
#   irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
#
# One-liner uninstall:
#   irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Confirm'
#   irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Purge'
#
# Flags:
#   -Version vX.Y.Z   pin a specific release (default: latest)
#   -Main             ignore releases; clone main and `cargo build --release`
#   -InstallDir <p>   install dir (default: $env:LOCALAPPDATA\Programs\rtrt)
#   -Uninstall        compatibility shim — defers to uninstall.ps1 logic
#   -DryRun           print intended actions without writing anything

[CmdletBinding()]
param(
    [string] $Version = "",
    [switch] $Main,
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\rtrt"),
    [switch] $Uninstall,
    [switch] $DryRun
)

$ErrorActionPreference = "Stop"
$Repo = "kernalix7/rtrt"
$Bins = @("rtrt.exe", "rtrt-mcp.exe", "rtrt-dashboard.exe")

function Invoke-Step($Action, $Script) {
    if ($DryRun) {
        Write-Host "[dry-run] $Action"
    } else {
        Write-Host ">> $Action"
        & $Script
    }
}

# ---------- uninstall (compat shim) ----------
# Canonical uninstaller is uninstall.ps1 (interactive + -Confirm + -Purge).
# This branch keeps `install.ps1 -Uninstall` working for users who memorised it.
if ($Uninstall) {
    Write-Host "== rtrt uninstall (compat shim) =="
    Write-Host "For interactive / purge flow, use uninstall.ps1 instead:"
    Write-Host "  irm https://raw.githubusercontent.com/$Repo/main/uninstall.ps1 | iex -Args '-Confirm'"
    Write-Host ""
    foreach ($bin in $Bins) {
        $target = Join-Path $InstallDir $bin
        if (Test-Path $target) {
            Invoke-Step "remove $target" { Remove-Item -Force $target }
        } else {
            Write-Host "  skip $target (not present)"
        }
    }
    Write-Host "rtrt uninstalled. Local state under `$env:USERPROFILE\.rtrt is untouched."
    return
}

# ---------- detect arch ----------
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$TargetTriple = "$arch-pc-windows-msvc"

Write-Host "== rtrt install =="
Write-Host "  target: $TargetTriple"
Write-Host "  prefix: $InstallDir"

# ---------- source build ----------
if ($Main) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found; install Rust (https://rustup.rs) and retry, or omit -Main."
    }
    $work = Join-Path $env:TEMP "rtrt-install-$(Get-Random)"
    Invoke-Step "create $work" { New-Item -ItemType Directory -Path $work | Out-Null }
    try {
        Invoke-Step "git clone main" { git clone --depth 1 "https://github.com/$Repo" $work }
        Invoke-Step "cargo build --release" {
            Push-Location $work
            try { cargo build --release --workspace } finally { Pop-Location }
        }
        if (-not (Test-Path $InstallDir)) {
            Invoke-Step "mkdir $InstallDir" { New-Item -ItemType Directory -Path $InstallDir | Out-Null }
        }
        foreach ($bin in $Bins) {
            $src = Join-Path $work "target\release\$bin"
            $dst = Join-Path $InstallDir $bin
            Invoke-Step "install $bin" { Copy-Item -Force $src $dst }
        }
    } finally {
        if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    }
    Show-InstallCheck
    return
}

# ---------- release tarball ----------
if (-not $Version) {
    $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Version = $latest.tag_name
    if (-not $Version) {
        throw "could not resolve latest release. Pass -Version vX.Y.Z, or -Main to build from source."
    }
}
Write-Host "  version: $Version"

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
            $expectedRaw = (Invoke-WebRequest -Uri $checksumUrl -UseBasicParsing).Content
            $expected = ($expectedRaw -split '\s+')[0]
            if ($expected) {
                $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
                if ($actual -ne $expected.ToLowerInvariant()) {
                    throw "checksum mismatch:`n  expected $expected`n  actual   $actual"
                }
                Write-Host "  checksum: ok"
            }
        } catch [System.Net.WebException] {
            Write-Host "  checksum: SHA256 file not yet attached; skipping verification"
        }
    }

    Invoke-Step "expand archive" { Expand-Archive -Force -Path $archivePath -DestinationPath $work }

    if (-not (Test-Path $InstallDir)) {
        Invoke-Step "mkdir $InstallDir" { New-Item -ItemType Directory -Path $InstallDir | Out-Null }
    }
    foreach ($bin in $Bins) {
        $candidates = @(
            (Join-Path $work $bin),
            (Join-Path $work "$($asset -replace '\.zip$','')\$bin")
        )
        $src = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
        if (-not $src) { throw "binary missing from archive: $bin" }
        $dst = Join-Path $InstallDir $bin
        Invoke-Step "install $bin" { Copy-Item -Force $src $dst }
    }
} finally {
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
}

function Show-InstallCheck {
    $userPath = [System.Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$InstallDir*") {
        Write-Host ""
        Write-Host "WARNING: $InstallDir is not on PATH."
        Write-Host "  Add it permanently with:"
        Write-Host "    [Environment]::SetEnvironmentVariable('Path', `"$env:Path;$InstallDir`", 'User')"
        Write-Host ""
    }
    Write-Host "rtrt installed:"
    foreach ($bin in $Bins) { Write-Host "  $(Join-Path $InstallDir $bin)" }
    Write-Host ""
    Write-Host "Next:"
    Write-Host "  rtrt --version"
    Write-Host "  rtrt info"
    Write-Host "  rtrt templates"
}

Show-InstallCheck
