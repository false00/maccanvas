#Requires -Version 5.1
param(
    [Parameter(Mandatory = $true)][int]$ProcessId,
    [int]$Port,
    [ValidateSet('connect', 'bounds', 'fullscreen', 'exit-fullscreen', 'disconnect', 'focus')][string]$Action = 'bounds'
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
    $element = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
    if ($null -eq $element) { throw "Test control was not found: $Name" }
    return $element
}
switch ($Action) {
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
