<#
.SYNOPSIS
  Build the release exe and package it as an MSI with WiX (the `wix` dotnet tool, v4 or newer).
.DESCRIPTION
  The MSI version is -Version, or Cargo.toml's when omitted. The MSI goes to
  target\KXS-Watcher\KXS-Watcher-<version>-x64.msi (in this repo) and its path is the script's output,
  so other scripts can use it: $msi = .\build_msi.ps1
.PARAMETER Version
  x.y.z for the MSI. Tag forms work too (v0.2.0, v.0.2.0, 0.2.0-rc1): the x.y.z inside is used.
  MSI limits: x and y up to 255, z up to 65535. A higher version replaces a lower one when installed.
.EXAMPLE
  .\build_msi.ps1                  # cargo build --release, then package with Cargo.toml's version
.EXAMPLE
  .\build_msi.ps1 -Version 0.2.0   # same, as MSI version 0.2.0
.EXAMPLE
  .\build_msi.ps1 v.0.0.2 -SkipBuild   # package the release exe already built, as 0.0.2
#>
param([Parameter(Position = 0)][string]$Version, [switch]$SkipBuild)
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
$given = if ($Version) { $Version } else { ($meta.packages | Where-Object name -eq 'kxs-watcher').version }
# MSI versions are numbers only: v.0.2.0 / 0.2.0-rc1 -> 0.2.0
if ($given -notmatch '(\d+)\.(\d+)\.(\d+)') { throw "No x.y.z version in '$given'" }
$Version = $Matches[0]
if ([int]$Matches[1] -gt 255 -or [int]$Matches[2] -gt 255 -or [int]$Matches[3] -gt 65535) {
    throw "MSI version $Version is out of range: x and y up to 255, z up to 65535"
}
Write-Host "MSI version: $Version$(if ($given -ne $Version) { " (from '$given')" })"

if (-not $SkipBuild) {
    cargo build --release | Out-Host
    if ($LASTEXITCODE) { throw "cargo build failed" }
}
$exe = Join-Path $meta.target_directory 'release\kxs-watcher.exe'
if (-not (Test-Path $exe)) { throw "Missing $exe - run without -SkipBuild" }
# The icon build.rs drew for this build (in its OUT_DIR).
$ico = Get-ChildItem (Join-Path $meta.target_directory 'release\build') -Recurse -Filter kxs.ico -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $ico) { throw "Missing kxs.ico - run without -SkipBuild" }

$outDir = Join-Path $PSScriptRoot 'target\KXS-Watcher'
New-Item -ItemType Directory -Force $outDir | Out-Null
$msi = Join-Path $outDir "KXS-Watcher-$Version-x64.msi"

$ext = $extensions | ForEach-Object { '-ext', $_ }
wix build (Join-Path $PSScriptRoot 'wix\main.wxs') -arch x64 @ext -d "Version=$Version" -d "ExePath=$exe" -d "IconPath=$($ico.FullName)" -pdbtype none -o $msi | Out-Host
if ($LASTEXITCODE) { throw "wix build failed" }

$mb = [math]::Round((Get-Item $msi).Length / 1MB, 1)
Write-Host "MSI ($mb MB): $msi" -ForegroundColor Green
$msi
