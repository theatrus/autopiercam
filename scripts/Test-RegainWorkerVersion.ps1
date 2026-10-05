[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'RegainWorkerVersion.ps1')
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location $repositoryRoot
try {
    $metadata = (& cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json)
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
    $version = ($metadata.packages | Where-Object name -eq 'autopiercam-regain-worker').version
    if ([string]::IsNullOrWhiteSpace($version)) { throw 'Worker package version is missing.' }
    # --version returns before driver dispatch: no discovery, device handles or resets.
    & cargo build --locked -p autopiercam-regain-worker
    if ($LASTEXITCODE -ne 0) { throw 'Worker build failed.' }
    $workerPath = Join-Path $metadata.target_directory 'debug/regain-device.exe'
    $output = (& $workerPath --version 2>&1 | Out-String).Trim()
    Assert-RegainWorkerVersion -Output $output -ExitCode $LASTEXITCODE -Version $version
    foreach ($case in @(
        @{ Output = "autopiercam-regain-worker $version (Regain 75c8e93bb9f5)"; ExitCode = 0 },
        @{ Output = "autopiercam-regain-worker 0.0.0 (Regain 0.5.10)"; ExitCode = 0 },
        @{ Output = ''; ExitCode = 0 },
        @{ Output = $output; ExitCode = 1 }
    )) {
        $rejected = $false
        try { Assert-RegainWorkerVersion @case -Version $version }
        catch { $rejected = $true }
        if (-not $rejected) { throw 'Invalid worker version result was accepted.' }
    }
    Write-Host "Regain worker packaging contract passed: $output; four invalid results rejected."
} finally {
    Pop-Location
}
