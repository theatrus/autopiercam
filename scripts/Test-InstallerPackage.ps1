[CmdletBinding()]
param(
    [string] $MsiPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'PeImports.ps1')

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts'))
$expectedSdkSha256 = '0c8778c3cce2012961b079e3c7d0d8348a8b3823939335d9e98148cb5d5dc34a'
$expectedUpgradeCode = '{CFB03866-5B27-42FD-8CC3-84E6F58DB343}'

if ([string]::IsNullOrWhiteSpace($MsiPath)) {
    $candidates = @(
        Get-ChildItem `
            -LiteralPath (Join-Path $repositoryRoot 'artifacts\installer\output') `
            -Filter 'AutoPierCam-*-x64.msi' `
            -File `
            -ErrorAction SilentlyContinue
    )
    if ($candidates.Count -ne 1) {
        throw 'Pass -MsiPath when the installer output directory does not contain exactly one MSI.'
    }
    $MsiPath = $candidates[0].FullName
}

$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path
$versionMatch = [Regex]::Match(
    [IO.Path]::GetFileName($resolvedMsiPath),
    '^AutoPierCam-(?<version>\d+\.\d+\.\d+)-x64\.msi$')
if (-not $versionMatch.Success) {
    throw "The MSI file name does not contain the expected product version: $resolvedMsiPath"
}
$productVersion = $versionMatch.Groups['version'].Value
$fileVersion = "$productVersion.0"
$testRoot = Join-Path $artifactRoot ("installer\package-test-{0}" -f [Guid]::NewGuid().ToString('N'))
$adminImageRoot = Join-Path $testRoot 'image'
$adminLogPath = Join-Path $testRoot 'admin-image.log'

function Get-MsiScalar([object] $Database, [string] $Sql) {
    $view = $null
    $record = $null
    try {
        $view = $Database.GetType().InvokeMember(
            'OpenView', 'InvokeMethod', $null, $Database, @($Sql))
        $view.GetType().InvokeMember(
            'Execute', 'InvokeMethod', $null, $view, $null) | Out-Null
        $record = $view.GetType().InvokeMember(
            'Fetch', 'InvokeMethod', $null, $view, $null)
        if ($null -eq $record) {
            return $null
        }
        return $record.StringData(1)
    } finally {
        if ($null -ne $record) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        }
        if ($null -ne $view) {
            $view.GetType().InvokeMember(
                'Close', 'InvokeMethod', $null, $view, $null) | Out-Null
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
        }
    }
}

function Assert-MsiScalar(
    [object] $Database,
    [string] $Sql,
    [string] $Expected,
    [string] $Description
) {
    $actual = Get-MsiScalar $Database $Sql
    if ($actual -cne $Expected) {
        $shownActual = if ($null -eq $actual) { '<missing>' } else { "'$actual'" }
        throw "$Description mismatch: got $shownActual; expected '$Expected'."
    }
}

