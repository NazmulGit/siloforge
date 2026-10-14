<#
.SYNOPSIS
  Build a SiloForge v2 release: UI, release binary, staged folder, zip and SHA-256.

.DESCRIPTION
  1. Reads the version from VERSION (and checks it against `siloforge version`).
  2. ui\: `npm ci` + `npm run build` (npm cache D:\Programs\npm-cache) -> ui\dist, which is embedded in the exe.
  3. `cargo +stable-x86_64-pc-windows-gnu build --release --locked -p siloforge`
     (CARGO_TARGET_DIR=D:\tmp\cargo-target\siloforge unless already set).
  4. Stages dist\<version>\:
       siloforge.exe, onnxruntime.dll, DirectML.dll, onnxruntime_providers_shared.dll (from -RuntimeDir),
       THIRD_PARTY_NOTICES.txt + licenses\ (ONNX Runtime MIT, DirectML license terms),
       README.md, CHANGELOG.md, LICENSE, siloforge.example.toml,
       scripts\{install,cutover-from-v1,rollback-to-v1}.ps1 + scripts\_env.ps1, SHA256SUMS.txt
  5. Zips it to dist\siloforge-<version>-windows-x64.zip (top folder siloforge-<version>\) and writes
     dist\siloforge-<version>-windows-x64.zip.sha256.

  The model (all-MiniLM-L6-v2, ~90 MB) is NOT in the zip; install.ps1 copies it from D:\Download or runs
  `siloforge fetch-model`. PowerShell 5.1 compatible. Builds only: nothing is installed or started.

.PARAMETER RuntimeDir
  Folder with onnxruntime.dll + DirectML.dll (ONNX Runtime DirectML build). Default D:\Download\onnxruntime\dml-1.24.4.
.PARAMETER LicenseDir
  Folder with the unpacked NuGet packages (for their LICENSE / ThirdPartyNotices files). Default D:\Download\onnxruntime\x.
.PARAMETER SkipUi
  Reuse the existing ui\dist (no npm).
.PARAMETER SkipBuild
  Reuse the existing release exe in $env:CARGO_TARGET_DIR\release.
.PARAMETER NoRuntime
  Do not bundle the ONNX Runtime DLLs (smaller zip; run `siloforge fetch-runtime` after install).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
  powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 -SkipUi -SkipBuild
#>
[CmdletBinding()]
param(
    [string]$RuntimeDir = 'D:\Download\onnxruntime\dml-1.24.4',
    [string]$LicenseDir = 'D:\Download\onnxruntime\x',
    [switch]$SkipUi,
    [switch]$SkipBuild,
    [switch]$NoRuntime
)

. (Join-Path $PSScriptRoot '_env.ps1')
$root = $script:SfRoot
Set-Location $root
$null = Assert-DPath $root 'repo root'
$null = Assert-DPath $env:CARGO_TARGET_DIR 'CARGO_TARGET_DIR'

function Fail([string]$msg) { Write-Host "BUILD FAILED: $msg" -ForegroundColor Red; exit 1 }

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$version = Get-SfVersion
Write-Step "building SiloForge $version"

# Cargo.toml [workspace.package] version must match VERSION.
$cargoToml = [System.IO.File]::ReadAllText((Join-Path $root 'Cargo.toml'))
$m = [regex]::Match($cargoToml, '(?s)\[workspace\.package\].*?\bversion\s*=\s*"([^"]+)"')
if (-not $m.Success) { Fail 'no version in [workspace.package] of Cargo.toml' }
if ($m.Groups[1].Value -ne $version) { Fail ("Cargo.toml version " + $m.Groups[1].Value + " != VERSION $version") }
$cl = Join-Path $root 'CHANGELOG.md'
if ((Test-Path $cl) -and ([System.IO.File]::ReadAllText($cl) -notmatch [regex]::Escape("## [$version]"))) {
    Write-Warning "CHANGELOG.md has no '## [$version]' section"
}

# --- 1. UI -------------------------------------------------------------------------------------
if (-not $SkipUi) {
    $npm = (Get-Command npm.cmd -ErrorAction SilentlyContinue)
    if (-not $npm) { Fail 'npm not found (install Node.js, or pass -SkipUi to reuse ui\dist)' }
    Push-Location (Join-Path $root 'ui')
    try {
        Write-Step "ui: npm ci (cache $env:npm_config_cache)"
        if ((Invoke-Native $npm.Source @('ci', '--no-audit', '--no-fund')) -ne 0) { Fail 'npm ci failed' }
        Write-Step 'ui: npm run build'
        if ((Invoke-Native $npm.Source @('run', 'build')) -ne 0) { Fail 'npm run build failed' }
    } finally { Pop-Location }
}
if (-not (Test-Path (Join-Path $root 'ui\dist\index.html'))) { Fail 'ui\dist\index.html missing (the exe would ship without the UI)' }

