#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [string]$Executable = '',
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
$output = & $dumpbin.FullName /DEPENDENTS $exe
if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect release imports.' }
$imports = @($output | ForEach-Object {
    if ($_ -match '^\s+([A-Za-z0-9_.-]+\.dll)\s*$') { $Matches[1] }
})
if ($imports.Count -eq 0) { throw 'Release executable imports could not be verified.' }
foreach ($dll in $imports) {
    if ($dll -match '^(MSVCP|VCRUNTIME|libgcc|libstdc|Qt|slint)' -or
        ($dll -notmatch '^(api|ext)-ms-' -and -not (Test-Path -LiteralPath (Join-Path "$env:SystemRoot\System32" $dll) -PathType Leaf))) {
        throw "Release is not self-contained; external runtime import: $dll"
    }
}
$dist = if ([IO.Path]::IsPathRooted($OutputDirectory)) { $OutputDirectory } else { Join-Path $root $OutputDirectory }
if (Test-Path -LiteralPath $dist) { throw 'dist already exists; preserve the previous package and choose a clean build directory.' }
$null = [IO.Directory]::CreateDirectory($dist)
$destination = Join-Path $dist 'MacCanvas-windows-x64.exe'
Copy-Item -LiteralPath $exe -Destination $destination
Copy-Item -LiteralPath (Join-Path $root 'install.ps1') -Destination $dist
$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), "$hash  MacCanvas-windows-x64.exe`n", [Text.UTF8Encoding]::new($false))
Write-Output "Portable executable: $destination"
Write-Output "SHA-256: $hash"
Write-Output "Windows system imports only: $($imports.Count)"
