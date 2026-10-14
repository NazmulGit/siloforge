<#
.SYNOPSIS
  Cut over from SiloForge v1 (Docker, 7 containers) to v2 (siloforge.exe) on 127.0.0.1:8600/8601.

.DESCRIPTION
  Implements docs/research/04-integrations.md section 9. Without -Execute it is a DRY RUN: it runs the
  read-only pre-checks and prints the plan, and changes nothing.

  With -Execute:
    1. Pre-checks (read-only): docker + the 7 v1 containers, no queued/running v1 jobs
       (`docker exec siloforge_pg psql` with default_transaction_read_only), v2 installed (current\siloforge.exe,
       data home with secrets.env + siloforge.toml, scheduled task), disk space, JWT_SECRET parity.
    2. Stop the v1 WRITERS (stop, never remove): siloforge_api, siloforge_ui, siloforge-worker-1, siloforge-worker-2.
       siloforge_pg, siloforge_redis and siloforge_qdrant keep running (source for the import, and rollback).
    3. Final dump of the now-frozen v1 DB: `pg_dump -Fc` (read-only) -> D:\Backups\SiloForge\v1-final-<ts>.dump,
       checked with `pg_restore --list`. (The writers are stopped first so the dump and the import see the same data.)
    4. Fresh import: an existing <home>\siloforge.db (from an earlier test import) is moved to
       <home>\pre-cutover-<ts>\ (never deleted), then `siloforge migrate from-postgres` imports everything again
       (the importer uses INSERT OR IGNORE, so a re-import into an old DB would keep stale rows).
    5. `siloforge migrate verify` (row counts + embedding samples vs v1). Failure -> v1 is restarted automatically.
    6. Enable + start the scheduled task; wait for v2 /health on :8600 (v1's /health has no "version").
       No healthy v2 within 120 s -> automatic rollback (v2 stopped, v1 containers started).
    7. Smoke: login (credentials read from secrets.env, never printed), GET /api/v1/sites count == v1 count,
       UI on :8601, MCP `tools/list` through `siloforge mcp` (expects 28 tools), `siloforge doctor`.
    8. Prints the manual follow-ups (Hermes, Backup Hub, Model Control, Mission Control).

  Everything is logged to <Root>\logs\cutover-<ts>.log. Secret values are never printed or logged.
  Rollback at any time: scripts\rollback-to-v1.ps1. PowerShell 5.1 compatible.

.EXAMPLE
  .\cutover-from-v1.ps1                 # dry run: checks + plan
  .\cutover-from-v1.ps1 -Execute        # do it
#>
[CmdletBinding()]
param(
    [switch]$Execute,
    [string]$Root = 'D:\Final_Apps\SiloForge',
    [Alias('Home')][string]$SfHome = '',
    [string]$TaskName = 'siloforge-keepalive',
    [string]$V1Env = 'D:\Code\SiloForge-Suite\app\.env',
    [string]$BackupDir = 'D:\Backups\SiloForge',
    [int]$StaleJobHours = 0,
    [int]$ExpectedTools = 28,
    [switch]$SkipDump,
    [switch]$NoAutoRollback
)

$ErrorActionPreference = 'Stop'
function Assert-DPath([string]$Path, [string]$What) {
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($full -notmatch '^[Dd]:\\' -and $env:SILOFORGE_ALLOW_ANY_DRIVE -ne '1') { throw "$What '$full' is not on D:\ (house rule)" }
    if ($full -match '^[Dd]:\\Ryzen(\\|$)') { throw "$What '$full' is inside D:\Ryzen (another user's folder)" }
    return $full.TrimEnd('\')
}
$Root = Assert-DPath $Root 'Root'
if (-not $SfHome) { $SfHome = Join-Path $Root 'data' }
$SfHome = Assert-DPath $SfHome 'Home'
$BackupDir = Assert-DPath $BackupDir 'BackupDir'
$exe = Join-Path $Root 'current\siloforge.exe'
$ts = Get-Date -Format 'yyyyMMdd-HHmmss'
$LogDir = Join-Path $Root 'logs'
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$LogFile = Join-Path $LogDir ("cutover-$ts" + $(if ($Execute) { '' } else { '-dryrun' }) + '.log')

$V1Writers = @('siloforge_api', 'siloforge_ui', 'siloforge-worker-1', 'siloforge-worker-2')
$V1Keep = @('siloforge_pg', 'siloforge_redis', 'siloforge_qdrant')
$Api = 'http://127.0.0.1:8600'
$Ui = 'http://127.0.0.1:8601'

function Log([string]$msg, [string]$color = 'Gray') {
    $line = (Get-Date -Format 'HH:mm:ss') + '  ' + $msg
    Write-Host $line -ForegroundColor $color
    Add-Content -Path $LogFile -Value $line -Encoding UTF8
}
function Step([string]$msg) { Log ''; Log ('== ' + $msg) 'Cyan' }
function Die([string]$msg) { Log ('FAILED: ' + $msg) 'Red'; Log "log: $LogFile"; exit 1 }
function Run-Native([string]$Exe, [string[]]$ArgList, [switch]$Quiet) {
    $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $out = & $Exe @ArgList 2>&1 | ForEach-Object { "$_" }
    $rc = $LASTEXITCODE
    $ErrorActionPreference = $prev
    if (-not $Quiet) { foreach ($l in $out) { if ($l.Trim()) { Log ('   ' + $l) 'DarkGray' } } }
    return , @($rc, $out)
}
function Psql-ReadOnly([string]$sql) {
    # One read-only session; output = unaligned tuples only.
    $r = Run-Native 'docker' @('exec', 'siloforge_pg', 'psql', '-U', 'siloforge', '-d', 'siloforge', '-v', 'ON_ERROR_STOP=1', '-tA',
        '-c', 'SET default_transaction_read_only = on;', '-c', $sql) -Quiet
    if ($r[0] -ne 0) { throw ('psql failed: ' + (($r[1] | Select-Object -Last 2) -join ' | ')) }
    return @($r[1] | Where-Object { $_ -and $_ -ne 'SET' })
}
function Read-EnvFile([string]$path) {
    $h = @{}
    if (-not (Test-Path $path)) { return $h }
    foreach ($line in [System.IO.File]::ReadAllLines($path)) {
        $l = $line.Trim().TrimStart([char]0xFEFF)
        if (-not $l -or $l.StartsWith('#')) { continue }
        if ($l.StartsWith('export ')) { $l = $l.Substring(7) }
        $i = $l.IndexOf('=')
        if ($i -lt 1) { continue }
        $k = $l.Substring(0, $i).Trim(); $v = $l.Substring($i + 1).Trim()
        if ($v.Length -ge 2 -and (($v[0] -eq '"' -and $v[-1] -eq '"') -or ($v[0] -eq "'" -and $v[-1] -eq "'"))) { $v = $v.Substring(1, $v.Length - 2) }
        $h[$k] = $v
    }
    return $h
}
function Get-V2Health([int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        try {
            $r = Invoke-RestMethod -Uri "$Api/health" -TimeoutSec 5 -UseBasicParsing
            if ($r.ok -eq $true -and $r.version) { return $r }   # v1's /health has no version field
        } catch { }
        Start-Sleep -Seconds 2
    }
    return $null
}
function Start-V1 {
    Log 'starting v1 writers again (docker start)' 'Yellow'
    foreach ($c in $V1Keep + $V1Writers) { $null = Run-Native 'docker' @('start', $c) -Quiet }
}
function Invoke-Rollback([string]$why) {
    Log ("ROLLBACK: $why") 'Red'
    if ($NoAutoRollback) { Log 'auto-rollback disabled (-NoAutoRollback). Run scripts\rollback-to-v1.ps1 yourself.' 'Yellow'; exit 1 }
    $rb = Join-Path $PSScriptRoot 'rollback-to-v1.ps1'
    if (Test-Path $rb) {
        & $rb -Root $Root -TaskName $TaskName -KeepConfigs -Confirm:$false
    } else {
        Start-V1
    }
    Log "log: $LogFile"
    exit 1
}

Log ("SiloForge cutover v1 -> v2   mode: " + $(if ($Execute) { 'EXECUTE' } else { 'DRY RUN (nothing is changed)' })) 'Cyan'
Log "root $Root   home $SfHome   task $TaskName   log $LogFile"

# ================================================================================================ 1. pre-checks
Step '1. pre-checks (read-only)'
$problems = New-Object System.Collections.ArrayList
$warnings = New-Object System.Collections.ArrayList

$docker = Get-Command docker -ErrorAction SilentlyContinue
if (-not $docker) { Die 'docker not found' }
$ps = Run-Native 'docker' @('ps', '-a', '--format', '{{.Names}}|{{.State}}') -Quiet
$states = @{}
foreach ($l in $ps[1]) { $p = $l.Split('|'); if ($p.Count -eq 2) { $states[$p[0]] = $p[1] } }
foreach ($c in $V1Keep + $V1Writers) {
    $s = $states[$c]
    if (-not $s) { [void]$problems.Add("container $c not found") }
    else { Log ("   {0,-20} {1}" -f $c, $s) }
}
if ($states['siloforge_pg'] -ne 'running') { [void]$problems.Add('siloforge_pg is not running (needed for the import)') }

$v1Sites = $null
if ($states['siloforge_pg'] -eq 'running') {
    try {
        $active = Psql-ReadOnly "SELECT id, kind, status, coalesce(started_at, created_at), round(extract(epoch FROM now() - coalesce(started_at, created_at))/3600.0, 1) FROM job_runs WHERE status IN ('queued','running') ORDER BY created_at"
        $blocking = @()
        foreach ($row in $active) {
            $f = $row.Split('|')
            $age = [double]$f[4]
            $stale = ($StaleJobHours -gt 0 -and $age -ge $StaleJobHours)
            Log ("   v1 job {0} {1} {2} since {3} ({4} h){5}" -f $f[0], $f[1], $f[2], $f[3], $f[4], $(if ($stale) { '  [stale, ignored]' } else { '' })) 'Yellow'
            if (-not $stale) { $blocking += $row }
        }
        if ($blocking.Count -gt 0) {
            [void]$problems.Add(("{0} v1 job(s) queued/running. Wait, or if they are phantom rows pass -StaleJobHours N" -f $blocking.Count))
        } else { Log '   v1 jobs: none queued/running (or only stale ones)' }
        $v1Sites = [int](Psql-ReadOnly 'SELECT count(*) FROM sites')[0]
        $v1Counts = (Psql-ReadOnly 'SELECT (SELECT count(*) FROM posts), (SELECT count(*) FROM paragraphs), (SELECT count(*) FROM link_edges)')[0]
        Log ("   v1 data: sites $v1Sites, posts|paragraphs|edges $v1Counts")
        $pgSize = (Psql-ReadOnly "SELECT pg_size_pretty(pg_database_size('siloforge'))")[0]
        Log "   v1 database size $pgSize"
    } catch { [void]$problems.Add('v1 database query failed: ' + $_.Exception.Message) }
}

if (-not (Test-Path $exe)) { [void]$problems.Add("v2 not installed: $exe missing (run scripts\install.ps1 -FromV1Env $V1Env)") }
else {
    $vr = Run-Native $exe @('version') -Quiet
    Log ('   v2 binary: ' + (($vr[1] | Select-Object -First 1)))
}
foreach ($f in @('secrets.env', 'siloforge.toml')) {
    if (-not (Test-Path (Join-Path $SfHome $f))) { [void]$problems.Add("$SfHome\$f missing (run scripts\install.ps1)") }
}
if (-not (Test-Path (Join-Path $SfHome 'runtime\onnxruntime.dll'))) { [void]$warnings.Add('no runtime\onnxruntime.dll in the home: local embeddings fall back to the remote rung') }
if (-not (Test-Path (Join-Path $SfHome 'models\all-MiniLM-L6-v2\model.onnx'))) { [void]$warnings.Add('no models\all-MiniLM-L6-v2\model.onnx in the home') }
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $task) { [void]$problems.Add("scheduled task $TaskName missing (run scripts\install.ps1)") }
else { Log ("   task $TaskName state " + $task.State) }

$secrets = Read-EnvFile (Join-Path $SfHome 'secrets.env')
$v1env = Read-EnvFile $V1Env
if ($secrets['JWT_SECRET'] -and $v1env['JWT_SECRET']) {
    if ($secrets['JWT_SECRET'] -ceq $v1env['JWT_SECRET']) { Log '   JWT_SECRET matches v1 (existing UI sessions and plugin site tokens stay valid)' }
    else { [void]$warnings.Add('JWT_SECRET differs from v1: UI users log in again and plugin sites re-handshake (both handle 401)') }
}
if (-not $secrets['ADMIN_PASSWORD']) { [void]$warnings.Add('ADMIN_PASSWORD not in secrets.env: the login smoke test will be skipped') }

$drive = Get-PSDrive -Name D -ErrorAction SilentlyContinue
if ($drive) {
    $freeGb = [math]::Round($drive.Free / 1GB, 1)
    Log "   D: free $freeGb GB"
    if ($freeGb -lt 10) { [void]$problems.Add("only $freeGb GB free on D: (need ~10 GB: dump ~1 GB + SQLite ~2 GB + headroom)") }
}

foreach ($w in $warnings) { Log ('   WARN ' + $w) 'Yellow' }
foreach ($p in $problems) { Log ('   BLOCKER ' + $p) 'Red' }

# ================================================================================================ plan
$dumpFile = Join-Path $BackupDir "v1-final-$ts.dump"
Step 'plan'
$plan = @(
    "2. docker stop $($V1Writers -join ' ')   (stop, NOT remove; pg/redis/qdrant keep running)",
    $(if ($SkipDump) { '3. (skipped: -SkipDump)' } else { "3. docker exec siloforge_pg pg_dump -Fc -U siloforge siloforge > $dumpFile  (+ pg_restore --list check)" }),
    "4. move an existing $SfHome\siloforge.db* to $SfHome\pre-cutover-$ts\, then: siloforge migrate from-postgres --home $SfHome",
    "5. siloforge migrate verify --home $SfHome   (failure -> v1 restarted)",
    "6. Enable + Start scheduled task $TaskName; wait for v2 /health on :8600 (failure -> auto rollback)",
    "7. smoke: login, /api/v1/sites count == $v1Sites, UI :8601, MCP tools/list == $ExpectedTools, doctor",
    '8. print the manual follow-ups'
)
foreach ($l in $plan) { Log ('   ' + $l) }

if (-not $Execute) {
    if ($problems.Count -gt 0) { Log ("DRY RUN: {0} blocker(s) above must be fixed first." -f $problems.Count) 'Yellow' }
    else { Log 'DRY RUN: all pre-checks passed. Re-run with -Execute to cut over.' 'Green' }
    Log "log: $LogFile"
    exit 0
}
if ($problems.Count -gt 0) { Die ("{0} blocker(s); nothing was changed" -f $problems.Count) }

# ================================================================================================ 2. stop writers
Step '2. stop v1 writers (containers are kept)'
foreach ($c in $V1Writers) {
    $r = Run-Native 'docker' @('stop', '-t', '30', $c)
    if ($r[0] -ne 0) { Start-V1; Die "docker stop $c failed (v1 restarted)" }
}
for ($i = 0; $i -lt 30; $i++) {
    if (-not (Get-NetTCPConnection -LocalPort 8600 -State Listen -ErrorAction SilentlyContinue) -and
        -not (Get-NetTCPConnection -LocalPort 8601 -State Listen -ErrorAction SilentlyContinue)) { break }
    Start-Sleep -Seconds 1
}
if (Get-NetTCPConnection -LocalPort 8600 -State Listen -ErrorAction SilentlyContinue) { Start-V1; Die 'port 8600 still in use after stopping v1 (v1 restarted)' }
Log '   ports 8600/8601 are free'

# ================================================================================================ 3. dump
if (-not $SkipDump) {
    Step '3. final v1 dump (read-only)'
    New-Item -ItemType Directory -Force -Path $BackupDir | Out-Null
    # cmd redirection keeps the custom-format dump byte-exact (PowerShell pipes would re-encode it).
    $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    cmd /c "docker exec siloforge_pg pg_dump -Fc -U siloforge siloforge > `"$dumpFile`"" 2>&1 | ForEach-Object { Log ('   ' + $_) 'DarkGray' }
    $rc = $LASTEXITCODE
    $toc = cmd /c "docker exec -i siloforge_pg pg_restore --list < `"$dumpFile`"" 2>$null
    $ErrorActionPreference = $prev
    $size = if (Test-Path $dumpFile) { (Get-Item $dumpFile).Length } else { 0 }
    $tocLines = @($toc | Where-Object { $_ -and -not $_.StartsWith(';') }).Count
    if ($rc -ne 0 -or $size -lt 1MB -or $tocLines -lt 10) { Invoke-Rollback "pg_dump failed (rc $rc, $size bytes, $tocLines toc entries)" }
    Log ("   $dumpFile  " + [math]::Round($size / 1MB, 1) + " MB, $tocLines TOC entries, sha256 " + (Get-FileHash $dumpFile -Algorithm SHA256).Hash.ToLower())
}

# ================================================================================================ 4. import
Step '4. fresh import into the v2 home'
$moved = Join-Path $SfHome "pre-cutover-$ts"
$old = @(Get-ChildItem $SfHome -File -Filter 'siloforge.db*' -ErrorAction SilentlyContinue)
if ($old.Count -gt 0) {
    New-Item -ItemType Directory -Force -Path $moved | Out-Null
    foreach ($f in $old) { Move-Item $f.FullName (Join-Path $moved $f.Name) }
    Log ("   moved {0} old db file(s) to {1}" -f $old.Count, $moved)
}
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$r = Run-Native $exe @('migrate', 'from-postgres', '--home', $SfHome)
$sw.Stop()
if ($r[0] -ne 0) { Invoke-Rollback 'migrate from-postgres failed' }
Log ('   import done in ' + [math]::Round($sw.Elapsed.TotalMinutes, 1) + ' min')

# ================================================================================================ 5. verify
Step '5. migrate verify'
$r = Run-Native $exe @('migrate', 'verify', '--home', $SfHome)
if ($r[0] -ne 0) { Invoke-Rollback 'migrate verify reported differences' }

# ================================================================================================ 6. start v2
Step '6. start v2'
Enable-ScheduledTask -TaskName $TaskName | Out-Null
Start-ScheduledTask -TaskName $TaskName
$h = Get-V2Health 120
if (-not $h) { Invoke-Rollback 'v2 did not become healthy on :8600 within 120 s' }
Log ("   v2 healthy: version {0}, db {1}, jobs running {2} queued {3}" -f $h.version, $h.services.db, $h.jobs.running, $h.jobs.queued) 'Green'

# ================================================================================================ 7. smoke
Step '7. smoke tests'
$smokeFail = 0
$token = $null
if ($secrets['ADMIN_PASSWORD']) {
    $email = if ($secrets['ADMIN_EMAIL']) { $secrets['ADMIN_EMAIL'] } else { 'admin@siloforge.local' }
    try {
        $body = @{ email = $email; password = $secrets['ADMIN_PASSWORD'] } | ConvertTo-Json -Compress
        $login = Invoke-RestMethod -Method Post -Uri "$Api/api/v1/auth/login" -ContentType 'application/json' -Body $body -TimeoutSec 15 -UseBasicParsing
        $token = $login.token
        Log ("   login ok (" + $login.user.email + ", role " + $login.user.role + ")") 'Green'
    } catch { $smokeFail++; Log ('   login FAILED: ' + $_.Exception.Message) 'Red' }
} else { Log '   login skipped (no ADMIN_PASSWORD in secrets.env)' 'Yellow' }

if ($token) {
    $hdr = @{ Authorization = "Bearer $token" }
    try {
        $sites = @(Invoke-RestMethod -Uri "$Api/api/v1/sites" -Headers $hdr -TimeoutSec 60 -UseBasicParsing)
        $n = $sites.Count
        if ($null -ne $v1Sites -and $n -ne $v1Sites) { $smokeFail++; Log "   /api/v1/sites: $n (v1 had $v1Sites)" 'Red' }
        else { Log "   /api/v1/sites: $n (v1 had $v1Sites)" 'Green' }
    } catch { $smokeFail++; Log ('   /api/v1/sites FAILED: ' + $_.Exception.Message) 'Red' }

    # MCP over stdio: initialize, initialized, tools/list. The token only lives in the child's environment.
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $exe
        $psi.Arguments = 'mcp'
        $psi.UseShellExecute = $false
        $psi.RedirectStandardInput = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
        $psi.EnvironmentVariables['SILOFORGE_API_URL'] = $Api
        $psi.EnvironmentVariables['SILOFORGE_TOKEN'] = $token
        $proc = [System.Diagnostics.Process]::Start($psi)
        $null = $proc.StandardError.ReadToEndAsync()
        $proc.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cutover-smoke","version":"1"}}}')
        $proc.StandardInput.WriteLine('{"jsonrpc":"2.0","method":"notifications/initialized"}')
        $proc.StandardInput.WriteLine('{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}')
        $proc.StandardInput.Flush()
        $tools = -1
        $deadline = (Get-Date).AddSeconds(30)
        while ((Get-Date) -lt $deadline -and $tools -lt 0) {
            $t = $proc.StandardOutput.ReadLineAsync()
            if (-not $t.Wait(30000)) { break }
            $line = $t.Result
            if ($null -eq $line) { break }
            if ($line -match '"id"\s*:\s*2') { $tools = @(($line | ConvertFrom-Json).result.tools).Count }
        }
        $proc.StandardInput.Close()
        if (-not $proc.WaitForExit(5000)) { $proc.Kill() }
        if ($tools -eq $ExpectedTools) { Log "   MCP tools/list: $tools tools" 'Green' }
        else { $smokeFail++; Log "   MCP tools/list: $tools tools (expected $ExpectedTools)" 'Red' }
    } catch { $smokeFail++; Log ('   MCP smoke FAILED: ' + $_.Exception.Message) 'Red' }
}
try {
    $u = Invoke-WebRequest -Uri "$Ui/" -TimeoutSec 15 -UseBasicParsing
    if ($u.StatusCode -eq 200 -and $u.Content -match '<div id="root"') { Log '   UI :8601 serves the app' 'Green' }
    else { $smokeFail++; Log ('   UI :8601 status ' + $u.StatusCode) 'Red' }
} catch { $smokeFail++; Log ('   UI :8601 FAILED: ' + $_.Exception.Message) 'Red' }

