param([string]$Ffmpeg = 'ffmpeg')

$ErrorActionPreference = 'Stop'
$crate = Split-Path $PSScriptRoot -Parent
$scratch = Join-Path $crate '.validation\fixtures'
$data = Join-Path $PSScriptRoot 'data'
New-Item -ItemType Directory -Force $scratch, $data | Out-Null

# These are original synthetic YCbCr samples, not extracted images or media.
$fixtures = @(
    @{ Name = 'pattern444'; Width = 64; Height = 48; Frames = 1; Shift = 0; BFrames = 0; Range = 'full' },
    @{ Name = 'pattern420'; Width = 64; Height = 48; Frames = 7; Shift = 1; BFrames = 2; Range = 'full' },
    @{ Name = 'cropped420'; Width = 62; Height = 46; Frames = 1; Shift = 1; BFrames = 0; Range = 'limited' },
    @{ Name = 'quadrants444_4k'; Width = 3840; Height = 2160; Frames = 1; Shift = 0; BFrames = 0; Range = 'full'; Quadrants = $true }
)

foreach ($fixture in $fixtures) {
    $raw = Join-Path $scratch ($fixture.Name + '.yuv')
    $stream = [System.IO.File]::Create($raw)
    try {
        for ($frame = 0; $frame -lt $fixture.Frames; $frame++) {
            for ($channel = 0; $channel -lt 3; $channel++) {
                if ($fixture.Quadrants) {
                    # Full-range BT.709 red, green, blue, white, rounded to YCbCr8.
                    $topLeft = @(54, 99, 255)
                    $topRight = @(182, 30, 12)
                    $bottomLeft = @(18, 255, 116)
                    $bottomRight = @(255, 128, 128)
                    $half = $fixture.Width / 2
                    $upperRow = [byte[]]((@($topLeft[$channel]) * $half) + (@($topRight[$channel]) * $half))
                    $lowerRow = [byte[]]((@($bottomLeft[$channel]) * $half) + (@($bottomRight[$channel]) * $half))
                    for ($row = 0; $row -lt $fixture.Height; $row++) {
                        $pixels = if ($row -lt $fixture.Height / 2) { $upperRow } else { $lowerRow }
                        $stream.Write($pixels, 0, $pixels.Length)
                    }
                    continue
                }
                $shift = if ($channel -eq 0) { 0 } else { $fixture.Shift }
                $width = $fixture.Width -shr $shift
                $height = $fixture.Height -shr $shift
                $plane = [byte[]]::new($width * $height)
                for ($row = 0; $row -lt $height; $row++) {
                    for ($column = 0; $column -lt $width; $column++) {
                        $x = $column -shl $shift
                        $y = $row -shl $shift
                        $value = switch ($channel) {
                            0 { ($x * 3 + $y * 5 + $frame * 17) % 256 }
                            1 { 64 + (([int][Math]::Floor($x / 8) * 29 + [int][Math]::Floor($y / 8) * 17 + $frame * 13) % 129) }
                            2 { 64 + (([int][Math]::Floor($x / 8) * 11 + [int][Math]::Floor($y / 8) * 31 + $frame * 19) % 129) }
                        }
                        $plane[$row * $width + $column] = [byte]$value
                    }
                }
                $stream.Write($plane, 0, $plane.Length)
            }
        }
    } finally {
        $stream.Dispose()
    }

    $format = if ($fixture.Shift -eq 0) { 'yuv444p' } else { 'yuv420p' }
    $profile = if ($fixture.Shift -eq 0) { 'main444-8' } else { 'main' }
    $parameters = 'lossless=1:colorprim=bt709:transfer=bt709:colormatrix=bt709:repeat-headers=1:aud=1:info=0:pools=none:wpp=0:frame-threads=1:log-level=error:b-adapt=0:open-gop=0'
    $keyint = if ($fixture.Quadrants) { 1 } else { 30 }
    $parameters += ":keyint=${keyint}:range=$($fixture.Range):bframes=$($fixture.BFrames)"
    try {
        & $Ffmpeg -hide_banner -loglevel error -nostdin -y -f rawvideo -pixel_format $format `
            -video_size "$($fixture.Width)x$($fixture.Height)" -framerate 25 -i $raw `
            -frames:v $fixture.Frames -c:v libx265 -preset medium -profile:v $profile `
            -x265-params $parameters -f hevc (Join-Path $data ($fixture.Name + '.hevc'))
        if ($LASTEXITCODE -ne 0) { throw "ffmpeg failed for $($fixture.Name)" }
    } finally {
        Remove-Item -LiteralPath $raw -ErrorAction SilentlyContinue
    }
}

Get-FileHash (Join-Path $data '*.hevc') -Algorithm SHA256 |
    Select-Object @{ Name = 'Fixture'; Expression = { Split-Path $_.Path -Leaf } }, Hash
