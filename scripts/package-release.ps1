<#
.SYNOPSIS
  Package a release (kept for `cargo xtask release`, which prints "package with: scripts/package-release.ps1 <version>").

.DESCRIPTION
  Thin wrapper around build-release.ps1. The optional Version argument must match the VERSION file; every
  other argument is passed through (for example -SkipUi -SkipBuild -RuntimeDir <dir>).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1 2.0.0 -SkipUi
#>
param(
    [Parameter(Position = 0)][string]$Version = '',
    [Parameter(ValueFromRemainingArguments = $true)]$Rest
)

. (Join-Path $PSScriptRoot '_env.ps1')
if ($Version -and ($Version -ne (Get-SfVersion))) {
    Write-Host ("package-release: version $Version != VERSION " + (Get-SfVersion)) -ForegroundColor Red
    exit 1
}
$splat = @{}
$i = 0
$list = @($Rest)
while ($i -lt $list.Count) {
    $a = [string]$list[$i]
    if ($a -match '^-(\w+)$') {
        $name = $Matches[1]
        if (($i + 1) -lt $list.Count -and ([string]$list[$i + 1]) -notmatch '^-\w+$') {
            $splat[$name] = $list[$i + 1]; $i += 2; continue
        }
        $splat[$name] = $true
    }
    $i++
}
& (Join-Path $PSScriptRoot 'build-release.ps1') @splat
exit $LASTEXITCODE
