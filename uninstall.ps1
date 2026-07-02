# rtrt uninstaller — Windows PowerShell.
#
# Interactive (asks before each step):
#   .\uninstall.ps1
#
# Auto (agent wiring + task + binaries, keeps state):
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1))) -Confirm
#
# Full purge (the above + %USERPROFILE%\.rtrt + caches):
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1))) -Purge
#
# (`irm ... | iex` cannot forward parameters — the scriptblock form above can.)
#
# Flags:
#   -Confirm                non-interactive: Claude Code wiring + task + binaries
#   -Purge                  non-interactive, also wipes %USERPROFILE%\.rtrt + caches
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
    Write-Log "Mode: FULL PURGE (binaries + %USERPROFILE%\.rtrt + fastembed cache)"
    Write-Warn "purge is irreversible — memory store, prompt registry, and model caches will be deleted."
} else {
    Write-Log "Mode: BINARIES ONLY (use -Purge for full wipe)"
}
Write-Host ""

# Unwire the Claude Code integration first, while rtrt.exe still exists to do
# it — otherwise settings keep hooks + a statusline that point at deleted
# binaries. Best-effort: no prior `rtrt setup` is fine.
$rtrtExe = Join-Path $InstallDir 'rtrt.exe'
if (-not (Test-Path $rtrtExe)) {
    $found = Get-Command rtrt.exe -ErrorAction SilentlyContinue
    if ($found) { $rtrtExe = $found.Source } else { $rtrtExe = $null }
}
$claudeJson = Join-Path $env:USERPROFILE '.claude.json'
$claudeSettings = Join-Path $env:USERPROFILE '.claude\settings.json'
$claudeWired = ((Test-Path $claudeJson) -and (Select-String -Path $claudeJson -Pattern '"rtrt"' -Quiet -ErrorAction SilentlyContinue)) -or
               ((Test-Path $claudeSettings) -and (Select-String -Path $claudeSettings -Pattern 'rtrt hook' -Quiet -ErrorAction SilentlyContinue))
if ($claudeWired) {
    if ($rtrtExe) {
        if (Confirm-Step "Remove the Claude Code integration (MCP server, hooks, statusline, skills)?") {
            & $rtrtExe uninstall --agent claude --plugin --apply *> $null
            if ($LASTEXITCODE -eq 0) {
                Write-Log "  Claude Code integration removed (.claude.json, .claude\settings.json)"
                Write-Log "  restart Claude Code to drop the unloaded MCP server + hooks"
            } else {
                Write-Warn "  could not unwire Claude Code — run: rtrt uninstall --agent claude --plugin --apply"
            }
        }
    } else {
        Write-Warn "  Claude Code is wired to rtrt but no rtrt.exe was found to unwire it."
        Write-Warn "  Reinstall rtrt and run: rtrt uninstall --agent claude --plugin --apply"
    }
}

# Remove the dashboard logon task first (best-effort).
$existingTask = Get-ScheduledTask -TaskName "rtrt-dashboard" -ErrorAction SilentlyContinue
if ($existingTask -and (Confirm-Step "Stop + remove the rtrt-dashboard logon task?")) {
    try {
        Stop-ScheduledTask -TaskName "rtrt-dashboard" -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName "rtrt-dashboard" -Confirm:$false
        Write-Log "  dashboard task removed"
    } catch {
        Write-Warn "  could not remove dashboard task: $($_.Exception.Message)"
    }
}

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
    Write-Log "Local state left intact. Use -Purge to wipe %USERPROFILE%\.rtrt + caches."
}

Write-Host ""
Write-Log "rtrt uninstalled."
