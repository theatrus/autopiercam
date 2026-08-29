[CmdletBinding()]
param(
    [string] $Version,

    # Stop after creating and validating the payload. Release signing belongs
    # in this gap so users run signed programs, not only a signed container.
    [switch] $StageOnly,

    # Package an existing payload without rebuilding or overwriting signatures
    # applied after -StageOnly.
    [switch] $PackageOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($StageOnly -and $PackageOnly) {
    throw 'Specify at most one of -StageOnly and -PackageOnly.'
}

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$manifestText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
$versionMatch = [Regex]::Match(
    $manifestText,
    '(?ms)^\[workspace\.package\]\s*$.*?^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
if (-not $versionMatch.Success) {
    throw 'Unable to determine the workspace version from Cargo.toml.'
}
$sourceVersion = $versionMatch.Groups['version'].Value
if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = $sourceVersion
} elseif ($Version -cne $sourceVersion) {
    throw "Installer version $Version does not match workspace version $sourceVersion."
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Installer version must have three numeric parts: $Version"
}

$artifactRoot = Join-Path $repositoryRoot 'artifacts\installer'
$stageRoot = Join-Path $artifactRoot 'stage'
$viewerStageRoot = Join-Path $stageRoot 'Viewer'
$intermediateRoot = Join-Path $artifactRoot 'obj'
$outputRoot = Join-Path $artifactRoot 'output'
$msiPath = Join-Path $outputRoot ("AutoPierCam-{0}-x64.msi" -f $Version)
$sdkSource = Join-Path $repositoryRoot 'vendor\zwo\ASI SDK\lib\x64\ASICamera2.dll'
$expectedSdkSha256 = '0c8778c3cce2012961b079e3c7d0d8348a8b3823939335d9e98148cb5d5dc34a'

function Reset-GeneratedDirectory([string] $Path) {
    $resolvedArtifactRoot = [IO.Path]::GetFullPath($artifactRoot).TrimEnd('\')
    $resolvedPath = [IO.Path]::GetFullPath($Path)
    if (-not $resolvedPath.StartsWith(
        $resolvedArtifactRoot + '\',
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw "Refusing to reset a directory outside the installer artifact root: $resolvedPath"
    }
    if (Test-Path -LiteralPath $resolvedPath) {
        Remove-Item -LiteralPath $resolvedPath -Recurse -Force
    }
    New-Item -ItemType Directory -Path $resolvedPath | Out-Null
}

function Copy-RequiredFile([string] $Source, [string] $Destination) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "Required payload source is missing: $Source"
    }
    $destinationDirectory = Split-Path -Parent $Destination
    if (-not (Test-Path -LiteralPath $destinationDirectory -PathType Container)) {
        New-Item -ItemType Directory -Path $destinationDirectory | Out-Null
    }
    Copy-Item -LiteralPath $Source -Destination $Destination
}

function Assert-SdkHash([string] $Path, [string] $Description) {
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $expectedSdkSha256) {
        throw "$Description has SHA-256 $actual; expected $expectedSdkSha256. Refusing to package an unreviewed SDK runtime."
    }
}

