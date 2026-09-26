<#
.SYNOPSIS
  Publish to GitHub under a tag you choose: push the code, tag it, create the release, upload the MSI.
.DESCRIPTION
  1. Pushes the current branch to `origin`.
  2. Creates the annotated tag (on the current commit) and pushes it.
  3. Builds the MSI with build_msi.ps1. Its version is the x.y.z found in the tag (v1.2.3, 1.2.3-rc1,
     release-1.2.3); tags without one use the version in Cargo.toml.
  4. Creates the GitHub release for the tag (or reuses it) and uploads the MSI, replacing an asset
     with the same name.

  Token, first found: $env:GITHUB_TOKEN, $env:GH_TOKEN, `gh auth token`, or the github.com
  credential Git already uses (Git Credential Manager). It needs Contents: read and write.

  Refuses to run with uncommitted changes (the MSI would not match the tagged commit) or when the
  tag already exists on another commit.
.EXAMPLE
  .\publish_github.ps1 -Tag v0.2.0
.EXAMPLE
  .\publish_github.ps1 -Tag v0.2.0-rc1 -Prerelease -Notes "Search in configs and secrets"
.EXAMPLE
  .\publish_github.ps1 -Tag v0.2.0 -DryRun   # build and show what would be published; nothing is pushed
#>
param(
    [Parameter(Mandatory)][string]$Tag,
    [switch]$SkipBuild,     # package the release exe already built
    [switch]$Draft,
    [switch]$Prerelease,
    [string]$Notes,         # release body; GitHub generates notes from commits when empty
    [switch]$AllowDirty,    # publish even with uncommitted changes
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# Not named Git: PowerShell names are case-insensitive, so it would shadow git.exe and recurse.
function Invoke-Git { $out = git @args; if ($LASTEXITCODE) { throw "git $args failed" }; $out }

# --- checks before anything is built or pushed
git check-ref-format "refs/tags/$Tag"
if ($LASTEXITCODE) { throw "'$Tag' is not a valid tag name" }
$url = Invoke-Git remote get-url origin
if ($url -notmatch 'github\.com[:/](?<owner>[^/]+)/(?<repo>[^/]+?)(\.git)?/?$') { throw "origin is not a GitHub repository: $url" }
$slug = "$($Matches.owner)/$($Matches.repo)"
$dirty = Invoke-Git status --porcelain
if ($dirty -and -not $AllowDirty) {
    throw "Uncommitted changes: commit them first (or pass -AllowDirty).`n$($dirty -join "`n")"
}
$head = Invoke-Git rev-parse HEAD
$branch = Invoke-Git rev-parse --abbrev-ref HEAD   # "HEAD" when detached
$tagged = git rev-parse -q --verify "refs/tags/$Tag^{commit}"
if ($tagged -and $tagged -ne $head) {
    throw "Tag $Tag already exists on another commit ($tagged). Pick another tag."
}

# --- MSI
$version = if ($Tag -match '\d+\.\d+\.\d+') { $Matches[0] } else { '' }   # empty: Cargo.toml's
$msi = & (Join-Path $PSScriptRoot 'build_msi.ps1') -SkipBuild:$SkipBuild -Version $version | Select-Object -Last 1
$asset = Split-Path $msi -Leaf

Write-Host "Repository : $slug"
Write-Host "Branch     : $(if ($branch -eq 'HEAD') { '(detached, not pushed)' } else { $branch })"
Write-Host "Release    : $Tag$(if ($Draft) { ' (draft)' })$(if ($Prerelease) { ' (pre-release)' })"
Write-Host "Commit     : $head"
Write-Host "Asset      : $msi"
if ($DryRun) {
    Write-Host "Dry run: nothing tagged, pushed or uploaded." -ForegroundColor Yellow
    return
}

# --- token (never printed)
function Get-Token {
    foreach ($v in 'GITHUB_TOKEN', 'GH_TOKEN') {
        $t = [Environment]::GetEnvironmentVariable($v)
        if ($t) { return $t }
    }
    if (Get-Command gh -ErrorAction SilentlyContinue) {
        $t = gh auth token 2>$null
        if ($LASTEXITCODE -eq 0 -and $t) { return "$t".Trim() }
    }
    # Same account Git pushes with (Git Credential Manager stores a GitHub OAuth token).
    $cred = "protocol=https`nhost=github.com`n`n" | git credential fill 2>$null
    $pw = $cred | Where-Object { $_ -like 'password=*' } | Select-Object -First 1
    if ($pw) { return $pw.Substring(9) }
    throw "No GitHub token: set GITHUB_TOKEN (fine-grained, Contents: read and write) or run 'gh auth login'."
}
$headers = @{
    Authorization          = "Bearer $(Get-Token)"
    Accept                 = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
    'User-Agent'           = 'kxs-watcher-publish'
}
$api = "https://api.github.com/repos/$slug"

# --- code and tag
if ($branch -ne 'HEAD') {
    Invoke-Git push origin "refs/heads/${branch}:refs/heads/$branch" | Out-Host
}
if (-not $tagged) {
    Invoke-Git tag -a $Tag -m "KXS Watcher $Tag" | Out-Null
}
Invoke-Git push origin "refs/tags/$Tag" | Out-Host

# --- release (the list includes drafts, which the by-tag endpoint doesn't)
$releases = Invoke-RestMethod "$api/releases?per_page=100" -Headers $headers  # assigned first: PS 5.1 doesn't unroll it in a pipe
$release = $releases | Where-Object tag_name -eq $Tag | Select-Object -First 1
if (-not $release) {
    $body = @{ tag_name = $Tag; name = "KXS Watcher $Tag"; draft = [bool]$Draft; prerelease = [bool]$Prerelease }
    if ($Notes) { $body.body = $Notes } else { $body.generate_release_notes = $true }
    $release = Invoke-RestMethod "$api/releases" -Method Post -Headers $headers -ContentType 'application/json' -Body ($body | ConvertTo-Json)
    Write-Host "Created release $Tag"
} else {
    Write-Host "Updating existing release $Tag"
}

# --- asset (replace one with the same name)
foreach ($old in @($release.assets | Where-Object name -eq $asset)) {
    Invoke-RestMethod "$api/releases/assets/$($old.id)" -Method Delete -Headers $headers | Out-Null
    Write-Host "Replaced previous $asset"
}
$upload = ($release.upload_url -replace '\{.*\}$', '') + "?name=$([uri]::EscapeDataString($asset))"
Invoke-RestMethod $upload -Method Post -Headers $headers -ContentType 'application/octet-stream' -InFile $msi | Out-Null

Write-Host "Published: $($release.html_url)" -ForegroundColor Green
