function Assert-AutoPierCamPeRange {
    param(
        [long] $Length,
        [long] $Offset,
        [long] $Size,
        [string] $Description
    )

    if ($Offset -lt 0 -or $Size -lt 0 -or $Offset -gt ($Length - $Size)) {
        throw "Invalid PE range for ${Description}: offset=$Offset, size=$Size, length=$Length."
    }
}

function Read-AutoPierCamPeUInt16 {
    param(
        [byte[]] $Bytes,
        [long] $Offset,
        [string] $Description
    )

    Assert-AutoPierCamPeRange $Bytes.LongLength $Offset 2 $Description
    return [BitConverter]::ToUInt16($Bytes, [int] $Offset)
}

function Read-AutoPierCamPeUInt32 {
    param(
        [byte[]] $Bytes,
        [long] $Offset,
        [string] $Description
    )

    Assert-AutoPierCamPeRange $Bytes.LongLength $Offset 4 $Description
    return [BitConverter]::ToUInt32($Bytes, [int] $Offset)
}

function Convert-AutoPierCamPeRvaToOffset {
    param(
        [uint32] $Rva,
        [object[]] $Sections,
        [uint32] $SizeOfHeaders,
        [long] $FileLength,
        [string] $Description
    )

    if ([uint64] $Rva -lt [uint64] $SizeOfHeaders) {
        Assert-AutoPierCamPeRange $FileLength ([long] $Rva) 1 $Description
        return [long] $Rva
    }

    foreach ($section in $Sections) {
        $sectionStart = [uint64] $section.VirtualAddress
        $sectionSpan = [Math]::Max(
            [uint64] $section.VirtualSize,
            [uint64] $section.SizeOfRawData)
        $sectionEnd = $sectionStart + $sectionSpan
        if ([uint64] $Rva -lt $sectionStart -or [uint64] $Rva -ge $sectionEnd) {
            continue
        }

        $delta = [uint64] $Rva - $sectionStart
        if ($delta -ge [uint64] $section.SizeOfRawData) {
            throw "$Description points into zero-filled PE section data."
        }
        $fileOffset = [uint64] $section.PointerToRawData + $delta
        if ($fileOffset -gt [uint64] [int]::MaxValue) {
            throw "$Description has an unsupported file offset: $fileOffset."
        }
        Assert-AutoPierCamPeRange $FileLength ([long] $fileOffset) 1 $Description
        return [long] $fileOffset
    }

    throw ("$Description uses RVA 0x{0:X8}, which is not mapped by a PE section." -f $Rva)
}

function Read-AutoPierCamPeAsciiZ {
    param(
        [byte[]] $Bytes,
        [long] $Offset,
        [int] $MaximumLength,
        [string] $Description
    )

    Assert-AutoPierCamPeRange $Bytes.LongLength $Offset 1 $Description
    $characters = [Collections.Generic.List[byte]]::new()
    for ($index = 0; $index -lt $MaximumLength; $index++) {
        $currentOffset = $Offset + $index
        Assert-AutoPierCamPeRange $Bytes.LongLength $currentOffset 1 $Description
        $value = $Bytes[[int] $currentOffset]
        if ($value -eq 0) {
            if ($characters.Count -eq 0) {
                throw "$Description is empty."
            }
            return [Text.Encoding]::ASCII.GetString($characters.ToArray())
        }
        if ($value -lt 0x20 -or $value -gt 0x7e) {
            throw "$Description contains a non-printable byte."
        }
        $characters.Add($value)
    }

    throw "$Description is not NUL-terminated within $MaximumLength bytes."
}

