function Read-AutoPierCamIcoUInt16 {
    param([byte[]] $Bytes, [int] $Offset, [string] $Description)

    if ($Offset -lt 0 -or $Offset + 2 -gt $Bytes.Length) {
        throw "ICO $Description is outside the file."
    }
    return [BitConverter]::ToUInt16($Bytes, $Offset)
}

function Read-AutoPierCamIcoUInt32 {
    param([byte[]] $Bytes, [int] $Offset, [string] $Description)

    if ($Offset -lt 0 -or $Offset + 4 -gt $Bytes.Length) {
        throw "ICO $Description is outside the file."
    }
    return [BitConverter]::ToUInt32($Bytes, $Offset)
}

function Read-AutoPierCamPngUInt32 {
    param([byte[]] $Bytes, [int] $Offset, [string] $Description)

    if ($Offset -lt 0 -or $Offset + 4 -gt $Bytes.Length) {
        throw "PNG $Description is outside the file."
    }
    return [uint32] (
        ([uint32] $Bytes[$Offset] -shl 24) -bor
        ([uint32] $Bytes[$Offset + 1] -shl 16) -bor
        ([uint32] $Bytes[$Offset + 2] -shl 8) -bor
        [uint32] $Bytes[$Offset + 3]
    )
}

function Assert-AutoPierCamIconFile {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string] $Path,

        [string] $Description = 'AutoPierCam icon'
    )

    $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $bytes = [IO.File]::ReadAllBytes($resolvedPath)
    $expectedSizes = @(16, 20, 24, 32, 40, 48, 64, 128, 256)
    if ($bytes.Length -lt (6 + 16 * $expectedSizes.Count)) {
        throw "$Description is too small to contain the expected ICO directory: $resolvedPath"
    }
    $reserved = Read-AutoPierCamIcoUInt16 $bytes 0 'reserved word'
    $type = Read-AutoPierCamIcoUInt16 $bytes 2 'type word'
    $count = Read-AutoPierCamIcoUInt16 $bytes 4 'image count'
    if ($reserved -ne 0 -or $type -ne 1 -or $count -ne $expectedSizes.Count) {
        throw "$Description has an invalid ICO header: reserved=$reserved, type=$type, count=$count."
    }

    $nextPayloadOffset = 6 + (16 * $count)
    for ($index = 0; $index -lt $count; $index++) {
        $entry = 6 + (16 * $index)
        $width = if ($bytes[$entry] -eq 0) { 256 } else { [int] $bytes[$entry] }
        $height = if ($bytes[$entry + 1] -eq 0) { 256 } else { [int] $bytes[$entry + 1] }
        $colors = $bytes[$entry + 2]
        $entryReserved = $bytes[$entry + 3]
        $planes = Read-AutoPierCamIcoUInt16 $bytes ($entry + 4) "frame $index planes"
        $bitCount = Read-AutoPierCamIcoUInt16 $bytes ($entry + 6) "frame $index bit count"
        $length = Read-AutoPierCamIcoUInt32 $bytes ($entry + 8) "frame $index length"
        $offset = Read-AutoPierCamIcoUInt32 $bytes ($entry + 12) "frame $index offset"
        $expectedSize = $expectedSizes[$index]
        if (
            $width -ne $expectedSize -or
            $height -ne $expectedSize -or
            $colors -ne 0 -or
            $entryReserved -ne 0 -or
            $planes -ne 1 -or
            $bitCount -ne 32
        ) {
            throw ("$Description frame $index is invalid: ${width}x${height}, " +
                   "colors=$colors, reserved=$entryReserved, planes=$planes, bits=$bitCount.")
        }
        if ($offset -ne $nextPayloadOffset -or $length -lt 33 -or $offset + $length -gt $bytes.Length) {
            throw "$Description frame $index has an invalid or non-contiguous payload range."
        }

        $pngSignature = @(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
        for ($signatureIndex = 0; $signatureIndex -lt $pngSignature.Count; $signatureIndex++) {
            if ($bytes[$offset + $signatureIndex] -ne $pngSignature[$signatureIndex]) {
                throw "$Description frame $index is not PNG-compressed."
            }
        }
        $ihdrLength = Read-AutoPierCamPngUInt32 $bytes ($offset + 8) "frame $index IHDR length"
        $ihdrType = [Text.Encoding]::ASCII.GetString($bytes, [int] ($offset + 12), 4)
        $pngWidth = Read-AutoPierCamPngUInt32 $bytes ($offset + 16) "frame $index width"
        $pngHeight = Read-AutoPierCamPngUInt32 $bytes ($offset + 20) "frame $index height"
        $pngBitDepth = $bytes[$offset + 24]
        $pngColorType = $bytes[$offset + 25]
        if (
            $ihdrLength -ne 13 -or
            $ihdrType -cne 'IHDR' -or
            $pngWidth -ne $expectedSize -or
            $pngHeight -ne $expectedSize -or
            $pngBitDepth -ne 8 -or
            $pngColorType -ne 6
        ) {
            throw "$Description frame $index is not an exact ${expectedSize}px 8-bit RGBA PNG."
        }
        $nextPayloadOffset += $length
    }
    if ($nextPayloadOffset -ne $bytes.Length) {
        throw "$Description contains unreferenced bytes after the final ICO frame."
    }

    Write-Host "$Description contains the exact nine-frame 32-bit icon matrix."
}

