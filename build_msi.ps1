<#
.SYNOPSIS
  Build the release exe and package it as an MSI with WiX (the `wix` dotnet tool, v4 or newer).
.DESCRIPTION
  Version comes from Cargo.toml. The MSI goes to target\KXS-Watcher\KXS-Watcher-<version>-x64.msi (in this repo)
  and its path is the script's output, so other scripts can use it: $msi = .\build_msi.ps1
.EXAMPLE
  .\build_msi.ps1              # cargo build --release, then package
.EXAMPLE
  .\build_msi.ps1 -SkipBuild   # package the release exe that is already built
.EXAMPLE
  .\build_msi.ps1 -Version 0.2.0   # MSI version other than Cargo.toml's (publish_github.ps1 passes the tag's)
#>
param([switch]$SkipBuild, [string]$Version)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    throw "WiX not found. Install it with: dotnet tool install --global wix"
}
# The wizard (UI) and the "launch after install" action (Util) come from WiX extensions of the same version.
$wixVersion = (wix --version) -replace '\+.*$', ''
$extensions = 'WixToolset.UI.wixext', 'WixToolset.Util.wixext'
$have = wix extension list -g
foreach ($e in $extensions) {
    if (-not ($have -match "^$([regex]::Escape($e)) ")) {
        wix extension add -g "$e/$wixVersion" | Out-Host
        if ($LASTEXITCODE) { throw "cannot add WiX extension $e" }
    }
}

$meta = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
if ($LASTEXITCODE) { throw "cargo metadata failed" }
if (-not $Version) { $Version = ($meta.packages | Where-Object name -eq 'kxs-watcher').version }
$Version = $Version -replace '[-+].*$', ''   # MSI versions are numbers only: 0.1.0-beta -> 0.1.0
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "MSI version must be x.y.z (got '$Version')" }

if (-not $SkipBuild) {
    cargo build --release | Out-Host
    if ($LASTEXITCODE) { throw "cargo build failed" }
}
$exe = Join-Path $meta.target_directory 'release\kxs-watcher.exe'
if (-not (Test-Path $exe)) { throw "Missing $exe - run without -SkipBuild" }

$outDir = Join-Path $PSScriptRoot 'target\KXS-Watcher'
New-Item -ItemType Directory -Force $outDir | Out-Null
$msi = Join-Path $outDir "KXS-Watcher-$Version-x64.msi"

$ext = $extensions | ForEach-Object { '-ext', $_ }
wix build (Join-Path $PSScriptRoot 'wix\main.wxs') -arch x64 @ext -d "Version=$Version" -d "ExePath=$exe" -pdbtype none -o $msi | Out-Host
if ($LASTEXITCODE) { throw "wix build failed" }

$mb = [math]::Round((Get-Item $msi).Length / 1MB, 1)
Write-Host "MSI ($mb MB): $msi" -ForegroundColor Green
$msi
