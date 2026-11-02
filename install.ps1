#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDirectory = '',
    [switch]$Uninstall,
    [switch]$NoLaunch,
    [switch]$NoShortcut
)

& {
    param($Options)
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'

    function Assert-MacCanvasPath([string]$Path) {
        if ($Path -notmatch '^[A-Za-z]:\\' -or $Path.Length -le 3 -or
            $Path.Substring(2).Contains(':') -or $Path -match '(^|[\\/])\.\.([\\/]|$)') {
            throw 'Choose an absolute local Windows application directory, not a root or network path.'
        }
        foreach ($component in [regex]::Split($Path.Substring(3), '[\\/]')) {
            if ($component -match '[<>:"|?*\x00-\x1f]' -or $component.EndsWith('.') -or
                $component.EndsWith(' ') -or $component.Split('.')[0] -match '^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])$') {
                throw 'The installation path contains an unsafe Windows component.'
            }
        }
        $full = [IO.Path]::GetFullPath($Path)
        $current = $full
        while ($null -ne $current) {
            if (Test-Path -LiteralPath $current) {
                if (((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw "Reparse points are not allowed in the installation path: $current"
                }
            }
            $parent = [IO.Directory]::GetParent($current)
            $current = if ($null -eq $parent) { $null } else { $parent.FullName }
        }
        return $full
    }

    function Assert-MacCanvasUrl([uri]$Uri) {
        $hosts = @('api.github.com', 'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com')
        if ($Uri.Scheme -cne 'https' -or $Uri.Port -ne 443 -or $Uri.UserInfo -ne '' -or $hosts -notcontains $Uri.DnsSafeHost) {
            throw 'Download URL is not on the HTTPS GitHub allowlist.'
        }
    }

    function New-MacCanvasClient {
        Add-Type -AssemblyName System.Net.Http
        $handler = [Net.Http.HttpClientHandler]::new()
        $handler.AllowAutoRedirect = $false
        $handler.SslProtocols = [Security.Authentication.SslProtocols]::Tls12
        $client = [Net.Http.HttpClient]::new($handler)
        $client.Timeout = [TimeSpan]::FromMinutes(5)
        $client.DefaultRequestHeaders.UserAgent.ParseAdd('MacCanvas-installer')
        return $client
    }

    function Receive-MacCanvasBytes($Client, [uri]$Uri, [long]$Maximum) {
        for ($redirect = 0; $redirect -le 5; $redirect++) {
            Assert-MacCanvasUrl $Uri
            $response = $Client.GetAsync($Uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            try {
                if ([int]$response.StatusCode -in @(301, 302, 303, 307, 308)) {
                    if ($null -eq $response.Headers.Location) { throw 'Download redirect has no destination.' }
                    $Uri = [uri]::new($Uri, $response.Headers.Location)
                    continue
                }
                $null = $response.EnsureSuccessStatusCode()
                if ($null -ne $response.Content.Headers.ContentLength -and $response.Content.Headers.ContentLength -gt $Maximum) {
                    throw 'Download exceeds its allowed size.'
                }
                $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                $memory = [IO.MemoryStream]::new()
                $timeout = [Threading.CancellationTokenSource]::new([TimeSpan]::FromMinutes(5))
                try {
                    $buffer = New-Object byte[] 65536
                    while (($count = $stream.ReadAsync($buffer, 0, $buffer.Length, $timeout.Token).GetAwaiter().GetResult()) -gt 0) {
                        if ($memory.Length + $count -gt $Maximum) { throw 'Download exceeds its allowed size.' }
                        $memory.Write($buffer, 0, $count)
                    }
                    return ,$memory.ToArray()
                } finally {
                    $timeout.Dispose()
                    $memory.Dispose()
                    $stream.Dispose()
                }
            } finally { $response.Dispose() }
        }
        throw 'Too many download redirects.'
    }

    function Get-MacCanvasAsset($Release, [string]$Name, [long]$Maximum) {
        $assets = @($Release.assets | Where-Object { $_.name -ceq $Name })
        if ($assets.Count -ne 1) { throw "Release must contain exactly one $Name asset." }
        $asset = $assets[0]
        $expected = "https://github.com/false00/maccanvas/releases/download/$($Release.tag_name)/$Name"
        if ($asset.browser_download_url -cne $expected -or [long]$asset.size -le 0 -or
            [long]$asset.size -gt $Maximum -or $asset.digest -notmatch '^sha256:[a-fA-F0-9]{64}$') {
            throw "Release asset $Name has an invalid source, size, or SHA-256 digest."
        }
        return $asset
    }

    function Get-MacCanvasHash([byte[]]$Bytes) {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return ([BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant() }
        finally { $sha.Dispose() }
    }

    function Get-MacCanvasChecksum([string]$Text, [string]$Name) {
        $matches = @()
        $pattern = '^([a-fA-F0-9]{64})[ \t]+\*?' + [regex]::Escape($Name) + '$'
        foreach ($line in ($Text -split '\r?\n')) {
            $match = [regex]::Match($line, $pattern)
            if ($match.Success) { $matches += $match.Groups[1].Value.ToLowerInvariant() }
        }
        if ($matches.Count -ne 1) { throw 'Exactly one executable checksum is required.' }
        return $matches[0]
    }

    function Assert-MacCanvasExecutable([byte[]]$Bytes) {
        if ($Bytes.Length -lt 256 -or $Bytes[0] -ne 0x4d -or $Bytes[1] -ne 0x5a) {
            throw 'Download is not a Windows executable.'
        }
        $offset = [BitConverter]::ToUInt32($Bytes, 0x3c)
        if ($offset -gt $Bytes.Length - 26 -or $Bytes[$offset] -ne 0x50 -or
            $Bytes[$offset + 1] -ne 0x45 -or $Bytes[$offset + 2] -ne 0 -or $Bytes[$offset + 3] -ne 0 -or
            [BitConverter]::ToUInt16($Bytes, $offset + 4) -ne 0x8664 -or
            [BitConverter]::ToUInt16($Bytes, $offset + 24) -ne 0x20b) {
            throw 'Download is not a Windows x64 PE32+ application.'
        }
    }

    function Read-MacCanvasReceipt([string]$Directory) {
        $path = Join-Path $Directory '.maccanvas-install.json'
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'This directory is not managed by the MacCanvas installer.' }
        $receipt = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
        if ($receipt.repository -cne 'false00/maccanvas' -or $receipt.sha256 -notmatch '^[a-fA-F0-9]{64}$') {
            throw 'The MacCanvas installation receipt is invalid.'
        }
        $executable = Join-Path $Directory 'MacCanvas.exe'
        if (-not (Test-Path -LiteralPath $executable -PathType Leaf) -or
            (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash -ine $receipt.sha256) {
            throw 'The installed executable was changed; refusing to overwrite or remove it.'
        }
        return $receipt
    }

    function Get-MacCanvasShortcutPaths {
        foreach ($folder in @('Programs', 'DesktopDirectory')) {
            $directory = [Environment]::GetFolderPath($folder)
            if ([string]::IsNullOrWhiteSpace($directory)) { throw "Windows could not locate the $folder shortcut folder." }
            Join-Path $directory 'MacCanvas.lnk'
        }
    }

    function Set-MacCanvasShortcuts([string]$Executable) {
        $paths = @(Get-MacCanvasShortcutPaths)
        $shell = New-Object -ComObject WScript.Shell
        foreach ($path in $paths) {
            if (Test-Path -LiteralPath $path) {
                if ($shell.CreateShortcut($path).TargetPath -ine $Executable) {
                    throw "Existing MacCanvas shortcut points elsewhere; it was not overwritten: $path"
                }
            }
        }
        foreach ($path in $paths) {
            $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
            $shortcut = $shell.CreateShortcut($path)
            $shortcut.TargetPath = $Executable
            $shortcut.WorkingDirectory = Split-Path -Parent $Executable
            $shortcut.Arguments = ''
            $shortcut.IconLocation = "$Executable,0"
            $shortcut.Description = 'Windows VNC viewer for macOS'
            $shortcut.Save()
        }
    }

    function Remove-MacCanvasShortcuts([string]$Executable) {
        $shell = New-Object -ComObject WScript.Shell
        foreach ($path in @(Get-MacCanvasShortcutPaths)) {
            if (Test-Path -LiteralPath $path) {
                if ($shell.CreateShortcut($path).TargetPath -ieq $Executable) { [IO.File]::Delete($path) }
            }
        }
    }

    function Install-MacCanvas([string]$Version, [string]$InstallDirectory, [switch]$NoLaunch, [switch]$NoShortcut) {
        if (-not [Environment]::Is64BitOperatingSystem -or
            [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE', 'Machine') -ine 'AMD64') {
            throw 'The release supports Windows x64 (Intel/AMD).'
        }
        $directory = Assert-MacCanvasPath $InstallDirectory
        $oldReceipt = $null
        if (Test-Path -LiteralPath $directory) {
            $items = @(Get-ChildItem -LiteralPath $directory -Force)
            if ($items.Count -gt 0) {
                if (@($items | Where-Object { $_.Name -notin @('MacCanvas.exe', '.maccanvas-install.json') }).Count -gt 0) {
                    throw 'Installation directory contains unrelated files.'
                }
                $oldReceipt = Read-MacCanvasReceipt $directory
            }
        }
        if ($Version -notmatch '^(latest|v?[0-9]+\.[0-9]+\.[0-9]+)$') { throw 'Version must be latest or vX.Y.Z.' }
        if ($Version -ne 'latest' -and -not $Version.StartsWith('v')) { $Version = "v$Version" }
        $client = New-MacCanvasClient
        try {
            $endpoint = if ($Version -eq 'latest') { 'latest' } else { "tags/$Version" }
            $metadata = Receive-MacCanvasBytes $client ([uri]"https://api.github.com/repos/false00/maccanvas/releases/$endpoint") 262144
            $release = [Text.Encoding]::UTF8.GetString($metadata) | ConvertFrom-Json
            if ($release.draft -or $release.prerelease -or $release.tag_name -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$' -or
                ($Version -ne 'latest' -and $release.tag_name -cne $Version)) {
                throw 'The installer requires a matching published stable release.'
            }
            $binary = Get-MacCanvasAsset $release 'MacCanvas-windows-x64.exe' 104857600
            $checksums = Get-MacCanvasAsset $release 'SHA256SUMS.txt' 16384
            $checksumBytes = Receive-MacCanvasBytes $client ([uri]$checksums.browser_download_url) 16384
            if ("sha256:$(Get-MacCanvasHash $checksumBytes)" -ine $checksums.digest) { throw 'Checksum file digest mismatch.' }
            $expected = Get-MacCanvasChecksum ([Text.Encoding]::UTF8.GetString($checksumBytes)) $binary.name
            if ("sha256:$expected" -ine $binary.digest) { throw 'Release executable digests disagree.' }
            $bytes = Receive-MacCanvasBytes $client ([uri]$binary.browser_download_url) 104857600
            if ($bytes.Length -ne [long]$binary.size -or (Get-MacCanvasHash $bytes) -ine $expected) {
                throw 'Executable SHA-256 or size mismatch. Nothing was installed.'
            }
            Assert-MacCanvasExecutable $bytes
        } finally { $client.Dispose() }

        $null = [IO.Directory]::CreateDirectory($directory)
        $exe = Join-Path $directory 'MacCanvas.exe'
        $receiptPath = Join-Path $directory '.maccanvas-install.json'
        $stage = Join-Path $directory ('.maccanvas-' + [guid]::NewGuid().ToString('N') + '.tmp')
        $stageReceipt = "$stage.json"
        $backup = "$stage.backup"
        $backupReceipt = "$stage.receipt-backup"
        $preserveBackup = $false
        try {
            [IO.File]::WriteAllBytes($stage, $bytes)
            $receipt = @{ repository = 'false00/maccanvas'; version = $release.tag_name; sha256 = $expected }
            # Older viewers reject receipt fields they do not recognize.
            if ([version]$release.tag_name.Substring(1) -ge [version]'0.3.1') { $receipt.shortcuts = -not $NoShortcut }
            [IO.File]::WriteAllText($stageReceipt, ($receipt | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
            if ($null -ne $oldReceipt) {
                [IO.File]::Replace($stage, $exe, $backup)
                try { [IO.File]::Replace($stageReceipt, $receiptPath, $backupReceipt) }
                catch {
                    try { [IO.File]::Copy($backup, $exe, $true) }
                    catch {
                        $preserveBackup = $true
                        throw "Installation rollback could not finish. The previous executable is preserved at $backup. Close MacCanvas before restoring it."
                    }
                    throw 'Installation failed; the previous executable was restored. Close MacCanvas and retry.'
                }
            } else {
                [IO.File]::Move($stage, $exe)
                try { [IO.File]::Move($stageReceipt, $receiptPath) }
                catch {
                    [IO.File]::Delete($exe)
                    throw 'Installation receipt failed; the new executable was removed.'
                }
            }
        } finally {
            foreach ($path in @($stage, $stageReceipt, $backup, $backupReceipt)) {
                if ($preserveBackup -and $path -eq $backup) { continue }
                if (Test-Path -LiteralPath $path -PathType Leaf) { [IO.File]::Delete($path) }
            }
        }
        if (-not $NoShortcut) {
            Set-MacCanvasShortcuts $exe
        }
        if (-not $NoLaunch) { Start-Process -FilePath $exe -WorkingDirectory $directory }
        return [pscustomobject]@{ Executable = $exe; Version = $release.tag_name }
    }

    function Uninstall-MacCanvas([string]$InstallDirectory) {
        $directory = Assert-MacCanvasPath $InstallDirectory
        $null = Read-MacCanvasReceipt $directory
        $exe = Join-Path $directory 'MacCanvas.exe'
        Remove-MacCanvasShortcuts $exe
        [IO.File]::Delete($exe)
        [IO.File]::Delete((Join-Path $directory '.maccanvas-install.json'))
        if (@(Get-ChildItem -LiteralPath $directory -Force).Count -eq 0) { [IO.Directory]::Delete($directory, $false) }
        Write-Output 'MacCanvas removed. Machine profiles and Windows credentials were preserved. Delete saved machines inside MacCanvas first if you want to remove their credentials.'
    }

    $directory = $Options.InstallDirectory
    if ([string]::IsNullOrWhiteSpace($directory)) { $directory = Join-Path $env:LOCALAPPDATA 'Programs\MacCanvas' }
    if ($Options.Uninstall) {
        Uninstall-MacCanvas $directory
    } else {
        Install-MacCanvas -Version $Options.Version -InstallDirectory $directory -NoLaunch:$Options.NoLaunch -NoShortcut:$Options.NoShortcut
    }
} @{
    Version = $Version
    InstallDirectory = $InstallDirectory
    Uninstall = $Uninstall
    NoLaunch = $NoLaunch
    NoShortcut = $NoShortcut
}
