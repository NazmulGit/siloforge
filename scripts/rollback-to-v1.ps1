<#
.SYNOPSIS
  Roll back from SiloForge v2 to the v1 Docker stack on 127.0.0.1:8600/8601.

.DESCRIPTION
  1. Disables + stops the v2 scheduled task and stops siloforge.exe processes running from <Root>.
  2. `docker start` siloforge_pg, siloforge_redis, siloforge_qdrant (if stopped), then siloforge_api,
     siloforge_ui, siloforge-worker-1, siloforge-worker-2 (the containers cutover-from-v1.ps1 only stopped).
  3. Waits for the v1 /health on :8600 (v1's /health has no "version" field; v2's has).
  4. Restores integration config files from their `.bak-pre-sf2` copies when present (Hermes ap13 config.yaml,
     Mission Control services.json, Backup Hub config.json, Model Control inventory.js / localroutes.js).
     The v2 version of each file is kept next to it as `<file>.sf2-rolledback-<ts>`. Skip with -KeepConfigs.

  v2 data is not touched (data\siloforge.db stays as it is), so going forward again is just
  `Enable-ScheduledTask siloforge-keepalive` after stopping v1. v1 never received writes from v2, so no
  database restore is needed. (If v1's own DB is damaged: pg_restore the dump in D:\Backups\SiloForge\v1-final-*.dump.)

  PowerShell 5.1 compatible. Supports -WhatIf / -Confirm. Log: <Root>\logs\rollback-<ts>.log.

.EXAMPLE
  .\rollback-to-v1.ps1 -WhatIf
  .\rollback-to-v1.ps1
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [string]$Root = 'D:\Final_Apps\SiloForge',
    [string]$TaskName = 'siloforge-keepalive',
    [switch]$KeepConfigs,
    [string[]]$ConfigFiles = @(
        'D:\Agent\hermes\profiles\ap13_internallink\config.yaml',
        'D:\Code\mission-control-port\services.json',
        'D:\Code\Ops\backup-hub\data\config.json',
        'D:\Code\Hermes-Tools\model-control\core\inventory.js',
        'D:\Code\Hermes-Tools\model-control\core\localroutes.js'
    )
)

$ErrorActionPreference = 'Stop'
$full = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
if (($full -notmatch '^[Dd]:\\' -and $env:SILOFORGE_ALLOW_ANY_DRIVE -ne '1') -or $full -match '^[Dd]:\\Ryzen(\\|$)') { throw "Root '$full' must be on D:\ and outside D:\Ryzen" }
$Root = $full
$ts = Get-Date -Format 'yyyyMMdd-HHmmss'
$LogDir = Join-Path $Root 'logs'
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$LogFile = Join-Path $LogDir "rollback-$ts.log"
$V1Order = @('siloforge_pg', 'siloforge_redis', 'siloforge_qdrant', 'siloforge_api', 'siloforge_ui', 'siloforge-worker-1', 'siloforge-worker-2')

function Log([string]$msg, [string]$color = 'Gray') {
    $line = (Get-Date -Format 'HH:mm:ss') + '  ' + $msg
    Write-Host $line -ForegroundColor $color
    if (-not $WhatIfPreference) { Add-Content -Path $LogFile -Value $line -Encoding UTF8 }
}
function Docker([string[]]$a) {
    $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $out = & docker @a 2>&1 | ForEach-Object { "$_" }
    $rc = $LASTEXITCODE
    $ErrorActionPreference = $prev
    return , @($rc, $out)
}

Log "SiloForge rollback to v1   root $Root   task $TaskName" 'Cyan'

# 1. stop v2 ------------------------------------------------------------------------------------
if ($PSCmdlet.ShouldProcess("v2 ($TaskName + siloforge.exe under $Root)", 'disable and stop')) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Disable-ScheduledTask -TaskName $TaskName | Out-Null
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Log "task $TaskName disabled"
    }
    $pattern = '^' + [regex]::Escape($Root) + '\\'
    $procs = @(Get-CimInstance Win32_Process -Filter "Name='siloforge.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath -match $pattern -and $_.CommandLine -match '\sserve(\s|$)' })
    foreach ($p in $procs) { Log ("stopping siloforge pid " + $p.ProcessId); Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
    for ($i = 0; $i -lt 30; $i++) {
        if (-not (Get-NetTCPConnection -LocalPort 8600 -State Listen -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Seconds 1
    }
    if (Get-NetTCPConnection -LocalPort 8600 -State Listen -ErrorAction SilentlyContinue) {
        Log 'WARNING: something still listens on :8600 (another siloforge.exe outside Root?)' 'Yellow'
    }
}

# 2. start v1 -----------------------------------------------------------------------------------
if ($PSCmdlet.ShouldProcess(($V1Order -join ', '), 'docker start')) {
    foreach ($c in $V1Order) {
        $r = Docker @('start', $c)
        if ($r[0] -ne 0) { Log ("docker start $c FAILED: " + (($r[1] | Select-Object -Last 1))) 'Red' } else { Log "started $c" }
        if ($c -eq 'siloforge_qdrant') { Start-Sleep -Seconds 3 }   # api/workers need pg/redis/qdrant first
    }
}

# 3. health -------------------------------------------------------------------------------------
$healthy = $false
if (-not $WhatIfPreference) {
    $deadline = (Get-Date).AddSeconds(120)
    while ((Get-Date) -lt $deadline) {
        try {
            $h = Invoke-RestMethod -Uri 'http://127.0.0.1:8600/health' -TimeoutSec 5 -UseBasicParsing
            if ($h.ok -eq $true -and -not $h.version) { $healthy = $true; break }
        } catch { }
        Start-Sleep -Seconds 2
    }
    if ($healthy) { Log 'v1 healthy on :8600 (db/redis/qdrant ok)' 'Green' }
    else { Log 'v1 /health not OK within 120 s. Check: docker ps; docker logs siloforge_api --tail 50' 'Red' }
    try {
        $u = Invoke-WebRequest -Uri 'http://127.0.0.1:8601/' -TimeoutSec 10 -UseBasicParsing
        Log ('v1 UI :8601 status ' + $u.StatusCode)
    } catch { Log 'v1 UI :8601 not answering yet' 'Yellow' }
}

# 4. configs ------------------------------------------------------------------------------------
if (-not $KeepConfigs) {
    foreach ($f in $ConfigFiles) {
        $bak = $f + '.bak-pre-sf2'
        if (-not (Test-Path $bak)) { continue }
        if ($PSCmdlet.ShouldProcess($f, "restore from $bak")) {
            if (Test-Path $f) { Copy-Item $f ($f + ".sf2-rolledback-$ts") -Force }
            Copy-Item $bak $f -Force
            Log "restored $f from .bak-pre-sf2"
        }
    }
    Log 'configs: done (files without a .bak-pre-sf2 copy were left alone)'
    Log 'NOTE: Hermes picks up the restored ap13 config in new agent sessions; Backup Hub/Model Control/MC reload their config on their own schedule (restart them if unsure).' 'Yellow'
}

if ($WhatIfPreference) { exit 0 }
Log ("rollback " + $(if ($healthy) { 'DONE' } else { 'FINISHED WITH ERRORS' }) + "   log: $LogFile") $(if ($healthy) { 'Green' } else { 'Red' })
if ($healthy) { exit 0 } else { exit 1 }