function Assert-StagedPayload {
    if (-not (Test-Path -LiteralPath $stageRoot -PathType Container)) {
        throw "The staged payload is missing: $stageRoot"
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
    $actualRootFiles = @(
        Get-ChildItem -LiteralPath $stageRoot -File | ForEach-Object Name
    )
    $missingRootFiles = @($allowedRootFiles | Where-Object { $_ -notin $actualRootFiles })
    $unexpectedRootFiles = @($actualRootFiles | Where-Object { $_ -notin $allowedRootFiles })
    if ($missingRootFiles.Count -ne 0) {
        throw "Staged payload is missing root files: $($missingRootFiles -join ', ')"
    }
    if ($unexpectedRootFiles.Count -ne 0) {
        throw "Staged payload contains unapproved root files: $($unexpectedRootFiles -join ', ')"
    }

    $allowedRootDirectories = @('licenses', 'Viewer')
    $actualRootDirectories = @(
        Get-ChildItem -LiteralPath $stageRoot -Directory | ForEach-Object Name
    )
    $missingRootDirectories = @(
        $allowedRootDirectories | Where-Object { $_ -notin $actualRootDirectories }
    )
    $unexpectedRootDirectories = @(
        $actualRootDirectories | Where-Object { $_ -notin $allowedRootDirectories }
    )
    if ($missingRootDirectories.Count -ne 0) {
        throw "Staged payload is missing directories: $($missingRootDirectories -join ', ')"
    }
    if ($unexpectedRootDirectories.Count -ne 0) {
        throw "Staged payload contains unapproved directories: $($unexpectedRootDirectories -join ', ')"
    }

    $requiredLicenseFiles = @(
        'dotnet-LICENSE.txt',
        'dotnet-ThirdPartyNotices.txt',
        'windows-app-sdk-LICENSE.txt',
        'windows-app-sdk-NOTICE.txt',
        'ZWO-ASI-SDK-license.txt'
    )
    $licenseRoot = Join-Path $stageRoot 'licenses'
    $actualLicenseFiles = @(
        Get-ChildItem -LiteralPath $licenseRoot -File | ForEach-Object Name
    )
    $missingLicenseFiles = @(
        $requiredLicenseFiles | Where-Object { $_ -notin $actualLicenseFiles }
    )
    $unexpectedLicenseFiles = @(
        $actualLicenseFiles | Where-Object { $_ -notin $requiredLicenseFiles }
    )
    $nestedLicenseDirectories = @(Get-ChildItem -LiteralPath $licenseRoot -Directory)
    if (
        $missingLicenseFiles.Count -ne 0 -or
        $unexpectedLicenseFiles.Count -ne 0 -or
        $nestedLicenseDirectories.Count -ne 0
    ) {
        throw ("Staged license allowlist mismatch. Missing: " +
               "$($missingLicenseFiles -join ', '); unexpected: " +
               "$($unexpectedLicenseFiles -join ', '); nested directories: " +
               "$($nestedLicenseDirectories.Name -join ', ').")
    }

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
        if (-not (Test-Path -LiteralPath (Join-Path $viewerStageRoot $requiredViewerFile) -PathType Leaf)) {
            throw "Self-contained WinUI payload is missing Viewer\$requiredViewerFile"
        }
    }

    $debugFiles = @(Get-ChildItem -LiteralPath $stageRoot -Filter '*.pdb' -File -Recurse)
    if ($debugFiles.Count -ne 0) {
        throw "Staged release payload contains PDB files: $($debugFiles.FullName -join ', ')"
    }
    $emptyFiles = @(Get-ChildItem -LiteralPath $stageRoot -File -Recurse | Where-Object Length -eq 0)
    if ($emptyFiles.Count -ne 0) {
        throw "Staged payload contains empty files: $($emptyFiles.FullName -join ', ')"
    }
    $reparsePoints = @(
        Get-ChildItem -LiteralPath $stageRoot -Force -Recurse |
            Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }
    )
    if ($reparsePoints.Count -ne 0) {
        throw "Staged payload contains reparse points: $($reparsePoints.FullName -join ', ')"
    }

    Assert-SdkHash (Join-Path $stageRoot 'ASICamera2.dll') 'Staged ASICamera2.dll'

    $cliPath = Join-Path $stageRoot 'autopiercam.exe'
    $cliVersion = (& $cliPath --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $cliVersion -cne "autopiercam $Version") {
        throw "Staged capture CLI version mismatch: '$cliVersion' (expected 'autopiercam $Version')."
    }
    $shutdownHelp = (& $cliPath shutdown-agent --help 2>&1 | Out-String)
    if (
        $LASTEXITCODE -ne 0 -or
        -not $shutdownHelp.Contains('--if-running') -or
        -not $shutdownHelp.Contains('--timeout-seconds')
    ) {
        throw 'Staged capture CLI does not expose the graceful shutdown-agent contract required by the MSI.'
    }

    $trayPath = Join-Path $stageRoot 'autopiercam-tray.exe'
    $trayVersion = (& $trayPath --version 2>&1 | Out-String).Trim()
    if ($trayVersion -cne "autopiercam-tray $Version") {
        throw "Staged tray version mismatch: '$trayVersion' (expected 'autopiercam-tray $Version')."
    }

    $viewerVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo(
        (Join-Path $viewerStageRoot 'AutoPierCam.Viewer.exe'))
    if (
        $viewerVersion.FileVersion -cne "$Version.0" -or
        $viewerVersion.ProductVersion -cne $Version
    ) {
        throw ("Staged Viewer version mismatch: file=$($viewerVersion.FileVersion), " +
               "product=$($viewerVersion.ProductVersion), expected=$Version")
    }
}

