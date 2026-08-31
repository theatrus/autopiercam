# AutoPierCam for N.I.N.A.

This N.I.N.A. 3.2 plugin adds a **Pier Camera** panel to the Imaging tab. It is
a read-only viewer for the local AutoPierCam preview stream: N.I.N.A. does not
open the ZWO camera, change exposure or gain, or compete with the AutoPierCam
tray process for camera ownership.

## Use it

1. Install and start AutoPierCam for the same Windows user that runs N.I.N.A.
2. Close N.I.N.A. before installing or updating the plugin.
3. Extract the plugin archive into:

       %LOCALAPPDATA%\NINA\Plugins\3.0.0\AutoPierCam

4. Start N.I.N.A. 3.2 or newer and open the Imaging tab. Select the **Pier
   Camera** dockable panel if it is not already present in the current layout.

No endpoint, camera, or credential configuration is required. The plugin uses
the protected local `autopiercam-preview-v1` named pipe. If AutoPierCam is not
running yet, the panel waits and reconnects automatically.

The panel displays the latest snapshot, local capture time, age, dimensions,
exposure, gain, day/night mode, and the producer's skipped-preview count. A
lost or stalled stream does not blank the pane: the last good image remains
visible at reduced opacity and is clearly labeled stale.

## Troubleshooting

- **Waiting for AutoPierCam** — start the AutoPierCam tray application in the
  same Windows logon session.
- **Connected; waiting for the first frame** — verify that AutoPierCam has
  opened the camera and is capturing.
- **Preview was interrupted** — the plugin will retry automatically. Check the
  AutoPierCam tray status if it continues.
- **Could not display the newest frame** — the last valid snapshot is retained;
  inspect AutoPierCam logs if new frames continue to be rejected.

## Develop and package

Build and test from this directory:

    dotnet build AutoPierCam.NINA.slnx --configuration Release
    dotnet test AutoPierCam.NINA.slnx --configuration Release

Copying into a live N.I.N.A. plugin directory is deliberately opt-in:

    dotnet build src/AutoPierCam.NINA/AutoPierCam.NINA.csproj `
      --configuration Release -p:CopyToNina=true

Build a release archive and SHA-256 N.I.N.A. manifest:

    ./build-nina-package.ps1 -Version 0.1.0.0

The default download URL uses the matching three-part product release tag
(`v0.1.0` in this example) while the plugin archive and manifest retain the
four-part N.I.N.A. version. Pass `-ReleaseTag v0.1.0-preview.1` when packaging
a prerelease of that product version. Its three numeric components must continue
to match `-Version`. The manifest also defaults to the repository's published
AutoPierCam featured image; pass `-FeaturedImageUrl` only when a release needs
to reference a different absolute HTTP(S) image.

Use `-StageOnly` to stop after creating the allowlisted package directory (for
example, before code signing), then `-PackageOnly` to archive that exact staged
content and generate its checksum manifest.
