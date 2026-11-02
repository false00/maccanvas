#Requires -Version 5.1
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$root = Split-Path -Parent $PSScriptRoot
$assets = Join-Path $root 'assets'
$null = [IO.Directory]::CreateDirectory($assets)
$images = @()
foreach ($size in @(16, 24, 32, 48, 64, 128, 256)) {
    $bitmap = [Drawing.Bitmap]::new($size, $size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.TextRenderingHint = [Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    $graphics.Clear([Drawing.Color]::Transparent)
    $path = [Drawing.Drawing2D.GraphicsPath]::new()
    $radius = [single]($size * 0.26)
    $edge = [single]($size - 1)
    $path.AddArc(0, 0, $radius, $radius, 180, 90)
    $path.AddArc(($edge - $radius), 0, $radius, $radius, 270, 90)
    $path.AddArc(($edge - $radius), ($edge - $radius), $radius, $radius, 0, 90)
    $path.AddArc(0, ($edge - $radius), $radius, $radius, 90, 90)
    $path.CloseFigure()
    $brush = [Drawing.SolidBrush]::new([Drawing.Color]::FromArgb(8, 126, 114))
    $graphics.FillPath($brush, $path)
    $font = [Drawing.Font]::new('Segoe UI', [single]($size * 0.34), [Drawing.FontStyle]::Bold, [Drawing.GraphicsUnit]::Pixel)
    $format = [Drawing.StringFormat]::new()
    $format.Alignment = [Drawing.StringAlignment]::Center
    $format.LineAlignment = [Drawing.StringAlignment]::Center
    $graphics.DrawString('MC', $font, [Drawing.Brushes]::White, [Drawing.RectangleF]::new(0, 0, $size, $size), $format)
    $memory = [IO.MemoryStream]::new()
    $bitmap.Save($memory, [Drawing.Imaging.ImageFormat]::Png)
    $images += [pscustomobject]@{ Size = $size; Bytes = $memory.ToArray() }
    if ($size -eq 256) { $bitmap.Save((Join-Path $assets 'icon.png'), [Drawing.Imaging.ImageFormat]::Png) }
    $memory.Dispose(); $format.Dispose(); $font.Dispose(); $brush.Dispose()
    $path.Dispose(); $graphics.Dispose(); $bitmap.Dispose()
}
$stream = [IO.File]::Create((Join-Path $assets 'icon.ico'))
$writer = [IO.BinaryWriter]::new($stream)
try {
    $writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$images.Count)
    $offset = 6 + 16 * $images.Count
    foreach ($image in $images) {
        $dimension = if ($image.Size -eq 256) { 0 } else { $image.Size }
        $writer.Write([byte]$dimension); $writer.Write([byte]$dimension)
        $writer.Write([byte]0); $writer.Write([byte]0)
        $writer.Write([uint16]1); $writer.Write([uint16]32)
        $writer.Write([uint32]$image.Bytes.Length); $writer.Write([uint32]$offset)
        $offset += $image.Bytes.Length
    }
    foreach ($image in $images) { $writer.Write([byte[]]$image.Bytes) }
} finally { $writer.Dispose(); $stream.Dispose() }
