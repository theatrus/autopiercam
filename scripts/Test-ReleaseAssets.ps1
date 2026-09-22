[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('autopiercam-release-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
try {
    $scripts = New-Item -ItemType Directory -Path (Join-Path $fixture 'scripts')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'Stage-ReleaseAssets.ps1') -Destination $scripts.FullName
    $msiRoot = New-Item -ItemType Directory -Path (Join-Path $fixture 'artifacts/installer/output') -Force
    $pluginRoot = New-Item -ItemType Directory -Path (Join-Path $fixture 'integrations/AutoPierCam.NINA/artifacts/nina-plugin') -Force
    [IO.File]::WriteAllText((Join-Path $msiRoot.FullName 'AutoPierCam-0.1.0-x64.msi'), 'fixture-msi')
    $zip = Join-Path $pluginRoot.FullName 'AutoPierCam.NINA.0.1.0.0.zip'
    [IO.File]::WriteAllText($zip, 'fixture-archive')
    $manifestPath = Join-Path $pluginRoot.FullName 'AutoPierCam.NINA.0.1.0.0.manifest.json'
    $manifest = @{ Installer = @{
        ChecksumType = 'SHA256'
        Checksum = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
        URL = 'https://github.com/theatrus/autopiercam/releases/download/v0.1.0/AutoPierCam.NINA.0.1.0.0.zip'
    }}
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json))
    $stage = Join-Path $scripts.FullName 'Stage-ReleaseAssets.ps1'
    & $stage -Version 0.1.0
    $releaseRoot = Join-Path $fixture 'artifacts/release'
    $lines = @(Get-Content -LiteralPath (Join-Path $releaseRoot 'SHA256SUMS.txt'))
    if ($lines.Count -ne 3) { throw 'Expected three release checksums.' }
    foreach ($line in $lines) {
        if ($line -cnotmatch '^([0-9a-f]{64})  (.+)$') { throw 'Malformed checksum line.' }
        $expectedHash = $Matches[1]
        $actualHash = (Get-FileHash -LiteralPath (Join-Path $releaseRoot $Matches[2]) -Algorithm SHA256).Hash
        if ($actualHash -ine $expectedHash) { throw 'Checksum disagrees with staged content.' }
    }
    function Assert-Rejected([scriptblock] $Action) {
        $rejected = $false
        try { & $Action } catch { $rejected = $true }
        if (-not $rejected) { throw 'Unsafe release fixture was accepted.' }
    }
    [IO.File]::WriteAllText($zip, 'tampered')
    Assert-Rejected { & $stage -Version 0.1.0 }
    [IO.File]::WriteAllText($zip, 'fixture-archive')
    $manifest.Installer.URL = 'https://example.invalid/wrong.zip'
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json))
    Assert-Rejected { & $stage -Version 0.1.0 }
    $manifest.Installer.URL = 'https://github.com/theatrus/autopiercam/releases/download/v0.1.0/AutoPierCam.NINA.0.1.0.0.zip'
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json))
    [IO.File]::WriteAllText((Join-Path $releaseRoot 'unexpected.exe'), 'injected')
    Assert-Rejected { & $stage -Version 0.1.0 }
    Write-Host 'Release assets: exact checksums, tamper rejection, URL binding, and output allowlist passed.'
} finally {
    # Only this invocation's newly created, fixed-name private fixture is removed.
    $resolved = [IO.Path]::GetFullPath($fixture)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (-not $resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notmatch '^autopiercam-release-test-[0-9a-f]{32}$') {
        throw "Refusing cleanup outside the release-test fixture: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
