# Uninstall the portable (zip) version of zar-logs.
#
# If you used the installer, uninstall from Windows instead (Settings > Apps > Installed apps > zar-logs).
#
# The portable version is a single program file plus a data folder (%LOCALAPPDATA%\com.vicplusplus.zar-logs). It
# installs nothing into Windows (no registry entries, no services, no startup entries) and nothing into the game.
# This script removes what it did create:
#
#   - the program: zar-logs.exe next to this script, and this uninstaller
#   - the window data Windows keeps for the app's viewer (the EBWebView folder inside the data folder)
#   - and, only if you say so:
#       your recorded missions (logs)
#       the rest of the data folder: settings and the cache of what zar-logs read from your game install
#
# It deletes only files zar-logs itself writes, never a folder's other contents, and asks before each step.
#
#   Double-click uninstall.cmd, or:   powershell -ExecutionPolicy Bypass -File uninstall.ps1
#
# Options (all optional):
#   -DryRun                  show what would be deleted, delete nothing
#   -Logs yes|no|ask         delete recorded missions            (default: ask)
#   -Data yes|no|ask         delete settings and cache           (default: ask)
#   -Program yes|no|ask      delete the program and window data  (default: ask)
#   -DataDir <folder>        the data folder, if you moved it    (default: %ZARLOGS_DATA% or
#                            %LOCALAPPDATA%\com.vicplusplus.zar-logs)

param(
    [switch]$DryRun,
    [ValidateSet("yes", "no", "ask")][string]$Logs = "ask",
    [ValidateSet("yes", "no", "ask")][string]$Data = "ask",
    [ValidateSet("yes", "no", "ask")][string]$Program = "ask",
    [string]$DataDir = ""
)

$ErrorActionPreference = "Stop"
$AppIdentifier = "com.vicplusplus.zar-logs"
$ExeName = "zar-logs.exe"

function Say($text) { Write-Host $text }

function Confirm-Step($question, $preset) {
    if ($preset -eq "yes") { return $true }
    if ($preset -eq "no") { return $false }
    $answer = Read-Host "$question [y/N]"
    return ($answer -match '^(y|yes)$')
}

function Get-Size($path) {
    if (-not (Test-Path -LiteralPath $path)) { return 0 }
    $sum = (Get-ChildItem -LiteralPath $path -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object -Property Length -Sum).Sum
    if ($null -eq $sum) { return 0 }
    return $sum
}

function Format-Size($bytes) {
    if ($bytes -ge 1MB) { return ("{0:N1} MB" -f ($bytes / 1MB)) }
    if ($bytes -ge 1KB) { return ("{0:N0} KB" -f ($bytes / 1KB)) }
    return "$bytes bytes"
}

