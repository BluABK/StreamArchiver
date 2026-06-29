<#
.SYNOPSIS
    StreamArchiver CLI Process Manager.

.DESCRIPTION
    Queries the detached-process registry in the StreamArchiver SQLite database and
    checks which download tool processes are currently running. Mirrors the in-app
    Process Manager window — useful when the UI is unavailable (building, crashed,
    debugging).

    Requires sqlite3.exe. Install via: scoop install sqlite

.PARAMETER DbPath
    Path to the database file.
    Default: %APPDATA%\StreamArchiver\data\streamarchiver.sqlite3

.PARAMETER Watch
    Refresh every 2 seconds until Ctrl+C.

.PARAMETER ShowAll
    Also list stale registry rows whose PID is no longer alive (normally filtered out).

.EXAMPLE
    .\Show-Processes.ps1
    .\Show-Processes.ps1 -Watch
    .\Show-Processes.ps1 -ShowAll
    .\Show-Processes.ps1 -DbPath "D:\other\streamarchiver.sqlite3"
#>
[CmdletBinding()]
param(
    [string]$DbPath = (Join-Path $env:APPDATA 'StreamArchiver\data\streamarchiver.sqlite3'),
    [switch]$Watch,
    [switch]$ShowAll
)

# ── sqlite3 discovery ─────────────────────────────────────────────────────────

function Find-Sqlite3 {
    $cmd = Get-Command sqlite3 -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($p in @(
        "$env:USERPROFILE\scoop\apps\sqlite\current\sqlite3.exe",
        "$env:ProgramFiles\SQLite\sqlite3.exe",
        "${env:ProgramFiles(x86)}\SQLite\sqlite3.exe",
        "$env:ChocolateyInstall\bin\sqlite3.exe",
        "$env:LOCALAPPDATA\Programs\sqlite3.exe"
    )) {
        if (Test-Path $p) { return $p }
    }
    return $null
}

# sqlite3 outputs UTF-8; ensure PowerShell decodes stdout bytes correctly.
# Without this, Windows uses the console's OEM codepage (CP437) and non-ASCII
# characters in channel names / file paths are garbled.
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$Script:Sqlite3 = Find-Sqlite3
if (-not $Script:Sqlite3) {
    Write-Error @'
sqlite3.exe not found on PATH or common install locations.
Install via one of:
  scoop install sqlite
  choco install sqlite
  winget install SQLite.SQLite
Or download sqlite-tools-win-x64 from https://www.sqlite.org/download.html
and add the extracted folder to your PATH.
'@
    exit 1
}

# ── Helpers ───────────────────────────────────────────────────────────────────

function Format-Uptime([long]$StartedAt) {
    $total = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() - $StartedAt
    if ($total -lt 0) { $total = 0 }
    $h = [long]($total / 3600); $total -= $h * 3600
    $m = [long]($total / 60);   $total -= $m * 60
    $s = $total
    if ($h -gt 0) { return "${h}h ${m}m" }
    if ($m -gt 0) { return "${m}m ${s}s" }
    return "${s}s"
}

function Get-TypeLabel([string]$Kind, [bool]$Secondary) {
    switch ($Kind) {
        'recording' { if ($Secondary) { 'video (DASH)' } else { 'video' } }
        'video'     { 'VOD' }
        'chat'      { 'chat' }
        default     { $Kind }
    }
}

function Test-PidAlive([int]$ProcessId) {
    return $null -ne (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)
}

# ── Query ─────────────────────────────────────────────────────────────────────

# Mirrors the enrichment done in app_core::list_processes():
#   Recording/Chat → channel name + monitor.tool
#   Video          → video title (or url) + video.tool
$Script:Query = @'
SELECT
    dp.kind,
    dp.pid,
    CAST(dp.secondary AS INTEGER) AS secondary,
    dp.spawn_build,
    dp.started_at,
    dp.log_path,
    dp.capture_path,
    CASE
        WHEN dp.kind IN ('recording','chat')
            THEN coalesce(nullif(trim(c.name),''), '(unknown channel)')
        WHEN dp.kind = 'video'
            THEN CASE WHEN trim(coalesce(v.title,'')) != ''
                      THEN v.title ELSE coalesce(v.url,'(unknown)') END
        ELSE '(unknown)'
    END AS name,
    CASE
        WHEN dp.kind IN ('recording','chat') THEN coalesce(m.tool, '?')
        WHEN dp.kind = 'video'              THEN coalesce(v.tool, '?')
        ELSE '?'
    END AS tool
FROM detached_process dp
LEFT JOIN monitor m ON dp.monitor_id = m.id AND dp.kind IN ('recording','chat')
LEFT JOIN channel c ON m.channel_id = c.id
LEFT JOIN video   v ON dp.ref_id    = v.id AND dp.kind = 'video'
ORDER BY dp.id;
'@

