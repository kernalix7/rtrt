# rtrt uninstaller — Windows PowerShell.
#
# Interactive (asks before each step):
#   .\uninstall.ps1
#
# Auto (binaries only, keeps state):
#   irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Confirm'
#
# Full purge (binaries + %APPDATA%\rtrt):
#   irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Purge'
#
# Flags:
#   -Confirm                non-interactive, removes the three binaries only
#   -Purge                  non-interactive, also wipes %APPDATA%\rtrt
#   -InstallDir <path>      install dir (default: $env:LOCALAPPDATA\Programs\rtrt)

param(
    [switch]$Confirm,
    [switch]$Purge,
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\rtrt"
)

$ErrorActionPreference = 'Stop'

function Write-Log($Message)  { Write-Host "[rtrt] $Message"      -ForegroundColor Green }
function Write-Warn($Message) { Write-Host "[warn] $Message"      -ForegroundColor Yellow }
function Write-Err($Message)  { Write-Host "[error] $Message"     -ForegroundColor Red }

$Auto = $Confirm.IsPresent -or $Purge.IsPresent
$Bins = @('rtrt.exe', 'rtrt-mcp.exe', 'rtrt-dashboard.exe')

function Confirm-Step($Prompt) {
    if ($Auto) { return $true }
    $answer = Read-Host "  $Prompt (y/N)"
    return ($answer -match '^[Yy]')
}

Write-Host ""
Write-Host "=========================================="
Write-Host " rtrt uninstaller"
Write-Host "=========================================="
if ($Purge) {
    Write-Log "Mode: FULL PURGE (binaries + %APPDATA%\rtrt + fastembed cache)"
} else {
    Write-Log "Mode: BINARIES ONLY (use -Purge for full wipe)"
}
Write-Host ""

if (Confirm-Step "Remove binaries from $InstallDir?") {
    foreach ($bin in $Bins) {
        $target = Join-Path $InstallDir $bin
        if (Test-Path $target) {
            Remove-Item -Force $target
            Write-Log "  removed $target"
        } else {
            Write-Warn "  skip $target (not present)"
        }
    }
} else {
    Write-Warn "  skipped binary removal"
}

if ($Purge) {
    $candidates = @(
        (Join-Path $env:APPDATA 'rtrt'),
        (Join-Path $env:LOCALAPPDATA 'fastembed'),
        (Join-Path $env:USERPROFILE '.rtrt')
    )
    foreach ($dir in $candidates) {
        if (Test-Path $dir) {
            if (Confirm-Step "Wipe $dir?") {
                Remove-Item -Recurse -Force $dir
                Write-Log "  wiped $dir"
            }
        }
    }
} else {
    Write-Log "Local state left intact. Use -Purge to wipe %APPDATA%\rtrt + caches."
}

Write-Host ""
Write-Log "rtrt uninstalled."
