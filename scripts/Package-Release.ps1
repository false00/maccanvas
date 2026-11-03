#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [string]$Executable = '',
    [string]$AudioExecutable = '',
    [string]$OutputDirectory = 'dist'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
$manifest = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
$match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"')
if (-not $match.Success -or $Version -cne "v$($match.Groups[1].Value)") {
    throw 'Release tag does not match Cargo.toml.'
}
if (-not $Executable) { $Executable = Join-Path $root 'target\x86_64-pc-windows-msvc\release\maccanvas.exe' }
$exe = (Resolve-Path -LiteralPath $Executable).Path
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $visualStudio) { throw 'Visual Studio C++ tools were not found.' }
$dumpbin = Get-ChildItem -Path (Join-Path $visualStudio 'VC\Tools\MSVC') -Filter 'dumpbin.exe' -Recurse |
    Where-Object { $_.FullName -match '\\bin\\Hostx64\\x64\\dumpbin.exe$' } |
    Sort-Object FullName -Descending | Select-Object -First 1
if ($null -eq $dumpbin) { throw 'dumpbin.exe was not found.' }
function Test-SystemImports([string]$Path) {
    $output = & $dumpbin.FullName /DEPENDENTS $Path
    if ($LASTEXITCODE -ne 0) { throw "Cannot inspect release imports: $Path" }
    $imports = @($output | ForEach-Object {
        if ($_ -match '^\s+([A-Za-z0-9_.-]+\.dll)\s*$') { $Matches[1] }
    })
    if ($imports.Count -eq 0) { throw "Executable imports could not be verified: $Path" }
    foreach ($dll in $imports) {
        if ($dll -match '^(MSVCP|VCRUNTIME|libgcc|libstdc|Qt|slint)' -or
            ($dll -notmatch '^(api|ext)-ms-' -and -not (Test-Path -LiteralPath (Join-Path "$env:SystemRoot\System32" $dll) -PathType Leaf))) {
            throw "Release is not self-contained; external runtime import: $dll"
        }
    }
    return $imports.Count
}
function Read-ReleaseCli([string]$Path, [string]$Argument) {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $Path
    $start.Arguments = $Argument
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $start.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $process = [Diagnostics.Process]::Start($start)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(15000)) {
            Stop-Process -Id $process.Id -ErrorAction Stop
            throw "Release metadata command timed out: $Argument"
        }
        $text = $stdout.GetAwaiter().GetResult()
        $errorText = $stderr.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) { throw "Release metadata command failed: $errorText" }
        return $text
    } finally { $process.Dispose() }
}
$codec = Read-ReleaseCli $exe '--audio-component-info' | ConvertFrom-Json
if ($codec.version -cne $match.Groups[1].Value -or $codec.asset -cne 'MacCanvas-audio-windows-x64.exe' -or
    $codec.sha256 -cnotmatch '^[a-f0-9]{64}$' -or $codec.size -le 0 -or $codec.size -gt (8MB)) {
    throw 'The viewer did not provide valid matching-release audio component metadata.'
}
if (-not $AudioExecutable) {
    $build = Join-Path (Split-Path -Parent $exe) 'build'
    if (-not (Test-Path -LiteralPath $build -PathType Container)) {
        throw 'Pass -AudioExecutable when packaging a viewer outside its Cargo release directory.'
    }
    $candidates = @(Get-ChildItem -LiteralPath $build -Directory -Filter 'audio-codec-tool-*' |
        ForEach-Object { Join-Path $_.FullName 'out\bin\aac-to-wav.exe' } |
        Where-Object {
            (Test-Path -LiteralPath $_ -PathType Leaf) -and
            (Get-Item -LiteralPath $_).Length -eq $codec.size -and
            (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash -ieq $codec.sha256
        })
    if ($candidates.Count -eq 0) { throw 'The exact audio component pinned by this viewer was not found.' }
    $AudioExecutable = $candidates[0]
}
$audio = (Resolve-Path -LiteralPath $AudioExecutable).Path
if ((Get-Item -LiteralPath $audio).Length -ne $codec.size -or
    (Get-FileHash -LiteralPath $audio -Algorithm SHA256).Hash -ine $codec.sha256) {
    throw 'The audio executable does not match the size and SHA-256 compiled into this viewer.'
}
$imports = Test-SystemImports $exe
$audioImports = Test-SystemImports $audio
$audioNotices = Read-ReleaseCli $audio '--licenses'
if (-not $audioNotices.Contains('Software License for The Fraunhofer FDK AAC Codec Library for Android') -or
    -not $audioNotices.Contains('BSD Zero Clause License')) {
    throw 'The separate converter must include both complete license notices.'
}
$dist = if ([IO.Path]::IsPathRooted($OutputDirectory)) { $OutputDirectory } else { Join-Path $root $OutputDirectory }
if (Test-Path -LiteralPath $dist) { throw 'dist already exists; preserve the previous package and choose a clean build directory.' }
$null = [IO.Directory]::CreateDirectory($dist)
$destination = Join-Path $dist 'MacCanvas-windows-x64.exe'
Copy-Item -LiteralPath $exe -Destination $destination
Copy-Item -LiteralPath $audio -Destination (Join-Path $dist $codec.asset)
Copy-Item -LiteralPath (Join-Path $root 'install.ps1') -Destination $dist
[IO.File]::WriteAllText((Join-Path $dist 'MacCanvas-audio-NOTICES.txt'), $audioNotices, [Text.UTF8Encoding]::new($false))
$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), "$hash  MacCanvas-windows-x64.exe`n$($codec.sha256)  $($codec.asset)`n", [Text.UTF8Encoding]::new($false))
Write-Output "Portable executable: $destination"
Write-Output "SHA-256: $hash"
Write-Output "Optional audio component: $($codec.asset)"
Write-Output "Audio SHA-256: $($codec.sha256)"
Write-Output "Windows system imports only: viewer=$imports; audio=$audioImports"
