#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Address,
    [string]$Application = '',
    [ValidateSet(30, 60)][int]$Fps = 60,
    [ValidateRange(1, 120)][int]$ObserveSeconds = 40,
    [ValidateRange(10, 240)][int]$HelloTimeoutSeconds = 240,
    [switch]$RequireAudio,
    [switch]$EnableAudio,
    [switch]$ExerciseSwitching,
    [switch]$KeepEvidence
)
$ErrorActionPreference = 'Stop'
if ($ExerciseSwitching -and $Fps -ne 60) { throw 'Switching validation starts with the default 60 FPS preset.' }
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
if (-not $Application) {
    $Application = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\x86_64-pc-windows-msvc\release\maccanvas.exe'
}
$applicationPath = (Resolve-Path -LiteralPath $Application).Path
$source = Join-Path $env:LOCALAPPDATA 'MacCanvas\settings.json'
$settings = Get-Content -LiteralPath $source -Raw | ConvertFrom-Json
$matching = @($settings.profiles | Where-Object { $_.host -ieq $Address })
if ($matching.Count -ne 1 -or -not $matching[0].has_saved_credentials -or $matching[0].authentication -cne 'mac_login') {
    throw 'Select an existing, uniquely matched Mac account profile with a saved login. This test never receives password values.'
}
$profile = $matching[0]
$profile.name = 'HEVC live validation'
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('maccanvas-live-hevc-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($temporary)
$fixture = @{
    version = 1; profiles = @($profile); sidebar_open = $true; show_fps = $true
    ctrl_as_command = $true; clipboard_enabled = $false
    audio = @{ muted = $true; volume = 15 }
    streaming = @{
        encoding = if ($Fps -eq 60) { 'hevc60' } else { 'hevc30' }
        compression = 6; jpeg_quality = 8; target_kbps = 0; max_fps = $Fps
    }
}
if ($ExerciseSwitching) { $fixture.Remove('streaming') }
$settingsPath = Join-Path $temporary 'settings.json'
$auditPath = Join-Path $temporary 'audit\log.json'
function Read-OwnedAudit {
    if (-not (Test-Path -LiteralPath $auditPath -PathType Leaf)) { return '' }
    $stream = [IO.File]::Open($auditPath, [IO.FileMode]::Open, [IO.FileAccess]::Read,
        ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::UTF8, $true, 4096, $true)
        try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally { $stream.Dispose() }
}
function Read-OwnedRecords {
    $text = Read-OwnedAudit
    $end = $text.LastIndexOf("`n")
    if ($end -lt 0) { return @() }
    return @($text.Substring(0, $end).Split("`n") | Where-Object { $_.Length -gt 0 } | ConvertFrom-Json)
}
function Assert-OwnedStatus {
    $names = @($window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition) |
        ForEach-Object { $_.Current.Name })
    $status = @($names | Where-Object { $_.StartsWith('Connected to ') })
    if ($status.Count -ne 1 -or $status[0].Contains([char]0xfffd) -or
        @($status[0].ToCharArray() | Where-Object { [char]::IsControl($_) }).Count -ne 0) {
        throw 'The connected status must render one valid, readable Unicode desktop name.'
    }
}
[IO.File]::WriteAllText($settingsPath, ($fixture | ConvertTo-Json -Depth 10))
$process = $null
$success = $false
$reason = 'The Mac did not produce a verified HEVC frame.'
try {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $applicationPath
    $start.Arguments = "--data-dir `"$temporary`""
    $start.UseShellExecute = $false
    $process = [Diagnostics.Process]::Start($start)
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    do {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
        if ($process.HasExited) { throw 'The owned viewer exited during startup.' }
    } while ($process.MainWindowTitle -cne 'MacCanvas' -and [DateTime]::UtcNow -lt $deadline)
    if ($process.MainWindowTitle -cne 'MacCanvas') { throw 'The owned viewer did not create its window.' }
    Start-Sleep -Seconds 3
    $owned = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ProcessIdProperty, $process.Id)
    $window = [Windows.Automation.AutomationElement]::RootElement.FindFirst([Windows.Automation.TreeScope]::Children, $owned)
    if ($null -eq $window) { throw 'The owned viewer is not exposed to Windows accessibility.' }
    $condition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Connect')
    $button = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
    if ($null -eq $button) { throw 'The saved-login Connect button was not found.' }
    $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    Write-Output 'Approve the normal Windows Hello request to test this saved Mac login. Passwords never enter the test harness.'
    $deadline = [DateTime]::UtcNow.AddSeconds($HelloTimeoutSeconds)
    $approvalObserved = $false
    do {
        Start-Sleep -Milliseconds 250
        $text = Read-OwnedAudit
        if (-not $approvalObserved -and $text.Contains('"event_name":"security.consent.completed"')) {
            $approvalObserved = $true
            $deadline = [DateTime]::UtcNow.AddSeconds(90)
            Write-Output 'Windows Hello completed; the separate Mac negotiation/decode timeout is now running.'
        }
        $success = $text.Contains('"event_name":"media.hevc.first_frame"')
        if ($text.Contains('"event_name":"session.connection.closed"')) { break }
    } while (-not $success -and [DateTime]::UtcNow -lt $deadline)
    if ($success) {
        $deadline = [DateTime]::UtcNow.AddSeconds($ObserveSeconds)
        do {
            Start-Sleep -Milliseconds 250
            if ((Read-OwnedAudit).Contains('"event_name":"session.connection.closed"')) {
                $success = $false
                $reason = 'The HEVC connection closed during sustained validation.'
                break
            }
        } while ([DateTime]::UtcNow -lt $deadline)
    }
    if ($success) {
        if ($RequireAudio -and -not $EnableAudio) {
            $deadline = [DateTime]::UtcNow.AddSeconds(30)
            while (-not (Read-OwnedAudit).Contains('"event_name":"media.audio.first_frame"')) {
                if ([DateTime]::UtcNow -ge $deadline) { throw 'No authenticated AAC decoded. Play audio on the Mac and enable the matching converter before requiring this check.' }
                Start-Sleep -Milliseconds 100
            }
            Write-Output 'Actual Mac -> authenticated AAC-ELD-SBR -> isolated decoder -> muted Windows playback: PASS'
        }
        Assert-OwnedStatus
        if ($ExerciseSwitching -or $EnableAudio) {
            $modes = @()
            if ($EnableAudio) { $modes += 'audio' }
            if ($ExerciseSwitching) { $modes += @('hevc30', 'auto', 'hevc60') }
            foreach ($mode in $modes) {
                $expectedEncoding = if ($mode -ceq 'audio') {
                    if ($Fps -eq 60) { 'hevc60' } else { 'hevc30' }
                } else { $mode }
                $before = @(Read-OwnedRecords).Count
                if ($mode -ceq 'audio') {
                    & (Join-Path $PSScriptRoot 'Gui-Fixture.ps1') -ProcessId $process.Id -Action enable-audio
                } else {
                    & (Join-Path $PSScriptRoot 'Gui-Fixture.ps1') -ProcessId $process.Id -Action switch-stream -StreamMode $mode
                }
                $success = $false
                $deadline = [DateTime]::UtcNow.AddSeconds($HelloTimeoutSeconds + $(if ($mode -ceq 'audio') { 90 } else { 0 }))
                $approvalObserved = $false
                $generation = $null
                do {
                    Start-Sleep -Milliseconds 250
                    $fresh = @(Read-OwnedRecords | Select-Object -Skip $before)
                    $requests = @($fresh | Where-Object event_name -CEQ 'session.connection.requested')
                    if ($requests.Count -gt 0) { $generation = $requests[0].attributes.'vnc.session.generation' }
                    $approval = @($fresh | Where-Object {
                        $_.event_name -ceq 'security.consent.completed' -and $_.attributes.'event.outcome' -ceq 'success'
                    })
                    if (-not $approvalObserved -and $approval.Count -gt 0) {
                        $approvalObserved = $true
                        $deadline = [DateTime]::UtcNow.AddSeconds(90)
                    }
                    $failed = @($fresh | Where-Object {
                        $_.event_name -ceq 'session.connection.closed' -and
                        $_.attributes.'vnc.session.generation' -eq $generation
                    })
                    if ($failed.Count -gt 0) { break }
                    $expected = if ($mode -ceq 'auto') { 'security.authentication.completed' } else { 'media.hevc.first_frame' }
                    $ready = @($fresh | Where-Object {
                        $_.event_name -ceq $expected -and $_.attributes.'vnc.session.generation' -eq $generation
                    })
                    $audioReady = -not $RequireAudio -or $mode -ceq 'auto' -or @($fresh | Where-Object {
                        $_.event_name -ceq 'media.audio.first_frame' -and
                        $_.attributes.'vnc.session.generation' -eq $generation
                    }).Count -eq 1
                    $success = $ready.Count -eq 1 -and $approval.Count -eq 1 -and $audioReady
                } while (-not $success -and [DateTime]::UtcNow -lt $deadline)
                if (-not $success) { throw "Automatic switch to $mode did not produce an authenticated stream." }
                $names = @($fresh | ForEach-Object event_name)
                $closed = [Array]::IndexOf($names, 'session.connection.closed')
                $requested = [Array]::IndexOf($names, 'session.connection.requested')
                $consented = [Array]::IndexOf($names, 'security.consent.completed')
                $vaultRead = [Array]::IndexOf($names, 'credential.read.requested')
                if ($closed -lt 0 -or $closed -ge $requested -or $consented -lt 0 -or $consented -ge $vaultRead -or
                    $requests.Count -ne 1 -or $requests[0].attributes.'profile.id' -cne $profile.id -or
                    $requests[0].attributes.'rfb.streaming'.encoding -cne $expectedEncoding) {
                    throw 'Automatic restart must close first, keep the same Mac, and obtain fresh Hello before reading the vault.'
                }
                Start-Sleep -Milliseconds 500
                Assert-OwnedStatus
                $deadline = [DateTime]::UtcNow.AddSeconds($ObserveSeconds)
                do {
                    Start-Sleep -Milliseconds 250
                    $closed = @(Read-OwnedRecords | Where-Object {
                        $_.event_name -ceq 'session.connection.closed' -and
                        $_.attributes.'vnc.session.generation' -eq $generation
                    })
                    if ($closed.Count -gt 0) { $success = $false; throw 'The restarted stream did not remain connected.' }
                } while ([DateTime]::UtcNow -lt $deadline)
                Write-Output "Reconnect -> old stream closed -> fresh Hello -> same Mac in $mode ($ObserveSeconds seconds sustained): PASS"
            }
        }
        Write-Output "Actual Mac -> encrypted HEVC -> native RGBA -> connected Windows UI ($Fps FPS preset, $ObserveSeconds seconds sustained): PASS"
    } else {
        Start-Sleep -Seconds 1
        $names = @($window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition) |
            ForEach-Object { $_.Current.Name })
        $errors = @($names | Where-Object { $_ -match '^VNC protocol error:|^Windows Hello:|^The Mac rejected|^Display:|^Connection error:' } | Select-Object -Unique)
        if ($errors.Count -gt 0) { $reason = $errors -join ' ' }
        Write-Output $reason
    }
} catch {
    $success = $false
    throw
} finally {
    if ($null -ne $process -and -not $process.HasExited) {
        $null = $process.CloseMainWindow()
        if (-not $process.WaitForExit(7000)) { Stop-Process -Id $process.Id -Force }
    }
    if ($success -and -not $KeepEvidence) {
        foreach ($file in @($settingsPath, $auditPath, (Join-Path $temporary 'audit\.audit.lock'))) {
            if (Test-Path -LiteralPath $file -PathType Leaf) { [IO.File]::Delete($file) }
        }
        foreach ($directory in @((Join-Path $temporary 'audit'), $temporary)) {
            if ((Test-Path -LiteralPath $directory -PathType Container) -and @(Get-ChildItem -LiteralPath $directory -Force).Count -eq 0) {
                [IO.Directory]::Delete($directory, $false)
            }
        }
    } else {
        Write-Output "Owned diagnostic evidence preserved at $temporary"
    }
}
if (-not $success) { throw $reason }