function Assert-AutoPierCamIconMatch {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [string] $ExpectedPath,
        [string] $Description = 'AutoPierCam icon copy'
    )

    Assert-AutoPierCamIconFile -Path $Path -Description $Description
    $actualHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    $expectedHash = (Get-FileHash -LiteralPath $ExpectedPath -Algorithm SHA256).Hash
    if ($actualHash -cne $expectedHash) {
        throw "$Description does not match the canonical icon at $ExpectedPath."
    }
}

if ($null -eq ('AutoPierCam.NativeIconResources' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace AutoPierCam {
    public static class NativeIconResources {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern IntPtr LoadLibraryEx(
            string fileName,
            IntPtr file,
            uint flags);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool FreeLibrary(IntPtr module);

        [DllImport("user32.dll", SetLastError = true)]
        public static extern IntPtr LoadImage(
            IntPtr instance,
            IntPtr name,
            uint type,
            int width,
            int height,
            uint loadFlags);

        [DllImport("user32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool DestroyIcon(IntPtr icon);
    }
}
'@
}

function Assert-AutoPierCamIconResource {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Path,
        [int] $ResourceId = 1,
        [string] $Description = 'AutoPierCam executable'
    )

    $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $module = [AutoPierCam.NativeIconResources]::LoadLibraryEx(
        $resolvedPath,
        [IntPtr]::Zero,
        0x22)
    if ($module -eq [IntPtr]::Zero) {
        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "$Description could not be opened as a PE resource image (Win32 $errorCode)."
    }
    try {
        $icon = [AutoPierCam.NativeIconResources]::LoadImage(
            $module,
            [IntPtr]::new($ResourceId),
            1,
            32,
            32,
            0)
        if ($icon -eq [IntPtr]::Zero) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw "$Description is missing icon group resource $ResourceId (Win32 $errorCode)."
        }
        try {
            Write-Host "$Description exposes icon group resource $ResourceId at 32px."
        } finally {
            [AutoPierCam.NativeIconResources]::DestroyIcon($icon) | Out-Null
        }
    } finally {
        [AutoPierCam.NativeIconResources]::FreeLibrary($module) | Out-Null
    }
}

function Assert-AutoPierCamAssociatedIcon {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Path,
        [string] $Description = 'AutoPierCam executable'
    )

    Add-Type -AssemblyName System.Drawing.Common
    $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $icon = [Drawing.Icon]::ExtractAssociatedIcon($resolvedPath)
    if ($null -eq $icon -or $icon.Handle -eq [IntPtr]::Zero) {
        if ($null -ne $icon) {
            $icon.Dispose()
        }
        throw "$Description has no extractable Windows application icon."
    }
    try {
        if ($icon.Width -lt 16 -or $icon.Height -lt 16) {
            throw "$Description returned an undersized associated icon: $($icon.Width)x$($icon.Height)."
        }
        Write-Host "$Description has an extractable associated application icon."
    } finally {
        $icon.Dispose()
    }
}