function Get-RelativeFileMap([string] $Root) {
    $map = @{}
    foreach ($file in Get-ChildItem -LiteralPath $Root -File -Recurse) {
        $relative = [IO.Path]::GetRelativePath($Root, $file.FullName).Replace('/', '\')
        $map[$relative.ToLowerInvariant()] = $file.FullName
    }
    return $map
}

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null

    $wixVersion = (& wix --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $wixVersion -notmatch '^6\.') {
        throw "WiX 6 is required to test the package; found '$wixVersion'."
    }
    # ICE90/ICE91 warn about a hypothetical ALLUSERS change. This package is
    # deliberately fixed to Scope=perUser, which the table checks below also
    # enforce. Keep all component/key-path/directory ICEs enabled.
    & wix msi validate $resolvedMsiPath `
        -pdb ([IO.Path]::ChangeExtension($resolvedMsiPath, '.wixpdb')) `
        -sice ICE90 `
        -sice ICE91
    if ($LASTEXITCODE -ne 0) {
        throw "MSI validation failed with exit code $LASTEXITCODE"
    }

    $decompiledPath = Join-Path $testRoot 'package.wxs'
    & wix msi decompile $resolvedMsiPath -o $decompiledPath | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "MSI decompilation failed with exit code $LASTEXITCODE"
    }
    $decompiledPackage = Get-Content -LiteralPath $decompiledPath -Raw
    foreach ($requiredAuthoring in @(
        'Scope="perUser"',
        'RegistrySearch Id="WindowsBuildNumberSearch"',
        'StandardDirectory Id="LocalAppDataFolder"',
        'Directory Id="LOCALPROGRAMSDIRECTORY" Name="Programs"',
        'Directory Id="INSTALLFOLDER"',
        'Directory Id="VIEWERFOLDER" Name="Viewer"',
        'Feature Id="StartAtSignIn"',
        'Start AutoPierCam when I sign in (recommended)',
        'RegistryValue Id="AutoPierCamRunEntry"',
        '--config &quot;[LocalAppDataFolder]AutoPierCam\autopiercam.toml&quot;',
        '--sdk &quot;[INSTALLFOLDER]ASICamera2.dll&quot;',
        'Shortcut Id="AutoPierCamViewerShortcut"',
        'Shortcut Id="StartAutoPierCamShortcut"',
        'Apache License, Version 2.0',
        'Copyright 2026 Yann Ramin',
        '9. Accepting Warranty or Additional Liability',
        'END OF TERMS AND CONDITIONS',
        'CustomAction Id="SetGracefulStopAutoPierCam"',
        'CustomAction Id="GracefulStopAutoPierCam"',
        'DllEntry="WixSilentExec"',
        'shutdown-agent --if-running --timeout-seconds 30',
        'CustomAction Id="LaunchTrayAfterInstall"',
        'Return="asyncNoWait"',
        '--config &quot;[LocalAppDataFolder]AutoPierCam\autopiercam.toml&quot; --sdk &quot;[INSTALLFOLDER]ASICamera2.dll&quot;'
    )) {
        if (-not $decompiledPackage.Contains($requiredAuthoring)) {
            throw "MSI database is missing required authoring: $requiredAuthoring"
        }
    }
    if (
        $decompiledPackage.Contains('<CustomTable Id="Wix4CloseApplication">') -or
        $decompiledPackage.Contains('WixCloseApplications') -or
        $decompiledPackage.Contains('CloseAutoPierCam')
    ) {
        throw 'MSI must not contain process-basename CloseApplication handling.'
    }

    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $null
    try {
        $database = $installer.GetType().InvokeMember(
            'OpenDatabase', 'InvokeMethod', $null, $installer, @($resolvedMsiPath, 0))

        foreach ($propertyExpectation in @(
            @('ProductName', 'AutoPierCam'),
            @('Manufacturer', 'Yann Ramin'),
            @('ProductVersion', $productVersion),
            @('UpgradeCode', $expectedUpgradeCode),
            @('ARPURLINFOABOUT', 'https://github.com/theatrus/autopiercam'),
            @('ARPHELPLINK', 'https://github.com/theatrus/autopiercam/issues'),
            @('ARPCONTACT', 'Yann Ramin'),
            @(
                'ARPCOMMENTS',
                "Automatic capture and monitoring for ZWO ASI planetary cameras. Configuration, captures, upload state, and 14 daily logs are kept in the current user's Local AppData."
            ),
            @('MSIDISABLERMRESTART', '1')
        )) {
            Assert-MsiScalar `
                $database `
                "SELECT `Value` FROM `Property` WHERE `Property`='$($propertyExpectation[0])'" `
                $propertyExpectation[1] `
                "MSI property $($propertyExpectation[0])"
        }

        if ($null -ne (Get-MsiScalar $database "SELECT `Property` FROM `Property` WHERE `Property`='ALLUSERS'")) {
            throw 'Fixed current-user MSI unexpectedly contains an ALLUSERS property.'
        }

        Assert-MsiScalar $database `
            "SELECT `Directory_Parent` FROM `Directory` WHERE `Directory`='INSTALLFOLDER'" `
            'LOCALPROGRAMSDIRECTORY' `
            'Install directory parent'
        Assert-MsiScalar $database `
            "SELECT `Directory_Parent` FROM `Directory` WHERE `Directory`='LOCALPROGRAMSDIRECTORY'" `
            'LocalAppDataFolder' `
            'Local Programs directory parent'
        $unexpectedAppDataDirectory = Get-MsiScalar $database `
            "SELECT `Directory` FROM `Directory` WHERE `Directory_Parent`='LocalAppDataFolder' AND `Directory`<>'LOCALPROGRAMSDIRECTORY'"
        if ($null -ne $unexpectedAppDataDirectory) {
            throw "MSI unexpectedly owns a user-data directory beneath Local AppData: $unexpectedAppDataDirectory"
        }
        $installDirectoryName = Get-MsiScalar $database `
            "SELECT `DefaultDir` FROM `Directory` WHERE `Directory`='INSTALLFOLDER'"
        if ($null -eq $installDirectoryName -or $installDirectoryName -notmatch '(^|\|)AutoPierCam$') {
            throw "Install directory name mismatch: got '$installDirectoryName'; expected long name AutoPierCam."
        }
        Assert-MsiScalar $database `
            "SELECT `DefaultDir` FROM `Directory` WHERE `Directory`='VIEWERFOLDER'" `
            'Viewer' `
            'Viewer directory name'
        Assert-MsiScalar $database `
            "SELECT `Dialog` FROM `Dialog` WHERE `Dialog`='LicenseAgreementDlg'" `
            'LicenseAgreementDlg' `
            'Apache license dialog'
        Assert-MsiScalar $database `
            "SELECT `Dialog` FROM `Dialog` WHERE `Dialog`='CustomizeDlg'" `
            'CustomizeDlg' `
            'Feature-selection dialog'
        Assert-MsiScalar $database `
            "SELECT ``Description`` FROM ``LaunchCondition`` WHERE ``Condition``='Installed OR VersionNT64'" `
            'AutoPierCam requires a 64-bit edition of Windows.' `
            '64-bit Windows launch condition'
        Assert-MsiScalar $database `
            "SELECT ``Description`` FROM ``LaunchCondition`` WHERE ``Condition``='Installed OR (WINDOWSBUILDNUMBER >= 17763)'" `
            'AutoPierCam requires Windows 10 version 1809 (build 17763) or later.' `
            'Minimum Windows build launch condition'

        Assert-MsiScalar $database `
            "SELECT `Level` FROM `Feature` WHERE `Feature`='StartAtSignIn'" `
            '1' `
            'Start-at-sign-in default selection level'
        Assert-MsiScalar $database `
            "SELECT `Feature_Parent` FROM `Feature` WHERE `Feature`='StartAtSignIn'" `
            'MainApplication' `
            'Start-at-sign-in feature parent'
        Assert-MsiScalar $database `
            "SELECT `Root` FROM `Registry` WHERE `Registry`='AutoPierCamRunEntry'" `
            '1' `
            'Autostart registry root'
        Assert-MsiScalar $database `
            "SELECT ``Key`` FROM ``Registry`` WHERE ``Registry``='AutoPierCamRunEntry'" `
            'Software\Microsoft\Windows\CurrentVersion\Run' `
            'Autostart registry key'
        Assert-MsiScalar $database `
            "SELECT `Name` FROM `Registry` WHERE `Registry`='AutoPierCamRunEntry'" `
            'AutoPierCam' `
            'Autostart registry value name'
        $runValue = Get-MsiScalar $database `
            "SELECT `Value` FROM `Registry` WHERE `Registry`='AutoPierCamRunEntry'"
        foreach ($requiredRunFragment in @(
            '"[INSTALLFOLDER]autopiercam-tray.exe"',
            '--config "[LocalAppDataFolder]AutoPierCam\autopiercam.toml"',
            '--sdk "[INSTALLFOLDER]ASICamera2.dll"'
        )) {
            if ($null -eq $runValue -or -not $runValue.Contains($requiredRunFragment)) {
                throw "Autostart command is missing: $requiredRunFragment"
            }
        }

        if ($null -ne (Get-MsiScalar $database 'SELECT `Registry` FROM `Registry` WHERE `Root`=2')) {
            throw 'Per-user MSI unexpectedly contains an HKLM registry row.'
        }
        Assert-MsiScalar $database `
            "SELECT `Directory_` FROM `Shortcut` WHERE `Shortcut`='AutoPierCamViewerShortcut'" `
            'AUTOPIERCAMPROGRAMMENU' `
            'Viewer Start Menu shortcut directory'
        Assert-MsiScalar $database `
            "SELECT `Target` FROM `Shortcut` WHERE `Shortcut`='AutoPierCamViewerShortcut'" `
            '[VIEWERFOLDER]AutoPierCam.Viewer.exe' `
            'Viewer Start Menu shortcut target'
        $startShortcutArguments = Get-MsiScalar $database `
            "SELECT `Arguments` FROM `Shortcut` WHERE `Shortcut`='StartAutoPierCamShortcut'"
        foreach ($requiredArgument in @('--config', 'autopiercam.toml', '--sdk', '[INSTALLFOLDER]ASICamera2.dll')) {
            if ($null -eq $startShortcutArguments -or -not $startShortcutArguments.Contains($requiredArgument)) {
                throw "Start Menu capture shortcut is missing argument: $requiredArgument"
            }
        }

        Assert-MsiScalar $database `
            "SELECT `Source` FROM `CustomAction` WHERE `Action`='SetGracefulStopAutoPierCam'" `
            'GracefulStopAutoPierCam' `
            'Graceful-stop CustomActionData property'
        $gracefulCommand = Get-MsiScalar $database `
            "SELECT `Target` FROM `CustomAction` WHERE `Action`='SetGracefulStopAutoPierCam'"
        if (
            $null -eq $gracefulCommand -or
            -not $gracefulCommand.Contains('"[INSTALLFOLDER]autopiercam.exe"') -or
            -not $gracefulCommand.Contains('shutdown-agent --if-running --timeout-seconds 30')
        ) {
            throw "Graceful-stop property action has an unexpected command: $gracefulCommand"
        }
        Assert-MsiScalar $database `
            "SELECT `Source` FROM `CustomAction` WHERE `Action`='GracefulStopAutoPierCam'" `
            'Wix4UtilCA_X64' `
            'Graceful-stop custom-action binary'
        Assert-MsiScalar $database `
            "SELECT `Target` FROM `CustomAction` WHERE `Action`='GracefulStopAutoPierCam'" `
            'WixSilentExec' `
            'Graceful-stop custom-action entry point'
        $gracefulType = [int](Get-MsiScalar $database `
            "SELECT `Type` FROM `CustomAction` WHERE `Action`='GracefulStopAutoPierCam'")
        if (
            ($gracefulType -band 1024) -eq 0 -or
            ($gracefulType -band 2048) -ne 0 -or
            ($gracefulType -band 64) -eq 0
        ) {
            throw "Graceful stop must be deferred, impersonated, and non-fatal; CustomAction.Type is $gracefulType."
        }
        $setterSequence = [int](Get-MsiScalar $database `
            "SELECT `Sequence` FROM `InstallExecuteSequence` WHERE `Action`='SetGracefulStopAutoPierCam'")
        $gracefulSequence = [int](Get-MsiScalar $database `
            "SELECT `Sequence` FROM `InstallExecuteSequence` WHERE `Action`='GracefulStopAutoPierCam'")
        if (
            $setterSequence -le 1500 -or
            $gracefulSequence -ne ($setterSequence + 1) -or
            $gracefulSequence -ge 4000
        ) {
            throw "Graceful-stop authoring must prepare CustomActionData directly after InstallInitialize and precede InstallFiles; got $setterSequence, $gracefulSequence."
        }
        foreach ($action in @('SetGracefulStopAutoPierCam', 'GracefulStopAutoPierCam')) {
            Assert-MsiScalar $database `
                "SELECT ``Condition`` FROM ``InstallExecuteSequence`` WHERE ``Action``='$action'" `
                'Installed OR WIX_UPGRADE_DETECTED' `
                "$action condition"
        }
        Assert-MsiScalar $database `
            "SELECT `Sequence` FROM `InstallExecuteSequence` WHERE `Action`='RemoveExistingProducts'" `
            '6501' `
            'RemoveExistingProducts sequence'
        Assert-MsiScalar $database `
            "SELECT `Source` FROM `CustomAction` WHERE `Action`='LaunchTrayAfterInstall'" `
            'TrayExecutable' `
            'Post-install tray executable'
        $launchArguments = Get-MsiScalar $database `
            "SELECT `Target` FROM `CustomAction` WHERE `Action`='LaunchTrayAfterInstall'"
        foreach ($requiredArgument in @(
            '--config "[LocalAppDataFolder]AutoPierCam\autopiercam.toml"',
            '--sdk "[INSTALLFOLDER]ASICamera2.dll"'
        )) {
            if ($null -eq $launchArguments -or -not $launchArguments.Contains($requiredArgument)) {
                throw "Post-install tray launch is missing argument: $requiredArgument"
            }
        }
        Assert-MsiScalar $database `
            "SELECT `Type` FROM `CustomAction` WHERE `Action`='LaunchTrayAfterInstall'" `
            '210' `
            'Async current-user tray custom-action type'
        $launchSequence = [int](Get-MsiScalar $database `
            "SELECT `Sequence` FROM `InstallExecuteSequence` WHERE `Action`='LaunchTrayAfterInstall'")
        if ($launchSequence -le 6600) {
            throw "Post-install tray launch must be authored after InstallFinalize; got sequence $launchSequence."
        }
        Assert-MsiScalar $database `
            "SELECT ``Condition`` FROM ``InstallExecuteSequence`` WHERE ``Action``='LaunchTrayAfterInstall'" `
            'NOT (REMOVE ~= "ALL")' `
            'Post-install tray launch condition'

        foreach ($fileId in @('CaptureCliExecutable', 'TrayExecutable')) {
            Assert-MsiScalar $database `
                "SELECT `Version` FROM `File` WHERE `File`='$fileId'" `
                $fileVersion `
                "$fileId MSI file version"
        }
    } finally {
        if ($null -ne $database) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
        }
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
    }

    $arguments = "/a `"$resolvedMsiPath`" TARGETDIR=`"$adminImageRoot`" /qn /norestart /l*v `"$adminLogPath`""
    $process = Start-Process `
        -FilePath "$env:SystemRoot\System32\msiexec.exe" `
        -ArgumentList $arguments `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    if ($process.ExitCode -ne 0) {
        throw "MSI administrative extraction failed with exit code $($process.ExitCode); see $adminLogPath"
    }

    $trayMatches = @(
        Get-ChildItem -LiteralPath $adminImageRoot -Filter 'autopiercam-tray.exe' -File -Recurse
    )
    if ($trayMatches.Count -ne 1) {
        throw "Administrative image must contain exactly one autopiercam-tray.exe; found $($trayMatches.Count)."
    }
    $installImage = $trayMatches[0].Directory.FullName
    $normalizedInstallImage = $installImage.Replace('/', '\').TrimEnd('\')
    # Administrative images map the LocalAppDataFolder standard directory to
    # a synthetic LocalApp directory. The Directory table checks above prove
    # that a real installation resolves through Windows' Local AppData path.
    if ($normalizedInstallImage -notmatch '\\(LocalApp|AppData\\Local)\\Programs\\AutoPierCam$') {
        throw "Administrative image used an unexpected install path: $installImage"
    }

    $allowedRootFiles = @(
        'ASICamera2.dll',
        'autopiercam-tray.exe',
        'autopiercam.exe',
        'autopiercam.example.toml',
        'installation.md',
        'LICENSE-AutoPierCam.txt',
        'THIRD_PARTY_NOTICES.md'
    )
    $actualRootFiles = @(Get-ChildItem -LiteralPath $installImage -File | ForEach-Object Name)
    $missingRoot = @($allowedRootFiles | Where-Object { $_ -notin $actualRootFiles })
    $unexpectedRoot = @($actualRootFiles | Where-Object { $_ -notin $allowedRootFiles })
    if ($missingRoot.Count -ne 0 -or $unexpectedRoot.Count -ne 0) {
        throw ("Administrative-image root allowlist mismatch. Missing: " +
               "$($missingRoot -join ', '); unexpected: $($unexpectedRoot -join ', ').")
    }
    $allowedRootDirectories = @('licenses', 'Viewer')
    $actualRootDirectories = @(Get-ChildItem -LiteralPath $installImage -Directory | ForEach-Object Name)
    $unexpectedRootDirectories = @(
        $actualRootDirectories | Where-Object { $_ -notin $allowedRootDirectories }
    )
    $missingRootDirectories = @(
        $allowedRootDirectories | Where-Object { $_ -notin $actualRootDirectories }
    )
    if ($missingRootDirectories.Count -ne 0 -or $unexpectedRootDirectories.Count -ne 0) {
        throw ("Administrative-image directory allowlist mismatch. Missing: " +
               "$($missingRootDirectories -join ', '); unexpected: $($unexpectedRootDirectories -join ', ').")
    }


    $licenseImage = Join-Path $installImage 'licenses'
    $requiredLicenseFiles = @(
        'dotnet-LICENSE.txt',
        'dotnet-ThirdPartyNotices.txt',
        'Rust-Standard-Library-COPYRIGHT.html',
        'Rust-Third-Party-Licenses.md',
        'windows-app-sdk-LICENSE.txt',
        'windows-app-sdk-NOTICE.txt',
        'ZWO-ASI-SDK-license.txt'
    )
    $actualLicenseFiles = @(
        Get-ChildItem -LiteralPath $licenseImage -File | ForEach-Object Name
    )
    $missingLicenseFiles = @(
        $requiredLicenseFiles | Where-Object { $_ -notin $actualLicenseFiles }
    )
    $unexpectedLicenseFiles = @(
        $actualLicenseFiles | Where-Object { $_ -notin $requiredLicenseFiles }
    )
    if ($missingLicenseFiles.Count -ne 0 -or $unexpectedLicenseFiles.Count -ne 0) {
        throw ("Administrative-image license allowlist mismatch. Missing: " +
               "$($missingLicenseFiles -join ', '); unexpected: $($unexpectedLicenseFiles -join ', ').")
    }
    if (@(Get-ChildItem -LiteralPath $licenseImage -Directory).Count -ne 0) {
        throw 'Administrative-image licenses directory unexpectedly contains nested directories.'
    }

    $rustStandardLibraryNotice = Get-Content `
        -LiteralPath (Join-Path $licenseImage 'Rust-Standard-Library-COPYRIGHT.html') `
        -Raw
    foreach ($requiredNoticeText in @(
        '<h1>Copyright notices for The Rust Standard Library</h1>',
        'The Rust Standard Library is dual-licensed under Apache 2.0 and MIT terms.',
        'Apache License',
        'Version 2.0, January 2004',
        'END OF TERMS AND CONDITIONS',
        'Permission is hereby granted, free of charge, to any',
        'THE SOFTWARE IS PROVIDED &#34;AS IS&#34;, WITHOUT WARRANTY OF'
    )) {
        if (-not $rustStandardLibraryNotice.Contains($requiredNoticeText)) {
            throw "Packaged Rust standard-library notice is missing: $requiredNoticeText"
        }
    }

    $rustNoticeSource = Join-Path `
        $repositoryRoot `
        'third-party\rust\Rust-Third-Party-Licenses.md'
    $rustNoticeImage = Join-Path $licenseImage 'Rust-Third-Party-Licenses.md'
    $rustNoticeSourceHash = (Get-FileHash -LiteralPath $rustNoticeSource -Algorithm SHA256).Hash
    $rustNoticeImageHash = (Get-FileHash -LiteralPath $rustNoticeImage -Algorithm SHA256).Hash
    if ($rustNoticeImageHash -cne $rustNoticeSourceHash) {
        throw 'Administrative extraction changed the generated Rust dependency notices.'
    }
    $rustNoticeText = Get-Content -LiteralPath $rustNoticeImage -Raw
    foreach ($requiredNoticeText in @(
        '# Rust dependency licenses',
        'x86_64-pc-windows-msvc',
        '## Components',
        '## License and copyright texts'
    )) {
        if (-not $rustNoticeText.Contains($requiredNoticeText)) {
            throw "Generated Rust dependency notices are missing: $requiredNoticeText"
        }
    }

    $installedLicense = Get-Content -LiteralPath (Join-Path $installImage 'LICENSE-AutoPierCam.txt') -Raw
    foreach ($requiredLicenseText in @(
        'Apache License',
        'Version 2.0, January 2004',
        'END OF TERMS AND CONDITIONS'
    )) {
        if (-not $installedLicense.Contains($requiredLicenseText)) {
            throw "Installed Apache license is missing: $requiredLicenseText"
        }
    }
    $installedDocumentation = Get-Content -LiteralPath (Join-Path $installImage 'installation.md') -Raw
    $normalizedDocumentation = [Regex]::Replace($installedDocumentation, '\s+', ' ')
    foreach ($requiredDocumentation in @(
        '%LOCALAPPDATA%\AutoPierCam\logs\autopiercam.YYYY-MM-DD.log',
        'The date in each filename is UTC.',
        'retains the newest 14 daily log files',
        'never owns or removes `%LOCALAPPDATA%\AutoPierCam`'
    )) {
        if (-not $normalizedDocumentation.Contains($requiredDocumentation)) {
            throw "Installed user documentation is missing: $requiredDocumentation"
        }
    }

    $viewerImage = Join-Path $installImage 'Viewer'
    foreach ($requiredViewerFile in @(
        'AutoPierCam.Viewer.deps.json',
        'AutoPierCam.Viewer.dll',
        'AutoPierCam.Viewer.exe',
        'AutoPierCam.Viewer.runtimeconfig.json',
        'App.xbf',
        'MainWindow.xbf',
        'Microsoft.WindowsAppRuntime.Bootstrap.dll',
        'Microsoft.ui.xaml.dll',
        'Microsoft.UI.Xaml.Phone.dll',
        'coreclr.dll',
        'hostfxr.dll',
        'hostpolicy.dll',
        'resources.pri'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $viewerImage $requiredViewerFile) -PathType Leaf)) {
            throw "Administrative image is missing Viewer\$requiredViewerFile"
        }
    }
    if (@(Get-ChildItem -LiteralPath $installImage -Filter '*.pdb' -File -Recurse).Count -ne 0) {
        throw 'Administrative image unexpectedly contains PDB files.'
    }

    $sdkPath = Join-Path $installImage 'ASICamera2.dll'
    $sdkHash = (Get-FileHash -LiteralPath $sdkPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($sdkHash -cne $expectedSdkSha256) {
        throw "Packaged ASICamera2.dll hash mismatch: got $sdkHash; expected $expectedSdkSha256."
    }

    $viewerVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo(
        (Join-Path $viewerImage 'AutoPierCam.Viewer.exe'))
    if (
        $viewerVersion.FileVersion -cne $fileVersion -or
        $viewerVersion.ProductVersion -cne $productVersion
    ) {
        throw ("Packaged Viewer version mismatch: file=$($viewerVersion.FileVersion), " +
               "product=$($viewerVersion.ProductVersion), expected=$productVersion")
    }

    $cliPath = Join-Path $installImage 'autopiercam.exe'
    Assert-AutoPierCamStaticCrt `
        -Path $cliPath `
        -Description 'Packaged autopiercam.exe'
    Assert-AutoPierCamVersionResource `
        -Path $cliPath `
        -Version $productVersion `
        -FileDescription 'AutoPierCam capture engine and diagnostic CLI' `
        -OriginalFilename 'autopiercam.exe'
    Assert-AutoPierCamApplicationManifest `
        -Path $cliPath `
        -Version $productVersion `
        -AssemblyName 'AutoPierCam.Capture'
    $cliVersion = (& $cliPath --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $cliVersion -cne "autopiercam $productVersion") {
        throw "Packaged capture CLI version mismatch: '$cliVersion'."
    }
    $shutdownHelp = (& $cliPath shutdown-agent --help 2>&1 | Out-String)
    if (
        $LASTEXITCODE -ne 0 -or
        -not $shutdownHelp.Contains('--if-running') -or
        -not $shutdownHelp.Contains('--timeout-seconds')
    ) {
        throw 'Packaged capture CLI does not expose the MSI graceful-stop command contract.'
    }

    $trayPath = Join-Path $installImage 'autopiercam-tray.exe'
    Assert-AutoPierCamStaticCrt `
        -Path $trayPath `
        -Description 'Packaged autopiercam-tray.exe'
    Assert-AutoPierCamVersionResource `
        -Path $trayPath `
        -Version $productVersion `
        -FileDescription 'AutoPierCam Windows notification-area host' `
        -OriginalFilename 'autopiercam-tray.exe'
    Assert-AutoPierCamApplicationManifest `
        -Path $trayPath `
        -Version $productVersion `
        -AssemblyName 'AutoPierCam.Tray' `
        -GraphicalShell
    $trayVersion = (& $trayPath --version 2>&1 | Out-String).Trim()
    if ($trayVersion -cne "autopiercam-tray $productVersion") {
        throw "Packaged tray version mismatch: '$trayVersion'."
    }

    $stageRoot = Join-Path $repositoryRoot 'artifacts\installer\stage'
    if (Test-Path -LiteralPath $stageRoot -PathType Container) {
        $stageFiles = Get-RelativeFileMap $stageRoot
        $imageFiles = Get-RelativeFileMap $installImage
        $missingFromImage = @($stageFiles.Keys | Where-Object { -not $imageFiles.ContainsKey($_) })
        $unexpectedInImage = @($imageFiles.Keys | Where-Object { -not $stageFiles.ContainsKey($_) })
        if ($missingFromImage.Count -ne 0 -or $unexpectedInImage.Count -ne 0) {
            throw ("Administrative extraction does not exactly match the staged file allowlist. " +
                   "Missing: $($missingFromImage -join ', '); unexpected: $($unexpectedInImage -join ', ').")
        }
        foreach ($relativePath in $stageFiles.Keys) {
            $stageHash = (Get-FileHash -LiteralPath $stageFiles[$relativePath] -Algorithm SHA256).Hash
            $imageHash = (Get-FileHash -LiteralPath $imageFiles[$relativePath] -Algorithm SHA256).Hash
            if ($stageHash -cne $imageHash) {
                throw "Administrative extraction changed payload bytes: $relativePath"
            }
        }
    }

    Write-Host ('MSI authoring, validation, administrative extraction, allowlists, hashes, ' +
                'versions, static CRT, and shutdown-command authoring passed. ' +
                'A normal install/repair/upgrade lifecycle was not executed by this package test.')
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedTestRoot = [IO.Path]::GetFullPath($testRoot)
        $resolvedArtifactRoot = $artifactRoot.TrimEnd('\')
        if (-not $resolvedTestRoot.StartsWith(
            $resolvedArtifactRoot + '\installer\package-test-',
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw "Refusing to remove a package test outside its exact artifact prefix: $resolvedTestRoot"
        }
        Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
    }
}
