#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Application = '',
    [string]$Probe = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $Application) { $Application = Join-Path $root 'target\x86_64-pc-windows-msvc\release\maccanvas.exe' }
if (-not $Probe) { $Probe = Join-Path $root 'target\x86_64-pc-windows-msvc\release\examples\update_probe.exe' }
$applicationPath = (Resolve-Path -LiteralPath $Application).Path
$probePath = (Resolve-Path -LiteralPath $Probe).Path
& $probePath --preflight $applicationPath
if ($LASTEXITCODE -ne 0) { throw 'Candidate executable preflight failed before replacement.' }
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-replace-test-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($temporary)
$executable = Join-Path $temporary 'MacCanvas.exe'
Copy-Item -LiteralPath $probePath -Destination $executable
$process = $null
try {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $executable
    $start.Arguments = "`"$applicationPath`""
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($start)
    if (-not $process.WaitForExit(15000)) {
        Stop-Process -Id $process.Id -Force
        throw 'Portable replacement fixture did not finish.'
    }
    if ($process.ExitCode -ne 0) { throw $process.StandardError.ReadToEnd() }
    if ((Get-FileHash -LiteralPath $executable).Hash -cne (Get-FileHash -LiteralPath $applicationPath).Hash) {
        throw 'The Windows replacement did not produce the expected executable.'
    }
    $versionStart = [Diagnostics.ProcessStartInfo]::new()
    $versionStart.FileName = $executable
    $versionStart.Arguments = '--version'
    $versionStart.UseShellExecute = $false
    $versionStart.CreateNoWindow = $true
    $versionStart.RedirectStandardOutput = $true
    $version = [Diagnostics.Process]::Start($versionStart)
    try {
        if (-not $version.WaitForExit(10000)) {
            Stop-Process -Id $version.Id -Force
            throw 'The replaced executable did not start.'
        }
        $manifest = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
        $expected = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
        if ($version.ExitCode -ne 0 -or $version.StandardOutput.ReadToEnd().Trim() -cne "MacCanvas $expected") {
            throw 'The replaced executable reports an unexpected version.'
        }
    } finally { $version.Dispose() }
    Write-Output 'Windows running-EXE replacement, restartability, and checksum: PASS'
} finally {
    if ($null -ne $process) { $process.Dispose() }
    if (Test-Path -LiteralPath $executable -PathType Leaf) { [IO.File]::Delete($executable) }
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while (@(Get-ChildItem -LiteralPath $temporary -Force).Count -gt 0 -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 100
    }
    if (@(Get-ChildItem -LiteralPath $temporary -Force).Count -eq 0) {
        [IO.Directory]::Delete($temporary, $false)
    } else {
        throw "Replacement helper cleanup did not finish. The isolated test directory was preserved: $temporary"
    }
}