function Get-StablePayloadIdentity([string] $RelativePath) {
    # Component identity is derived only from the case-normalized installed
    # path. It remains stable across machines and builds, while removing a file
    # also removes its independently reference-counted MSI component.
    $identityInput = "391447DF-4D8E-4F19-B756-F86623A1E1F1|$($RelativePath.ToLowerInvariant())"
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = $sha256.ComputeHash([Text.Encoding]::UTF8.GetBytes($identityInput))
    } finally {
        $sha256.Dispose()
    }
    $token = [Convert]::ToHexString($digest).Substring(0, 24)
    $guidBytes = [byte[]]::new(16)
    [Array]::Copy($digest, $guidBytes, 16)
    # Mark the deterministic identifier as RFC 4122 variant/version 5. The
    # digest algorithm is SHA-256 rather than UUIDv5's SHA-1, but the bits make
    # diagnostics recognize it as a name-derived identifier.
    $guidBytes[7] = [byte](($guidBytes[7] -band 0x0f) -bor 0x50)
    $guidBytes[8] = [byte](($guidBytes[8] -band 0x3f) -bor 0x80)
    return [pscustomobject] @{
        Token = $token
        Guid = ([Guid]::new($guidBytes)).ToString().ToUpperInvariant()
    }
}

function New-PayloadWixSource([string] $DestinationPath) {
    $excludedPaths = @(
        'autopiercam.exe',
        'autopiercam-tray.exe',
        'ASICamera2.dll',
        'Viewer\AutoPierCam.Viewer.exe',
        'Viewer\Microsoft.ui.xaml.dll',
        'Viewer\Microsoft.UI.Xaml.Phone.dll'
    )
    $files = @(
        Get-ChildItem -LiteralPath $stageRoot -File -Recurse |
            Sort-Object FullName |
            Where-Object {
                $relative = [IO.Path]::GetRelativePath($stageRoot, $_.FullName).Replace('/', '\')
                $relative -notin $excludedPaths
            }
    )
    if ($files.Count -eq 0) {
        throw 'No generated payload files were found.'
    }

    $payloadDirectories = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase)
    foreach ($baseDirectory in @('INSTALLFOLDER|', 'VIEWERFOLDER|', 'LICENSESFOLDER|')) {
        [void] $payloadDirectories.Add($baseDirectory)
    }
    $payloadEntries = [Collections.Generic.List[object]]::new()

    foreach ($file in $files) {
        $relativePath = [IO.Path]::GetRelativePath($stageRoot, $file.FullName).Replace('/', '\')
        $identity = Get-StablePayloadIdentity $relativePath
        [string] $relativeDirectory = Split-Path -Parent $relativePath
        $baseDirectoryId = 'INSTALLFOLDER'
        $subdirectory = $relativeDirectory
        if ($relativeDirectory -ieq 'Viewer') {
            $baseDirectoryId = 'VIEWERFOLDER'
            $subdirectory = ''
        } elseif ($relativeDirectory.StartsWith('Viewer\', [StringComparison]::OrdinalIgnoreCase)) {
            $baseDirectoryId = 'VIEWERFOLDER'
            $subdirectory = $relativeDirectory.Substring('Viewer\'.Length)
        } elseif ($relativeDirectory -ieq 'licenses') {
            $baseDirectoryId = 'LICENSESFOLDER'
            $subdirectory = ''
        } elseif ($relativeDirectory.StartsWith('licenses\', [StringComparison]::OrdinalIgnoreCase)) {
            $baseDirectoryId = 'LICENSESFOLDER'
            $subdirectory = $relativeDirectory.Substring('licenses\'.Length)
        }

        $directoryParts = @(
            if (-not [string]::IsNullOrWhiteSpace($subdirectory)) {
                $subdirectory.Split('\', [StringSplitOptions]::RemoveEmptyEntries)
            }
        )
        if ($directoryParts.Count -eq 0) {
            [void] $payloadDirectories.Add("$baseDirectoryId|")
        } else {
            for ($index = 1; $index -le $directoryParts.Count; $index++) {
                $path = $directoryParts[0..($index - 1)] -join '\'
                [void] $payloadDirectories.Add("$baseDirectoryId|$path")
            }
        }

        $targetDirectoryId = $baseDirectoryId
        if (-not [string]::IsNullOrWhiteSpace($subdirectory)) {
            $directoryIdentity = Get-StablePayloadIdentity "__directory_id__\$baseDirectoryId|$subdirectory"
            $targetDirectoryId = "PayloadDirectory_$($directoryIdentity.Token)"
        }
        $payloadEntries.Add([pscustomobject] @{
            File = $file
            Identity = $identity
            DirectoryId = $targetDirectoryId
        })
    }

    $xml = [Text.StringBuilder]::new()
    [void] $xml.AppendLine('<?xml version="1.0" encoding="utf-8"?>')
    [void] $xml.AppendLine('<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">')
    [void] $xml.AppendLine('  <Fragment>')
    $nestedDirectories = @(
        $payloadDirectories |
            Where-Object { -not $_.EndsWith('|', [StringComparison]::Ordinal) } |
            Sort-Object `
                @{ Expression = { ($_.Substring($_.IndexOf('|') + 1).Split('\')).Count } }, `
                @{ Expression = { $_ } }
    )
    foreach ($directoryKey in $nestedDirectories) {
        $separator = $directoryKey.IndexOf('|')
        $baseDirectoryId = $directoryKey.Substring(0, $separator)
        $subdirectory = $directoryKey.Substring($separator + 1)
        [string] $parentSubdirectory = Split-Path -Parent $subdirectory
        $parentDirectoryId = $baseDirectoryId
        if (-not [string]::IsNullOrWhiteSpace($parentSubdirectory)) {
            $parentIdentity = Get-StablePayloadIdentity "__directory_id__\$baseDirectoryId|$parentSubdirectory"
            $parentDirectoryId = "PayloadDirectory_$($parentIdentity.Token)"
        }
        $directoryIdentity = Get-StablePayloadIdentity "__directory_id__\$directoryKey"
        $directoryId = "PayloadDirectory_$($directoryIdentity.Token)"
        $directoryName = [Security.SecurityElement]::Escape((Split-Path -Leaf $subdirectory))
        [void] $xml.AppendLine("    <DirectoryRef Id=`"$parentDirectoryId`">")
        [void] $xml.AppendLine("      <Directory Id=`"$directoryId`" Name=`"$directoryName`" />")
        [void] $xml.AppendLine('    </DirectoryRef>')
    }
    [void] $xml.AppendLine('  </Fragment>')
    [void] $xml.AppendLine('  <Fragment>')
    [void] $xml.AppendLine('    <ComponentGroup Id="GeneratedPayloadFiles">')
    foreach ($entry in $payloadEntries) {
        $identity = $entry.Identity
        $file = $entry.File
        $escapedSource = [Security.SecurityElement]::Escape($file.FullName)
        [void] $xml.AppendLine("      <Component Id=`"PayloadComponent_$($identity.Token)`" Guid=`"$($identity.Guid)`" Directory=`"$($entry.DirectoryId)`">")
        [void] $xml.AppendLine("        <File Id=`"PayloadFile_$($identity.Token)`" Source=`"$escapedSource`" />")
        [void] $xml.AppendLine("        <RegistryValue Id=`"PayloadRegistry_$($identity.Token)`"")
        [void] $xml.AppendLine('                       Root="HKCU"')
        [void] $xml.AppendLine('                       Key="Software\AutoPierCam\Installer\Payload"')
        [void] $xml.AppendLine("                       Name=`"$($identity.Token)`"")
        [void] $xml.AppendLine('                       Type="integer"')
        [void] $xml.AppendLine('                       Value="1"')
        [void] $xml.AppendLine('                       KeyPath="yes" />')
        [void] $xml.AppendLine('      </Component>')
    }

    foreach ($directoryIdentity in @($payloadDirectories | Sort-Object)) {
        $separator = $directoryIdentity.IndexOf('|')
        $baseDirectoryId = $directoryIdentity.Substring(0, $separator)
        $subdirectory = $directoryIdentity.Substring($separator + 1)
        $identity = Get-StablePayloadIdentity "__directory__\$directoryIdentity"
        $targetDirectoryId = $baseDirectoryId
        if (-not [string]::IsNullOrWhiteSpace($subdirectory)) {
            $targetIdentity = Get-StablePayloadIdentity "__directory_id__\$directoryIdentity"
            $targetDirectoryId = "PayloadDirectory_$($targetIdentity.Token)"
        }
        [void] $xml.AppendLine("      <Component Id=`"PayloadDirectoryComponent_$($identity.Token)`" Guid=`"$($identity.Guid)`" Directory=`"$targetDirectoryId`">")
        [void] $xml.AppendLine("        <RemoveFolder Id=`"PayloadRemoveFolder_$($identity.Token)`" On=`"uninstall`" />")
        [void] $xml.AppendLine("        <RegistryValue Id=`"PayloadDirectoryRegistry_$($identity.Token)`"")
        [void] $xml.AppendLine('                       Root="HKCU"')
        [void] $xml.AppendLine('                       Key="Software\AutoPierCam\Installer\PayloadDirectories"')
        [void] $xml.AppendLine("                       Name=`"$($identity.Token)`"")
        [void] $xml.AppendLine('                       Type="integer"')
        [void] $xml.AppendLine('                       Value="1"')
        [void] $xml.AppendLine('                       KeyPath="yes" />')
        [void] $xml.AppendLine('      </Component>')
    }
    [void] $xml.AppendLine('    </ComponentGroup>')
    [void] $xml.AppendLine('  </Fragment>')
    [void] $xml.AppendLine('</Wix>')
    Set-Content -LiteralPath $DestinationPath -Value $xml.ToString() -Encoding utf8
}

function Set-MsiFileLanguageNeutral(
    [string] $DatabasePath,
    [string[]] $FileIds
) {
    # Windows App SDK 2.3 marks two runtime DLLs with a comma-separated LCID
    # list that exceeds MSI's File.Language column. WiX cannot override the
    # binder's harvested version metadata, so normalize only the authored rows.
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $null
    try {
        $database = $installer.GetType().InvokeMember(
            'OpenDatabase',
            'InvokeMethod',
            $null,
            $installer,
            @($DatabasePath, 1))
        foreach ($fileId in $FileIds) {
            $updateSql = "UPDATE `File` SET `Language`='0' WHERE `File`='$fileId'"
            $updateView = $database.GetType().InvokeMember(
                'OpenView', 'InvokeMethod', $null, $database, @($updateSql))
            try {
                $updateView.GetType().InvokeMember(
                    'Execute', 'InvokeMethod', $null, $updateView, $null) | Out-Null
            } finally {
                $updateView.GetType().InvokeMember(
                    'Close', 'InvokeMethod', $null, $updateView, $null) | Out-Null
                [Runtime.InteropServices.Marshal]::FinalReleaseComObject($updateView) | Out-Null
            }
        }
        $database.GetType().InvokeMember(
            'Commit', 'InvokeMethod', $null, $database, $null) | Out-Null
    } finally {
        if ($null -ne $database) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
        }
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
    }
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'The installer build currently supports only Windows x64.'
}

$requiredTools = if ($PackageOnly) { @('wix') } else { @('cargo', 'dotnet', 'wix') }
foreach ($tool in $requiredTools) {
    if ($null -eq (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "Required build tool is unavailable: $tool"
    }
}
$wixVersion = (& wix --version 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $wixVersion -notmatch '^6\.') {
    throw "WiX 6 is required; found '$wixVersion'."
}

if (-not $PackageOnly) {
    Reset-GeneratedDirectory $stageRoot
}
Reset-GeneratedDirectory $intermediateRoot
Reset-GeneratedDirectory $outputRoot

Push-Location $repositoryRoot
try {
    if (-not $PackageOnly) {
        Assert-SdkHash $sdkSource 'Vendored ASICamera2.dll'

        & cargo build --release -p autopiercam -p autopiercam-tray
        if ($LASTEXITCODE -ne 0) {
            throw "Rust release build failed with exit code $LASTEXITCODE"
        }

        New-Item -ItemType Directory -Path $viewerStageRoot | Out-Null
        & dotnet publish apps/AutoPierCam.Viewer/AutoPierCam.Viewer.csproj `
            --configuration Release `
            --runtime win-x64 `
            --self-contained true `
            -p:Version=$Version `
            -p:AssemblyVersion="$Version.0" `
            -p:FileVersion="$Version.0" `
            -p:InformationalVersion=$Version `
            -p:SatelliteResourceLanguages=en-US `
            -p:DebugSymbols=false `
            -p:DebugType=None `
            --output $viewerStageRoot
        if ($LASTEXITCODE -ne 0) {
            throw "WinUI publish failed with exit code $LASTEXITCODE"
        }

        # Windows App SDK 2.3 publishes three satellite assemblies whose LCID
        # metadata MSI cannot represent. The Viewer is English-only today.
        foreach ($culture in @('gd-gb', 'mi-NZ', 'ug-CN')) {
            $cultureDirectory = Join-Path $viewerStageRoot $culture
            if (Test-Path -LiteralPath $cultureDirectory) {
                Remove-Item -LiteralPath $cultureDirectory -Recurse -Force
            }
        }
        Get-ChildItem -LiteralPath $stageRoot -Filter '*.pdb' -File -Recurse |
            Remove-Item -Force

        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'target\release\autopiercam.exe') `
            (Join-Path $stageRoot 'autopiercam.exe')
        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'target\release\autopiercam-tray.exe') `
            (Join-Path $stageRoot 'autopiercam-tray.exe')
        Copy-RequiredFile $sdkSource (Join-Path $stageRoot 'ASICamera2.dll')
        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'autopiercam.example.toml') `
            (Join-Path $stageRoot 'autopiercam.example.toml')
        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'docs\installation.md') `
            (Join-Path $stageRoot 'installation.md')
        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'LICENSE') `
            (Join-Path $stageRoot 'LICENSE-AutoPierCam.txt')
        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'THIRD_PARTY_NOTICES.md') `
            (Join-Path $stageRoot 'THIRD_PARTY_NOTICES.md')

        $licenseRoot = Join-Path $stageRoot 'licenses'
        New-Item -ItemType Directory -Path $licenseRoot | Out-Null
        Copy-RequiredFile `
            (Join-Path $repositoryRoot 'vendor\zwo\ASI SDK\license.txt') `
            (Join-Path $licenseRoot 'ZWO-ASI-SDK-license.txt')

        $dotnetRoot = Split-Path -Parent (Get-Command dotnet).Source
        Copy-RequiredFile `
            (Join-Path $dotnetRoot 'LICENSE.txt') `
            (Join-Path $licenseRoot 'dotnet-LICENSE.txt')
        Copy-RequiredFile `
            (Join-Path $dotnetRoot 'ThirdPartyNotices.txt') `
            (Join-Path $licenseRoot 'dotnet-ThirdPartyNotices.txt')

        [xml] $viewerProject = Get-Content -LiteralPath `
            (Join-Path $repositoryRoot 'apps\AutoPierCam.Viewer\AutoPierCam.Viewer.csproj') -Raw
        $windowsAppSdkReference = @(
            $viewerProject.Project.ItemGroup.PackageReference |
                Where-Object Include -eq 'Microsoft.WindowsAppSDK'
        )
        if ($windowsAppSdkReference.Count -ne 1) {
            throw 'Could not identify exactly one Microsoft.WindowsAppSDK package reference.'
        }
        $windowsAppSdkVersion = [string] $windowsAppSdkReference[0].Version
        if ([string]::IsNullOrWhiteSpace($windowsAppSdkVersion)) {
            throw 'Microsoft.WindowsAppSDK package reference has no version.'
        }
        $globalPackagesOutput = (& dotnet nuget locals global-packages --list 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) {
            throw "Could not locate the NuGet global-packages directory: $globalPackagesOutput"
        }
        $globalPackagesMatch = [Regex]::Match($globalPackagesOutput, '^global-packages:\s*(?<path>.+)$')
        if (-not $globalPackagesMatch.Success) {
            throw "Unexpected dotnet nuget locals output: $globalPackagesOutput"
        }
        $windowsAppSdkRoot = Join-Path `
            $globalPackagesMatch.Groups['path'].Value.Trim() `
            ("microsoft.windowsappsdk\{0}" -f $windowsAppSdkVersion.ToLowerInvariant())
        Copy-RequiredFile `
            (Join-Path $windowsAppSdkRoot 'license.txt') `
            (Join-Path $licenseRoot 'windows-app-sdk-LICENSE.txt')
        Copy-RequiredFile `
            (Join-Path $windowsAppSdkRoot 'NOTICE.txt') `
            (Join-Path $licenseRoot 'windows-app-sdk-NOTICE.txt')
    }

    Assert-StagedPayload

    if ($StageOnly) {
        Write-Host "Staged and validated payload at $stageRoot"
        Write-Host 'Sign the first-party executables in this directory, then run this script with -PackageOnly.'
        return
    }

    $payloadSource = Join-Path $intermediateRoot 'GeneratedPayload.wxs'
    New-PayloadWixSource $payloadSource

    & wix build installer/Product.wxs $payloadSource `
        -arch x64 `
        -ext WixToolset.UI.wixext `
        -ext WixToolset.Util.wixext `
        -d "StageDir=$stageRoot" `
        -d "LicenseRtf=$(Join-Path $repositoryRoot 'installer\License.rtf')" `
        -d "ProductVersion=$Version" `
        -intermediatefolder $intermediateRoot `
        -out $msiPath
    if ($LASTEXITCODE -ne 0) {
        throw "WiX build failed with exit code $LASTEXITCODE"
    }

    Set-MsiFileLanguageNeutral `
        -DatabasePath $msiPath `
        -FileIds @('WinUiXamlRuntime', 'WinUiXamlPhoneRuntime')

    & (Join-Path $PSScriptRoot 'Test-InstallerPackage.ps1') -MsiPath $msiPath
    if ($LASTEXITCODE -ne 0) {
        throw "Installer package tests failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

$msiHash = (Get-FileHash -LiteralPath $msiPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Built and validated $msiPath"
Write-Host "SHA-256: $msiHash"
Get-Item -LiteralPath $msiPath
