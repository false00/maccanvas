#Requires -Version 5.1
param(
    [Parameter(Mandatory = $true)][int]$ProcessId,
    [int]$Port,
    [ValidateSet('connect', 'bounds', 'fullscreen', 'exit-fullscreen', 'disconnect', 'focus', 'toggle-sidebar', 'telemetry', 'quality', 'manual-quality', 'close-quality', 'show-macs', 'new', 'saved-login-check')][string]$Action = 'bounds'
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$process = Get-Process -Id $ProcessId
$window = [Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle)
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
