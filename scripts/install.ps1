<#
.SYNOPSIS
  Install or upgrade SiloForge v2 on this PC (default D:\Final_Apps\SiloForge), with a keepalive Scheduled Task.

.DESCRIPTION
  Layout (Root = D:\Final_Apps\SiloForge unless -Root is given):
    <Root>\app\<version>\     siloforge.exe + ONNX Runtime DLLs + docs (one folder per installed version)
    <Root>\current            junction -> app\<version> (or a plain copy with -CopyCurrent)
    <Root>\data\              SILOFORGE_HOME (siloforge.db, siloforge.toml, secrets.env, logs, models, runtime, backups)
    <Root>\keeper\<task>.vbs  hidden keeper started by the Scheduled Task
    <Root>\logs\              install/cutover logs
    <Root>\MANIFEST.json      what is installed (no secrets)

  Steps (each one is idempotent; run it again to repair or upgrade):
    1. Pre-upgrade: when a database already exists, `siloforge backup create --kind db` with the installed exe.
    2. Copy the release into app\<version>\ and point `current` at it (the server is stopped first on upgrade).
    3. Data home: create it; copy runtime DLLs and the all-MiniLM-L6-v2 model into it if missing.
    4. `siloforge init` when secrets.env is missing (-FromV1Env imports JWT_SECRET/ADMIN_*/OPENROUTER_*/DARIO_*
       from the v1 .env so v1 tokens stay valid). Values are never printed.
    5. Scheduled Task (default `siloforge-keepalive`): as the current user, at logon + every 5 min, hidden
       (wscript keeper.vbs). The keeper exits at once when the first bind port already listens, else runs
       `siloforge serve` with SILOFORGE_HOME set (and SILOFORGE_BINDS when -Binds is given).
    6. Start + health check GET http://<first bind>/health.

  If the first bind port is held by something else (the v1 Docker stack holds 8600 until cutover), the task is
  registered DISABLED and nothing is started; scripts\cutover-from-v1.ps1 enables it.

  PowerShell 5.1 compatible. No elevation needed (the task runs as you, Interactive, Limited). Supports -WhatIf.

.EXAMPLE
  .\install.ps1                                         # from an unzipped release (siloforge.exe next to scripts\)
  .\install.ps1 -Source D:\Code\SiloForge-Suite\siloforge-rs\dist\2.0.0 -FromV1Env D:\Code\SiloForge-Suite\app\.env
  .\install.ps1 -Root D:\tmp\sf-install-test -Binds 127.0.0.1:8656,127.0.0.1:8657 -TaskName siloforge-keepalive-test
  .\install.ps1 -Uninstall                              # task + keeper + process; keeps app\ and data\
  .\install.ps1 -Uninstall -Purge                       # also deletes <Root> (data included!)
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$Source = '',
    [string]$Root = 'D:\Final_Apps\SiloForge',
    [Alias('Home')][string]$SfHome = '',
    [string]$TaskName = 'siloforge-keepalive',
    [string]$Binds = '',
    [string]$FromV1Env = '',
    [string]$RuntimeDir = 'D:\Download\onnxruntime\dml-1.24.4',
    [string]$ModelDir = 'D:\Download\models\Xenova-all-MiniLM-L6-v2',
    [string]$BackupDir = '',
    [int]$IntervalMinutes = 5,
    [switch]$FetchModel,
    [switch]$CopyCurrent,
    [switch]$NoStart,
    [switch]$Uninstall,
    [switch]$Force,
    [switch]$Purge
)
# -Home is an alias of -SfHome ($Home is a read-only automatic variable in PowerShell).

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot

