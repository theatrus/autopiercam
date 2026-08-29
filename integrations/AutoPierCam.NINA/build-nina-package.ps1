[CmdletBinding()]
param(
    [ValidatePattern('^\d+\.\d+\.\d+\.\d+$')]
    [string] $Version = '0.1.0.0',

    [string] $InstallerUrl = '',

    [string] $FeaturedImageUrl = '',

    [ValidateNotNullOrEmpty()]
    [string] $OutputDirectory = 'artifacts/nina-plugin',

    # Sign staged binaries after this step and before creating the checksum.
    [switch] $StageOnly,

    [switch] $PackageOnly
)

if ($StageOnly -and $PackageOnly) {
    throw 'Specify at most one of -StageOnly and -PackageOnly.'
}

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Resolve-FullPath {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [string] $BasePath
    )

    if ([IO.Path]::IsPathRooted($Path)) {
        return [IO.Path]::GetFullPath($Path)
    }

    return [IO.Path]::GetFullPath((Join-Path $BasePath $Path))
}

function Assert-StrictChildPath {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [string] $Parent,
        [Parameter(Mandatory)] [string] $Label
    )

    $relative = [IO.Path]::GetRelativePath($Parent, $Path)
    if ($relative -eq '.' -or
        [IO.Path]::IsPathRooted($relative) -or
        $relative -eq '..' -or
        $relative.StartsWith("..$([IO.Path]::DirectorySeparatorChar)", [StringComparison]::Ordinal)) {
        throw "$Label must be a strict child of $Parent; got $Path."
    }
}

function Reset-Directory {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [string] $Parent
    )

    Assert-StrictChildPath -Path $Path -Parent $Parent -Label 'Build directory'
    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
    New-Item -ItemType Directory -Path $Path -Force | Out-Null
}

