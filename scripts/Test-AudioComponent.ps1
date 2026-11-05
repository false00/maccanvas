#Requires -Version 5.1
$ErrorActionPreference = 'Stop'
$scriptPath = Join-Path $PSScriptRoot 'Prepare-AudioComponent.ps1'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw ($errors | Out-String) }
foreach ($definition in $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
    . ([scriptblock]::Create($definition.Extent.Text))
}
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-audio-reuse-test-' + [guid]::NewGuid().ToString('N'))
$repository = Join-Path $temporary 'source'
$output = Join-Path $temporary 'output'
$null = [IO.Directory]::CreateDirectory($repository)
$script:passed = 0
$script:downloads = 0
$script:corruptDownload = $false
$script:networkFailure = $false
$script:binary = New-Object byte[] 512
$script:binary[0] = 0x4d; $script:binary[1] = 0x5a
[BitConverter]::GetBytes([uint32]128).CopyTo($script:binary, 0x3c)
$script:binary[128] = 0x50; $script:binary[129] = 0x45
$script:binary[132] = 0x64; $script:binary[133] = 0x86
$script:binary[152] = 0x0b; $script:binary[153] = 0x02
$sha = [Security.Cryptography.SHA256]::Create()
try { $script:hash = ([BitConverter]::ToString($sha.ComputeHash($script:binary))).Replace('-', '').ToLowerInvariant() }
finally { $sha.Dispose() }
$script:release = @{
    tagName = 'v0.5.4'; isDraft = $false; isPrerelease = $false
    url = 'https://github.com/false00/maccanvas/releases/tag/v0.5.4'
    assets = @(@{ name = 'MacCanvas-audio-windows-x64.exe'; size = $script:binary.Length; digest = "sha256:$script:hash" })
}
function gh {
    $global:LASTEXITCODE = 0
    if ($args[0] -ceq 'release' -and $args[1] -ceq 'view') {
        if ($script:networkFailure) { $global:LASTEXITCODE = 1; return }
        return ($script:release | ConvertTo-Json -Depth 6)
    }
    if ($args[0] -ceq 'release' -and $args[1] -ceq 'download') {
        $script:downloads++
        $destination = $args[[Array]::IndexOf($args, '--dir') + 1]
        $bytes = $script:binary.Clone()
        if ($script:corruptDownload) { $bytes[300] = 99 }
        [IO.File]::WriteAllBytes((Join-Path $destination 'MacCanvas-audio-windows-x64.exe'), $bytes)
        return
    }
    throw 'Unexpected network request in the offline audio-component test.'
}
function Assert-AudioTest([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "FAIL: $Message" }
    $script:passed++
    Write-Output "PASS: $Message"
}
function Assert-AudioThrows([scriptblock]$Action, [string]$Message) {
    $threw = $false
    try { & $Action | Out-Null } catch { $threw = $true }
    Assert-AudioTest $threw $Message
}
function Write-FixtureFile([string]$Relative, [string]$Text) {
    $path = Join-Path $repository $Relative
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
    [IO.File]::WriteAllText($path, $Text)
}
try {
    Write-FixtureFile 'Cargo.toml' "[package]`nname = `"viewer`"`nversion = `"0.5.4`"`n"
    Write-FixtureFile 'crates\audio-codec-tool\Cargo.toml' "[package]`nname = `"audio-codec-tool`"`nversion = `"0.1.0`"`n"
    Write-FixtureFile 'crates\audio-codec-tool\native\main.cpp' 'int main() { return 0; }'
    Write-FixtureFile 'crates\audio-codec-tool\vendor\fixture.cpp' 'int fixture = 1;'
    Write-FixtureFile 'crates\audio-codec-tool\LICENSE' 'Fixture license'
    Write-FixtureFile 'crates\audio-codec-tool\DISTRIBUTION-NOTICES.txt' 'Fixture notices'
    Write-FixtureFile 'crates\audio-codec-tool\build.rs' '// Build metadata'
    git -C $repository init --quiet
    if ($LASTEXITCODE -ne 0) { throw 'Fixture Git initialization failed.' }
    git -C $repository config user.name 'MacCanvas test'
    git -C $repository config user.email 'fixture@example.invalid'
    git -C $repository add .
    git -C $repository -c commit.gpgsign=false commit --quiet -m fixture
    if ($LASTEXITCODE -ne 0) { throw 'Fixture commit failed.' }
    git -C $repository tag v0.5.4
    if ($LASTEXITCODE -ne 0) { throw 'Fixture reference tag failed.' }
    $first = Resolve-MacCanvasAudioComponent $repository latest $output
    Assert-AudioTest ($first.Reused -and $first.Sha256 -ceq $script:hash -and $script:downloads -eq 1) 'unchanged component reuses the verified published artifact'
    Write-FixtureFile 'Cargo.toml' "[package]`nname = `"viewer`"`nversion = `"0.5.5`"`n"
    Write-FixtureFile 'crates\audio-codec-tool\build.rs' '// Updated Rust metadata plumbing'
    $second = Resolve-MacCanvasAudioComponent $repository latest $output
    Assert-AudioTest ($second.Reused -and $second.Path -ceq $first.Path -and $second.Sha256 -ceq $first.Sha256 -and $script:downloads -eq 1) 'viewer-only updates preserve the exact audio bytes and cache key without another download'
    Write-FixtureFile 'crates\audio-codec-tool\native\main.cpp' 'int main() { return 1; }'
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'changed native code cannot silently reuse an old component version'
    Write-FixtureFile 'crates\audio-codec-tool\Cargo.toml' "[package]`nname = `"audio-codec-tool`"`nversion = `"0.1.1`"`n"
    $changed = Resolve-MacCanvasAudioComponent $repository latest $output
    Assert-AudioTest (-not $changed.Reused -and $null -eq $changed.Path) 'a component version change selects a fresh source build'
    Write-FixtureFile 'crates\audio-codec-tool\native\main.cpp' 'int main() { return 0; }'
    Write-FixtureFile 'crates\audio-codec-tool\Cargo.toml' "[package]`nname = `"audio-codec-tool`"`nversion = `"0.1.0`"`n"
    Write-FixtureFile 'crates\audio-codec-tool\vendor\untracked.cpp' 'int new_input = 1;'
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'untracked native inputs cannot bypass the component-version guard'
    [IO.File]::Delete((Join-Path $repository 'crates\audio-codec-tool\vendor\untracked.cpp'))
    $script:release.assets[0].digest = 'sha256:invalid'
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'invalid published checksum metadata is rejected'
    $script:release.assets[0].digest = "sha256:$script:hash"
    $script:release.isDraft = $true
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'draft components are rejected'
    $script:release.isDraft = $false
    $script:release.isPrerelease = $true
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'prerelease components are rejected'
    $script:release.isPrerelease = $false
    $script:release.url = 'https://github.com/other/project/releases/tag/v0.5.4'
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'foreign reference releases are rejected'
    $script:release.url = 'https://github.com/false00/maccanvas/releases/tag/v0.5.4'
    $script:networkFailure = $true
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'metadata lookup failures cannot silently cause a new converter build'
    $script:networkFailure = $false
    [IO.File]::WriteAllBytes($first.Path, [byte[]]@(1, 2, 3))
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest $output } 'a corrupt cached artifact fails verification instead of being trusted'
    $script:corruptDownload = $true
    Assert-AudioThrows { Resolve-MacCanvasAudioComponent $repository latest (Join-Path $temporary 'bad-download') } 'a corrupt download fails published checksum verification'
    Write-Output "Audio artifact reuse checks passed: $script:passed"
} finally {
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
}