function Get-AutoPierCamPeImports {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $file = Get-Item -LiteralPath $resolvedPath
    if ($file.Length -lt 64 -or $file.Length -gt 512MB) {
        throw "PE file has an unsupported size: $resolvedPath ($($file.Length) bytes)."
    }
    $bytes = [IO.File]::ReadAllBytes($resolvedPath)

    if (
        $bytes[0] -ne 0x4d -or
        $bytes[1] -ne 0x5a
    ) {
        throw "PE file is missing the DOS MZ signature: $resolvedPath"
    }
    $peOffset = [long](Read-AutoPierCamPeUInt32 $bytes 0x3c 'PE header offset')
    $signature = Read-AutoPierCamPeUInt32 $bytes $peOffset 'PE signature'
    if ($signature -ne 0x00004550) {
        throw "PE file has an invalid PE signature: $resolvedPath"
    }

    $coffOffset = $peOffset + 4
    $machine = Read-AutoPierCamPeUInt16 $bytes $coffOffset 'COFF machine'
    if ($machine -ne 0x8664) {
        throw ("Expected an x86-64 PE image; $resolvedPath has machine 0x{0:X4}." -f $machine)
    }
    $numberOfSections = Read-AutoPierCamPeUInt16 $bytes ($coffOffset + 2) 'COFF section count'
    if ($numberOfSections -lt 1 -or $numberOfSections -gt 96) {
        throw "PE file has an invalid section count: $numberOfSections."
    }
    $sizeOfOptionalHeader = Read-AutoPierCamPeUInt16 `
        $bytes `
        ($coffOffset + 16) `
        'COFF optional-header size'
    $optionalOffset = $coffOffset + 20
    Assert-AutoPierCamPeRange `
        $bytes.LongLength `
        $optionalOffset `
        $sizeOfOptionalHeader `
        'PE optional header'
    $optionalMagic = Read-AutoPierCamPeUInt16 $bytes $optionalOffset 'PE optional-header magic'
    if ($optionalMagic -ne 0x20b) {
        throw ("Expected a PE32+ image; $resolvedPath has optional-header magic 0x{0:X4}." -f $optionalMagic)
    }
    if ($sizeOfOptionalHeader -lt 128) {
        throw "PE32+ optional header is too small to contain the import directory."
    }

    $sizeOfHeaders = Read-AutoPierCamPeUInt32 `
        $bytes `
        ($optionalOffset + 60) `
        'PE SizeOfHeaders'
    $numberOfDirectories = Read-AutoPierCamPeUInt32 `
        $bytes `
        ($optionalOffset + 108) `
        'PE NumberOfRvaAndSizes'
    if ($numberOfDirectories -lt 2) {
        throw "PE image does not declare an import data directory."
    }
    $importRva = Read-AutoPierCamPeUInt32 `
        $bytes `
        ($optionalOffset + 120) `
        'PE import-directory RVA'
    $importSize = Read-AutoPierCamPeUInt32 `
        $bytes `
        ($optionalOffset + 124) `
        'PE import-directory size'
    if ($importRva -eq 0 -or $importSize -lt 20) {
        throw "PE image has no usable import directory: $resolvedPath"
    }

    $sectionTableOffset = $optionalOffset + $sizeOfOptionalHeader
    Assert-AutoPierCamPeRange `
        $bytes.LongLength `
        $sectionTableOffset `
        ([long] $numberOfSections * 40) `
        'PE section table'
    $sections = [Collections.Generic.List[object]]::new()
    for ($index = 0; $index -lt $numberOfSections; $index++) {
        $sectionOffset = $sectionTableOffset + ($index * 40)
        $sections.Add([pscustomobject] @{
            VirtualSize = Read-AutoPierCamPeUInt32 `
                $bytes ($sectionOffset + 8) "section $index virtual size"
            VirtualAddress = Read-AutoPierCamPeUInt32 `
                $bytes ($sectionOffset + 12) "section $index virtual address"
            SizeOfRawData = Read-AutoPierCamPeUInt32 `
                $bytes ($sectionOffset + 16) "section $index raw size"
            PointerToRawData = Read-AutoPierCamPeUInt32 `
                $bytes ($sectionOffset + 20) "section $index raw pointer"
        })
    }

    $importOffset = Convert-AutoPierCamPeRvaToOffset `
        $importRva `
        $sections.ToArray() `
        $sizeOfHeaders `
        $bytes.LongLength `
        'PE import directory'
    Assert-AutoPierCamPeRange `
        $bytes.LongLength `
        $importOffset `
        ([long] $importSize) `
        'PE import directory'
    $importEnd = $importOffset + [long] $importSize
    $descriptorOffset = $importOffset
    $terminated = $false
    $imports = [Collections.Generic.List[string]]::new()
    while ($descriptorOffset + 20 -le $importEnd) {
        $originalFirstThunk = Read-AutoPierCamPeUInt32 `
            $bytes $descriptorOffset 'import OriginalFirstThunk'
        $timeDateStamp = Read-AutoPierCamPeUInt32 `
            $bytes ($descriptorOffset + 4) 'import TimeDateStamp'
        $forwarderChain = Read-AutoPierCamPeUInt32 `
            $bytes ($descriptorOffset + 8) 'import ForwarderChain'
        $nameRva = Read-AutoPierCamPeUInt32 `
            $bytes ($descriptorOffset + 12) 'import Name RVA'
        $firstThunk = Read-AutoPierCamPeUInt32 `
            $bytes ($descriptorOffset + 16) 'import FirstThunk'
        if (
            $originalFirstThunk -eq 0 -and
            $timeDateStamp -eq 0 -and
            $forwarderChain -eq 0 -and
            $nameRva -eq 0 -and
            $firstThunk -eq 0
        ) {
            $terminated = $true
            break
        }
        if ($nameRva -eq 0) {
            throw "PE import descriptor has no DLL-name RVA: $resolvedPath"
        }
        $nameOffset = Convert-AutoPierCamPeRvaToOffset `
            $nameRva `
            $sections.ToArray() `
            $sizeOfHeaders `
            $bytes.LongLength `
            'PE imported DLL name'
        $imports.Add((Read-AutoPierCamPeAsciiZ `
            $bytes $nameOffset 260 'PE imported DLL name'))
        $descriptorOffset += 20
    }
    if (-not $terminated) {
        throw "PE import descriptor table is not terminated within its declared size: $resolvedPath"
    }

    return @($imports | Sort-Object -Unique)
}

function Assert-AutoPierCamStaticCrt {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string] $Path,

        [Parameter(Mandatory)]
        [string] $Description
    )

    $imports = @(Get-AutoPierCamPeImports -Path $Path)
    $forbiddenImports = @(
        $imports | Where-Object {
            $_ -match '^(?i:VCRUNTIME.*\.DLL|MSVCP.*\.DLL|UCRTBASE\.DLL|API-MS-WIN-CRT-.*\.DLL)$'
        }
    )
    if ($forbiddenImports.Count -ne 0) {
        throw ("$Description dynamically imports the Microsoft C/C++ runtime: " +
               "$($forbiddenImports -join ', '). Build with crt-static before packaging.")
    }
    Write-Host "$Description static-CRT import check passed ($($imports.Count) imported DLLs inspected)."
}

function Assert-AutoPierCamVersionResource {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string] $Path,

        [Parameter(Mandatory)]
        [string] $Version,

        [Parameter(Mandatory)]
        [string] $FileDescription,

        [Parameter(Mandatory)]
        [string] $OriginalFilename
    )

    $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $versionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo($resolvedPath)
    $expectations = [ordered] @{
        FileVersion = "$Version.0"
        ProductVersion = $Version
        CompanyName = 'Yann Ramin'
        FileDescription = $FileDescription
        OriginalFilename = $OriginalFilename
        ProductName = 'AutoPierCam'
        LegalCopyright = 'Copyright (c) 2026 Yann Ramin. Licensed under Apache-2.0.'
    }
    foreach ($field in $expectations.Keys) {
        $actual = [string] $versionInfo.$field
        if ($actual -cne $expectations[$field]) {
            throw ("Windows version resource mismatch for $OriginalFilename ${field}: " +
                   "got '$actual'; expected '$($expectations[$field])'.")
        }
    }
}

function Assert-AutoPierCamApplicationManifest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string] $Path,

        [Parameter(Mandatory)]
        [string] $Version,

        [Parameter(Mandatory)]
        [string] $AssemblyName,

        [switch] $GraphicalShell
    )

    $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $manifest = [AutoPierCamPeResource]::ReadApplicationManifest($resolvedPath)
    $document = [xml] $manifest
    $identity = $document.SelectSingleNode(
        "/*[local-name()='assembly']/*[local-name()='assemblyIdentity']")
    if (
        $null -eq $identity -or
        $identity.GetAttribute('name') -cne $AssemblyName -or
        $identity.GetAttribute('version') -cne "$Version.0" -or
        $identity.GetAttribute('processorArchitecture') -cne 'amd64'
    ) {
        throw "Application-manifest identity mismatch for $resolvedPath"
    }
    foreach ($requiredText in @(
        '<requestedExecutionLevel level="asInvoker" uiAccess="false" />',
        '<longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>',
        '<supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}" />'
    )) {
        if (-not $manifest.Contains($requiredText)) {
            throw "Application manifest for $resolvedPath is missing: $requiredText"
        }
    }
    if (
        $manifest.Contains('level="requireAdministrator"') -or
        $manifest.Contains('level="highestAvailable"')
    ) {
        throw "Application manifest for $resolvedPath must remain asInvoker."
    }
    if ($GraphicalShell) {
        foreach ($requiredText in @(
            '>true/pm</dpiAware>',
            '>PerMonitorV2, PerMonitor</dpiAwareness>',
            'name="Microsoft.Windows.Common-Controls"',
            'version="6.0.0.0"'
        )) {
            if (-not $manifest.Contains($requiredText)) {
                throw "Graphical application manifest for $resolvedPath is missing: $requiredText"
            }
        }
    }
}
if (-not ('AutoPierCamPeResource' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class AutoPierCamPeResource
{
    private const uint LoadLibraryAsDataFile = 0x00000002;
    private const uint LoadLibraryAsImageResource = 0x00000020;
    private static readonly IntPtr ApplicationManifestResource = new IntPtr(1);
    private static readonly IntPtr ManifestResourceType = new IntPtr(24);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryEx(
        string fileName,
        IntPtr file,
        uint flags);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr FindResource(
        IntPtr module,
        IntPtr name,
        IntPtr type);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint SizeofResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LoadResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LockResource(IntPtr resourceData);

    [DllImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool FreeLibrary(IntPtr module);

    public static string ReadApplicationManifest(string path)
    {
        IntPtr module = LoadLibraryEx(
            path,
            IntPtr.Zero,
            LoadLibraryAsDataFile | LoadLibraryAsImageResource);
        if (module == IntPtr.Zero)
        {
            throw new Win32Exception(
                Marshal.GetLastWin32Error(),
                "Could not load the PE image as resource data.");
        }

        try
        {
            IntPtr resource = FindResource(
                module,
                ApplicationManifestResource,
                ManifestResourceType);
            if (resource == IntPtr.Zero)
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "The PE image has no RT_MANIFEST resource 1.");
            }

            uint size = SizeofResource(module, resource);
            if (size == 0 || size > 1024 * 1024)
            {
                throw new InvalidOperationException(
                    "The PE application manifest has an invalid size.");
            }

            IntPtr loaded = LoadResource(module, resource);
            IntPtr bytes = loaded == IntPtr.Zero ? IntPtr.Zero : LockResource(loaded);
            if (bytes == IntPtr.Zero)
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "Could not read the PE application manifest.");
            }

            var managed = new byte[size];
            Marshal.Copy(bytes, managed, 0, managed.Length);
            return Encoding.UTF8.GetString(managed).TrimEnd('\0');
        }
        finally
        {
            FreeLibrary(module);
        }
    }
}
'@
}
