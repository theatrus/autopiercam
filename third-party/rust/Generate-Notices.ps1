[CmdletBinding()]
param(
    [switch] $Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$requiredAboutVersion = 'cargo-about 0.9.1'
$targetTriple = 'x86_64-pc-windows-msvc'
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$configPath = Join-Path $PSScriptRoot 'about.toml'
$templatePath = Join-Path $PSScriptRoot 'licenses.hbs'
$outputPath = Join-Path $PSScriptRoot 'Rust-Third-Party-Licenses.md'
$jsonPath = [IO.Path]::GetTempFileName()
$renderedPath = [IO.Path]::GetTempFileName()
$normalizedPath = [IO.Path]::GetTempFileName()
$previousNoColor = $env:NO_COLOR

function Format-Diagnostics {
    param([object[]] $Lines)

    return (($Lines | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
}

try {
    Push-Location $repositoryRoot
    $env:NO_COLOR = '1'

    $actualAboutVersion = (& cargo about --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo-about is not installed. See third-party/rust/README.md.'
    }
    if ($actualAboutVersion -cne $requiredAboutVersion) {
        throw "Expected $requiredAboutVersion, found '$actualAboutVersion'."
    }

    $jsonDiagnostics = @(& cargo about -L debug -c never generate `
            --frozen `
            --workspace `
            --fail `
            --config $configPath `
            --format json `
            --output-file $jsonPath 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo-about inventory failed:`n$(Format-Diagnostics $jsonDiagnostics)"
    }

    $seriousDiagnostics = @($jsonDiagnostics | Where-Object {
            $_.ToString() -match '\[(WARN|ERROR)\]'
        })
    if ($seriousDiagnostics.Count -ne 0) {
        throw "cargo-about emitted warnings or errors:`n$(Format-Diagnostics $seriousDiagnostics)"
    }

    # Workspace members have their Apache license at the repository root, so
    # cargo-about cannot discover it relative to each member manifest. That
    # expected fallback is excluded from the third-party template. Any fallback
    # for a registry crate is a release-blocking loss of copyright text.
    $unexpectedFallbacks = @($jsonDiagnostics | Where-Object {
            $line = $_.ToString()
            $line -match 'falling back to canonical text' -and
            $line -notmatch "crate 'autopiercam(?:-(?:asi|core|protocol|tray))? [^']+'"
        })
    if ($unexpectedFallbacks.Count -ne 0) {
        throw "A third-party crate fell back to generic license text:`n$(Format-Diagnostics $unexpectedFallbacks)"
    }

    $aboutData = Get-Content -Raw -LiteralPath $jsonPath | ConvertFrom-Json
    $aboutPackages = @($aboutData.crates | ForEach-Object {
            "$($_.package.name) $($_.package.version)"
        })
    $uniqueAboutPackages = @($aboutPackages | Sort-Object -Unique)
    if ($uniqueAboutPackages.Count -ne $aboutPackages.Count) {
        throw 'The cargo-about graph contains package name/version collisions; strengthen the graph comparison before release.'
    }

    $treeOutput = @(& cargo tree `
            --frozen `
            --workspace `
            --target $targetTriple `
            --edges normal `
            --prefix none `
            --no-dedupe `
            --format '{p}' 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo tree failed:`n$(Format-Diagnostics $treeOutput)"
    }

    $treePackages = @($treeOutput | ForEach-Object {
            if ($_.ToString() -match '^(?<name>\S+) v(?<version>\S+)') {
                "$($Matches.name) $($Matches.version)"
            }
        } | Sort-Object -Unique)
    $graphDifference = @(Compare-Object $uniqueAboutPackages $treePackages)
    if ($graphDifference.Count -ne 0) {
        throw "cargo-about does not match Cargo's locked Windows normal-dependency graph:`n$($graphDifference | Out-String)"
    }

    $externalPackages = @($aboutData.crates | Where-Object { $_.package.source })
    $canonicalExternal = @($aboutData.licenses | Where-Object {
            -not $_.source_path -and
            @($_.used_by | Where-Object { $_.crate.source }).Count -ne 0
        })
    if ($canonicalExternal.Count -ne 0) {
        throw 'At least one third-party dependency has no source-backed license text.'
    }

    # NOTICE, COPYRIGHT, AUTHORS, and SPDX attribution files do not always look
    # like licenses to scanners. Fail closed unless every such packaged file is
    # represented by a generated source path (including checksum-backed
    # clarifications in about.toml).
    $coveredPaths = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($license in $aboutData.licenses) {
        if (-not $license.source_path) {
            continue
        }

        if ([IO.Path]::IsPathRooted($license.source_path)) {
            [void] $coveredPaths.Add([IO.Path]::GetFullPath($license.source_path))
            continue
        }

        foreach ($usedBy in $license.used_by) {
            $crateRoot = Split-Path -Parent $usedBy.crate.manifest_path
            $candidate = Join-Path $crateRoot $license.source_path
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                [void] $coveredPaths.Add([IO.Path]::GetFullPath($candidate))
            }
        }
    }

    $uncoveredAttributionFiles = [Collections.Generic.List[string]]::new()
    foreach ($entry in $externalPackages) {
        $crateRoot = Split-Path -Parent $entry.package.manifest_path
        $attributionFiles = @(Get-ChildItem -LiteralPath $crateRoot -Recurse -File | Where-Object {
                $_.Name -match '^(NOTICE|NOTICES|COPYRIGHT|COPYRIGHTS|AUTHORS)(\..*)?$|\.spdx$'
            })
        foreach ($file in $attributionFiles) {
            if (-not $coveredPaths.Contains([IO.Path]::GetFullPath($file.FullName))) {
                $uncoveredAttributionFiles.Add(
                    "$($entry.package.name) $($entry.package.version): $($file.FullName.Substring($crateRoot.Length + 1))"
                )
            }
        }
    }
    if ($uncoveredAttributionFiles.Count -ne 0) {
        throw "Packaged attribution files are missing from the report:`n$($uncoveredAttributionFiles -join "`n")"
    }

    $renderDiagnostics = @(& cargo about -L warn -c never generate `
            --frozen `
            --workspace `
            --fail `
            --config $configPath `
            --output-file $renderedPath `
            $templatePath 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo-about rendering failed:`n$(Format-Diagnostics $renderDiagnostics)"
    }
    if ($renderDiagnostics.Count -ne 0) {
        throw "cargo-about emitted diagnostics while rendering:`n$(Format-Diagnostics $renderDiagnostics)"
    }

    $rendered = [IO.File]::ReadAllText($renderedPath)
    $normalized = $rendered.Replace("`r`n", "`n").Replace("`r", "`n")
    $normalized = [Text.RegularExpressions.Regex]::Replace(
        $normalized,
        '[ \t]+(?=\n)',
        ''
    )
    $normalized = $normalized.TrimEnd([char[]] "`n") + "`n"
    [IO.File]::WriteAllText(
        $normalizedPath,
        $normalized,
        [Text.UTF8Encoding]::new($false)
    )

    if ($Check) {
        if (-not (Test-Path -LiteralPath $outputPath -PathType Leaf)) {
            throw "Generated report is missing: $outputPath"
        }
        $expectedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $normalizedPath).Hash
        $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $outputPath).Hash
        if ($expectedHash -cne $actualHash) {
            throw 'Rust-Third-Party-Licenses.md is stale; regenerate it without -Check.'
        }
    } else {
        Copy-Item -LiteralPath $normalizedPath -Destination $outputPath -Force
    }

    $reportHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $outputPath).Hash.ToLowerInvariant()
    Write-Host ("Validated {0} third-party crates ({1} packages including workspace members); SHA-256 {2}" -f `
            $externalPackages.Count, $aboutPackages.Count, $reportHash)
} finally {
    if ($null -eq $previousNoColor) {
        Remove-Item Env:NO_COLOR -ErrorAction SilentlyContinue
    } else {
        $env:NO_COLOR = $previousNoColor
    }
    Pop-Location -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $jsonPath, $renderedPath, $normalizedPath -Force -ErrorAction SilentlyContinue
}