function Assert-PackageAllowlist {
    param([Parameter(Mandatory)] [string] $PackageDirectory)

    $expected = @(
        'AutoPierCam.NINA.dll',
        'LICENSE',
        'README.txt',
        'THIRD_PARTY_NOTICES.md'
    ) | Sort-Object
    $actual = @(
        Get-ChildItem -LiteralPath $PackageDirectory -Recurse -File |
            ForEach-Object {
                [IO.Path]::GetRelativePath($PackageDirectory, $_.FullName).Replace('\', '/')
            }
    ) | Sort-Object

    $difference = @(Compare-Object -ReferenceObject $expected -DifferenceObject $actual)
    if ($difference.Count -ne 0) {
        $description = $difference |
            ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }
        throw "Staged N.I.N.A. package does not match its allowlist:`n$($description -join "`n")"
    }
}

$integrationRoot = [IO.Path]::GetFullPath($PSScriptRoot)
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $integrationRoot '../..'))
$project = Join-Path $integrationRoot 'src/AutoPierCam.NINA/AutoPierCam.NINA.csproj'
$outputRoot = Resolve-FullPath -Path $OutputDirectory -BasePath $integrationRoot
$outputRootParent = [IO.Path]::GetDirectoryName($outputRoot)
if ([string]::IsNullOrWhiteSpace($outputRootParent) -or
    $outputRoot -eq [IO.Path]::GetPathRoot($outputRoot)) {
    throw "OutputDirectory must not be a filesystem root: $outputRoot"
}

$archiveName = "AutoPierCam.NINA.$Version.zip"
if ([string]::IsNullOrWhiteSpace($InstallerUrl)) {
    $InstallerUrl = "https://github.com/theatrus/autopiercam/releases/download/v$Version/$archiveName"
}

$installerUri = $null
if (-not [Uri]::TryCreate($InstallerUrl, [UriKind]::Absolute, [ref] $installerUri) -or
    $installerUri.Scheme -notin @('http', 'https')) {
    throw 'InstallerUrl must be an absolute http:// or https:// URL.'
}

if (-not [string]::IsNullOrWhiteSpace($FeaturedImageUrl)) {
    $featuredImageUri = $null
    if (-not [Uri]::TryCreate($FeaturedImageUrl, [UriKind]::Absolute, [ref] $featuredImageUri) -or
        $featuredImageUri.Scheme -notin @('http', 'https')) {
        throw 'FeaturedImageUrl must be empty or an absolute http:// or https:// URL.'
    }
}

$buildDirectory = Join-Path $outputRoot 'build'
$packageRoot = Join-Path $outputRoot 'package'
$packageDirectory = Join-Path $packageRoot 'AutoPierCam'
$archivePath = Join-Path $outputRoot $archiveName
$manifestPath = Join-Path $outputRoot "AutoPierCam.NINA.$Version.manifest.json"

Assert-StrictChildPath -Path $buildDirectory -Parent $outputRoot -Label 'Build directory'
Assert-StrictChildPath -Path $packageRoot -Parent $outputRoot -Label 'Package root'
Assert-StrictChildPath -Path $packageDirectory -Parent $packageRoot -Label 'Package directory'

New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
if (-not $PackageOnly) {
    Reset-Directory -Path $buildDirectory -Parent $outputRoot
    Reset-Directory -Path $packageRoot -Parent $outputRoot
    New-Item -ItemType Directory -Path $packageDirectory -Force | Out-Null
} elseif (-not (Test-Path -LiteralPath $packageDirectory -PathType Container)) {
    throw "-PackageOnly needs a staged package at $packageDirectory; run -StageOnly first."
}

foreach ($file in @($archivePath, $manifestPath)) {
    if (Test-Path -LiteralPath $file) {
        Remove-Item -LiteralPath $file -Force
    }
}

$pluginDll = Join-Path $packageDirectory 'AutoPierCam.NINA.dll'
if (-not $PackageOnly) {
    dotnet build $project `
        --configuration Release `
        --output $buildDirectory `
        -p:Version=$Version `
        -p:AssemblyVersion=$Version `
        -p:FileVersion=$Version
    if ($LASTEXITCODE -ne 0) {
        throw "AutoPierCam N.I.N.A. plugin build failed with exit code $LASTEXITCODE."
    }

    $builtPluginDll = Join-Path $buildDirectory 'AutoPierCam.NINA.dll'
    if (-not (Test-Path -LiteralPath $builtPluginDll -PathType Leaf)) {
        throw "Expected plugin assembly was not produced at $builtPluginDll."
    }

    Copy-Item -LiteralPath $builtPluginDll -Destination $pluginDll
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'LICENSE') `
        -Destination (Join-Path $packageDirectory 'LICENSE')
    Copy-Item -LiteralPath (Join-Path $integrationRoot 'PACKAGE_README.txt') `
        -Destination (Join-Path $packageDirectory 'README.txt')
    Copy-Item -LiteralPath (Join-Path $integrationRoot 'THIRD_PARTY_NOTICES.md') `
        -Destination (Join-Path $packageDirectory 'THIRD_PARTY_NOTICES.md')
}

Assert-PackageAllowlist -PackageDirectory $packageDirectory

if ($StageOnly) {
    [pscustomobject]@{
        Package = $packageDirectory
        Plugin = $pluginDll
        Files = @(Get-ChildItem -LiteralPath $packageDirectory -File | Select-Object -ExpandProperty Name)
    }
    return
}

Compress-Archive -Path (Join-Path $packageDirectory '*') -DestinationPath $archivePath

$checksum = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
$versionParts = @($Version.Split('.') | ForEach-Object { [int] $_ })
$manifest = [ordered]@{
    Name = 'AutoPierCam'
    Identifier = 'cb626d89-4f49-454f-8d42-01153902d12b'
    Version = [ordered]@{
        Major = $versionParts[0]
        Minor = $versionParts[1]
        Patch = $versionParts[2]
        Build = $versionParts[3]
    }
    Author = 'Yann Ramin'
    Homepage = 'https://github.com/theatrus/autopiercam'
    Repository = 'https://github.com/theatrus/autopiercam'
    License = 'Apache-2.0'
    LicenseURL = 'https://github.com/theatrus/autopiercam/blob/main/LICENSE'
    ChangelogURL = 'https://github.com/theatrus/autopiercam/releases'
    Tags = @('pier camera', 'observatory', 'monitoring', 'preview')
    MinimumApplicationVersion = [ordered]@{
        Major = 3
        Minor = 2
        Patch = 0
        Build = 9001
    }
    Descriptions = [ordered]@{
        ShortDescription = 'Show the latest AutoPierCam pier camera snapshot in the Imaging tab'
        LongDescription = 'Adds a read-only Pier Camera panel to the N.I.N.A. Imaging tab. It connects automatically to the local AutoPierCam preview stream, displays frame metadata, retains the last good snapshot during interruptions, and never takes ownership of the camera.'
        FeaturedImageURL = $FeaturedImageUrl
        ScreenshotURL = ''
        AltScreenshotURL = ''
    }
    Installer = [ordered]@{
        URL = $InstallerUrl
        Type = 'ARCHIVE'
        Checksum = $checksum
        ChecksumType = 'SHA256'
    }
}

$json = $manifest | ConvertTo-Json -Depth 10
[IO.File]::WriteAllText($manifestPath, $json, [Text.UTF8Encoding]::new($false))

[pscustomobject]@{
    Archive = $archivePath
    Manifest = $manifestPath
    Checksum = $checksum
    InstallerUrl = $InstallerUrl
    PackageFiles = @(Get-ChildItem -LiteralPath $packageDirectory -File | Select-Object -ExpandProperty Name)
}
