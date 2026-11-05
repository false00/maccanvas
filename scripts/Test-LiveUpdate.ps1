#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PreviousExecutable,
    [Parameter(Mandatory = $true)][string]$ExpectedTag
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
if ($ExpectedTag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { throw 'ExpectedTag must be a stable version tag.' }
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-live-update-' + [guid]::NewGuid().ToString('N'))
$data = Join-Path $temporary 'data'
$null = [IO.Directory]::CreateDirectory($data)
$exe = Join-Path $temporary 'MacCanvas.exe'
Copy-Item -LiteralPath (Resolve-Path -LiteralPath $PreviousExecutable).Path -Destination $exe
$identifier = [guid]::NewGuid().ToString()
$settings = @{
    version = 1
    profiles = @(@{
        id = $identifier; name = 'Update fixture Mac'; host = '192.0.2.40'; port = 5900
        authentication = 'mac_login'; has_saved_credentials = $false
    })
    ctrl_as_command = $false; sidebar_open = $true; show_fps = $true
    streaming = @{ encoding = 'zrle'; compression = 4; jpeg_quality = 8; target_kbps = 1500; max_fps = 24 }
}
[IO.File]::WriteAllText((Join-Path $data 'settings.json'), ($settings | ConvertTo-Json -Depth 8))
$oldHash = (Get-FileHash -LiteralPath $exe).Hash.ToLowerInvariant()
$receiptPath = Join-Path $temporary '.maccanvas-install.json'
[IO.File]::WriteAllText($receiptPath, (@{ repository = 'false00/maccanvas'; version = 'v0.2.1'; sha256 = $oldHash } | ConvertTo-Json))
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = $exe
$start.Arguments = "--data-dir `"$data`""
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$process = [Diagnostics.Process]::Start($start)
$restarted = $null
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    while ($process.MainWindowTitle -cne 'MacCanvas' -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
        if ($process.HasExited) { throw 'The previous viewer exited before its update check.' }
    }
    if ($process.MainWindowTitle -cne 'MacCanvas') { throw 'The fixture viewer did not create its window.' }
    $window = [Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle)
    $condition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Update & restart')
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    do {
        $button = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
        if ($null -eq $button) { Start-Sleep -Milliseconds 100 }
    } while ($null -eq $button -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $button -or -not $button.Current.IsEnabled) { throw 'The real startup check did not offer the newer release.' }
    $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    if (-not $process.WaitForExit(180000) -or $process.ExitCode -ne 0) { throw 'The old viewer did not exit after updating.' }
    $receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
    if ($receipt.version -cne $ExpectedTag -or
        $receipt.sha256 -cne (Get-FileHash -LiteralPath $exe).Hash.ToLowerInvariant() -or
        $receipt.sha256 -ceq $oldHash) { throw 'The updated executable and installer receipt do not match the new release.' }
    $after = Get-Content -LiteralPath (Join-Path $data 'settings.json') -Raw | ConvertFrom-Json
    if ($after.profiles.Count -ne 1 -or $after.profiles[0].id -cne $identifier -or
        $after.profiles[0].name -cne 'Update fixture Mac' -or
        $after.profiles[0].host -cne '192.0.2.40' -or $after.ctrl_as_command -or
        $after.streaming.target_kbps -ne 1500 -or $after.streaming.max_fps -ne 24) {
        throw 'Machine profiles or preferences were not preserved.'
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $candidate = Get-CimInstance Win32_Process -Filter "Name = 'MacCanvas.exe'" |
            Where-Object { $_.ExecutablePath -ieq $exe -and $_.ProcessId -ne $process.Id } |
            Select-Object -First 1
        if ($null -ne $candidate) { $restarted = Get-Process -Id $candidate.ProcessId }
        if ($null -eq $restarted) { Start-Sleep -Milliseconds 100 }
    } while ($null -eq $restarted -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $restarted) { throw 'The new viewer did not restart.' }
    Write-Output 'Real startup check -> update prompt -> verified download -> replacement -> restart: PASS'
    Write-Output 'Profiles, preferences, and managed installer receipt preserved: PASS'
} finally {
    foreach ($owned in @($process, $restarted)) {
        if ($null -ne $owned) {
            try {
                if (-not $owned.HasExited) {
                    $null = $owned.CloseMainWindow()
                    if (-not $owned.WaitForExit(5000)) {
                        Stop-Process -Id $owned.Id -Force
                        if (-not $owned.WaitForExit(10000)) {
                            throw 'The owned update-test process did not finish terminating.'
                        }
                    }
                }
            } finally { $owned.Dispose() }
        }
    }
    foreach ($file in @($exe, $receiptPath, (Join-Path $data 'settings.json'))) {
        if (Test-Path -LiteralPath $file -PathType Leaf) { [IO.File]::Delete($file) }
    }
    foreach ($directory in @($data, $temporary)) {
        if ((Test-Path -LiteralPath $directory -PathType Container) -and @(Get-ChildItem -LiteralPath $directory -Force).Count -eq 0) {
            [IO.Directory]::Delete($directory, $false)
        }
    }
}
