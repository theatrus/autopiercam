[CmdletBinding()]
param(
    [string] $MsiPath = (Join-Path `
        $PSScriptRoot `
        '..\artifacts\installer\output\AutoPierCam-0.1.0-x64.msi')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$expectedUpgradeCode = '{CFB03866-5B27-42FD-8CC3-84E6F58DB343}'
$msiTimeoutSeconds = 180
$trayStartupTimeoutSeconds = 30
$trayShutdownTimeoutSeconds = 45
$cleanupQuiescenceTimeoutSeconds = 60
$script:msiTransactionTimedOut = $false
$script:msiTimeoutDescriptions = [Collections.Generic.List[string]]::new()

if ($null -eq ('AutoPierCam.InstallerTest.NativeDirectoryIdentity' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace AutoPierCam.InstallerTest
{
    public static class NativeDirectoryIdentity
    {
        private const uint OpenExisting = 3;
        private const uint FileFlagBackupSemantics = 0x02000000;
        private const uint FileFlagOpenReparsePoint = 0x00200000;
        private const uint FileAttributeReparsePoint = 0x00000400;
        private const uint FileShareRead = 0x00000001;
        private const uint FileShareWrite = 0x00000002;
        private const uint FileShareDelete = 0x00000004;

        [StructLayout(LayoutKind.Sequential)]
        private struct ByHandleFileInformation
        {
            public uint FileAttributes;
            public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
            public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
            public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
            public uint VolumeSerialNumber;
            public uint FileSizeHigh;
            public uint FileSizeLow;
            public uint NumberOfLinks;
            public uint FileIndexHigh;
            public uint FileIndexLow;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafeFileHandle CreateFileW(
            string fileName,
            uint desiredAccess,
            uint shareMode,
            IntPtr securityAttributes,
            uint creationDisposition,
            uint flagsAndAttributes,
            IntPtr templateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetFileInformationByHandle(
            SafeFileHandle file,
            out ByHandleFileInformation information);

        public static string Get(string path)
        {
            using (SafeFileHandle handle = CreateFileW(
                path,
                0,
                FileShareRead | FileShareWrite | FileShareDelete,
                IntPtr.Zero,
                OpenExisting,
                FileFlagBackupSemantics | FileFlagOpenReparsePoint,
                IntPtr.Zero))
            {
                if (handle.IsInvalid)
                {
                    throw new Win32Exception(
                        Marshal.GetLastWin32Error(),
                        "Could not open the directory for identity inspection.");
                }

                ByHandleFileInformation information;
                if (!GetFileInformationByHandle(handle, out information))
                {
                    throw new Win32Exception(
                        Marshal.GetLastWin32Error(),
                        "Could not read the directory identity.");
                }

                if ((information.FileAttributes & FileAttributeReparsePoint) != 0)
                {
                    throw new InvalidOperationException(
                        "Directory identity targets must not be reparse points.");
                }

                ulong fileIndex = ((ulong)information.FileIndexHigh << 32) |
                                  information.FileIndexLow;
                return information.VolumeSerialNumber.ToString("X8") + ":" +
                       fileIndex.ToString("X16");
            }
        }
    }
}
'@
}

function Get-DirectoryIdentity([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        throw "Directory identity target is missing: $Path"
    }
    return [AutoPierCam.InstallerTest.NativeDirectoryIdentity]::Get(
        [IO.Path]::GetFullPath($Path)
    )
}

function Assert-DirectoryIdentity(
    [string] $Path,
    [string] $ExpectedIdentity,
    [string] $Description
) {
    $actualIdentity = Get-DirectoryIdentity $Path
    if (-not [string]::Equals(
        $actualIdentity,
        $ExpectedIdentity,
        [StringComparison]::Ordinal
    )) {
        throw "$Description changed filesystem identity."
    }
}

function Get-DataTreeInventory([string] $Root) {
    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    if (-not (Test-Path -LiteralPath $resolvedRoot -PathType Container)) {
        throw "Cannot inventory a missing data root: $resolvedRoot"
    }
    # Open the root namespace entry without following a junction or symlink.
    # Get-DirectoryIdentity rejects a reparse-point root before enumeration.
    [void] (Get-DirectoryIdentity $resolvedRoot)
    $entries = [Collections.Generic.List[object]]::new()
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($resolvedRoot)
    while ($pending.Count -ne 0) {
        $directory = $pending.Pop()
        foreach ($entryPath in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
            $attributes = [IO.File]::GetAttributes($entryPath)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "The test-owned data tree contains a reparse point: $entryPath"
            }
            $relativePath = [IO.Path]::GetRelativePath(
                $resolvedRoot,
                $entryPath
            ).Replace('/', '\')
            if ([IO.Path]::IsPathRooted($relativePath) -or
                $relativePath.StartsWith('..\', [StringComparison]::Ordinal) -or
                $relativePath.Split('\') -contains '..' -or
                $relativePath.Split('\') -contains '.') {
                throw "The data inventory produced an unsafe relative path: $relativePath"
            }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) {
                $entries.Add([pscustomobject] @{
                    RelativePath = $relativePath
                    Type = 'directory'
                    Length = [long] -1
                    Sha256 = ''
                })
                $pending.Push($entryPath)
            } else {
                $file = [IO.FileInfo]::new($entryPath)
                $entries.Add([pscustomobject] @{
                    RelativePath = $relativePath
                    Type = 'file'
                    Length = [long] $file.Length
                    Sha256 = (Get-FileHash `
                        -LiteralPath $entryPath `
                        -Algorithm SHA256).Hash.ToLowerInvariant()
                })
            }
        }
    }
    return @($entries | Sort-Object RelativePath)
}

function Assert-DataTreeInventoryEqual(
    [object[]] $Expected,
    [object[]] $Actual,
    [string] $Description
) {
    if ($Expected.Count -ne $Actual.Count) {
        throw "$Description inventory count changed from $($Expected.Count) to $($Actual.Count)."
    }
    for ($index = 0; $index -lt $Expected.Count; $index++) {
        $expectedEntry = $Expected[$index]
        $actualEntry = $Actual[$index]
        if (-not [string]::Equals(
                [string] $expectedEntry.RelativePath,
                [string] $actualEntry.RelativePath,
                [StringComparison]::Ordinal
            ) -or
            -not [string]::Equals(
                [string] $expectedEntry.Type,
                [string] $actualEntry.Type,
                [StringComparison]::Ordinal
            ) -or
            [long] $expectedEntry.Length -ne [long] $actualEntry.Length -or
            -not [string]::Equals(
                [string] $expectedEntry.Sha256,
                [string] $actualEntry.Sha256,
                [StringComparison]::Ordinal
            )) {
            throw "$Description inventory changed at sorted entry $index."
        }
    }
}

function Get-StableDataTreeInventory([string] $Root) {
    $first = @(Get-DataTreeInventory $Root)
    Start-Sleep -Milliseconds 250
    $second = @(Get-DataTreeInventory $Root)
    Assert-DataTreeInventoryEqual `
        -Expected $first `
        -Actual $second `
        -Description 'Pre-archive data tree'
    return $second
}

function Get-MsiScalar(
    [object] $Database,
    [string] $Sql
) {
    $view = $null
    $record = $null
    try {
        $view = $Database.GetType().InvokeMember(
            'OpenView',
            [Reflection.BindingFlags]::InvokeMethod,
            $null,
            $Database,
            @($Sql)
        )
        $view.GetType().InvokeMember(
            'Execute',
            [Reflection.BindingFlags]::InvokeMethod,
            $null,
            $view,
            $null
        ) | Out-Null
        $record = $view.GetType().InvokeMember(
            'Fetch',
            [Reflection.BindingFlags]::InvokeMethod,
            $null,
            $view,
            $null
        )
        if ($null -eq $record) {
            return $null
        }
        return [string] $record.StringData(1)
    } finally {
        if ($null -ne $record) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        }
        if ($null -ne $view) {
            $view.GetType().InvokeMember(
                'Close',
                [Reflection.BindingFlags]::InvokeMethod,
                $null,
                $view,
                $null
            ) | Out-Null
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
        }
    }
}

function Get-MsiIdentity([string] $Path) {
    $installer = $null
    $database = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.GetType().InvokeMember(
            'OpenDatabase',
            [Reflection.BindingFlags]::InvokeMethod,
            $null,
            $installer,
            @($Path, 0)
        )
        $identity = [ordered] @{}
        foreach ($property in @(
            'ProductCode',
            'ProductName',
            'ProductVersion',
            'UpgradeCode',
            'ALLUSERS'
        )) {
            $identity[$property] = Get-MsiScalar `
                $database `
                "SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$property'"
        }
        return [pscustomobject] $identity
    } finally {
        if ($null -ne $database) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
        }
        if ($null -ne $installer) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
        }
    }
}

function Get-RelatedProductCodes([string] $UpgradeCode) {
    $installer = $null
    $relatedProducts = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $relatedProducts = $installer.GetType().InvokeMember(
            'RelatedProducts',
            [Reflection.BindingFlags]::GetProperty,
            $null,
            $installer,
            @($UpgradeCode)
        )
        $count = [int] $relatedProducts.GetType().InvokeMember(
            'Count',
            [Reflection.BindingFlags]::GetProperty,
            $null,
            $relatedProducts,
            $null
        )
        $codes = @()
        for ($index = 0; $index -lt $count; $index++) {
            $codes += [string] $relatedProducts.GetType().InvokeMember(
                'Item',
                [Reflection.BindingFlags]::GetProperty,
                $null,
                $relatedProducts,
                @($index)
            )
        }
        return $codes
    } finally {
        if ($null -ne $relatedProducts) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($relatedProducts) | Out-Null
        }
        if ($null -ne $installer) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
        }
    }
}

function Get-InstallerProductState([string] $ProductCode) {
    $installer = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        return [int] $installer.GetType().InvokeMember(
            'ProductState',
            [Reflection.BindingFlags]::GetProperty,
            $null,
            $installer,
            @($ProductCode)
        )
    } finally {
        if ($null -ne $installer) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
        }
    }
}

function Get-InstallerFeatureState(
    [string] $ProductCode,
    [string] $Feature
) {
    $installer = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        return [int] $installer.GetType().InvokeMember(
            'FeatureState',
            [Reflection.BindingFlags]::GetProperty,
            $null,
            $installer,
            @($ProductCode, $Feature)
        )
    } finally {
        if ($null -ne $installer) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
        }
    }
}

function Get-AutoPierCamArpEntries {
    $entries = @()
    $seenEntries = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($hive in @(
        [Microsoft.Win32.RegistryHive]::CurrentUser,
        [Microsoft.Win32.RegistryHive]::LocalMachine
    )) {
        foreach ($view in @(
            [Microsoft.Win32.RegistryView]::Registry64,
            [Microsoft.Win32.RegistryView]::Registry32
        )) {
            $baseKey = $null
            $uninstallKey = $null
            try {
                $baseKey = [Microsoft.Win32.RegistryKey]::OpenBaseKey($hive, $view)
                $uninstallKey = $baseKey.OpenSubKey(
                    'Software\Microsoft\Windows\CurrentVersion\Uninstall',
                    $false
                )
                if ($null -eq $uninstallKey) {
                    continue
                }
                foreach ($subKeyName in $uninstallKey.GetSubKeyNames()) {
                    $productKey = $null
                    try {
                        $productKey = $uninstallKey.OpenSubKey($subKeyName, $false)
                        if ($null -eq $productKey) {
                            continue
                        }
                        $displayName = [string] $productKey.GetValue('DisplayName', '')
                        if (-not [string]::Equals(
                            $displayName,
                            'AutoPierCam',
                            [StringComparison]::Ordinal
                        )) {
                            continue
                        }
                        # HKCU's Uninstall key is shared between the 32-bit
                        # and 64-bit registry views on supported Windows
                        # versions. Count one underlying ARP subkey once while
                        # still examining both views for fail-closed preflight.
                        $entryIdentity = '{0}|{1}' -f $hive, $subKeyName
                        if (-not $seenEntries.Add($entryIdentity)) {
                            continue
                        }
                        $entries += [pscustomobject] @{
                            Hive = [string] $hive
                            View = [string] $view
                            KeyName = $subKeyName
                            DisplayVersion = [string] $productKey.GetValue(
                                'DisplayVersion',
                                ''
                            )
                        }
                    } finally {
                        if ($null -ne $productKey) {
                            $productKey.Dispose()
                        }
                    }
                }
            } finally {
                if ($null -ne $uninstallKey) {
                    $uninstallKey.Dispose()
                }
                if ($null -ne $baseKey) {
                    $baseKey.Dispose()
                }
            }
        }
    }
    return $entries
}

function Get-AutoPierCamProcesses {
    return @(
        Get-Process `
            -Name 'autopiercam', 'autopiercam-tray' `
            -ErrorAction SilentlyContinue
    )
}

function Get-ProcessPath([Diagnostics.Process] $Process) {
    try {
        return [IO.Path]::GetFullPath($Process.Path)
    } catch {
        return $null
    }
}

function Assert-ExactTrayProcessAlive(
    [int] $ProcessId,
    [string] $ExpectedPath
) {
    $process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $process) {
        throw "The expected tray PID $ProcessId exited before the MSI transaction began."
    }
    if ($process.ProcessName -cne 'autopiercam-tray') {
        throw "PID $ProcessId was reused by an unexpected process before MSI startup."
    }
    $actualPath = Get-ProcessPath $process
    $expectedFullPath = [IO.Path]::GetFullPath($ExpectedPath)
    if ($null -eq $actualPath -or -not [string]::Equals(
        $actualPath,
        $expectedFullPath,
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw "Tray PID $ProcessId no longer runs the exact installed executable."
    }
    $allProcesses = @(Get-AutoPierCamProcesses)
    if ($allProcesses.Count -ne 1 -or $allProcesses[0].Id -ne $ProcessId) {
        throw 'The AutoPierCam process set changed before the MSI transaction began.'
    }
}

function Wait-ForCondition(
    [scriptblock] $Condition,
    [int] $TimeoutSeconds,
    [string] $FailureMessage
) {
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    while ($stopwatch.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        $result = & $Condition
        if ($result) {
            return $result
        }
        Start-Sleep -Milliseconds 250
    }
    throw $FailureMessage
}

function Wait-ForInstalledTray(
    [string] $ExpectedPath,
    [int] $TimeoutSeconds
) {
    $expectedFullPath = [IO.Path]::GetFullPath($ExpectedPath)
    $found = Wait-ForCondition -TimeoutSeconds $TimeoutSeconds -FailureMessage `
        "The installed tray did not remain running at $expectedFullPath." -Condition {
            $processes = @(Get-AutoPierCamProcesses)
            if ($processes.Count -eq 0) {
                return $false
            }
            if ($processes.Count -ne 1) {
                $description = $processes | ForEach-Object {
                    "PID=$($_.Id), name=$($_.ProcessName), path=$(Get-ProcessPath $_)"
                }
                throw "Unexpected AutoPierCam processes appeared: $($description -join '; ')"
            }
            $candidatePath = Get-ProcessPath $processes[0]
            if ($null -eq $candidatePath) {
                throw "Cannot verify the executable path for AutoPierCam PID $($processes[0].Id)."
            }
            if (-not [string]::Equals(
                $candidatePath,
                $expectedFullPath,
                [StringComparison]::OrdinalIgnoreCase
            )) {
                throw "Unexpected AutoPierCam executable is running: $candidatePath"
            }
            return $processes[0]
        }

    # Confirm that the asynchronous post-install process survives beyond the
    # msiexec return and its initial event-loop setup.
    Start-Sleep -Seconds 2
    $stable = Get-Process -Id $found.Id -ErrorAction SilentlyContinue
    if ($null -eq $stable) {
        throw "The installed tray PID $($found.Id) exited during its stability check."
    }
    $stablePath = Get-ProcessPath $stable
    if ($null -eq $stablePath -or -not [string]::Equals(
        $stablePath,
        $expectedFullPath,
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw "The stable tray PID $($stable.Id) does not run the expected executable."
    }
    return $stable
}

function Wait-ForNoAutoPierCamProcesses([int] $TimeoutSeconds) {
    [void] (Wait-ForCondition -TimeoutSeconds $TimeoutSeconds -FailureMessage `
        'An AutoPierCam process did not exit before the bounded timeout.' -Condition {
            @(Get-AutoPierCamProcesses).Count -eq 0
        })
}

function ConvertTo-QuotedProcessArgument([string] $Value) {
    if ($Value.Contains('"')) {
        throw 'A process argument unexpectedly contains a double quote.'
    }
    return '"' + $Value + '"'
}

function Invoke-BoundedProcess(
    [string] $FilePath,
    [string] $Arguments,
    [int] $TimeoutSeconds,
    [int[]] $SuccessExitCodes,
    [string] $Description,
    [switch] $WindowsInstallerTransaction
) {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = $Arguments
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "Could not start $Description."
        }
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            if ($WindowsInstallerTransaction) {
                $script:msiTransactionTimedOut = $true
                $script:msiTimeoutDescriptions.Add($Description)
            }
            # This is the exact child process created by this test. Stopping it
            # bounds the client wait. The Windows Installer service may still
            # own the transaction, so final cleanup must prove quiescence before
            # it changes or archives any test-owned state.
            try {
                $process.Kill()
                [void] $process.WaitForExit(5000)
            } catch {
                # Preserve the timeout as the primary failure.
            }
            throw "$Description exceeded its $TimeoutSeconds-second timeout."
        }
        $exitCode = $process.ExitCode
        if ($exitCode -notin $SuccessExitCodes) {
            throw "$Description failed with exit code $exitCode."
        }
        return $exitCode
    } finally {
        $process.Dispose()
    }
}

function Invoke-MsiOperation(
    [ValidateSet('Install', 'Uninstall')]
    [string] $Operation,
    [string] $Target,
    [string] $LogPath,
    [string[]] $Properties = @()
) {
    $verb = if ($Operation -ceq 'Install') { '/i' } else { '/x' }
    $argumentParts = @(
        $verb,
        (ConvertTo-QuotedProcessArgument $Target),
        '/qn',
        '/norestart',
        '/L*V',
        (ConvertTo-QuotedProcessArgument $LogPath)
    ) + $Properties
    $arguments = $argumentParts -join ' '
    Write-Host "$Operation AutoPierCam; verbose log: $LogPath"
    [void] (Invoke-BoundedProcess `
        -FilePath (Join-Path $env:SystemRoot 'System32\msiexec.exe') `
        -Arguments $arguments `
        -TimeoutSeconds $msiTimeoutSeconds `
        -SuccessExitCodes @(0, 3010) `
        -Description "$Operation transaction" `
        -WindowsInstallerTransaction)
}

function Invoke-GracefulTrayStop(
    [string] $CliPath,
    [int] $TimeoutSeconds
) {
    if (-not (Test-Path -LiteralPath $CliPath -PathType Leaf)) {
        throw "The installed shutdown CLI is missing: $CliPath"
    }
    [void] (Invoke-BoundedProcess `
        -FilePath $CliPath `
        -Arguments 'shutdown-agent --if-running --timeout-seconds 30' `
        -TimeoutSeconds $TimeoutSeconds `
        -SuccessExitCodes @(0) `
        -Description 'AutoPierCam graceful shutdown')
    Wait-ForNoAutoPierCamProcesses -TimeoutSeconds 10
}

function Get-RunValue([string] $RegistryPath) {
    if (-not (Test-Path -LiteralPath $RegistryPath)) {
        return $null
    }
    $registryKey = Get-Item -LiteralPath $RegistryPath
    return $registryKey.GetValue(
        'AutoPierCam',
        $null,
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
    )
}

function Assert-RunValue(
    [string] $RegistryPath,
    [AllowNull()]
    [object] $Expected
) {
    $actual = Get-RunValue $RegistryPath
    if ($null -eq $Expected) {
        if ($null -ne $actual) {
            throw "The opt-out installation unexpectedly created an AutoPierCam Run value: $actual"
        }
        return
    }
    $expectedText = [string] $Expected
    if ($null -eq $actual -or -not [string]::Equals(
        [string] $actual,
        $expectedText,
        [StringComparison]::Ordinal
    )) {
        $shown = if ($null -eq $actual) { '<missing>' } else { "'$actual'" }
        throw "AutoPierCam Run value mismatch: got $shown; expected '$expectedText'."
    }
}

function Assert-Shortcut(
    [string] $Path,
    [string] $ExpectedTarget,
    [AllowEmptyString()]
    [string] $ExpectedArguments
) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Installed Start Menu shortcut is missing: $Path"
    }
    $shell = $null
    $shortcut = $null
    try {
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut($Path)
        $target = [IO.Path]::GetFullPath([string] $shortcut.TargetPath)
        $expectedTargetPath = [IO.Path]::GetFullPath($ExpectedTarget)
        if (-not [string]::Equals(
            $target,
            $expectedTargetPath,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw "Shortcut target mismatch for $Path`: got '$target'."
        }
        $actualArguments = [string] $shortcut.Arguments
        if (-not [string]::Equals(
            $actualArguments,
            $ExpectedArguments,
            [StringComparison]::Ordinal
        )) {
            throw "Shortcut arguments do not exactly match the MSI contract for $Path."
        }
    } finally {
        if ($null -ne $shortcut) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) | Out-Null
        }
        if ($null -ne $shell) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null
        }
    }
}

function Assert-InstalledPayload(
    [string] $InstallDirectory,
    [string] $DataDirectory,
    [string] $StartMenuDirectory
) {
    foreach ($relativePath in @(
        'autopiercam.exe',
        'autopiercam-tray.exe',
        'ASICamera2.dll',
        'autopiercam.example.toml',
        'installation.md',
        'LICENSE-AutoPierCam.txt',
        'THIRD_PARTY_NOTICES.md',
        'licenses\Rust-Standard-Library-COPYRIGHT.html',
        'licenses\Rust-Third-Party-Licenses.md',
        'licenses\ZWO-ASI-SDK-license.txt',
        'Viewer\AutoPierCam.Viewer.exe',
        'Viewer\AutoPierCam.Viewer.dll',
        'Viewer\AutoPierCam.Viewer.deps.json',
        'Viewer\AutoPierCam.Viewer.runtimeconfig.json'
    )) {
        $path = Join-Path $InstallDirectory $relativePath
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Installed payload file is missing: $path"
        }
        if ((Get-Item -LiteralPath $path).Length -eq 0) {
            throw "Installed payload file is empty: $path"
        }
    }

    $configPath = Join-Path $DataDirectory 'autopiercam.toml'
    [void] (Wait-ForCondition -TimeoutSeconds 30 -FailureMessage `
        "The tray did not create its default configuration: $configPath" -Condition {
            Test-Path -LiteralPath $configPath -PathType Leaf
        })
    $configText = Get-Content -LiteralPath $configPath -Raw
    foreach ($section in @('[camera]', '[capture]', '[upload]', '[video]', '[api]')) {
        if (-not $configText.Contains($section)) {
            throw "Default configuration is missing section $section."
        }
    }

    $viewerShortcut = Join-Path $StartMenuDirectory 'AutoPierCam Viewer.lnk'
    $startShortcut = Join-Path $StartMenuDirectory 'Start AutoPierCam.lnk'
    Assert-Shortcut `
        -Path $viewerShortcut `
        -ExpectedTarget (Join-Path $InstallDirectory 'Viewer\AutoPierCam.Viewer.exe') `
        -ExpectedArguments ''
    $expectedStartArguments = '--config "{0}" --sdk "{1}"' -f `
        (Join-Path $DataDirectory 'autopiercam.toml'), `
        (Join-Path $InstallDirectory 'ASICamera2.dll')
    Assert-Shortcut `
        -Path $startShortcut `
        -ExpectedTarget (Join-Path $InstallDirectory 'autopiercam-tray.exe') `
        -ExpectedArguments $expectedStartArguments
}

function Assert-ProductRegistered(
    [string] $ProductCode,
    [string] $UpgradeCode,
    [string] $ExpectedVersion
) {
    $state = Get-InstallerProductState $ProductCode
    if ($state -ne 5) {
        throw "Windows Installer state for $ProductCode is $state; expected installed state 5."
    }
    $related = @(Get-RelatedProductCodes $UpgradeCode)
    if ($related.Count -ne 1 -or $related[0] -ine $ProductCode) {
        throw "Related-products registration mismatch: $($related -join ', ')"
    }
    $arpEntries = @(Get-AutoPierCamArpEntries)
    if ($arpEntries.Count -ne 1 -or
        -not [string]::Equals(
            $arpEntries[0].DisplayVersion,
            $ExpectedVersion,
            [StringComparison]::Ordinal
        )) {
        $description = $arpEntries | ForEach-Object {
            "$($_.Hive)/$($_.View)/$($_.KeyName), version=$($_.DisplayVersion)"
        }
        throw "AutoPierCam ARP registration mismatch: $($description -join '; ')"
    }
}

function Assert-InstalledFeatureStates(
    [string] $ProductCode,
    [bool] $StartAtSignInInstalled
) {
    # Windows Installer INSTALLSTATE_LOCAL is 3 and INSTALLSTATE_ABSENT is 2.
    $mainState = Get-InstallerFeatureState $ProductCode 'MainApplication'
    if ($mainState -ne 3) {
        throw "MainApplication feature state is $mainState; expected LOCAL (3)."
    }
    $expectedStartupState = if ($StartAtSignInInstalled) { 3 } else { 2 }
    $startupState = Get-InstallerFeatureState $ProductCode 'StartAtSignIn'
    if ($startupState -ne $expectedStartupState) {
        $expectedName = if ($StartAtSignInInstalled) { 'LOCAL (3)' } else { 'ABSENT (2)' }
        throw "StartAtSignIn feature state is $startupState; expected $expectedName."
    }
}

function Assert-GracefulStopLog([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "The running-tray uninstall log is missing: $Path"
    }
    $logText = Get-Content -LiteralPath $Path -Raw
    if ([string]::IsNullOrWhiteSpace($logText)) {
        throw "The running-tray uninstall log is empty: $Path"
    }
    $options = (
        [Text.RegularExpressions.RegexOptions]::IgnoreCase -bor
        [Text.RegularExpressions.RegexOptions]::Multiline -bor
        [Text.RegularExpressions.RegexOptions]::CultureInvariant
    )
    $gracefulStarts = [Regex]::Matches(
        $logText,
        '^[^\r\n]*Action\s+start\s+[^\r\n]*:\s*GracefulStopAutoPierCam\.\s*$',
        $options
    )
    $gracefulSuccesses = [Regex]::Matches(
        $logText,
        ('^[^\r\n]*Action\s+ended\s+[^\r\n]*:\s*' +
         'GracefulStopAutoPierCam\.\s*Return value 1\.\s*$'),
        $options
    )
    $installValidateStarts = [Regex]::Matches(
        $logText,
        '^[^\r\n]*Action\s+start\s+[^\r\n]*:\s*InstallValidate\.\s*$',
        $options
    )
    if ($gracefulStarts.Count -ne 1 -or
        $gracefulSuccesses.Count -ne 1 -or
        $installValidateStarts.Count -ne 1) {
        # Do not echo arbitrary verbose-log lines: MSI properties may contain
        # environment-specific values even though AutoPierCam stores no token.
        throw (
            'The uninstall log does not contain one unambiguous start, success, ' +
            'and InstallValidate record for the graceful-stop action.'
        )
    }
    if ($gracefulStarts[0].Index -ge $gracefulSuccesses[0].Index -or
        $gracefulSuccesses[0].Index -ge $installValidateStarts[0].Index) {
        throw 'The graceful-stop action did not complete successfully before InstallValidate.'
    }
}

function Test-WindowsInstallerInProgressMarker {
    $baseKey = $null
    $inProgressKey = $null
    try {
        $baseKey = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
            [Microsoft.Win32.RegistryHive]::LocalMachine,
            [Microsoft.Win32.RegistryView]::Registry64
        )
        $inProgressKey = $baseKey.OpenSubKey(
            'Software\Microsoft\Windows\CurrentVersion\Installer\InProgress',
            $false
        )
        return $null -ne $inProgressKey
    } finally {
        if ($null -ne $inProgressKey) {
            $inProgressKey.Dispose()
        }
        if ($null -ne $baseKey) {
            $baseKey.Dispose()
        }
    }
}

function Test-WindowsInstallerTransactionQuiescent {
    $mutex = $null
    $acquired = $false
    try {
        try {
            $mutex = [Threading.Mutex]::OpenExisting('Global\_MSIExecute')
        } catch [Threading.WaitHandleCannotBeOpenedException] {
            # No execution mutex exists. Still reject an Installer InProgress
            # marker so a damaged or transitioning service is never called safe.
            return -not (Test-WindowsInstallerInProgressMarker)
        }
        try {
            $acquired = $mutex.WaitOne(0)
        } catch [Threading.AbandonedMutexException] {
            $acquired = $true
        }
        if (-not $acquired) {
            return $false
        }
        return -not (Test-WindowsInstallerInProgressMarker)
    } finally {
        if ($acquired -and $null -ne $mutex) {
            $mutex.ReleaseMutex()
        }
        if ($null -ne $mutex) {
            $mutex.Dispose()
        }
    }
}

function Wait-ForWindowsInstallerQuiescence([int] $TimeoutSeconds) {
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $consecutiveSafeSamples = 0
    $lastInspectionError = $null
    while ($stopwatch.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        try {
            if (Test-WindowsInstallerTransactionQuiescent) {
                $consecutiveSafeSamples++
                if ($consecutiveSafeSamples -ge 2) {
                    return
                }
            } else {
                $consecutiveSafeSamples = 0
            }
            $lastInspectionError = $null
        } catch {
            $consecutiveSafeSamples = 0
            $lastInspectionError = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 500
    }
    $suffix = if ([string]::IsNullOrWhiteSpace($lastInspectionError)) {
        ''
    } else {
        " Last inspection error: $lastInspectionError"
    }
    throw "Windows Installer did not become verifiably quiescent within $TimeoutSeconds seconds.$suffix"
}

function Get-PostCleanupProblems(
    [string] $ProductCode,
    [string] $UpgradeCode,
    [string] $InstallDirectory,
    [string] $DataDirectory,
    [string] $StartMenuDirectory,
    [string] $RunRegistryPath,
    [string] $InstallerRegistryPath,
    [string] $SentinelPath,
    [string] $SentinelContents
) {
    $problems = [Collections.Generic.List[string]]::new()
    if (-not (Test-WindowsInstallerTransactionQuiescent)) {
        $problems.Add('Windows Installer transaction is not quiescent')
    }
    if ((Get-InstallerProductState $ProductCode) -ne -1) {
        $problems.Add('candidate ProductCode is not unknown')
    }
    if (@(Get-RelatedProductCodes $UpgradeCode).Count -ne 0) {
        $problems.Add('a related product remains registered')
    }
    if (@(Get-AutoPierCamArpEntries).Count -ne 0) {
        $problems.Add('an AutoPierCam ARP entry remains')
    }
    if (Test-Path -LiteralPath $InstallDirectory) {
        $problems.Add('program directory remains')
    }
    if (Test-Path -LiteralPath $StartMenuDirectory) {
        $problems.Add('Start Menu directory remains')
    }
    if ($null -ne (Get-RunValue $RunRegistryPath)) {
        $problems.Add('AutoPierCam Run value remains')
    }
    if (Test-Path -LiteralPath $InstallerRegistryPath) {
        $problems.Add('AutoPierCam installer registry key remains')
    }
    if (@(Get-AutoPierCamProcesses).Count -ne 0) {
        $problems.Add('an AutoPierCam process remains')
    }
    if (-not (Test-Path -LiteralPath $DataDirectory -PathType Container)) {
        $problems.Add('test-owned user-data directory is missing')
    } elseif (-not (Test-Path -LiteralPath $SentinelPath -PathType Leaf)) {
        $problems.Add('test-owned sentinel is missing')
    } elseif (-not [string]::Equals(
        (Get-Content -LiteralPath $SentinelPath -Raw),
        $SentinelContents,
        [StringComparison]::Ordinal
    )) {
        $problems.Add('test-owned sentinel changed')
    }
    return $problems
}

function Wait-ForSafePostCleanupState(
    [int] $TimeoutSeconds,
    [string] $ProductCode,
    [string] $UpgradeCode,
    [string] $InstallDirectory,
    [string] $DataDirectory,
    [string] $StartMenuDirectory,
    [string] $RunRegistryPath,
    [string] $InstallerRegistryPath,
    [string] $SentinelPath,
    [string] $SentinelContents
) {
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $consecutiveSafeSamples = 0
    $lastProblems = @('post-cleanup state was not inspected')
    while ($stopwatch.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        try {
            $lastProblems = @(Get-PostCleanupProblems `
                -ProductCode $ProductCode `
                -UpgradeCode $UpgradeCode `
                -InstallDirectory $InstallDirectory `
                -DataDirectory $DataDirectory `
                -StartMenuDirectory $StartMenuDirectory `
                -RunRegistryPath $RunRegistryPath `
                -InstallerRegistryPath $InstallerRegistryPath `
                -SentinelPath $SentinelPath `
                -SentinelContents $SentinelContents)
        } catch {
            $lastProblems = @('post-cleanup state could not be inspected safely')
        }
        if ($lastProblems.Count -eq 0) {
            $consecutiveSafeSamples++
            if ($consecutiveSafeSamples -ge 2) {
                return
            }
        } else {
            $consecutiveSafeSamples = 0
        }
        Start-Sleep -Milliseconds 500
    }
    throw (
        "Safe post-uninstall state was not reached within $TimeoutSeconds seconds: " +
        ($lastProblems -join ', ')
    )
}