$r = Run-Native $exe @('doctor', '--home', $SfHome)
if ($r[0] -ne 0) { Log '   doctor reported problems (see above)' 'Yellow' }

# ================================================================================================ 8. next steps
Step '8. manual follow-ups (docs/MIGRATION.md has the details)'
$next = @(
    "Hermes ap13: siloforge token create --name hermes-ap13 --scopes read,write --home `"$SfHome`" (printed once), then in",
    '  D:\Agent\hermes\profiles\ap13_internallink\config.yaml set mcp_servers.siloforge: command = D:\Final_Apps\SiloForge\current\siloforge.exe,',
    '  args [mcp], env SILOFORGE_API_URL=http://127.0.0.1:8600 + SILOFORGE_TOKEN; remove SILOFORGE_EMAIL/SILOFORGE_PASSWORD',
    '  (save the old file as config.yaml.bak-pre-sf2 first). New agent sessions pick it up; no gateway restart.',
    'Backup Hub: replace the pg_dump(siloforge_pg) + siloforge_qdrantdata/pgdata hooks in every tier profile + Hermes A-Z with',
    "  a pre-hook 'siloforge backup create --kind db --home $SfHome' and include D:\Backups\SiloForge\v2 (config.json.bak-pre-sf2 first).",
    'Model Control: repoint the siloforge inventory entry from app\.env DARIO_MODEL_* to the v2 home (secrets.env or Settings > Models);',
    '  restart command = Stop/Start-ScheduledTask siloforge-keepalive.',
    'Mission Control: services.json rows siloforge-api / siloforge-ui keep their probes (:8600/health, :8601/); update notes',
    '  and, optionally, the start command to Start-ScheduledTask siloforge-keepalive.',
    'Optional: Hermes webhook route siloforge-events + a "hermes" notification channel (docs/HERMES.md).',
    'Unpause t8-internallink cards one at a time. Keep the v1 containers (stopped) until v2 has run a week.',
    'Rollback: scripts\rollback-to-v1.ps1'
)
foreach ($l in $next) { Log ('   ' + $l) }

if ($smokeFail -gt 0) { Log ("CUTOVER DONE WITH $smokeFail SMOKE FAILURE(S). v2 is running; decide: fix, or scripts\rollback-to-v1.ps1") 'Yellow'; Log "log: $LogFile"; exit 3 }
Log 'CUTOVER DONE. v2 serves 127.0.0.1:8600/8601.' 'Green'
Log "log: $LogFile"
exit 0
