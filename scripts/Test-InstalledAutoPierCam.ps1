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

function Get-AutoPierCamArpEntries {
    $entries = @()
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
    [string] $Description
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
            # This is the exact child process created by this test. Stopping it
            # bounds the harness; final cleanup still checks MSI registration.
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
        -Description "$Operation transaction")
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
    [string] $Expected
) {
    $actual = Get-RunValue $RegistryPath
    if ($null -eq $Expected) {
        if ($null -ne $actual) {
            throw "The opt-out installation unexpectedly created an AutoPierCam Run value: $actual"
        }
        return
    }
    if ($null -eq $actual -or -not [string]::Equals(
        [string] $actual,
        $Expected,
        [StringComparison]::Ordinal
    )) {
        $shown = if ($null -eq $actual) { '<missing>' } else { "'$actual'" }
        throw "AutoPierCam Run value mismatch: got $shown; expected '$Expected'."
    }
}

function Assert-Shortcut(
    [string] $Path,
    [string] $ExpectedTarget,
    [AllowEmptyString()]
    [string] $RequiredArguments
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
        if (-not [string]::IsNullOrEmpty($RequiredArguments) -and
            -not ([string] $shortcut.Arguments).Contains($RequiredArguments)) {
            throw "Shortcut arguments for $Path are missing '$RequiredArguments'."
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
        -RequiredArguments ''
    Assert-Shortcut `
        -Path $startShortcut `
        -ExpectedTarget (Join-Path $InstallDirectory 'autopiercam-tray.exe') `
        -RequiredArguments '--config'
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

    Write-Host 'Cycle 1: default per-user install with sign-in startup enabled.'
    Invoke-MsiOperation `
        -Operation Install `
        -Target $resolvedMsiPath `
        -LogPath (Join-Path $artifactRoot '01-default-install.log')
    $firstTray = Wait-ForInstalledTray `
        -ExpectedPath $trayPath `
        -TimeoutSeconds $trayStartupTimeoutSeconds
    Assert-InstalledPayload `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory
    Assert-RunValue -RegistryPath $runRegistryPath -Expected $expectedRunValue
    Assert-ProductRegistered `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode `
        -ExpectedVersion $msiIdentity.ProductVersion

    Write-Host "Gracefully stopping default-install tray PID $($firstTray.Id)."
    Invoke-GracefulTrayStop `
        -CliPath $cliPath `
        -TimeoutSeconds $trayShutdownTimeoutSeconds
    Invoke-MsiOperation `
        -Operation Uninstall `
        -Target $msiIdentity.ProductCode `
        -LogPath (Join-Path $artifactRoot '02-default-uninstall.log')
    Assert-UninstalledState `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory `
        -RunRegistryPath $runRegistryPath `
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
    Assert-InstalledPayload `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory
    Assert-RunValue -RegistryPath $runRegistryPath -Expected $null
    Assert-ProductRegistered `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode `
        -ExpectedVersion $msiIdentity.ProductVersion

    Write-Host (
        "Uninstalling while tray PID $($secondTray.Id) is running; " +
        'the MSI must use its graceful-shutdown custom action.'
    )
    Invoke-MsiOperation `
        -Operation Uninstall `
        -Target $msiIdentity.ProductCode `
        -LogPath (Join-Path $artifactRoot '04-running-tray-uninstall.log')
    Assert-UninstalledState `
        -InstallDirectory $installDirectory `
        -DataDirectory $dataDirectory `
        -StartMenuDirectory $startMenuDirectory `
        -RunRegistryPath $runRegistryPath `
        -SentinelPath $sentinelPath `
        -SentinelContents $sentinelContents `
        -ProductCode $msiIdentity.ProductCode `
        -UpgradeCode $msiIdentity.UpgradeCode
} catch {
    $primaryFailure = $_
} finally {
    # Cleanup is limited to the ProductCode and paths proven absent during
    # preflight. It never recursively deletes user or program directories.
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
                    $cleanupErrors.Add("graceful cleanup stop failed: $($_.Exception.Message)")
                }
            }
            try {
                Invoke-MsiOperation `
                    -Operation Uninstall `
                    -Target $msiIdentity.ProductCode `
                    -LogPath (Join-Path $artifactRoot 'cleanup-uninstall.log')
            } catch {
                $cleanupErrors.Add("cleanup uninstall failed: $($_.Exception.Message)")
            }
        }
    } catch {
        $cleanupErrors.Add("could not inspect cleanup registration: $($_.Exception.Message)")
    }

    # A forced stop is only a last-resort cleanup for an executable underneath
    # this test's exact, previously absent install directory.
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

    if ($dataDirectoryOwned -and (Test-Path -LiteralPath $dataDirectory)) {
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
            Move-Item `
                -LiteralPath $resolvedOwnedData `
                -Destination $archivedDataDirectory
            Write-Host "Preserved lifecycle user data at $archivedDataDirectory"
        } catch {
            $cleanupErrors.Add("could not archive test-owned user data: $($_.Exception.Message)")
        }
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
