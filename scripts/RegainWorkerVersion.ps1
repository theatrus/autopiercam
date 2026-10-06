function Assert-RegainWorkerVersion {
    param(
        [string] $Output,
        [int] $ExitCode,
        [string] $Version
    )

    $expected = "autopiercam-regain-worker $Version (Regain 0.5.11)"
    if ($ExitCode -ne 0 -or $Output -cne $expected) {
        throw "Regain worker version mismatch: '$Output' (expected '$expected'; exit $ExitCode)."
    }
}