function Write-Step([string]$msg, [string]$color = 'Cyan') { Write-Host ('[siloforge] ' + $msg) -ForegroundColor $color }
function Assert-DPath([string]$Path, [string]$What) {
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($full -notmatch '^[Dd]:\\' -and $env:SILOFORGE_ALLOW_ANY_DRIVE -ne '1') { throw "$What '$full' is not on D:\ (house rule)" }
    if ($full -match '^[Dd]:\\Ryzen(\\|$)') { throw "$What '$full' is inside D:\Ryzen (another user's folder)" }
    return $full.TrimEnd('\')
}
function Write-Utf8NoBom([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}
function Invoke-Sf([string]$Exe, [string[]]$SfArgs) {
    # Runs siloforge.exe; stderr (tracing) is tolerated; returns @(exitcode, output-lines).
    $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $out = & $Exe @SfArgs 2>&1 | ForEach-Object { "$_" }
    $rc = $LASTEXITCODE
    $ErrorActionPreference = $prev
    return , @($rc, $out)
}
function Get-SfProcesses([string]$RootDir) {
    $pattern = '^' + [regex]::Escape($RootDir) + '\\'
    @(Get-CimInstance Win32_Process -Filter "Name='siloforge.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and ($_.ExecutablePath -match $pattern) -and ($_.CommandLine -match '\sserve(\s|$)') })
}
function Stop-SfServer([string]$RootDir, [string]$Task) {
    $t = Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
    if ($t) { Stop-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue }
    foreach ($p in (Get-SfProcesses $RootDir)) {
        Write-Step ("stopping siloforge pid " + $p.ProcessId)
        # Ctrl-C is not deliverable to a hidden process; jobs left 'running' are re-queued by the reaper at the next boot.
        Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
    }
    for ($i = 0; $i -lt 20 -and (Get-SfProcesses $RootDir).Count -gt 0; $i++) { Start-Sleep -Milliseconds 500 }
}
function Get-PortOwner([int]$Port) {
    $c = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $c) { return $null }
    $p = Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue
    if ($p) { return $p.ProcessName } else { return ('pid ' + $c.OwningProcess) }
}
function Test-Health([string]$Bind, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        try {
            $r = Invoke-RestMethod -Uri ("http://$Bind/health") -TimeoutSec 5 -UseBasicParsing
            if ($r.ok -eq $true -and $r.version) { return $r }
        } catch { }
        Start-Sleep -Seconds 2
    }
    return $null
}

# ------------------------------------------------------------------------------------------------ paths
$Root = Assert-DPath $Root 'Root'
if (-not $SfHome) { $SfHome = Join-Path $Root 'data' }
$SfHome = Assert-DPath $SfHome 'Home'
$AppDir = Join-Path $Root 'app'
$Current = Join-Path $Root 'current'
$KeeperDir = Join-Path $Root 'keeper'
$Keeper = Join-Path $KeeperDir ($TaskName + '.vbs')
$LogDir = Join-Path $Root 'logs'
$isDefaultRoot = ($Root -ieq 'D:\Final_Apps\SiloForge')
if (-not $BackupDir -and $isDefaultRoot) { $BackupDir = 'D:\Backups\SiloForge\v2' }
if ($BackupDir) { $BackupDir = Assert-DPath $BackupDir 'BackupDir' }

# ------------------------------------------------------------------------------------------------ uninstall
if ($Uninstall) {
    if ($PSCmdlet.ShouldProcess($TaskName, 'remove scheduled task, keeper and stop the server')) {
        Stop-SfServer $Root $TaskName
        if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
            Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
            Write-Step "removed scheduled task $TaskName"
        }
        if (Test-Path $Keeper) { Remove-Item $Keeper -Force }
    }
    if ($Purge) {
        if ($PSCmdlet.ShouldProcess($Root, 'DELETE the whole install folder including data')) {
            if (Test-Path $Current) { cmd /c rmdir "$Current" 2>$null | Out-Null }   # junction first (never recurse into it)
            if (Test-Path $Root) { Remove-Item $Root -Recurse -Force }
            Write-Step "purged $Root"
        }
    } else {
        Write-Step "kept $Root (app + data). Use -Purge to delete it."
    }
    exit 0
}

