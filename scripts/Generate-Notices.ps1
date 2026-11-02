#Requires -Version 5.1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
& cargo-about generate (Join-Path $PSScriptRoot 'licenses.hbs') --config (Join-Path $root 'about.toml') --manifest-path (Join-Path $root 'Cargo.toml') --target x86_64-pc-windows-msvc --locked --fail --output-file (Join-Path $root 'THIRD_PARTY_NOTICES.txt')
if ($LASTEXITCODE -ne 0) { throw 'Dependency license generation failed.' }
$path = Join-Path $root 'THIRD_PARTY_NOTICES.txt'
$text = [IO.File]::ReadAllText($path).Replace("`r`n", "`n")
$text = [regex]::Replace($text, '[ \t]+(?=\n)', '').TrimEnd() + "`n"
[IO.File]::WriteAllText($path, $text, [Text.UTF8Encoding]::new($false))
