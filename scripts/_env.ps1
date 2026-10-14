# Shared environment for SiloForge v2 scripts (dot-source me). PowerShell 5.1 compatible.
#
# Toolchain: GNU + WinLibs, the same pairing LanPool uses as its fallback, which is the only one this repo
# builds with (the folder already has `rustup override` = stable-x86_64-pc-windows-gnu):
#   cargo +stable-x86_64-pc-windows-gnu ...   with D:\Programs\winlibs\mingw64\bin first on PATH.
# Override with $env:SILOFORGE_TOOLCHAIN = "<rustup toolchain name>" (for example 1.88.0-x86_64-pc-windows-gnu).
#
# Everything lands on D:\ (house rule): CARGO_TARGET_DIR=D:\tmp\cargo-target\siloforge,
# npm cache D:\Programs\npm-cache. D:\Ryzen belongs to another user and is never touched.

$ErrorActionPreference = 'Stop'

$script:SfRoot        = Split-Path -Parent $PSScriptRoot
$script:GnuToolchain  = 'stable-x86_64-pc-windows-gnu'
$script:WinLibsBin    = 'D:\Programs\winlibs\mingw64\bin'
$script:NpmCache      = 'D:\Programs\npm-cache'
$script:DefaultTarget = 'D:\tmp\cargo-target\siloforge'

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path $cargoBin) -and ($env:Path -notlike "*$cargoBin*")) { $env:Path = "$cargoBin;$env:Path" }

if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = $script:DefaultTarget }
$env:npm_config_cache = $script:NpmCache

function Write-Step {
    param([string]$Msg, [string]$Color = 'Cyan')
    Write-Host ('[siloforge] ' + $Msg) -ForegroundColor $Color
}

# Refuses any path that is not on D:\ or that is inside D:\Ryzen (another user's folder).
function Assert-DPath {
    param([Parameter(Mandatory = $true)][string]$Path, [string]$What = 'path')
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($full -notmatch '^[Dd]:\\' -and $env:SILOFORGE_ALLOW_ANY_DRIVE -ne '1') { throw "$What '$full' is not on D:\ (house rule: everything lives on D:\)" }
    if ($full -match '^[Dd]:\\Ryzen(\\|$)') { throw "$What '$full' is inside D:\Ryzen, which belongs to another user" }
    return $full
}

function Get-SfToolchainArgs {
    $choice = $env:SILOFORGE_TOOLCHAIN
    if (-not $choice) { $choice = $script:GnuToolchain }
    if ($choice -like '*-gnu') {
        if (-not (Test-Path (Join-Path $script:WinLibsBin 'gcc.exe'))) {
            throw "The GNU toolchain needs WinLibs MinGW at $script:WinLibsBin (winget install BrechtSanders.WinLibs.POSIX.MSVCRT --scope user --location D:\Programs\winlibs)."
        }
        if ($env:Path -notlike "*$script:WinLibsBin*") { $env:Path = "$script:WinLibsBin;$env:Path" }
    }
    return @("+$choice")
}

# Pass args as ONE array: Invoke-Cargo @('build', '--release', '--locked', '-p', 'siloforge')
# (PowerShell strips a bare `--` from function-call arguments.) Returns the exit code.
function Invoke-Cargo {
    param([Parameter(Mandatory = $true, Position = 0)][string[]]$CargoArgs)
    $tc = Get-SfToolchainArgs
    $all = @($tc) + @($CargoArgs)
    Write-Host ('> cargo ' + ($all -join ' ') + "   (CARGO_TARGET_DIR=$env:CARGO_TARGET_DIR)") -ForegroundColor DarkGray
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'   # cargo writes progress to stderr
    & cargo @all 2>&1 | Out-Host
    $rc = $LASTEXITCODE
    $ErrorActionPreference = $prev
    return $rc
}

# Runs a native command with stderr tolerated; returns the exit code. Output goes to the host.
function Invoke-Native {
    param([Parameter(Mandatory = $true)][string]$Exe, [string[]]$Arguments = @())
    Write-Host ('> ' + $Exe + ' ' + ($Arguments -join ' ')) -ForegroundColor DarkGray
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $Exe @Arguments 2>&1 | Out-Host
    $rc = $LASTEXITCODE
    $ErrorActionPreference = $prev
    return $rc
}

function Get-SfVersion {
    $v = ([System.IO.File]::ReadAllText((Join-Path $script:SfRoot 'VERSION'))).Trim()
    if ($v -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') { throw "VERSION '$v' is not SemVer" }
    return $v
}

function Write-Utf8NoBom {
    param([string]$Path, [string]$Text)
    $enc = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $enc)
}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLower()
}
