#Requires -Version 5.1
param(
    [Parameter(Mandatory = $true)][int]$ProcessId,
    [int]$Port,
    [int]$SshPort,
    [string]$SshFingerprint,
    [ValidateSet('connect', 'bounds', 'fullscreen', 'exit-fullscreen', 'disconnect', 'focus', 'toggle-sidebar', 'telemetry', 'quality', 'manual-quality', 'close-quality', 'show-macs', 'new', 'saved-login-check', 'panels', 'ssh-dialogs')][string]$Action = 'bounds'
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$process = Get-Process -Id $ProcessId
$viewerHandle = $process.MainWindowHandle
$window = [Windows.Automation.AutomationElement]::FromHandle($viewerHandle)
function Find-FixtureElement([string]$Name, $Type) {
    $nameCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, $Name)
    if ($null -ne $Type) {
        $typeCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, $Type)
        $condition = [Windows.Automation.AndCondition]::new($nameCondition, $typeCondition)
    } else { $condition = $nameCondition }
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $element = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
        if ($null -eq $element) { Start-Sleep -Milliseconds 50 }
    } while ($null -eq $element -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $element) { throw "Test control was not found: $Name" }
    return $element
}
switch ($Action) {
    'ssh-dialogs' {
        if ($SshPort -le 0 -or -not $SshFingerprint) { throw 'An owned SSH fixture endpoint and fingerprint are required.' }
        Add-Type @'
using System;
using System.ComponentModel;
using System.Text;
using System.Runtime.InteropServices;
public static class MacCanvasOwnedDialogInput {
    [StructLayout(LayoutKind.Sequential)] struct Point { public int X, Y; }
    delegate bool WindowCallback(IntPtr h, IntPtr data);
    [DllImport("user32.dll")] static extern bool EnumWindows(WindowCallback callback, IntPtr data);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder value, int length);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr h, uint command);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool PostMessage(IntPtr h, uint message, IntPtr w, IntPtr l);
    [DllImport("user32.dll", SetLastError=true)] static extern bool ScreenToClient(IntPtr h, ref Point p);
    [DllImport("user32.dll", SetLastError=true)] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    public static IntPtr Find(int processId, IntPtr owner, string title) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, ignored) => {
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            if (IsWindowVisible(h) && (pid == processId || GetWindow(h, 4) == owner)) {
                var name = new StringBuilder(512);
                GetWindowText(h, name, name.Capacity);
                if (name.ToString() == title) { found = h; return false; }
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static string Describe(int processId, IntPtr owner) {
        var result = new StringBuilder();
        EnumWindows((h, ignored) => {
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            if (IsWindowVisible(h) && (pid == processId || GetWindow(h, 4) == owner)) {
                var name = new StringBuilder(512);
                GetWindowText(h, name, name.Capacity);
                result.Append("[").Append(h).Append(": ").Append(name).Append("] ");
            }
            return true;
        }, IntPtr.Zero);
        return result.ToString();
    }
    public static void Click(IntPtr h, int x, int y) {
        var old = SetThreadDpiAwarenessContext(new IntPtr(-4));
        if (old == IntPtr.Zero) throw new Win32Exception();
        try {
            var p = new Point { X=x, Y=y };
            if (!ScreenToClient(h, ref p)) throw new Win32Exception();
            var position = new IntPtr((p.Y << 16) | (p.X & 65535));
            if (!PostMessage(h, 0x0200, IntPtr.Zero, position) ||
                !PostMessage(h, 0x0201, new IntPtr(1), position) ||
                !PostMessage(h, 0x0202, IntPtr.Zero, position)) throw new Win32Exception();
        } finally { SetThreadDpiAwarenessContext(old); }
    }
}
'@
        function Cancel-OwnedPicker([string]$ButtonName, [string]$DialogTitle) {
            $button = Find-FixtureElement $ButtonName ([Windows.Automation.ControlType]::Button)
            $bounds = $button.Current.BoundingRectangle
            $owner = $viewerHandle
            [MacCanvasOwnedDialogInput]::Click($owner, [int]($bounds.X + $bounds.Width / 2), [int]($bounds.Y + $bounds.Height / 2))
            $deadline = [DateTime]::UtcNow.AddSeconds(10)
            do {
                $dialog = [MacCanvasOwnedDialogInput]::Find($ProcessId, $owner, $DialogTitle)
                if ($dialog -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 50 }
            } while ($dialog -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline)
            if ($dialog -eq [IntPtr]::Zero) {
                $names = [MacCanvasOwnedDialogInput]::Describe($ProcessId, $owner)
                throw "Owned Windows file dialog did not open: $DialogTitle. Owned windows: $names; button bounds: $bounds"
            }
            if (-not [MacCanvasOwnedDialogInput]::PostMessage($dialog, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)) {
                throw 'Cannot cancel the owned native file picker.'
            }
            $deadline = [DateTime]::UtcNow.AddSeconds(5)
            do {
                Start-Sleep -Milliseconds 50
                $dialog = [MacCanvasOwnedDialogInput]::Find($ProcessId, $owner, $DialogTitle)
            } while ($dialog -ne [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline)
            if ($dialog -ne [IntPtr]::Zero) { throw 'The owned file picker did not close.' }
        }
        (Find-FixtureElement 'Files and SSH clipboard' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        foreach ($entry in @(
            @('SSH port', "$SshPort"), @('SSH Mac username', 'fixture'),
            @('SSH Mac password', 'FixturePassword-DoNotPutInLogs')
        )) {
            (Find-FixtureElement $entry[0] ([Windows.Automation.ControlType]::Edit)).
                GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).SetValue($entry[1])
        }
        (Find-FixtureElement 'Connect securely' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $fingerprint = Find-FixtureElement 'SSH SHA-256 fingerprint' ([Windows.Automation.ControlType]::Edit)
        $observed = $fingerprint.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value
        if ($observed -ne $SshFingerprint) { throw 'The visible fingerprint is not the owned SSH fixture key.' }
        (Find-FixtureElement 'Trust key & continue' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Find-FixtureElement 'Receive source.txt' ([Windows.Automation.ControlType]::Button)
        $folder = Find-FixtureElement 'Mac folder Desktop' ([Windows.Automation.ControlType]::Group)
        $open = Find-FixtureElement 'Open Desktop' ([Windows.Automation.ControlType]::Button)
        $rowBounds = $folder.Current.BoundingRectangle
        $buttonBounds = $open.Current.BoundingRectangle
        if ([Math]::Abs(($rowBounds.Y + $rowBounds.Height / 2) - ($buttonBounds.Y + $buttonBounds.Height / 2)) -gt 2) {
            throw 'The folder Open button is not vertically centered in its row.'
        }
        $open.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            $names = @($window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition) |
                ForEach-Object { $_.Current.Name })
            if (-not ($names -match 'SSH operation error:.*full disk access for remote users')) { Start-Sleep -Milliseconds 50 }
        } while (-not ($names -match 'SSH operation error:.*full disk access for remote users') -and [DateTime]::UtcNow -lt $deadline)
        if (-not ($names -match 'SSH operation error:.*full disk access for remote users')) {
            throw 'Denied Desktop access did not show actionable macOS permission guidance.'
        }
        $pathField = Find-FixtureElement 'Mac folder' ([Windows.Automation.ControlType]::Edit)
        if ($pathField.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value -cne '/Users/fixture') {
            throw 'Denied folder navigation did not restore the last accessible path.'
        }
        (Find-FixtureElement 'Open Public' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Find-FixtureElement 'This folder is empty.' ([Windows.Automation.ControlType]::Text)
        (Find-FixtureElement 'Up' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Find-FixtureElement 'Receive source.txt' ([Windows.Automation.ControlType]::Button)
        Cancel-OwnedPicker 'Send a file...' 'Send a file to this Mac folder'
        Cancel-OwnedPicker 'Receive source.txt' 'Receive a new file - existing files are not replaced'
        (Find-FixtureElement 'Disconnect SSH' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        (Find-FixtureElement 'Done' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'panels' {
        (Find-FixtureElement 'About MacCanvas' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Find-FixtureElement 'GitHub profile' ([Windows.Automation.ControlType]::Button)
        $null = Find-FixtureElement 'curl.red' ([Windows.Automation.ControlType]::Button)
        $null = Find-FixtureElement 'AGPL-3.0 license' ([Windows.Automation.ControlType]::Button)
        $elements = $window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
        $names = @($elements | ForEach-Object { $_.Current.Name })
        $attribution = @($names | Where-Object { $_ -like 'Built by *' })
        if ($attribution.Count -ne 1 -or $attribution[0] -cne 'Built by @false00') {
            throw 'About must credit only the maintainer handle.'
        }
        if ($names -match 'Incident responder|Builder\.') { throw 'About must not display a personal tagline.' }
        if (-not ($names -match '^Built: \d{4}-\d\d-\d\d \d\d:\d\d:\d\d UTC')) { throw 'About is missing its UTC build timestamp.' }
        if (-not ($names -match '^Source: [0-9a-f]{40}')) { throw 'About is missing the full source revision.' }
        $null = Find-FixtureElement 'Target: x86_64-pc-windows-msvc' ([Windows.Automation.ControlType]::Text)
        (Find-FixtureElement 'Done' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        (Find-FixtureElement 'Clipboard sharing' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Find-FixtureElement 'Share plain-text clipboard over VNC' ([Windows.Automation.ControlType]::CheckBox)
        $null = Find-FixtureElement 'Encrypted SSH clipboard' ([Windows.Automation.ControlType]::Button)
        (Find-FixtureElement 'Done' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        (Find-FixtureElement 'Files and SSH clipboard' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Find-FixtureElement 'SSH port' ([Windows.Automation.ControlType]::Edit)
        $password = Find-FixtureElement 'SSH Mac password' ([Windows.Automation.ControlType]::Edit)
        if (-not $password.Current.IsPassword) { throw 'Manual SSH password field is not protected.' }
        (Find-FixtureElement 'Done' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'toggle-sidebar' {
        (Find-FixtureElement 'Toggle connections sidebar' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'show-macs' {
        (Find-FixtureElement 'Show saved Macs' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'new' {
        (Find-FixtureElement 'Add a Mac' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'quality' {
        (Find-FixtureElement 'Stream quality' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'manual-quality' {
        $slider = Find-FixtureElement 'Compression effort' ([Windows.Automation.ControlType]::Slider)
        $slider.GetCurrentPattern([Windows.Automation.RangeValuePattern]::Pattern).SetValue(3)
        $toggle = Find-FixtureElement 'Use a manual bandwidth target' ([Windows.Automation.ControlType]::CheckBox)
        $toggle.GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern).Toggle()
        Start-Sleep -Milliseconds 150
        $edit = Find-FixtureElement 'Bandwidth target Mbps' ([Windows.Automation.ControlType]::Edit)
        $edit.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).SetValue('1')
        $edit = Find-FixtureElement 'Maximum FPS' ([Windows.Automation.ControlType]::Edit)
        $edit.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).SetValue('15')
        Start-Sleep -Milliseconds 150
        (Find-FixtureElement 'Apply' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'close-quality' {
        (Find-FixtureElement 'Close' ([Windows.Automation.ControlType]::Button)).
            GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'telemetry' {
        $elements = $window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
        $rows = @()
        foreach ($element in $elements) {
            $name = $element.Current.Name
            if ($name -match '^(Connection status indicator|Connected to |Download rate:|Upload rate:|Live FPS:)') {
                $r = $element.Current.BoundingRectangle
                $rows += @{ name = $name; x = $r.X; y = $r.Y; width = $r.Width; height = $r.Height }
            }
        }
        ConvertTo-Json -InputObject $rows -Compress
    }
    'saved-login-check' {
        $condition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Mac password')
        if ($null -ne $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)) {
            throw 'Saved-login state exposed a password editor.'
        }
        $null = Find-FixtureElement 'Saved login is locked' ([Windows.Automation.ControlType]::Text)
    }
    'focus' { $window.SetFocus() }
    'connect' {
        foreach ($entry in @(
            @('Machine name', 'Fixture Mac'),
            @('Mac address', '127.0.0.1'),
            @('VNC port', "$Port"),
            @('Mac username', 'fixture-user'),
            @('Mac password', 'fixture-password')
        )) {
            $edit = Find-FixtureElement $entry[0] ([Windows.Automation.ControlType]::Edit)
            $pattern = $edit.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern)
            $pattern.SetValue($entry[1])
        }
        $button = Find-FixtureElement 'Connect to Mac' ([Windows.Automation.ControlType]::Button)
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        while (-not $button.Current.IsEnabled -and [DateTime]::UtcNow -lt $deadline) {
            Start-Sleep -Milliseconds 50
            $button = Find-FixtureElement 'Connect to Mac' ([Windows.Automation.ControlType]::Button)
        }
        if (-not $button.Current.IsEnabled) { throw 'Connection form did not become ready after entering fixture credentials.' }
        $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'fullscreen' {
        $button = Find-FixtureElement 'Full screen' ([Windows.Automation.ControlType]::Button)
        $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'exit-fullscreen' {
        $button = Find-FixtureElement 'Exit full screen' ([Windows.Automation.ControlType]::Button)
        $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'disconnect' {
        $button = Find-FixtureElement 'Disconnect' ([Windows.Automation.ControlType]::Button)
        $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    'bounds' {
        $canvas = Find-FixtureElement 'Remote desktop' $null
        $bounds = $canvas.Current.BoundingRectangle
        @{ x = $bounds.X; y = $bounds.Y; width = $bounds.Width; height = $bounds.Height } | ConvertTo-Json -Compress
    }
}
