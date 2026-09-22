[CmdletBinding()]
param([Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string] $Version)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$output = Join-Path $repositoryRoot 'artifacts/release'
$pluginRoot = Join-Path $repositoryRoot 'integrations/AutoPierCam.NINA/artifacts/nina-plugin'
$msiName = "AutoPierCam-$Version-x64.msi"
$zipName = "AutoPierCam.NINA.$Version.0.zip"
$manifestName = "AutoPierCam.NINA.$Version.0.manifest.json"
$sources = @(
    (Join-Path $repositoryRoot "artifacts/installer/output/$msiName"),
    (Join-Path $pluginRoot $zipName),
    (Join-Path $pluginRoot $manifestName)
)
foreach ($source in $sources) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing release asset: $source" }
}
$manifest = Get-Content -LiteralPath $sources[2] -Raw | ConvertFrom-Json
$zipHash = (Get-FileHash -LiteralPath $sources[1] -Algorithm SHA256).Hash
if ($manifest.Installer.ChecksumType -cne 'SHA256' -or $manifest.Installer.Checksum -ine $zipHash) {
    throw 'N.I.N.A. manifest checksum does not match the final signed archive.'
}
if ($manifest.Installer.URL -cne "https://github.com/theatrus/autopiercam/releases/download/v$Version/$zipName") {
    throw 'N.I.N.A. manifest does not point to this release archive.'
}
New-Item -ItemType Directory -Path $output -Force | Out-Null
if ((Get-Item -LiteralPath $output).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw 'Release output must not be a reparse point.'
}
$expected = @($msiName, $zipName, $manifestName, 'SHA256SUMS.txt')
foreach ($item in Get-ChildItem -LiteralPath $output -Force) {
    if ($item.PSIsContainer -or $item.Name -cnotin $expected -or
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Unexpected release output entry; refusing to publish it: $($item.FullName)"
    }
}
$checksums = foreach ($source in $sources) {
    $name = [IO.Path]::GetFileName($source)
    $destination = Join-Path $output $name
    Copy-Item -LiteralPath $source -Destination $destination -Force
    $hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $name"
}
[IO.File]::WriteAllText((Join-Path $output 'SHA256SUMS.txt'), ($checksums -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
Write-Host "Prepared MSI, N.I.N.A. archive/manifest, and SHA256SUMS.txt in $output"
