# Test-only dependency. FFmpeg is not bundled in AutoPierCam's MSI.
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$version = '9.0.2'
$expectedHash = '60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../artifacts/test-tools'))
New-Item -ItemType Directory -Path $root -Force | Out-Null
$archive = Join-Path $root "ffmpeg-$version-essentials_build.zip"
if (-not (Test-Path -LiteralPath $archive)) {
    Invoke-WebRequest -Uri "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-$version-essentials_build.zip" -OutFile $archive
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ine $expectedHash) {
    throw 'Test FFmpeg archive does not match its pinned publisher SHA-256; not executing it.'
}
$destination = Join-Path $root "ffmpeg-$version"
if (Test-Path -LiteralPath $destination) {
    if ((Get-Item -LiteralPath $destination).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'FFmpeg test directory must not be a reparse point.' }
} else {
    Expand-Archive -LiteralPath $archive -DestinationPath $destination
}
$binary = Join-Path $destination "ffmpeg-$version-essentials_build/bin/ffmpeg.exe"
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing extracted test FFmpeg: $binary" }
# Verify the extracted executable against the already hash-verified archive.
$zip = [IO.Compression.ZipFile]::OpenRead($archive)
try {
    $entry = $zip.GetEntry("ffmpeg-$version-essentials_build/bin/ffmpeg.exe")
    if ($null -eq $entry) { throw 'Expected FFmpeg executable not present in pinned archive.' }
    $stream = $entry.Open()
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $archiveBinaryHash = [Convert]::ToHexString($sha.ComputeHash($stream)) }
    finally { $sha.Dispose(); $stream.Dispose() }
    if ((Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -ine $archiveBinaryHash) { throw 'Extracted test FFmpeg was modified.' }
} finally { $zip.Dispose() }
$binary
