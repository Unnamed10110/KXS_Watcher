<#
.SYNOPSIS
  Publish to GitHub as release v<version>, the version in Cargo.toml.
.DESCRIPTION
  Same as publish_github.ps1 -Tag v<version>: push the code, tag, create the release, upload the MSI.
  Use publish_github.ps1 directly for a tag of your choice.
.EXAMPLE
  .\publish_release.ps1
.EXAMPLE
  .\publish_release.ps1 -Draft -Notes "First preview"
.EXAMPLE
  .\publish_release.ps1 -DryRun
#>
param(
    [switch]$SkipBuild,
    [switch]$Draft,
    [switch]$Prerelease,
    [string]$Notes,
    [switch]$AllowDirty,
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$version = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).packages | Where-Object name -eq 'kxs-watcher' | ForEach-Object version
& (Join-Path $PSScriptRoot 'publish_github.ps1') -Tag "v$version" @PSBoundParameters
