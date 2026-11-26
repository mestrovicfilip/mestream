# Draws the Mestream icon and puts a "Mestream" shortcut on the desktop.
# Run again after moving the project folder.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$icoPath = Join-Path $PSScriptRoot 'mestream.ico'
$pngPath = Join-Path $PSScriptRoot 'mestream.png'
$exe = Join-Path $root 'target\release\mestream.exe'

function New-RoundedRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    return $path
}

$size = 256
$bitmap = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.Clear([System.Drawing.Color]::Transparent)

# Green tile with a soft top-to-bottom gradient.
$tile = New-RoundedRect 8 8 240 240 52
$tileRect = New-Object System.Drawing.RectangleF(8, 8, 240, 240)
$green = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
    $tileRect,
    [System.Drawing.Color]::FromArgb(255, 32, 168, 32),
    [System.Drawing.Color]::FromArgb(255, 12, 104, 12),
    90.0)
$g.FillPath($green, $tile)

# White controller: a rounded body with two grips.
$white = [System.Drawing.Brushes]::White
$body = New-RoundedRect 48 88 160 76 34
$g.FillPath($white, $body)
$g.FillEllipse($white, 40, 104, 70, 96)
$g.FillEllipse($white, 146, 104, 70, 96)

# D-pad and face buttons, cut out in green.
$dark = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 16, 124, 16))
$g.FillRectangle($dark, 78, 108, 12, 40)
$g.FillRectangle($dark, 64, 122, 40, 12)
$g.FillEllipse($dark, 152, 104, 18, 18)
$g.FillEllipse($dark, 172, 122, 18, 18)
$g.FillEllipse($dark, 134, 122, 18, 18)
$g.FillEllipse($dark, 152, 140, 18, 18)
$g.Dispose()

$bitmap.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png)
$bitmap.Dispose()

# Wrap the PNG in an .ico container (one 256x256 entry).
$png = [System.IO.File]::ReadAllBytes($pngPath)
$stream = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter($stream)
$writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]1)
$writer.Write([byte]0); $writer.Write([byte]0); $writer.Write([byte]0); $writer.Write([byte]0)
$writer.Write([uint16]1); $writer.Write([uint16]32)
$writer.Write([uint32]$png.Length); $writer.Write([uint32]22)
$writer.Write($png)
$writer.Flush()
[System.IO.File]::WriteAllBytes($icoPath, $stream.ToArray())
$writer.Dispose()

$desktop = [Environment]::GetFolderPath('Desktop')
$linkPath = Join-Path $desktop 'Mestream.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($linkPath)
$link.TargetPath = $exe
$link.Arguments = ''
$link.WorkingDirectory = $root
$link.IconLocation = "$icoPath,0"
$link.Description = 'Xbox remote play'
# Start the helper console minimised; Mestream lets go of it at once.
$link.WindowStyle = 7
$link.Save()

"icon:     $icoPath"
"shortcut: $linkPath"
"target:   $exe (exists: $(Test-Path $exe))"