# --- 2. binary ---------------------------------------------------------------------------------
$exe = Join-Path $env:CARGO_TARGET_DIR 'release\siloforge.exe'
if (-not $SkipBuild) {
    Write-Step 'cargo build --release --locked -p siloforge'
    if ((Invoke-Cargo @('build', '--release', '--locked', '-p', 'siloforge')) -ne 0) { Fail 'cargo build failed' }
}
if (-not (Test-Path $exe)) { Fail "release exe not found at $exe" }
$reported = (& $exe version 2>$null | Out-String).Trim()
if ($reported -ne "siloforge $version") { Fail "binary reports '$reported', expected 'siloforge $version'" }
Write-Step "binary ok: $reported"

# --- 3. stage ----------------------------------------------------------------------------------
$distRoot = Join-Path $root 'dist'
$stage = Join-Path $distRoot $version
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'scripts') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'licenses') | Out-Null

Copy-Item $exe $stage
foreach ($f in @('README.md', 'CHANGELOG.md', 'LICENSE', 'siloforge.example.toml')) {
    $src = Join-Path $root $f
    if (Test-Path $src) { Copy-Item $src $stage } else { Write-Warning "$f missing; not staged" }
}
foreach ($s in @('_env.ps1', 'install.ps1', 'cutover-from-v1.ps1', 'rollback-to-v1.ps1')) {
    $src = Join-Path $PSScriptRoot $s
    if (Test-Path $src) { Copy-Item $src (Join-Path $stage 'scripts') }
}

$runtimeNote = 'not bundled (run `siloforge fetch-runtime` after install)'
if (-not $NoRuntime) {
    foreach ($dll in @('onnxruntime.dll', 'DirectML.dll')) {
        if (-not (Test-Path (Join-Path $RuntimeDir $dll))) {
            Fail "$dll not found in $RuntimeDir (pass -RuntimeDir, run 'siloforge fetch-runtime --home D:\tmp\rt' and point there, or use -NoRuntime)"
        }
    }
    foreach ($dll in @('onnxruntime.dll', 'DirectML.dll', 'onnxruntime_providers_shared.dll')) {
        $src = Join-Path $RuntimeDir $dll
        if (Test-Path $src) { Copy-Item $src $stage }
    }
    $runtimeNote = "bundled from $RuntimeDir"
}

# Third-party notices. License texts come from the NuGet packages when present, links otherwise.
function Find-First([string[]]$candidates) {
    foreach ($c in $candidates) { if ($c -and (Test-Path $c)) { return $c } }
    return $null
}
$ortLicense = Find-First @((Join-Path $LicenseDir 'microsoft.ml.onnxruntime.directml-1.24.4\LICENSE'),
                           (Join-Path $LicenseDir 'microsoft.ml.onnxruntime-1.24.4\LICENSE'))
$ortTpn     = Find-First @((Join-Path $LicenseDir 'microsoft.ml.onnxruntime.directml-1.24.4\ThirdPartyNotices.txt'),
                           (Join-Path $LicenseDir 'microsoft.ml.onnxruntime-1.24.4\ThirdPartyNotices.txt'))
$dmlLicense = Find-First @((Join-Path $LicenseDir 'microsoft.ai.directml-1.15.4\LICENSE.txt'))
$dmlTpn     = Find-First @((Join-Path $LicenseDir 'microsoft.ai.directml-1.15.4\ThirdPartyNotices.txt'))
if ($ortLicense) { Copy-Item $ortLicense (Join-Path $stage 'licenses\onnxruntime-LICENSE.txt') }
if ($ortTpn)     { Copy-Item $ortTpn     (Join-Path $stage 'licenses\onnxruntime-ThirdPartyNotices.txt') }
if ($dmlLicense) { Copy-Item $dmlLicense (Join-Path $stage 'licenses\DirectML-LICENSE.txt') }
if ($dmlTpn)     { Copy-Item $dmlTpn     (Join-Path $stage 'licenses\DirectML-ThirdPartyNotices.txt') }