function Get-PostArchiveProblems(
    [string] $ProductCode,
    [string] $UpgradeCode,
    [string] $InstallDirectory,
    [string] $SourceDataDirectory,
    [string] $ArchivedDataDirectory,
    [string] $StartMenuDirectory,
    [string] $RunRegistryPath,
    [string] $InstallerRegistryPath,
    [string] $ExpectedDataIdentity,
    [string] $ArchivedSentinelPath,
    [string] $SentinelContents
) {
    $problems = [Collections.Generic.List[string]]::new()
    if (-not (Test-WindowsInstallerTransactionQuiescent)) {
        $problems.Add('Windows Installer transaction is not quiescent')
    }
    if ((Get-InstallerProductState $ProductCode) -ne -1) {
        $problems.Add('candidate ProductCode is not unknown')
    }
    if (@(Get-RelatedProductCodes $UpgradeCode).Count -ne 0) {
        $problems.Add('a related product remains registered')
    }
    if (@(Get-AutoPierCamArpEntries).Count -ne 0) {
        $problems.Add('an AutoPierCam ARP entry remains')
    }
    if (Test-Path -LiteralPath $InstallDirectory) {
        $problems.Add('program directory remains')
    }
    if (Test-Path -LiteralPath $StartMenuDirectory) {
        $problems.Add('Start Menu directory remains')
    }
    if ($null -ne (Get-RunValue $RunRegistryPath)) {
        $problems.Add('AutoPierCam Run value remains')
    }
    if (Test-Path -LiteralPath $InstallerRegistryPath) {
        $problems.Add('AutoPierCam installer registry key remains')
    }
    if (@(Get-AutoPierCamProcesses).Count -ne 0) {
        $problems.Add('an AutoPierCam process remains')
    }
    if (Test-Path -LiteralPath $SourceDataDirectory) {
        $problems.Add('source user-data path reappeared')
    }
    if (-not (Test-Path -LiteralPath $ArchivedDataDirectory -PathType Container)) {
        $problems.Add('archived user-data directory is missing')
    } elseif (-not [string]::Equals(
        (Get-DirectoryIdentity $ArchivedDataDirectory),
        $ExpectedDataIdentity,
        [StringComparison]::Ordinal
    )) {
        $problems.Add('archived user-data directory identity changed')
    }
    if (-not (Test-Path -LiteralPath $ArchivedSentinelPath -PathType Leaf)) {
        $problems.Add('archived sentinel is missing')
    } elseif (-not [string]::Equals(
        (Get-Content -LiteralPath $ArchivedSentinelPath -Raw),
        $SentinelContents,
        [StringComparison]::Ordinal
    )) {
        $problems.Add('archived sentinel changed')
    }
    return $problems
}

