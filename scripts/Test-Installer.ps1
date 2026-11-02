#Requires -Version 5.1
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$installer = Join-Path (Split-Path -Parent $PSScriptRoot) 'install.ps1'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) { throw ($errors | Out-String) }
foreach ($definition in $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
    . ([scriptblock]::Create($definition.Extent.Text))
}
$script:passed = 0
function Assert-Test([bool]$Value, [string]$Message) {
    if (-not $Value) { throw "FAIL: $Message" }
    $script:passed++
    Write-Output "PASS: $Message"
}
function Assert-Throws([scriptblock]$Action, [string]$Message) {
    $threw = $false
    try { & $Action | Out-Null } catch { $threw = $true }
    Assert-Test $threw $Message
}
function Set-Fixture([string]$Version, [byte]$Marker) {
    $script:binary = New-Object byte[] 512
    $script:binary[0] = 0x4d; $script:binary[1] = 0x5a
    [BitConverter]::GetBytes([uint32]128).CopyTo($script:binary, 0x3c)
    $script:binary[128] = 0x50; $script:binary[129] = 0x45
    $script:binary[132] = 0x64; $script:binary[133] = 0x86
    $script:binary[152] = 0x0b; $script:binary[153] = 0x02
    $script:binary[300] = $Marker
    $script:hash = Get-MacCanvasHash $script:binary
    $script:checksums = [Text.Encoding]::UTF8.GetBytes("$script:hash  MacCanvas-windows-x64.exe`n")
    $base = "https://github.com/false00/maccanvas/releases/download/$Version"
    $script:release = [pscustomobject]@{
        tag_name = $Version; draft = $false; prerelease = $false
        assets = @(
            [pscustomobject]@{ name = 'MacCanvas-windows-x64.exe'; size = $script:binary.Length; digest = "sha256:$script:hash"; browser_download_url = "$base/MacCanvas-windows-x64.exe" }
            [pscustomobject]@{ name = 'SHA256SUMS.txt'; size = $script:checksums.Length; digest = "sha256:$(Get-MacCanvasHash $script:checksums)"; browser_download_url = "$base/SHA256SUMS.txt" }
        )
    }
}
function Receive-MacCanvasBytes($Client, [uri]$Uri, [long]$Maximum) {
    Assert-MacCanvasUrl $Uri
    if ($Uri.DnsSafeHost -eq 'api.github.com') {
        return ,[Text.Encoding]::UTF8.GetBytes(($script:release | ConvertTo-Json -Depth 8))
    }
    if ($Uri.AbsolutePath.EndsWith('/SHA256SUMS.txt')) { return ,$script:checksums }
    if ($Uri.AbsolutePath.EndsWith('/MacCanvas-windows-x64.exe')) { return ,$script:binary }
    throw 'Unexpected fixture network request'
}

$root = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-installer-test-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($root)
$managed = Join-Path $root 'managed'
$unrelated = Join-Path $root 'unrelated'
$null = [IO.Directory]::CreateDirectory($unrelated)
try {
    foreach ($path in @('relative', 'C:\', '\\host\share', 'C:\Temp\..\elsewhere', 'C:\Temp\CON.txt', 'C:\Temp\app.exe:secret', 'C:\Temp\ambiguous.')) {
        Assert-Throws { Assert-MacCanvasPath $path } "reject unsafe path: $path"
    }
    foreach ($uri in @('http://github.com/file', 'https://github.com.evil.example/file', 'https://user@github.com/file', 'https://github.com:444/file')) {
        Assert-Throws { Assert-MacCanvasUrl ([uri]$uri) } "reject unsafe download URL: $uri"
    }
    Assert-MacCanvasUrl ([uri]'https://release-assets.githubusercontent.com/file')
    Assert-Test $true 'accept HTTPS GitHub asset host'
    Assert-Throws { Get-MacCanvasChecksum (('a' * 64) + '  MacCanvas-windows-x64.exe' + "`n" + ('b' * 64) + '  MacCanvas-windows-x64.exe') 'MacCanvas-windows-x64.exe' } 'reject duplicate checksums'
    Set-Fixture 'v0.1.0' 1
    Assert-MacCanvasExecutable $script:binary
    Assert-Test $true 'accept x64 PE32+ fixture without executing it'
    $script:binary[132] = 0
    Assert-Throws { Assert-MacCanvasExecutable $script:binary } 'reject incorrect executable architecture'
    Set-Fixture 'v0.1.0' 1
    $first = Install-MacCanvas -Version latest -InstallDirectory $managed -NoLaunch -NoShortcut
    Assert-Test ((Get-FileHash -LiteralPath $first.Executable).Hash -ieq $script:hash) 'install matching release bytes'
    $again = Install-MacCanvas -Version v0.1.0 -InstallDirectory $managed -NoLaunch -NoShortcut
    Assert-Test ($again.Version -ceq 'v0.1.0') 'repeat install succeeds'
    $oldHash = $script:hash
    Set-Fixture 'v0.1.1' 2
    $receiptLock = [IO.File]::Open((Join-Path $managed '.maccanvas-install.json'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try { Assert-Throws { Install-MacCanvas -Version latest -InstallDirectory $managed -NoLaunch -NoShortcut } 'receipt lock triggers safe rollback' }
    finally { $receiptLock.Dispose() }
    Assert-Test ((Get-FileHash -LiteralPath $first.Executable).Hash -ieq $oldHash) 'failed upgrade preserves previous executable'
    $updated = Install-MacCanvas -Version latest -InstallDirectory $managed -NoLaunch -NoShortcut
    Assert-Test ((Get-FileHash -LiteralPath $updated.Executable).Hash -ieq $script:hash) 'upgrade replaces only managed executable'
    $before = [IO.File]::ReadAllBytes($updated.Executable)
    $script:binary[301] = 99
    Assert-Throws { Install-MacCanvas -Version latest -InstallDirectory $managed -NoLaunch -NoShortcut } 'digest mismatch refuses install'
    Assert-Test ((Get-MacCanvasHash ([IO.File]::ReadAllBytes($updated.Executable))) -ieq (Get-MacCanvasHash $before)) 'bad download leaves installation intact'
    [IO.File]::WriteAllText((Join-Path $unrelated 'keep.txt'), 'unrelated')
    Assert-Throws { Install-MacCanvas -Version latest -InstallDirectory $unrelated -NoLaunch -NoShortcut } 'unrelated files are never overwritten'
    Uninstall-MacCanvas -InstallDirectory $managed
    Assert-Test (-not (Test-Path -LiteralPath $managed)) 'uninstall removes only known managed files'
    Assert-Test (Test-Path -LiteralPath (Join-Path $unrelated 'keep.txt')) 'uninstall preserves unrelated files'
} finally {
    foreach ($path in @(
        (Join-Path $managed 'MacCanvas.exe'),
        (Join-Path $managed '.maccanvas-install.json'),
        (Join-Path $unrelated 'keep.txt')
    )) {
        if (Test-Path -LiteralPath $path -PathType Leaf) { [IO.File]::Delete($path) }
    }
    foreach ($path in @($managed, $unrelated, $root)) {
        if ((Test-Path -LiteralPath $path -PathType Container) -and @(Get-ChildItem -LiteralPath $path -Force).Count -eq 0) {
            [IO.Directory]::Delete($path, $false)
        }
    }
}
Write-Output "Installer checks passed: $script:passed"
