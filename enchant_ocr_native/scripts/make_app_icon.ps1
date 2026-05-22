param(
    [Parameter(Mandatory = $true)]
    [string]$Source,

    [string]$OutDir = ''
)

Add-Type -AssemblyName System.Drawing

$scriptRoot = if ($PSScriptRoot) {
    $PSScriptRoot
} else {
    Split-Path -Parent $MyInvocation.MyCommand.Path
}
if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $scriptRoot '..\assets'
}

$sourcePath = Resolve-Path -LiteralPath $Source
$outDirPath = Resolve-Path -LiteralPath $OutDir -ErrorAction SilentlyContinue
if (-not $outDirPath) {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $outDirPath = Resolve-Path -LiteralPath $OutDir
}

$pngPath = Join-Path $outDirPath 'app_icon.png'
$icoPath = Join-Path $outDirPath 'app_icon.ico'
$sizes = @(16, 24, 32, 48, 64, 128, 256)

function New-RoundedIconBitmap {
    param(
        [System.Drawing.Image]$Image,
        [int]$Size
    )

    $sourceSize = [Math]::Min($Image.Width, $Image.Height)
    $sourceX = [int](($Image.Width - $sourceSize) / 2)
    $sourceY = [int](($Image.Height - $sourceSize) / 2)
    $sourceRect = New-Object System.Drawing.Rectangle($sourceX, $sourceY, $sourceSize, $sourceSize)
    $destRect = New-Object System.Drawing.Rectangle(0, 0, $Size, $Size)

    $bitmap = New-Object System.Drawing.Bitmap($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $graphics.Clear([System.Drawing.Color]::Transparent)

    $radius = [Math]::Round($Size * 0.18)
    $diameter = $radius * 2
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc(0, 0, $diameter, $diameter, 180, 90)
    $path.AddArc($Size - $diameter, 0, $diameter, $diameter, 270, 90)
    $path.AddArc($Size - $diameter, $Size - $diameter, $diameter, $diameter, 0, 90)
    $path.AddArc(0, $Size - $diameter, $diameter, $diameter, 90, 90)
    $path.CloseFigure()
    $graphics.SetClip($path)
    $graphics.DrawImage($Image, $destRect, $sourceRect, [System.Drawing.GraphicsUnit]::Pixel)

    $path.Dispose()
    $graphics.Dispose()
    return $bitmap
}

function Get-PngBytes {
    param([System.Drawing.Bitmap]$Bitmap)
    $stream = New-Object System.IO.MemoryStream
    $Bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $stream.ToArray()
    $stream.Dispose()
    return $bytes
}

$sourceImage = [System.Drawing.Image]::FromFile($sourcePath)
try {
    $large = New-RoundedIconBitmap -Image $sourceImage -Size 1024
    try {
        $large.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $large.Dispose()
    }

    $entries = @()
    foreach ($size in $sizes) {
        $bitmap = New-RoundedIconBitmap -Image $sourceImage -Size $size
        try {
            $entries += [PSCustomObject]@{
                Size = $size
                Bytes = Get-PngBytes -Bitmap $bitmap
            }
        }
        finally {
            $bitmap.Dispose()
        }
    }

    $stream = [System.IO.File]::Create($icoPath)
    $writer = New-Object System.IO.BinaryWriter($stream)
    try {
        $writer.Write([UInt16]0)
        $writer.Write([UInt16]1)
        $writer.Write([UInt16]$entries.Count)

        $offset = 6 + ($entries.Count * 16)
        foreach ($entry in $entries) {
            $sizeByte = if ($entry.Size -ge 256) { 0 } else { [byte]$entry.Size }
            $writer.Write([byte]$sizeByte)
            $writer.Write([byte]$sizeByte)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([UInt16]1)
            $writer.Write([UInt16]32)
            $writer.Write([UInt32]$entry.Bytes.Length)
            $writer.Write([UInt32]$offset)
            $offset += $entry.Bytes.Length
        }

        foreach ($entry in $entries) {
            $writer.Write($entry.Bytes)
        }
    }
    finally {
        $writer.Dispose()
        $stream.Dispose()
    }
}
finally {
    $sourceImage.Dispose()
}

Write-Output "Wrote $pngPath"
Write-Output "Wrote $icoPath"