# ------------------------------------------------------------------------------------------------ source
if (-not $Source) {
    $parent = Split-Path -Parent $here
    if (Test-Path (Join-Path $parent 'siloforge.exe')) { $Source = $parent }                     # unzipped release
    elseif (Test-Path (Join-Path $parent 'VERSION')) {                                           # repo checkout
        $v = ([System.IO.File]::ReadAllText((Join-Path $parent 'VERSION'))).Trim()
        $Source = Join-Path $parent ("dist\" + $v)
    }
}
if (-not $Source -or -not (Test-Path (Join-Path $Source 'siloforge.exe'))) {
    throw "siloforge.exe not found (pass -Source <folder with siloforge.exe>, e.g. dist\<version> after build-release.ps1)"
}
$Source = (Resolve-Path $Source).Path.TrimEnd('\')
$res = Invoke-Sf (Join-Path $Source 'siloforge.exe') @('version')
$verLine = ($res[1] | Where-Object { $_ -match '^siloforge \d' } | Select-Object -First 1)
if ($res[0] -ne 0 -or -not $verLine) { throw "could not read the version of $Source\siloforge.exe" }
$Version = $verLine.Split(' ')[1].Trim()
$VerDir = Join-Path $AppDir $Version

$bindList = @()
if ($Binds) { $bindList = @($Binds.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ }) }
$firstBind = if ($bindList.Count -gt 0) { $bindList[0] } else { '127.0.0.1:8600' }
if ($bindList.Count -eq 0 -and (Test-Path (Join-Path $SfHome 'siloforge.toml'))) {
    $m = [regex]::Match([System.IO.File]::ReadAllText((Join-Path $SfHome 'siloforge.toml')), '(?m)^\s*binds\s*=\s*\[\s*"([^"]+)"')
    if ($m.Success) { $firstBind = $m.Groups[1].Value }
}
$port = [int]($firstBind.Split(':')[-1])

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$ts = Get-Date -Format 'yyyyMMdd-HHmmss'
Write-Step "installing SiloForge $Version"
Write-Step "  source  $Source"
Write-Step "  root    $Root"
Write-Step "  home    $SfHome"
Write-Step "  task    $TaskName (every $IntervalMinutes min + at logon)"
Write-Step "  listen  $firstBind"

# ------------------------------------------------------------------------------------------------ 1. pre-upgrade backup
$sameExe = (Test-Path (Join-Path $VerDir 'siloforge.exe')) -and
           ((Get-FileHash (Join-Path $VerDir 'siloforge.exe')).Hash -eq (Get-FileHash (Join-Path $Source 'siloforge.exe')).Hash)
$installedExe = Join-Path $Current 'siloforge.exe'
$dbPath = Join-Path $SfHome 'siloforge.db'
if ((-not $sameExe) -and (Test-Path $dbPath) -and (Test-Path $installedExe) -and (Test-Path (Join-Path $SfHome 'secrets.env'))) {
    if ($PSCmdlet.ShouldProcess($dbPath, 'pre-upgrade backup (siloforge backup create --kind db)')) {
        $r = Invoke-Sf $installedExe @('backup', 'create', '--kind', 'db', '--note', "pre-upgrade to $Version", '--home', $SfHome)
        $r[1] | Where-Object { $_ -match '^backup ' } | ForEach-Object { Write-Step $_ }
        if ($r[0] -ne 0) { throw ("pre-upgrade backup failed: " + (($r[1] | Select-Object -Last 3) -join ' | ')) }
    }
}

# ------------------------------------------------------------------------------------------------ 2. app files
$wasRunning = (Get-SfProcesses $Root).Count -gt 0
$currentTarget = $null
if (Test-Path $Current) {
    $ci = Get-Item $Current -Force
    if ($ci.Attributes -band [IO.FileAttributes]::ReparsePoint) { $currentTarget = [string]($ci.Target | Select-Object -First 1) }
}
$needSwitch = $CopyCurrent -or -not $currentTarget -or ([System.IO.Path]::GetFullPath($currentTarget).TrimEnd('\') -ne $VerDir)

if ((-not $sameExe -or $needSwitch) -and $wasRunning) {
    # Never kill a running job (an inject could stop between a WP write and its bookkeeping).
    try {
        $h0 = Invoke-RestMethod -Uri ("http://$firstBind/health") -TimeoutSec 5 -UseBasicParsing
        if ($h0.jobs -and [int]$h0.jobs.running -gt 0 -and -not $Force) {
            throw ("{0} job(s) running. Wait for them (UI > Jobs, or GET /api/v2/jobs?status=running) or pass -Force." -f $h0.jobs.running)
        }
    } catch [System.Net.WebException] { }
    if ($PSCmdlet.ShouldProcess('siloforge server', 'stop for upgrade')) { Stop-SfServer $Root $TaskName }
}
if (-not $sameExe) {
    if ($PSCmdlet.ShouldProcess($VerDir, "copy release $Version")) {
        New-Item -ItemType Directory -Force -Path $VerDir | Out-Null
        Get-ChildItem $Source -Force | Where-Object { $_.Name -ne 'data' } | ForEach-Object {
            Copy-Item $_.FullName (Join-Path $VerDir $_.Name) -Recurse -Force
        }
        Write-Step "copied release -> $VerDir"
    }
} else { Write-Step "app\$Version already present (same exe)" }

if ($needSwitch -and $PSCmdlet.ShouldProcess($Current, "point at app\$Version")) {
    if (Test-Path $Current) {
        $ci = Get-Item $Current -Force
        if ($ci.Attributes -band [IO.FileAttributes]::ReparsePoint) { cmd /c rmdir "$Current" | Out-Null }   # removes the link only
        else { Remove-Item $Current -Recurse -Force }                                                     # an old plain copy
    }
    $made = $false
    if (-not $CopyCurrent) {
        cmd /c mklink /J "$Current" "$VerDir" | Out-Null
        $made = ($LASTEXITCODE -eq 0)
    }
    if (-not $made) { Copy-Item $VerDir $Current -Recurse -Force; Write-Step "current = copy of app\$Version" }
    else { Write-Step "current -> app\$Version (junction)" }
}
$exe = Join-Path $Current 'siloforge.exe'

# ------------------------------------------------------------------------------------------------ 3. data home
if ($PSCmdlet.ShouldProcess($SfHome, 'create data home, runtime and model')) {
    foreach ($d in @($SfHome, (Join-Path $SfHome 'runtime'), (Join-Path $SfHome 'models\all-MiniLM-L6-v2'))) {
        New-Item -ItemType Directory -Force -Path $d | Out-Null
    }
    # ONNX Runtime: the exe dir is searched too, but a copy in <home>\runtime keeps the home portable.
    foreach ($dll in @('onnxruntime.dll', 'DirectML.dll', 'onnxruntime_providers_shared.dll')) {
        $dst = Join-Path $SfHome "runtime\$dll"
        if (Test-Path $dst) { continue }
        $src = @((Join-Path $Source $dll), (Join-Path $RuntimeDir $dll)) | Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($src) { Copy-Item $src $dst; Write-Step "runtime: $dll" }
    }
    if (-not (Test-Path (Join-Path $SfHome 'runtime\onnxruntime.dll'))) {
        Write-Warning "no onnxruntime.dll in $SfHome\runtime (run: siloforge fetch-runtime --home `"$SfHome`")"
    }
    $mdst = Join-Path $SfHome 'models\all-MiniLM-L6-v2'
    $pairs = @(@('config.json', 'config.json'), @('tokenizer.json', 'tokenizer.json'), @('onnx\model.onnx', 'model.onnx'), @('model.onnx', 'model.onnx'))
    foreach ($p in $pairs) {
        $src = Join-Path $ModelDir $p[0]; $dst = Join-Path $mdst $p[1]
        if ((Test-Path $src) -and -not (Test-Path $dst)) { Copy-Item $src $dst; Write-Step ("model: " + $p[1]) }
    }
    if (-not (Test-Path (Join-Path $mdst 'model.onnx'))) {
        if ($FetchModel) {
            $r = Invoke-Sf $exe @('fetch-model', '--home', $SfHome)
            if ($r[0] -ne 0) { Write-Warning 'fetch-model failed; embeddings will use the remote rung only' }
        } else {
            Write-Warning "model not found in $ModelDir; run: siloforge fetch-model --home `"$SfHome`" (or re-run with -FetchModel)"
        }
    }
}

# ------------------------------------------------------------------------------------------------ 4. init
$secrets = Join-Path $SfHome 'secrets.env'
$freshToml = -not (Test-Path (Join-Path $SfHome 'siloforge.toml'))
if (-not (Test-Path $secrets)) {
    if ($PSCmdlet.ShouldProcess($SfHome, 'siloforge init')) {
        $initArgs = @('init', '--home', $SfHome)
        if ($FromV1Env) {
            if (-not (Test-Path $FromV1Env)) { throw "-FromV1Env $FromV1Env not found" }
            $initArgs += @('--from-v1-env', $FromV1Env)
        }
        $r = Invoke-Sf $exe $initArgs
        $r[1] | Where-Object { $_ -match '^(wrote|imported|next:|backups are)' } | ForEach-Object { Write-Step $_ }
        if ($r[0] -ne 0) { throw ("siloforge init failed: " + (($r[1] | Select-Object -Last 3) -join ' | ')) }
        # secrets.env: only the current user (and SYSTEM/Administrators) may read it.
        try {
            $me = [Security.Principal.WindowsIdentity]::GetCurrent().Name
            & icacls.exe $secrets '/inheritance:r' '/grant:r' "${me}:(F)" '*S-1-5-18:(F)' '*S-1-5-32-544:(F)' 2>&1 | Out-Null
        } catch { Write-Warning ('could not tighten ACL on secrets.env: ' + $_.Exception.Message) }
    }
} else { Write-Step 'secrets.env present (init skipped)' }

if ($freshToml -and $BackupDir -and (Test-Path (Join-Path $SfHome 'siloforge.toml')) -and $PSCmdlet.ShouldProcess('siloforge.toml', "backup.dir = $BackupDir")) {
    $tp = Join-Path $SfHome 'siloforge.toml'
    $t = [System.IO.File]::ReadAllText($tp)
    $t2 = [regex]::Replace($t, '(?m)^dir = ""(\s*# default <home>/backups)', ('dir = "' + $BackupDir.Replace('\', '\\') + '"$1'), 1)
    if ($t2 -ne $t) { Write-Utf8NoBom $tp $t2; New-Item -ItemType Directory -Force -Path $BackupDir | Out-Null; Write-Step "backups -> $BackupDir" }
}

# ------------------------------------------------------------------------------------------------ 5. keeper + task
$owner = Get-PortOwner $port
$ours = (Get-SfProcesses $Root).Count -gt 0
$blocked = ($owner -and -not $ours -and $owner -ne 'siloforge')
if ($PSCmdlet.ShouldProcess($TaskName, 'write keeper and register scheduled task')) {
    New-Item -ItemType Directory -Force -Path $KeeperDir | Out-Null
    $envLines = @('Set env = sh.Environment("PROCESS")', ('env("SILOFORGE_HOME") = "' + $SfHome + '"'))
    if ($Binds) { $envLines += ('env("SILOFORGE_BINDS") = "' + ($bindList -join ',') + '"') }
    $vbs = @(
        "' SiloForge keeper (generated by scripts\install.ps1 for task $TaskName). Hidden; singleton by port $port.",
        'Set sh = CreateObject("WScript.Shell")',
        ('If sh.Run("cmd /c netstat -ano -p tcp | findstr /r /c:""TCP  *[0-9.]*:' + $port + '  *.*LISTENING""", 0, True) = 0 Then WScript.Quit(0)')
    ) + $envLines + @(
        ('sh.CurrentDirectory = "' + $SfHome + '"'),
        ('code = sh.Run("""' + $exe + '"" serve", 0, True)'),
        'WScript.Quit(code)'
    )
    [IO.File]::WriteAllText($Keeper, (($vbs -join "`r`n") + "`r`n"), (New-Object Text.ASCIIEncoding))

    $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    $action = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument ('"' + $Keeper + '"') -WorkingDirectory $SfHome
    $trLogon = New-ScheduledTaskTrigger -AtLogOn -User $user
    $trRepeat = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes $IntervalMinutes)
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable `
        -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero)
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trLogon, $trRepeat -Settings $settings `
        -Principal $principal -Description "SiloForge v2 keepalive ($firstBind, home $SfHome). Installed by scripts\install.ps1." -Force | Out-Null
    if ($blocked) {
        Disable-ScheduledTask -TaskName $TaskName | Out-Null
        Write-Step "port $port is held by '$owner' (v1 still running?). Task $TaskName registered DISABLED; cutover-from-v1.ps1 enables it." 'Yellow'
    } else {
        Enable-ScheduledTask -TaskName $TaskName | Out-Null
        Write-Step "scheduled task $TaskName registered (user $user, logon + every $IntervalMinutes min)"
    }
}

# ------------------------------------------------------------------------------------------------ manifest
if ($PSCmdlet.ShouldProcess((Join-Path $Root 'MANIFEST.json'), 'write')) {
    $manifest = [ordered]@{
        name             = 'siloforge'
        family           = 'SiloForge-Suite'
        version          = $Version
        installed_at     = (Get-Date -Format 's')
        ports            = @($(if ($bindList.Count) { $bindList } else { @('127.0.0.1:8600', '127.0.0.1:8601') }))
        run_cmd          = "$exe serve   (SILOFORGE_HOME=$SfHome)"
        home             = $SfHome
        autostart        = "Scheduled Task $TaskName (logon + every $IntervalMinutes min, hidden keeper $Keeper)"
        live_dev_path    = 'D:\Code\SiloForge-Suite\siloforge-rs'
        contains_secrets = $true
        secrets_note     = 'data\secrets.env (JWT/encryption keys, provider keys) - never copy it to cloud storage unencrypted'
        installed        = @(Get-ChildItem $AppDir -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    }
    Write-Utf8NoBom (Join-Path $Root 'MANIFEST.json') (($manifest | ConvertTo-Json -Depth 4) + "`n")
}
Add-Content -Path (Join-Path $LogDir 'install-history.log') -Encoding UTF8 -Value ("{0}`t{1}`troot={2}`thome={3}`ttask={4}`tby={5}" -f (Get-Date -Format 's'), $Version, $Root, $SfHome, $TaskName, $env:USERNAME) -WhatIf:$false

# ------------------------------------------------------------------------------------------------ 6. start + health
if ($NoStart -or $blocked -or $WhatIfPreference) {
    if ($blocked) { Write-Step 'not started (port busy).' 'Yellow' } else { Write-Step 'not started (-NoStart / -WhatIf).' }
} else {
    Start-ScheduledTask -TaskName $TaskName
    $h = Test-Health $firstBind 90
    if ($h) {
        Write-Step ("healthy: http://$firstBind/health  version " + $h.version + "  db " + $h.services.db) 'Green'
        if ($h.version -ne $Version) { Write-Warning ("server reports " + $h.version + ", expected $Version (an older process still running?)") }
    } else {
        Write-Warning "no healthy /health on $firstBind within 90 s. Check $SfHome\logs and run: `"$exe`" doctor --home `"$SfHome`""
        exit 2
    }
}

Write-Step 'done' 'Green'
Write-Host ''
Write-Host 'Next steps:'
Write-Host ("  doctor:        & `"$exe`" doctor --home `"$SfHome`"")
Write-Host ("  admin user:    (only if no v1 import / ADMIN_PASSWORD) `$env:SILOFORGE_NEW_PASSWORD='...'; & `"$exe`" user set-password admin@siloforge.local --create --home `"$SfHome`"")
Write-Host ("  API token:     & `"$exe`" token create --name hermes --scopes read,write --home `"$SfHome`"   (printed once)")
Write-Host ("  UI:            http://$firstBind/")
exit 0