function Wait-ForStablePostArchiveState(
    [int] $TimeoutSeconds,
    [int] $RequiredConsecutiveSamples,
    [string] $ProductCode,
    [string] $UpgradeCode,
    [string] $InstallDirectory,
    [string] $SourceDataDirectory,
    [string] $ArchivedDataDirectory,
    [string] $StartMenuDirectory,
    [string] $RunRegistryPath,
    [string] $InstallerRegistryPath,
    [string] $ExpectedDataIdentity,
    [string] $ArchivedSentinelPath,
    [string] $SentinelContents
) {
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $consecutiveSafeSamples = 0
    $lastProblems = @('post-archive state was not inspected')
    while ($stopwatch.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        try {
            $lastProblems = @(Get-PostArchiveProblems `
                -ProductCode $ProductCode `
                -UpgradeCode $UpgradeCode `
                -InstallDirectory $InstallDirectory `
                -SourceDataDirectory $SourceDataDirectory `
                -ArchivedDataDirectory $ArchivedDataDirectory `
                -StartMenuDirectory $StartMenuDirectory `
                -RunRegistryPath $RunRegistryPath `
                -InstallerRegistryPath $InstallerRegistryPath `
                -ExpectedDataIdentity $ExpectedDataIdentity `
                -ArchivedSentinelPath $ArchivedSentinelPath `
                -SentinelContents $SentinelContents)
        } catch {
            $lastProblems = @('post-archive state could not be inspected safely')
        }
        if ($lastProblems.Count -eq 0) {
            $consecutiveSafeSamples++
            if ($consecutiveSafeSamples -ge $RequiredConsecutiveSamples) {
                return
            }
        } else {
            $consecutiveSafeSamples = 0
        }
        Start-Sleep -Milliseconds 500
    }
    throw (
        "Stable post-archive state was not reached within $TimeoutSeconds seconds: " +
        ($lastProblems -join ', ')
    )
}

function Assert-ProductUnregistered(
    [string] $ProductCode,
    [string] $UpgradeCode
) {
    $state = Get-InstallerProductState $ProductCode
    if ($state -ne -1) {
        throw "Windows Installer still reports product $ProductCode with state $state."
    }
    $related = @(Get-RelatedProductCodes $UpgradeCode)
    if ($related.Count -ne 0) {
        throw "Related AutoPierCam products remain registered: $($related -join ', ')"
    }
    $arpEntries = @(Get-AutoPierCamArpEntries)
    if ($arpEntries.Count -ne 0) {
        throw 'An AutoPierCam Add/Remove Programs registration remains after uninstall.'
    }
}

function Assert-UninstalledState(
    [string] $InstallDirectory,
    [string] $DataDirectory,
    [string] $StartMenuDirectory,
    [string] $RunRegistryPath,
    [string] $InstallerRegistryPath,
    [string] $SentinelPath,
    [string] $SentinelContents,
    [string] $ProductCode,
    [string] $UpgradeCode
) {
    if (Test-Path -LiteralPath $InstallDirectory) {
        throw "The program directory remains after uninstall: $InstallDirectory"
    }
    if (Test-Path -LiteralPath $StartMenuDirectory) {
        throw "The Start Menu directory remains after uninstall: $StartMenuDirectory"
    }
    Assert-RunValue -RegistryPath $RunRegistryPath -Expected $null
    if (Test-Path -LiteralPath $InstallerRegistryPath) {
        throw "The AutoPierCam installer registry key remains: $InstallerRegistryPath"
    }
    Assert-ProductUnregistered -ProductCode $ProductCode -UpgradeCode $UpgradeCode
    Wait-ForNoAutoPierCamProcesses -TimeoutSeconds 15

    if (-not (Test-Path -LiteralPath $DataDirectory -PathType Container)) {
        throw 'Uninstall removed the user-owned AutoPierCam data directory.'
    }
    if (-not (Test-Path -LiteralPath $SentinelPath -PathType Leaf)) {
        throw 'Uninstall removed the lifecycle-test sentinel from user data.'
    }
    $actualSentinel = Get-Content -LiteralPath $SentinelPath -Raw
    if (-not [string]::Equals(
        $actualSentinel,
        $SentinelContents,
        [StringComparison]::Ordinal
    )) {
        throw 'Uninstall changed the lifecycle-test sentinel in user data.'
    }
}

if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
    throw 'LOCALAPPDATA is required for the per-user lifecycle test.'
}
if ([string]::IsNullOrWhiteSpace($env:APPDATA)) {
    throw 'APPDATA is required for the per-user Start Menu lifecycle test.'
}
if ([string]::IsNullOrWhiteSpace($env:SystemRoot)) {
    throw 'SystemRoot is required to invoke Windows Installer.'
}

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path
if (-not (Test-Path -LiteralPath $resolvedMsiPath -PathType Leaf)) {
    throw "The MSI path is not a file: $resolvedMsiPath"
}
$msiIdentity = Get-MsiIdentity $resolvedMsiPath
if ($msiIdentity.ProductName -cne 'AutoPierCam') {
    throw "Refusing to test an MSI named '$($msiIdentity.ProductName)'."
}
if ($msiIdentity.ProductCode -notmatch '^\{[0-9A-Fa-f-]{36}\}$') {
    throw "The MSI ProductCode is malformed: $($msiIdentity.ProductCode)"
}
if ($msiIdentity.UpgradeCode -ine $expectedUpgradeCode) {
    throw "The MSI UpgradeCode is not AutoPierCam's pinned UpgradeCode."
}
if ($msiIdentity.ProductVersion -notmatch '^\d+\.\d+\.\d+$') {
    throw "The MSI ProductVersion is malformed: $($msiIdentity.ProductVersion)"
}
if ($null -ne $msiIdentity.ALLUSERS) {
    throw 'Refusing to exercise an MSI that is not fixed to per-user scope.'
}

$installDirectory = [IO.Path]::GetFullPath(
    (Join-Path $env:LOCALAPPDATA 'Programs\AutoPierCam')
).TrimEnd('\')
$dataDirectory = [IO.Path]::GetFullPath(
    (Join-Path $env:LOCALAPPDATA 'AutoPierCam')
).TrimEnd('\')
$startMenuDirectory = [IO.Path]::GetFullPath(
    (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\AutoPierCam')
).TrimEnd('\')
$runRegistryPath = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$installerRegistryPath = 'HKCU:\Software\AutoPierCam\Installer'
$trayPath = Join-Path $installDirectory 'autopiercam-tray.exe'
$cliPath = Join-Path $installDirectory 'autopiercam.exe'
$configPath = Join-Path $dataDirectory 'autopiercam.toml'
$sdkPath = Join-Path $installDirectory 'ASICamera2.dll'
$expectedRunValue = (
    '"{0}" --config "{1}" --sdk "{2}"' -f $trayPath, $configPath, $sdkPath
)

# Fail before claiming any user path. Exact named processes are rejected even
# when their executable paths cannot be inspected, so another session or build
# can never be mistaken for lifecycle-test state.
$preflightConflicts = @()
foreach ($path in @($installDirectory, $dataDirectory, $startMenuDirectory)) {
    if (Test-Path -LiteralPath $path) {
        $preflightConflicts += "path exists: $path"
    }
}
$existingRunValue = Get-RunValue $runRegistryPath
if ($null -ne $existingRunValue) {
    $preflightConflicts += "HKCU Run value exists: AutoPierCam=$existingRunValue"
}
if (Test-Path -LiteralPath $installerRegistryPath) {
    $preflightConflicts += "installer registry key exists: $installerRegistryPath"
}
$existingRelatedProducts = @(Get-RelatedProductCodes $msiIdentity.UpgradeCode)
if ($existingRelatedProducts.Count -ne 0) {
    $preflightConflicts += "related MSI product(s): $($existingRelatedProducts -join ', ')"
}
$existingCurrentState = Get-InstallerProductState $msiIdentity.ProductCode
if ($existingCurrentState -ne -1) {
    $preflightConflicts += (
        "candidate ProductCode $($msiIdentity.ProductCode) has MSI state $existingCurrentState"
    )
}
$existingArpEntries = @(Get-AutoPierCamArpEntries)
if ($existingArpEntries.Count -ne 0) {
    $preflightConflicts += 'AutoPierCam is registered in Add/Remove Programs'
}
$existingProcesses = @(Get-AutoPierCamProcesses)
if ($existingProcesses.Count -ne 0) {
    $processDescription = $existingProcesses | ForEach-Object {
        "PID=$($_.Id), name=$($_.ProcessName), path=$(Get-ProcessPath $_)"
    }
    $preflightConflicts += "process(es) exist: $($processDescription -join '; ')"
}
if ($preflightConflicts.Count -ne 0) {
    throw (
        "Lifecycle preflight found state that this test does not own. No installation was run. " +
        ($preflightConflicts -join ' | ')
    )
}

$artifactParent = [IO.Path]::GetFullPath(
    (Join-Path $repositoryRoot 'artifacts\installer')
).TrimEnd('\')
$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$artifactRoot = Join-Path $artifactParent (
    'lifecycle-test-{0}-{1}' -f $timestamp, [Guid]::NewGuid().ToString('N')
)
$archivedDataDirectory = Join-Path $artifactRoot 'user-data'
$sentinelPath = Join-Path $dataDirectory '.installer-lifecycle-sentinel.txt'
$sentinelContents = (
    "AutoPierCam installer lifecycle data`r`nTest ID: {0}`r`n" -f `
        [Guid]::NewGuid().ToString('D')
)
$dataDirectoryOwned = $false
$claimedDataRootIdentity = $null
$primaryFailure = $null
$cleanupErrors = [Collections.Generic.List[string]]::new()

try {
    if (Test-Path -LiteralPath $artifactRoot) {
        throw "Refusing to overwrite lifecycle artifacts: $artifactRoot"
    }
    New-Item -ItemType Directory -Path $artifactRoot | Out-Null

    # Claim the path only after the fail-closed preflight. The MSI never owns
    # this directory; creating it here makes archival ownership unambiguous.
    New-Item -ItemType Directory -Path $dataDirectory | Out-Null
    $dataDirectoryOwned = $true
    [IO.File]::WriteAllText(
        $sentinelPath,
        $sentinelContents,
        [Text.UTF8Encoding]::new($false)
    )
    $claimedDataRootIdentity = Get-DirectoryIdentity $dataDirectory

    Write-Host 'Cycle 1: default per-user install with sign-in startup enabled.'
    Invoke-MsiOperation `
        -Operation Install `
        -Target $resolvedMsiPath `
        -LogPath (Join-Path $artifactRoot '01-default-install.log')
    $firstTray = Wait-ForInstalledTray `
        -ExpectedPath $trayPath `
        -TimeoutSeconds $trayStartupTimeoutSeconds
    Assert-DirectoryIdentity `
        -Path $dataDirectory `
        -ExpectedIdentity $claimedDataRootIdentity `
        -Description 'Cycle-one data root'
    Assert-InstalledPayload `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory
    Assert-RunValue -RegistryPath $runRegistryPath -Expected $expectedRunValue
    Assert-ProductRegistered `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode `
        -ExpectedVersion $msiIdentity.ProductVersion
    Assert-InstalledFeatureStates `
        -ProductCode $msiIdentity.ProductCode `
        -StartAtSignInInstalled $true

    Write-Host "Gracefully stopping default-install tray PID $($firstTray.Id)."
    Invoke-GracefulTrayStop `
        -CliPath $cliPath `
        -TimeoutSeconds $trayShutdownTimeoutSeconds
    Invoke-MsiOperation `
        -Operation Uninstall `
        -Target $msiIdentity.ProductCode `
        -LogPath (Join-Path $artifactRoot '02-default-uninstall.log')
    Assert-DirectoryIdentity `
        -Path $dataDirectory `
        -ExpectedIdentity $claimedDataRootIdentity `
        -Description 'Data root after cycle-one uninstall'
    Assert-UninstalledState `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory `
        -RunRegistryPath $runRegistryPath `
        -InstallerRegistryPath $installerRegistryPath `
        -SentinelPath $sentinelPath `
        -SentinelContents $sentinelContents `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode

    Write-Host 'Cycle 2: MainApplication only, excluding sign-in startup.'
    Invoke-MsiOperation `
        -Operation Install `
        -Target $resolvedMsiPath `
        -LogPath (Join-Path $artifactRoot '03-main-only-install.log') `
        -Properties @('ADDLOCAL=MainApplication')
    $secondTray = Wait-ForInstalledTray `
        -ExpectedPath $trayPath `
        -TimeoutSeconds $trayStartupTimeoutSeconds
    Assert-DirectoryIdentity `
        -Path $dataDirectory `
        -ExpectedIdentity $claimedDataRootIdentity `
        -Description 'Cycle-two data root'
    Assert-InstalledPayload `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory
    Assert-RunValue -RegistryPath $runRegistryPath -Expected $null
    Assert-ProductRegistered `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode `
        -ExpectedVersion $msiIdentity.ProductVersion
    Assert-InstalledFeatureStates `
        -ProductCode $msiIdentity.ProductCode `
        -StartAtSignInInstalled $false

    $runningTrayUninstallLog = Join-Path `
        $artifactRoot `
        '04-running-tray-uninstall.log'
    Write-Host (
        "Uninstalling while tray PID $($secondTray.Id) is running; " +
        'the MSI must use its graceful-shutdown custom action.'
    )
    Assert-ExactTrayProcessAlive `
        -ProcessId $secondTray.Id `
        -ExpectedPath $trayPath
    Invoke-MsiOperation `
        -Operation Uninstall `
        -Target $msiIdentity.ProductCode `
        -LogPath $runningTrayUninstallLog `
        -Properties @('MSIRESTARTMANAGERCONTROL=Disable')
    Assert-GracefulStopLog -Path $runningTrayUninstallLog
    Assert-DirectoryIdentity `
        -Path $dataDirectory `
        -ExpectedIdentity $claimedDataRootIdentity `
        -Description 'Data root after cycle-two uninstall'
    Assert-UninstalledState `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory `
        -RunRegistryPath $runRegistryPath `
        -InstallerRegistryPath $installerRegistryPath `
        -SentinelPath $sentinelPath `
        -SentinelContents $sentinelContents `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode
} catch {
    $primaryFailure = $_
} finally {
    # Cleanup is limited to the ProductCode and paths proven absent during
    # preflight. Never race a Windows Installer service transaction after its
    # client timed out, and never recursively delete program or data paths.
    $installerQuiescentForCleanup = $false
    try {
        Wait-ForWindowsInstallerQuiescence `
            -TimeoutSeconds $cleanupQuiescenceTimeoutSeconds
        $installerQuiescentForCleanup = $true
    } catch {
        $cleanupErrors.Add(
            "cleanup cannot safely proceed while MSI state is uncertain: $($_.Exception.Message)"
        )
    }

    if ($installerQuiescentForCleanup) {
        try {
            $currentState = Get-InstallerProductState $msiIdentity.ProductCode
            $relatedNow = @(Get-RelatedProductCodes $msiIdentity.UpgradeCode)
            if ($currentState -ne -1 -or $msiIdentity.ProductCode -iin $relatedNow) {
                if (Test-Path -LiteralPath $cliPath -PathType Leaf) {
                    try {
                        Invoke-GracefulTrayStop `
                            -CliPath $cliPath `
                            -TimeoutSeconds $trayShutdownTimeoutSeconds
                    } catch {
                        $cleanupErrors.Add(
                            "graceful cleanup stop failed: $($_.Exception.Message)"
                        )
                    }
                }
                try {
                    Invoke-MsiOperation `
                        -Operation Uninstall `
                        -Target $msiIdentity.ProductCode `
                        -LogPath (Join-Path $artifactRoot 'cleanup-uninstall.log') `
                        -Properties @('MSIRESTARTMANAGERCONTROL=Disable')
                } catch {
                    $cleanupErrors.Add("cleanup uninstall failed: $($_.Exception.Message)")
                }
            }
        } catch {
            $cleanupErrors.Add("could not inspect cleanup registration: $($_.Exception.Message)")
        }

        # A cleanup uninstall can itself time out. Re-establish service
        # quiescence before inspecting or stopping any process it may own.
        $installerQuiescentForCleanup = $false
        try {
            Wait-ForWindowsInstallerQuiescence `
                -TimeoutSeconds $cleanupQuiescenceTimeoutSeconds
            $installerQuiescentForCleanup = $true
        } catch {
            $cleanupErrors.Add(
                "MSI state remained uncertain after cleanup: $($_.Exception.Message)"
            )
        }
    }

    if ($installerQuiescentForCleanup) {
        # A forced stop is only a last resort for the exact executable under
        # this test's previously absent install directory.
        foreach ($process in @(Get-AutoPierCamProcesses)) {
            $processPath = Get-ProcessPath $process
            if ($null -ne $processPath -and [string]::Equals(
                $processPath,
                $trayPath,
                [StringComparison]::OrdinalIgnoreCase
            )) {
                try {
                    Stop-Process -Id $process.Id -Force -ErrorAction Stop
                    [void] $process.WaitForExit(5000)
                    $cleanupErrors.Add(
                        "tray PID $($process.Id) required a forced stop after graceful cleanup"
                    )
                } catch {
                    $cleanupErrors.Add(
                        "could not stop test-owned tray PID $($process.Id): $($_.Exception.Message)"
                    )
                }
            } else {
                $cleanupErrors.Add(
                    "refused to stop unexpected AutoPierCam PID $($process.Id) at '$processPath'"
                )
            }
        }
    }

    $safeToArchiveData = $false
    if ($dataDirectoryOwned -and $installerQuiescentForCleanup) {
        try {
            Wait-ForSafePostCleanupState `
                -TimeoutSeconds $cleanupQuiescenceTimeoutSeconds `
                -ProductCode $msiIdentity.ProductCode `
                -UpgradeCode $msiIdentity.UpgradeCode `
                -InstallDirectory $installDirectory `
                -DataDirectory $dataDirectory `
                -StartMenuDirectory $startMenuDirectory `
                -RunRegistryPath $runRegistryPath `
                -InstallerRegistryPath $installerRegistryPath `
                -SentinelPath $sentinelPath `
                -SentinelContents $sentinelContents
            $safeToArchiveData = $true
        } catch {
            $cleanupErrors.Add(
                "test-owned data was left in place because cleanup is not proven safe: $($_.Exception.Message)"
            )
        }
    } elseif ($dataDirectoryOwned) {
        $cleanupErrors.Add(
            "test-owned data was left in place at $dataDirectory because MSI quiescence is uncertain"
        )
    }

    if ($safeToArchiveData) {
        try {
            $resolvedOwnedData = [IO.Path]::GetFullPath($dataDirectory).TrimEnd('\')
            $expectedOwnedData = [IO.Path]::GetFullPath(
                (Join-Path $env:LOCALAPPDATA 'AutoPierCam')
            ).TrimEnd('\')
            if (-not [string]::Equals(
                $resolvedOwnedData,
                $expectedOwnedData,
                [StringComparison]::OrdinalIgnoreCase
            )) {
                throw "Refusing to move an unexpected data path: $resolvedOwnedData"
            }
            if (Test-Path -LiteralPath $archivedDataDirectory) {
                throw "Refusing to overwrite archived lifecycle data: $archivedDataDirectory"
            }
            if ([string]::IsNullOrWhiteSpace($claimedDataRootIdentity)) {
                throw 'The test-owned data root has no claimed filesystem identity.'
            }
            Assert-DirectoryIdentity `
                -Path $resolvedOwnedData `
                -ExpectedIdentity $claimedDataRootIdentity `
                -Description 'Pre-archive data root'

            $artifactRootIdentity = Get-DirectoryIdentity $artifactRoot
            $sourceVolume = $claimedDataRootIdentity.Split(':')[0]
            $destinationVolume = $artifactRootIdentity.Split(':')[0]
            if (-not [string]::Equals(
                $sourceVolume,
                $destinationVolume,
                [StringComparison]::Ordinal
            )) {
                throw 'Atomic lifecycle-data archival requires one filesystem volume.'
            }

            $preMoveInventory = @(Get-StableDataTreeInventory $resolvedOwnedData)
            if (-not (Test-Path -LiteralPath $sentinelPath -PathType Leaf) -or
                -not [string]::Equals(
                    (Get-Content -LiteralPath $sentinelPath -Raw),
                    $sentinelContents,
                    [StringComparison]::Ordinal
                )) {
                throw 'The sentinel changed before atomic archival.'
            }

            # Directory.Move is one same-volume namespace rename. Do not fall
            # back to provider recursion or copy/delete semantics.
            [IO.Directory]::Move($resolvedOwnedData, $archivedDataDirectory)
            if (Test-Path -LiteralPath $resolvedOwnedData) {
                throw 'The source data path still exists immediately after atomic archival.'
            }
            Assert-DirectoryIdentity `
                -Path $archivedDataDirectory `
                -ExpectedIdentity $claimedDataRootIdentity `
                -Description 'Archived data root'
            $postMoveInventory = @(Get-DataTreeInventory $archivedDataDirectory)
            Assert-DataTreeInventoryEqual `
                -Expected $preMoveInventory `
                -Actual $postMoveInventory `
                -Description 'Atomic archive'

            $archivedSentinelPath = Join-Path `
                $archivedDataDirectory `
                '.installer-lifecycle-sentinel.txt'
            if (-not (Test-Path -LiteralPath $archivedSentinelPath -PathType Leaf) -or
                -not [string]::Equals(
                    (Get-Content -LiteralPath $archivedSentinelPath -Raw),
                    $sentinelContents,
                    [StringComparison]::Ordinal
                )) {
                throw 'The archived sentinel is missing or changed.'
            }

            Wait-ForStablePostArchiveState `
                -TimeoutSeconds 10 `
                -RequiredConsecutiveSamples 4 `
                -ProductCode $msiIdentity.ProductCode `
                -UpgradeCode $msiIdentity.UpgradeCode `
                -InstallDirectory $installDirectory `
                -SourceDataDirectory $resolvedOwnedData `
                -ArchivedDataDirectory $archivedDataDirectory `
                -StartMenuDirectory $startMenuDirectory `
                -RunRegistryPath $runRegistryPath `
                -InstallerRegistryPath $installerRegistryPath `
                -ExpectedDataIdentity $claimedDataRootIdentity `
                -ArchivedSentinelPath $archivedSentinelPath `
                -SentinelContents $sentinelContents
            $stableInventory = @(Get-DataTreeInventory $archivedDataDirectory)
            Assert-DataTreeInventoryEqual `
                -Expected $preMoveInventory `
                -Actual $stableInventory `
                -Description 'Stable archive'
            Write-Host "Preserved lifecycle user data at $archivedDataDirectory"
        } catch {
            $cleanupErrors.Add(
                "could not prove exact atomic archival of test-owned user data: $($_.Exception.Message)"
            )
        }
    }

    if ($script:msiTransactionTimedOut) {
        Write-Host (
            'At least one bounded msiexec client wait timed out; archival was permitted ' +
            'only after the independent MSI quiescence and post-uninstall gates.'
        )
    }
}

if ($null -ne $primaryFailure) {
    $cleanupSuffix = if ($cleanupErrors.Count -eq 0) {
        ''
    } else {
        " Cleanup issues: $($cleanupErrors -join ' | ')"
    }
    throw [InvalidOperationException]::new(
        "Installer lifecycle test failed: $($primaryFailure.Exception.Message)$cleanupSuffix",
        $primaryFailure.Exception
    )
}
if ($cleanupErrors.Count -ne 0) {
    throw "Installer lifecycle assertions passed, but cleanup was incomplete: $($cleanupErrors -join ' | ')"
}

Write-Host (
    'Per-user default install, graceful stop, uninstall preservation, ' +
    'MainApplication-only install, and running-tray uninstall passed. ' +
    "Logs and preserved user data: $artifactRoot"
)