function Get-AllRows {
    if (-not (Test-Path $DbPath)) {
        Write-Warning "Database not found: $DbPath"
        return @()
    }

    $raw = & $Script:Sqlite3 -json $DbPath $Script:Query 2>&1
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "sqlite3 exited ${LASTEXITCODE}: $raw"
        return @()
    }

    $json = ($raw -join '') -replace '^\s*$'
    if ([string]::IsNullOrWhiteSpace($json) -or $json -eq '[]') { return @() }

    $data = $json | ConvertFrom-Json
    if (-not $data) { return @() }

    @($data | ForEach-Object {
        $alive = Test-PidAlive ([int]$_.pid)
        [PSCustomObject]@{
            PID         = [int]$_.pid
            Kind        = $_.kind
            Type        = Get-TypeLabel $_.kind ([int]$_.secondary -ne 0)
            Name        = [string]$_.name
            Tool        = [string]$_.tool
            Alive       = $alive
            Status      = if ($alive) { 'running' } else { 'stale' }
            Uptime      = if ($alive) { Format-Uptime ([long]$_.started_at) } else { '-' }
            Build       = [string]$_.spawn_build
            LogPath     = [string]$_.log_path
            CapturePath = [string]$_.capture_path
        }
    })
}

# ── Display ───────────────────────────────────────────────────────────────────

function Show-Snapshot {
    # Clear before the query so warnings/errors from Get-AllRows are visible,
    # not wiped by a Clear-Host that comes after them.
    Clear-Host

    $all    = @(Get-AllRows)
    $live   = @($all | Where-Object { $_.Alive })
    $stale  = @($all | Where-Object { -not $_.Alive })
    $rows   = if ($ShowAll) { $all } else { $live }

    Write-Host 'StreamArchiver Process Manager' -NoNewline -ForegroundColor Cyan
    Write-Host "  $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" -ForegroundColor DarkGray
    Write-Host "DB: $DbPath" -ForegroundColor DarkGray
    Write-Host ''

    if ($all.Count -eq 0) {
        Write-Host 'No processes in the detached-process registry.' -ForegroundColor DarkGray
        if ($Watch) { Write-Host '(refreshing every 2s — Ctrl+C to stop)' -ForegroundColor DarkGray }
        return
    }

    $summary = "$($live.Count) running"
    if ($stale.Count -gt 0 -and -not $ShowAll) {
        $summary += ", $($stale.Count) stale (pass -ShowAll to include)"
    } elseif ($stale.Count -gt 0) {
        $summary += ", $($stale.Count) stale"
    }
    Write-Host $summary -ForegroundColor White
    Write-Host ''

    if ($rows.Count -eq 0) {
        Write-Host 'No running download processes.' -ForegroundColor DarkGray
        return
    }

    # ── Table ────────────────────────────────────────────────────────────────
    $fmt = '{0,-7} {1,-13} {2,-28} {3,-12} {4,-10} {5}'
    Write-Host ($fmt -f 'PID', 'Type', 'Name', 'Tool', 'Uptime', 'Status') -ForegroundColor Yellow
    Write-Host ('-' * 86) -ForegroundColor DarkGray

    foreach ($r in $rows) {
        $nameCol = if ($r.Name.Length -gt 27) { $r.Name.Substring(0,24) + '...' } else { $r.Name }
        $line    = $fmt -f $r.PID, $r.Type, $nameCol, $r.Tool, $r.Uptime, $r.Status
        Write-Host $line -ForegroundColor $(if ($r.Alive) { 'Green' } else { 'DarkGray' })
    }

    # ── Per-process detail block ──────────────────────────────────────────────
    Write-Host ''
    Write-Host 'Details:' -ForegroundColor Yellow
    foreach ($r in $rows) {
        $hdr = "  PID $($r.PID)  [$($r.Type)]  $($r.Name)"
        Write-Host $hdr -ForegroundColor $(if ($r.Alive) { 'White' } else { 'DarkGray' })
        Write-Host "    Tool:    $($r.Tool)"   -ForegroundColor DarkGray
        Write-Host "    Build:   $($r.Build)"  -ForegroundColor DarkGray
        if ($r.CapturePath) { Write-Host "    Capture: $($r.CapturePath)" -ForegroundColor DarkGray }
        if ($r.LogPath)     { Write-Host "    Log:     $($r.LogPath)"     -ForegroundColor DarkGray }
    }

    if ($Watch) {
        Write-Host ''
        Write-Host '(refreshing every 2s — Ctrl+C to stop)' -ForegroundColor DarkGray
    } else {
        Write-Host ''
        Write-Host 'Tip: Stop-Process -Id <PID> -Force  to hard-kill a process.' -ForegroundColor DarkGray
        Write-Host 'Tip: Get-Content -Wait <LogPath>    to tail a log file.'     -ForegroundColor DarkGray
    }
}

# ── Main ──────────────────────────────────────────────────────────────────────

if ($Watch) {
    try {
        while ($true) {
            Show-Snapshot
            Start-Sleep -Seconds 2
        }
    } finally {
        # Ensure the cursor stays on a clean line after Ctrl+C
        Write-Host ''
    }
} else {
    Show-Snapshot
}
