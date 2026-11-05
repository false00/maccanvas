#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ReferenceTag = 'latest',
    [string]$OutputDirectory = (Join-Path ([IO.Path]::GetTempPath()) 'maccanvas-audio-component')
)
$ErrorActionPreference = 'Stop'

function Assert-MacCanvasAudioArtifact([string]$Path, [long]$Size, [string]$Sha256) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf) -or
        (Get-Item -LiteralPath $Path).Length -ne $Size -or
        (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ine $Sha256) {
        throw 'The reusable audio component does not match its published size and SHA-256.'
    }
}

function Get-MacCanvasAudioVersion([string]$Manifest) {
    $match = [regex]::Match($Manifest, '(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"')
    if (-not $match.Success) { throw 'The audio component must have a stable package version.' }
    return $match.Groups[1].Value
}

function Resolve-MacCanvasAudioComponent([string]$Root, [string]$ReferenceTag, [string]$OutputDirectory) {
    $repository = 'false00/maccanvas'
    $assetName = 'MacCanvas-audio-windows-x64.exe'
    $tagArgument = @()
    if ($ReferenceTag -cne 'latest') { $tagArgument = @($ReferenceTag) }
    $text = gh release view @tagArgument --repo $repository --json tagName,isDraft,isPrerelease,assets,url
    if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the official reference release for audio reuse.' }
    $release = $text | ConvertFrom-Json
    if ($release.tagName -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+$' -or
        $release.isDraft -or $release.isPrerelease -or
        $release.url -cne "https://github.com/$repository/releases/tag/$($release.tagName)") {
        throw 'Audio components can only be reused from an official stable published release.'
    }
    $assets = @($release.assets | Where-Object { $_.name -ceq $assetName })
    if ($assets.Count -eq 0) {
        Write-Host 'The reference release has no audio component; building it from source.'
        return [pscustomobject]@{ Reused = $false; Path = $null; Sha256 = $null }
    }
    if ($assets.Count -ne 1 -or $assets[0].size -le 0 -or $assets[0].size -gt (8MB) -or
        $assets[0].digest -cnotmatch '^sha256:[a-f0-9]{64}$') {
        throw 'The reference audio asset has invalid size or checksum metadata.'
    }
    $tag = $release.tagName
    git -C $Root show-ref --quiet --verify "refs/tags/$tag"
    if ($LASTEXITCODE -eq 1) {
        git -C $Root fetch origin "refs/tags/${tag}:refs/tags/$tag"
        if ($LASTEXITCODE -ne 0) { throw 'Could not fetch the audio reference source tag.' }
    } elseif ($LASTEXITCODE -ne 0) { throw 'Could not inspect the audio reference source tag.' }
    $manifestPath = Join-Path $Root 'crates\audio-codec-tool\Cargo.toml'
    $currentVersion = Get-MacCanvasAudioVersion (Get-Content -LiteralPath $manifestPath -Raw)
    $referenceManifest = git -C $Root show "${tag}:crates/audio-codec-tool/Cargo.toml"
    if ($LASTEXITCODE -ne 0) { throw 'Could not read the reference audio component version.' }
    $referenceVersion = Get-MacCanvasAudioVersion ($referenceManifest -join "`n")
    # Rust metadata glue is not converter code; native build-policy changes require a component version bump.
    $inputs = @(
        'crates\audio-codec-tool\native',
        'crates\audio-codec-tool\vendor',
        'crates\audio-codec-tool\Cargo.toml',
        'crates\audio-codec-tool\LICENSE',
        'crates\audio-codec-tool\DISTRIBUTION-NOTICES.txt'
    )
    $changed = @(git -C $Root diff --name-only $tag -- @inputs)
    if ($LASTEXITCODE -ne 0) { throw 'Could not compare audio component inputs.' }
    $untracked = @(git -C $Root ls-files --others --exclude-standard -- @inputs)
    if ($LASTEXITCODE -ne 0) { throw 'Could not inspect untracked audio component inputs.' }
    if ($changed.Count -gt 0 -or $untracked.Count -gt 0 -or $currentVersion -cne $referenceVersion) {
        if ($currentVersion -ceq $referenceVersion) {
            throw 'Audio inputs changed without an audio-codec-tool package-version bump; refusing to reuse stale converter bytes.'
        }
        Write-Host "Audio component changed ($referenceVersion -> $currentVersion); building it from source."
        return [pscustomobject]@{ Reused = $false; Path = $null; Sha256 = $null }
    }
    $hash = $assets[0].digest.Substring(7)
    $directory = Join-Path $OutputDirectory $hash
    $path = Join-Path $directory $assetName
    if (-not (Test-Path -LiteralPath $path)) {
        $null = [IO.Directory]::CreateDirectory($directory)
        gh release download $tag --repo $repository --dir $directory --pattern $assetName
        if ($LASTEXITCODE -ne 0) { throw 'Could not download the verified reusable audio component.' }
    }
    Assert-MacCanvasAudioArtifact $path $assets[0].size $hash
    Write-Host "Reusing unchanged audio component $currentVersion from $tag (SHA-256 $hash)."
    return [pscustomobject]@{
        Reused = $true
        Path = [IO.Path]::GetFullPath($path)
        Sha256 = $hash
    }
}

Resolve-MacCanvasAudioComponent (Split-Path -Parent $PSScriptRoot) $ReferenceTag $OutputDirectory