$nl = "`r`n"
$notice = New-Object System.Text.StringBuilder
[void]$notice.Append("SiloForge $version - third-party notices$nl$nl")
[void]$notice.Append("SiloForge itself is MIT licensed (see LICENSE). This package also contains:$nl$nl")
[void]$notice.Append("1. ONNX Runtime 1.24.4 (onnxruntime.dll, onnxruntime_providers_shared.dll)$nl")
[void]$notice.Append("   Copyright (c) Microsoft Corporation. MIT License.$nl")
[void]$notice.Append("   https://github.com/microsoft/onnxruntime  -  full text: licenses\onnxruntime-LICENSE.txt,$nl")
[void]$notice.Append("   its own third-party notices: licenses\onnxruntime-ThirdPartyNotices.txt$nl$nl")
[void]$notice.Append("2. DirectML 1.15.4 (DirectML.dll)$nl")
[void]$notice.Append("   Copyright (c) Microsoft Corporation. Microsoft Software License Terms for DirectX Machine Learning$nl")
[void]$notice.Append("   (redistributable with applications; Windows only). https://www.nuget.org/packages/Microsoft.AI.DirectML$nl")
[void]$notice.Append("   https://github.com/microsoft/DirectML  -  full text: licenses\DirectML-LICENSE.txt,$nl")
[void]$notice.Append("   notices: licenses\DirectML-ThirdPartyNotices.txt$nl$nl")
[void]$notice.Append("3. Embedded web UI: React, React Router, React Flow (MIT) - see ui/package.json in the source.$nl$nl")
[void]$notice.Append("4. Rust crates compiled into siloforge.exe (axum, tokio, rusqlite + bundled SQLite (public domain),$nl")
[void]$notice.Append("   rmcp, ort, tokenizers, rayon, reqwest + rustls/ring, jsonwebtoken, bcrypt, aes-gcm, scraper, sysinfo,$nl")
[void]$notice.Append("   croner, zip, clap, tracing, serde, ...) are MIT and/or Apache-2.0 (ring: ISC-style + OpenSSL-derived).$nl")
[void]$notice.Append("   The exact list with versions is Cargo.lock in the source repository.$nl$nl")
[void]$notice.Append("5. The embedding model all-MiniLM-L6-v2 (sentence-transformers, Apache-2.0; ONNX export by Xenova)$nl")
[void]$notice.Append("   is not bundled; install.ps1 or 'siloforge fetch-model' places it in <home>\models.$nl")
if (-not $ortLicense -or -not $dmlLicense) {
    [void]$notice.Append("$nl(License files were not found under $LicenseDir at build time; see the URLs above.)$nl")
}
Write-Utf8NoBom -Path (Join-Path $stage 'THIRD_PARTY_NOTICES.txt') -Text $notice.ToString()

# SHA256SUMS over every staged file (relative paths, forward slashes).
$sums = Get-ChildItem $stage -File -Recurse | Where-Object { $_.Name -ne 'SHA256SUMS.txt' } | Sort-Object FullName | ForEach-Object {
    $rel = $_.FullName.Substring($stage.Length + 1).Replace('\', '/')
    (Get-Sha256 $_.FullName) + '  ' + $rel
}
Write-Utf8NoBom -Path (Join-Path $stage 'SHA256SUMS.txt') -Text (($sums -join "`n") + "`n")

# --- 4. zip ------------------------------------------------------------------------------------
$zipName = "siloforge-$version-windows-x64.zip"
$zip = Join-Path $distRoot $zipName
if (Test-Path $zip) { Remove-Item $zip -Force }
$tmpParent = Join-Path $distRoot ("_zip-" + [guid]::NewGuid().ToString('N'))
$named = Join-Path $tmpParent "siloforge-$version"
New-Item -ItemType Directory -Force -Path $tmpParent | Out-Null
try {
    Copy-Item $stage $named -Recurse
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::CreateFromDirectory($named, $zip, [System.IO.Compression.CompressionLevel]::Optimal, $true)
} finally {
    Remove-Item $tmpParent -Recurse -Force -ErrorAction SilentlyContinue
}
$zipHash = Get-Sha256 $zip
Write-Utf8NoBom -Path ($zip + '.sha256') -Text ("$zipHash  $zipName`n")
$zipMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)

$sw.Stop()
Write-Step ("staged   $stage") 'Green'
Write-Step ("zip      $zip ($zipMb MB)") 'Green'
Write-Step ("sha256   $zipHash") 'Green'
Write-Step ("runtime  $runtimeNote") 'Green'
Write-Step ("done in " + [math]::Round($sw.Elapsed.TotalMinutes, 1) + " min") 'Green'
exit 0
