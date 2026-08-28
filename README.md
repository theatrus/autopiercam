# AutoPierCam

AutoPierCam is a Windows-first, portable Rust capture agent for ZWO ASI
planetary cameras. It is intended to live in the system tray, adapt between
bright days and dark nights, save debayered stills, maintain short security
video segments, and optionally upload completed artifacts.

The repository now contains a hardware-validated background capture slice:

- dynamic loading of the bundled ZWO ASI SDK 1.41;
- checked C ABI layouts and safe camera lifecycle/control wrappers;
- camera enumeration and capability probing without capture or exposure/gain
  changes (opening normalizes the SDK's persisted dark-subtraction flag);
- bounded full-resolution RAW8 capture from the attached ASI676MC;
- bilinear RG/BG/GR/GB debayering, JPEG/PNG output, and luminance statistics;
- a validated, forward-looking TOML configuration model;
- a continuous camera drain loop with auto-exposure settling and a bounded
  still-writer queue;
- a restartable camera supervisor with automatic 1/2/5/10/30-second reconnect
  backoff;
- a Windows notification-area host that owns and supervises the camera worker;
- a current-user-only, remote-rejected named pipe with versioned JSON control;
- a separate secured, outbound-only preview pipe backed by a bounded latest-frame
  producer;
- a durable SQLite HTTP outbox that records atomically published JPEGs, resumes
  retries after restart, and never delays capture for network work;
- atomic, content-revisioned configuration replacement through that pipe;
- a buildable unpackaged WinUI 3 viewer that shows live status, requests an
  immediate capture, renders the latest JPEG preview, and edits the supported
  live configuration fields.

## Quick start

From a 64-bit Windows Rust toolchain:

    cargo test --workspace
    cargo run -p autopiercam -- list
    cargo run -p autopiercam -- probe --camera-id 0
    cargo run --release -p autopiercam -- snapshot --camera-id 0 --output captures/test.jpg
    Copy-Item autopiercam.example.toml autopiercam.toml
    cargo run --release -p autopiercam-tray -- --config autopiercam.toml
    dotnet build apps/AutoPierCam.Viewer/AutoPierCam.Viewer.csproj
    dotnet run --project apps/AutoPierCam.Viewer/AutoPierCam.Viewer.csproj

The tray also accepts `AUTOPIERCAM_CONFIG` and `AUTOPIERCAM_ASI_SDK_PATH`.
Its menu can open the built Viewer, pause/resume scheduled stills, capture one
frame immediately, and perform an ordered shutdown. `autopiercam run` remains
available for a console-only installation or capture test.

Set AUTOPIERCAM_ASI_SDK_PATH or pass --sdk when the SDK DLL is not in the
bundled location. The x64 runtime DLL is expected at:

    vendor/zwo/ASI SDK/lib/x64/ASICamera2.dll

The ZWO library is redistributed under its included MIT-style license. The
camera driver is installed separately.

## Workspace

- crates/autopiercam-asi: dynamically loaded ASICamera2 wrapper.
- crates/autopiercam-core: portable configuration and image processing.
- crates/autopiercam-protocol: control envelopes plus bounded preview-v1 framing.
- crates/autopiercam: reusable capture engine plus diagnostic CLI.
- crates/autopiercam-tray: Windows notification-area host and named-pipe server.
- apps/AutoPierCam.Viewer: unpackaged WinUI 3 status/settings application.
- docs/architecture.md: target agent, UI, storage, video, and upload design.
- docs/upload.md: implemented HTTP request, retry, and durability contract.
- autopiercam.example.toml: initial configuration contract.

## Current hardware result

SDK enumeration found one ZWO ASI676MC at camera id 0: 3552 by 3552, RG Bayer,
12-bit ADC, 2.0 micrometre pixels, USB3, with RAW8/RGB24/Y8/RAW16 support.
The snapshot command captured, debayered, encoded, and closed the camera
successfully. The tray was also exercised end to end against this camera:
status, pause, resume, capture-now-while-paused, repeated pipe reconnects, and
protocol-driven ordered shutdown all completed successfully. A later recovery
test deliberately selected a nonexistent camera, observed the fault, restored
the configuration in the same tray process, and returned to capture without
counter regression. It also verified stale-revision rejection, two distinct
manual still requests, and the five-second bound imposed on an idle pipe
client. The WinUI Viewer then saved the complete configuration, scheduled a
camera restart, cleared its old preview, and recovered to a new live preview
session without reopening. A loopback upload test then returned HTTP 503 with a
three-second `Retry-After`; the agent retried the identical 2,211,675-byte JPEG,
idempotency key, and SHA-256 before receiving HTTP 204 while capture remained
healthy.

## Important constraints

- Only one worker thread will own an open camera and all SDK calls.
- ROI changes occur only while capture is stopped.
- Frame reads always use checked buffer sizes and finite timeouts.
- Slow writers, preview clients, encoders, and uploads apply independent
  backpressure policies and must never block the SDK drain loop. Upload work is
  recorded durably before its advisory wake signal is sent.
- HTTP upload intents, retry deadlines, acknowledgements, and terminal failures
  are persisted in a SQLite ledger beside the configuration file. The bounded
  in-process upload channel carries coalesced wake hints only, so a full channel
  does not lose a durable intent. The uploader never deletes the local JPEG.
- A ledger is bound to the canonical capture directory, normalized HTTP
  endpoint, and a one-way authorization identity. Root or endpoint changes
  fail closed. Credential rotation is accepted only after every upload is
  terminal and no crash-gap artifact awaits reconciliation; see
  `docs/upload.md` for the recovery and operator contract.
- Preview candidates are sampled at most every 500 milliseconds even while
  scheduled still capture is paused. A one-slot latest-only queue feeds an
  off-camera-thread 1280-pixel-edge, JPEG-quality-75 encoder.
- Auto-exposure limits use microseconds in AutoPierCam configuration. SDK 1.41
  documentation calls control 11 microseconds, while the ASI676MC exposes
  AutoExpMaxExpMS. The implementation detects the runtime control name and
  converts accordingly.
- The Viewer renders the live preview and marks it stale after five seconds
  without a new frame. Its camera selector remains read-only. Max exposure,
  max gain, still interval, upload endpoint/enable, and video enable are backed
  by versioned configuration replacement. It also reports durable upload
  pending, active, retrying, completed, and permanently failed counts. Security
  video remains a designed seam rather than an active sink.
