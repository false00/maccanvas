#Requires -Version 5.1
[CmdletBinding()]
param([string]$Executable = '')

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class MacCanvasScreenshot {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out Rect r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, uint attribute, out Rect bounds, int size);
    delegate bool WindowCallback(IntPtr window, IntPtr data);
    [DllImport("user32.dll")] static extern bool EnumWindows(WindowCallback callback, IntPtr data);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder name, int maximum);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint message, IntPtr w, IntPtr l);
    public static IntPtr Find(int processId) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, data) => {
            uint owner;
            GetWindowThreadProcessId(h, out owner);
            if (owner == processId && IsWindowVisible(h)) {
                var name = new StringBuilder(128);
                GetWindowText(h, name, 128);
                if (name.ToString() == "MacCanvas") { found = h; return false; }
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
'@
$root = Split-Path -Parent $PSScriptRoot
if (-not $Executable) { $Executable = Join-Path $root 'target\x86_64-pc-windows-msvc\debug\maccanvas.exe' }
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-screenshot-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($temporary)
$settings = @{
    version = 1
    profiles = @(
        @{ id = [guid]::NewGuid().ToString(); name = 'Studio Mac mini'; host = '192.0.2.10'; port = 5900; authentication = 'mac_login'; has_saved_credentials = $true },
        @{ id = [guid]::NewGuid().ToString(); name = 'MacBook Pro'; host = '192.0.2.20'; port = 5900; authentication = 'mac_login'; has_saved_credentials = $false }
    )
    ctrl_as_command = $true; sidebar_open = $true; show_fps = $true
}
[IO.File]::WriteAllText((Join-Path $temporary 'settings.json'), ($settings | ConvertTo-Json -Depth 8))
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = (Resolve-Path -LiteralPath $Executable).Path
$start.Arguments = "--data-dir `"$temporary`""
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardError = $true
$process = [Diagnostics.Process]::Start($start)
$handle = [IntPtr]::Zero

function Capture-OwnedWindow([string]$Destination) {
    $rectangle = [MacCanvasScreenshot+Rect]::new()
    if (-not [MacCanvasScreenshot]::GetWindowRect($handle, [ref]$rectangle)) { throw 'Cannot get the fixture window bounds.' }
    $bitmap = [Drawing.Bitmap]::new($rectangle.Right - $rectangle.Left, $rectangle.Bottom - $rectangle.Top)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $dc = $graphics.GetHdc()
    try {
        if (-not [MacCanvasScreenshot]::PrintWindow($handle, $dc, 2)) { throw 'Cannot capture the owned fixture window.' }
    } finally { $graphics.ReleaseHdc($dc) }
    try {
        $visible = [MacCanvasScreenshot+Rect]::new()
        if ([MacCanvasScreenshot]::DwmGetWindowAttribute($handle, 9, [ref]$visible, 16) -ne 0) { throw 'Cannot get visible fixture frame bounds.' }
        $crop = [Drawing.Rectangle]::new(
            [Math]::Max(0, $visible.Left - $rectangle.Left), [Math]::Max(0, $visible.Top - $rectangle.Top),
            [Math]::Min($bitmap.Width, $visible.Right - $visible.Left), [Math]::Min($bitmap.Height, $visible.Bottom - $visible.Top)
        )
        $cropped = $bitmap.Clone($crop, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
        try { $cropped.Save($Destination, [Drawing.Imaging.ImageFormat]::Png) }
        finally { $cropped.Dispose() }
    } finally { $graphics.Dispose(); $bitmap.Dispose() }
}

try {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while ($handle -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline) {
        if ($process.HasExited) { throw $process.StandardError.ReadToEnd() }
        Start-Sleep -Milliseconds 100
        $handle = [MacCanvasScreenshot]::Find($process.Id)
    }
    if ($handle -eq [IntPtr]::Zero) { throw 'Fixture did not create its window.' }
    Start-Sleep -Seconds 1
    $images = Join-Path $root 'docs\images'
    $null = [IO.Directory]::CreateDirectory($images)
    Capture-OwnedWindow (Join-Path $images 'connections.png')
    $window = [Windows.Automation.AutomationElement]::FromHandle($handle)
    $buttonCondition = [Windows.Automation.AndCondition]::new(
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Button),
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Details')
    )
    $details = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $buttonCondition)
    if ($null -eq $details) { throw 'Saved-machine details button was not found.' }
    $details.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    Start-Sleep -Milliseconds 400
    $passwordCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Mac password')
    if ($null -ne $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $passwordCondition)) {
        throw 'Saved-login details exposed a password control.'
    }
    $helloCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Saved login is locked')
    if ($null -eq $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $helloCondition)) {
        throw 'Mandatory Windows Hello protection is not visible.'
    }
    Capture-OwnedWindow (Join-Path $images 'saved-login.png')
    $allCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Show saved Macs')
    $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $allCondition).
        GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    Start-Sleep -Milliseconds 150
    $searchCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Search saved Macs')
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $search = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $searchCondition)
        if ($null -eq $search) { Start-Sleep -Milliseconds 50 }
    } while ($null -eq $search -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $search) { throw 'Saved-Mac list did not reopen after activating All Macs.' }
    $search.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).SetValue('MacBook')
    Start-Sleep -Milliseconds 150
    $details = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $buttonCondition)
    $details.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    $addressCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Mac address')
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $address = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $addressCondition)
        if ($null -eq $address) { Start-Sleep -Milliseconds 50 }
    } while ($null -eq $address -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $address) { throw 'Saved-Mac details did not open after selecting the filtered result.' }
    if ($address.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value -cne '192.0.2.20') {
        throw 'Search selected the wrong saved Mac.'
    }
    Write-Output 'Synthetic screenshot capture and saved-password non-disclosure checks: PASS'
} catch {
    Write-Output $_.ToString()
    throw
} finally {
    if (-not $process.HasExited) {
        $null = [MacCanvasScreenshot]::PostMessage($handle, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
        if (-not $process.WaitForExit(5000)) { Stop-Process -Id $process.Id -Force }
    }
    $stderr = $process.StandardError.ReadToEnd()
    if ($stderr) { Write-Output $stderr }
    [IO.File]::Delete((Join-Path $temporary 'settings.json'))
    if (@(Get-ChildItem -LiteralPath $temporary -Force).Count -eq 0) { [IO.Directory]::Delete($temporary, $false) }
    if ($process.ExitCode -ne 0) { throw "Screenshot fixture exited with $($process.ExitCode)." }
}