# Delete one file or one folder tree that zar-logs created. Refuses anything that is not below a folder we expect.
function Remove-Ours($path, $mustBeUnder) {
    if (-not (Test-Path -LiteralPath $path)) { return }
    $full = [System.IO.Path]::GetFullPath($path)
    $root = [System.IO.Path]::GetFullPath($mustBeUnder).TrimEnd('\') + '\'
    if (-not $full.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) {
        Say "  skipped (outside $root): $full"
        return
    }
    if ($DryRun) { Say "  would delete: $full"; return }
    Remove-Item -LiteralPath $full -Recurse -Force -Confirm:$false
    Say "  deleted: $full"
}

function Remove-IfEmpty($path) {
    if (-not (Test-Path -LiteralPath $path)) { return }
    if (@(Get-ChildItem -LiteralPath $path -Force).Count -ne 0) { return }
    if ($DryRun) { Say "  would remove empty folder: $path"; return }
    Remove-Item -LiteralPath $path -Force -Confirm:$false
    Say "  removed empty folder: $path"
}

Say ""
Say "zar-logs uninstaller"
if ($DryRun) { Say "(dry run: nothing will be deleted)" }
Say ""

# ---- the app must not be running
if (@(Get-Process -Name "zar-logs" -ErrorAction SilentlyContinue).Count -gt 0) {
    Say "zar-logs is running. Close its window first, then run this again."
    if (-not $DryRun) { exit 2 }
}

# ---- where things are
if ($DataDir -eq "") {
    if ($env:ZARLOGS_DATA) { $DataDir = $env:ZARLOGS_DATA } else { $DataDir = Join-Path $env:LOCALAPPDATA $AppIdentifier }
}
$configFile = Join-Path $DataDir "config.json"
$config = $null
if (Test-Path -LiteralPath $configFile) {
    try { $config = Get-Content -LiteralPath $configFile -Raw | ConvertFrom-Json } catch { $config = $null }
}
$logsDir = Join-Path $DataDir "logs"
$cacheDir = Join-Path $DataDir "cache"
if ($null -ne $config -and $config.PSObject.Properties.Name -contains "logs" -and $config.logs) { $logsDir = [string]$config.logs }
if ($null -ne $config -and $config.PSObject.Properties.Name -contains "cache" -and $config.cache) { $cacheDir = [string]$config.cache }
if ($env:ZARLOGS_LOGS) { $logsDir = $env:ZARLOGS_LOGS }
if ($env:ZARLOGS_CACHE) { $cacheDir = $env:ZARLOGS_CACHE }
# Windows keeps the viewer's window data under the app's identifier, whatever ZARLOGS_DATA says
$webViewHome = Join-Path $env:LOCALAPPDATA $AppIdentifier
$webViewDir = Join-Path $webViewHome "EBWebView"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $here $ExeName

$logFiles = @()
if (Test-Path -LiteralPath $logsDir) {
    $logFiles = @(Get-ChildItem -LiteralPath $logsDir -File -Filter "enc-*.json.gz" -ErrorAction SilentlyContinue)
}
$logBytes = ($logFiles | Measure-Object -Property Length -Sum).Sum
if ($null -eq $logBytes) { $logBytes = 0 }

Say "Found:"
if (Test-Path -LiteralPath $exe) { Say "  program           $exe" } else { Say "  program           not next to this script (delete zar-logs.exe yourself, wherever you put it)" }
Say ("  window data       {0}  ({1})" -f $webViewDir, (Format-Size (Get-Size $webViewDir)))
Say ("  recorded missions {0}  ({1} logs, {2})" -f $logsDir, $logFiles.Count, (Format-Size $logBytes))
Say ("  cache             {0}  ({1})" -f $cacheDir, (Format-Size (Get-Size $cacheDir)))
Say ("  settings          {0}" -f $configFile)
Say ""
Say "Nothing is installed in the game's folder or in Windows itself, so there is nothing to remove there."
Say ""

$removedProgram = $false

# ---- 1. the program and its window data
if (Confirm-Step "Remove the program (zar-logs.exe, this uninstaller) and its window data?" $Program) {
    Remove-Ours $webViewDir $webViewHome
    if (Test-Path -LiteralPath $exe) {
        Remove-Ours $exe $here
        $removedProgram = $true
    }
} else {
    Say "  kept the program."
}

# ---- 2. recorded missions
if ($logFiles.Count -gt 0 -or (Test-Path -LiteralPath (Join-Path $logsDir "index.json"))) {
    if (Confirm-Step ("Also delete your {0} recorded missions? This cannot be undone." -f $logFiles.Count) $Logs) {
        foreach ($f in $logFiles) {
            if (-not $DryRun) { Remove-Item -LiteralPath $f.FullName -Force -Confirm:$false }
        }
        if ($DryRun) { Say ("  would delete: {0} logs in {1}" -f $logFiles.Count, $logsDir) } else { Say ("  deleted: {0} logs in {1}" -f $logFiles.Count, $logsDir) }
        Remove-Ours (Join-Path $logsDir "index.json") $logsDir
        Remove-Ours (Join-Path $logsDir "index.tmp") $logsDir
        Remove-IfEmpty $logsDir
    } else {
        Say "  kept your recorded missions in $logsDir"
    }
}

# ---- 3. settings and cache
if ((Test-Path -LiteralPath $cacheDir) -or (Test-Path -LiteralPath $configFile)) {
    if (Confirm-Step "Also delete zar-logs' settings and its cache (what it read from your game install; it is rebuilt if you install again)?" $Data) {
        Remove-Ours (Join-Path $cacheDir "game") $cacheDir
        Remove-Ours (Join-Path $cacheDir "current.json") $cacheDir
        Remove-Ours (Join-Path $cacheDir "current.tmp") $cacheDir
        Remove-IfEmpty $cacheDir
        Remove-Ours $configFile $DataDir
        Remove-Ours (Join-Path $DataDir "config.tmp") $DataDir
    } else {
        Say "  kept settings and cache in $DataDir"
    }
}
Remove-IfEmpty $DataDir
Remove-IfEmpty $webViewHome

Say ""
if ($DryRun) { Say "Dry run finished. Nothing was deleted." } else { Say "Done." }

# ---- the uninstaller itself goes last (uninstall.cmd removes itself when it sees exit code 10)
if ($removedProgram -and -not $DryRun) {
    Remove-Item -LiteralPath $MyInvocation.MyCommand.Path -Force -Confirm:$false -ErrorAction SilentlyContinue
    exit 10
}
exit 0
