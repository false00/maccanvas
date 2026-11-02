#Requires -Version 5.1
param([string]$Executable = '')
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$root = Split-Path -Parent $PSScriptRoot
if (-not $Executable) { $Executable = Join-Path $root 'target\x86_64-pc-windows-msvc\debug\maccanvas.exe' }
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-update-prompt-' + [guid]::NewGuid().ToString('N'))
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = (Resolve-Path -LiteralPath $Executable).Path
$start.Arguments = "--data-dir `"$temporary`" --preview-update"
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardError = $true
$process = [Diagnostics.Process]::Start($start)
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while (($process.MainWindowHandle -eq [IntPtr]::Zero -or $process.MainWindowTitle -cne 'MacCanvas') -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
        if ($process.HasExited) { throw $process.StandardError.ReadToEnd() }
    }
    if ($process.MainWindowTitle -cne 'MacCanvas') { throw 'Update preview did not create its native window.' }
    $window = [Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle)
    function Find-UpdateControl([string]$Name) {
        $condition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, $Name)
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            $element = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
            if ($null -eq $element) { Start-Sleep -Milliseconds 50 }
        } while ($null -eq $element -and [DateTime]::UtcNow -lt $deadline)
        if ($null -eq $element) { throw "Update control was not found: $Name" }
        return $element
    }
    $null = Find-UpdateControl 'MacCanvas update notification'
    $update = Find-UpdateControl 'Update & restart'
    if (-not $update.Current.IsEnabled) { throw 'Idle update confirmation was not enabled.' }
    (Find-UpdateControl 'Later').GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    Start-Sleep -Milliseconds 200
    $condition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'MacCanvas update notification')
    if ($null -ne $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)) {
        throw 'Later did not dismiss the update prompt.'
    }
    (Find-UpdateControl 'Check for updates').GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    $null = Find-UpdateControl 'MacCanvas update notification'
    Write-Output 'New-version prompt, explicit update confirmation, Later, and reopening the prompt: PASS'
} finally {
    if (-not $process.HasExited) {
        $null = $process.CloseMainWindow()
        if (-not $process.WaitForExit(5000)) { Stop-Process -Id $process.Id -Force }
    }
    $stderr = $process.StandardError.ReadToEnd()
    if ($stderr) { Write-Output $stderr }
    if (Test-Path -LiteralPath (Join-Path $temporary 'settings.json')) { [IO.File]::Delete((Join-Path $temporary 'settings.json')) }
    if ((Test-Path -LiteralPath $temporary -PathType Container) -and @(Get-ChildItem -LiteralPath $temporary -Force).Count -eq 0) {
        [IO.Directory]::Delete($temporary, $false)
    }
    if ($process.ExitCode -ne 0) { throw "Update prompt fixture exited with $($process.ExitCode)." }
}
