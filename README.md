# AutoPierCam

AutoPierCam is a Windows-first, portable Rust capture agent for ZWO ASI
planetary cameras. It is intended to live in the system tray, adapt between
bright days and dark nights, save debayered stills, maintain short security
video segments, and optionally upload completed artifacts.

The repository currently contains the first hardware-validated vertical slice:

- dynamic loading of the bundled ZWO ASI SDK 1.41;
- checked C ABI layouts and safe camera lifecycle/control wrappers;
- camera enumeration and capability probing without capture or exposure/gain
  changes (opening normalizes the SDK's persisted dark-subtraction flag);
- bounded full-resolution RAW8 capture from the attached ASI676MC;
- bilinear RG/BG/GR/GB debayering, JPEG/PNG output, and luminance statistics;
- a validated, forward-looking TOML configuration model;
- a continuous camera drain loop with auto-exposure settling and a bounded
  still-writer queue;
- a buildable unpackaged WinUI 3 viewer/settings shell.

## Quick start

From a 64-bit Windows Rust toolchain:

    cargo test --workspace
    cargo run -p autopiercam -- list
    cargo run -p autopiercam -- probe --camera-id 0
    cargo run --release -p autopiercam -- snapshot --camera-id 0 --output captures/test.jpg
    Copy-Item autopiercam.example.toml autopiercam.local.toml
    cargo run --release -p autopiercam -- run --config autopiercam.local.toml
    dotnet build apps/AutoPierCam.Viewer/AutoPierCam.Viewer.csproj

Set AUTOPIERCAM_ASI_SDK_PATH or pass --sdk when the SDK DLL is not in the
bundled location. The x64 runtime DLL is expected at:

    vendor/zwo/ASI SDK/lib/x64/ASICamera2.dll

The ZWO library is redistributed under its included MIT-style license. The
camera driver is installed separately.

## Workspace

- crates/autopiercam-asi: dynamically loaded ASICamera2 wrapper.
- crates/autopiercam-core: portable configuration and image processing.
- crates/autopiercam: diagnostic CLI and hardware smoke-test path.
- apps/AutoPierCam.Viewer: unpackaged WinUI 3 viewer/settings shell.
- docs/architecture.md: target agent, UI, storage, video, and upload design.
- autopiercam.example.toml: initial configuration contract.

## Current hardware result

SDK enumeration found one ZWO ASI676MC at camera id 0: 3552 by 3552, RG Bayer,
12-bit ADC, 2.0 micrometre pixels, USB3, with RAW8/RGB24/Y8/RAW16 support.
The snapshot command captured, debayered, encoded, and closed the camera
successfully.

## Important constraints

- Only one worker thread will own an open camera and all SDK calls.
- ROI changes occur only while capture is stopped.
- Frame reads always use checked buffer sizes and finite timeouts.
- Slow writers, preview clients, encoders, and uploads may drop or queue their
  own work, but must never block the SDK drain loop.
- Auto-exposure limits use microseconds in AutoPierCam configuration. SDK 1.41
  documentation calls control 11 microseconds, while the ASI676MC exposes
  AutoExpMaxExpMS. The implementation detects the runtime control name and
  converts accordingly.
