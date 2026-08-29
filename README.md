# AutoPierCam

AutoPierCam is a Windows-first capture suite for ZWO ASI planetary cameras,
built around a portable Rust core and capture agent. On Windows it lives in the
system tray, adapts between bright days and dark nights, saves debayered stills,
and can upload completed artifacts. Short security-video segments remain a
planned storage sink rather than an implemented feature.

AutoPierCam 0.1.0 is authored by Yann Ramin and licensed under the
[Apache License 2.0](LICENSE). Its canonical repository is
[github.com/theatrus/autopiercam](https://github.com/theatrus/autopiercam).

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
- per-user Windows installer packaging with optional start-at-sign-in,
  self-contained Viewer deployment, orderly upgrade shutdown, and retained
  user data;
- a current-user-only, remote-rejected named pipe with versioned JSON control;
- a separate same-user, outbound-only preview pipe backed by a bounded
  latest-frame producer and isolated fan-out to four concurrent viewers;
- a durable SQLite HTTP outbox that records atomically published JPEGs, resumes
  retries after restart, and supports bounded operator inspection and safe,
  revision-fenced requeue without delaying capture for network work;
- a Windows-safe retention worker with age, managed-byte, and minimum-free-space
  policies, upload-ledger protection, and scheduled-capture suspension when
  protected data makes a byte target impossible;
- atomic, content-revisioned configuration replacement through that pipe;
- a buildable unpackaged WinUI 3 viewer that shows live status, requests an
  immediate capture, renders the latest JPEG preview, manages the durable
  outbox, reports storage pressure, and edits the supported live configuration
  fields;
- a separately packaged N.I.N.A. 3.2 plugin that adds a read-only **Pier
  Camera** panel to the Imaging tab.

## Install on Windows

The x64 MSI installs AutoPierCam for the current Windows user under
`%LOCALAPPDATA%\Programs\AutoPierCam`. It includes the ZWO SDK runtime and a
self-contained Viewer; install the ZWO Windows camera driver separately first.
Windows 10 version 1809 or newer is required.

Configuration and user data live outside the application directory under
`%LOCALAPPDATA%\AutoPierCam`: captures default to `captures\`, persistent logs
to `logs\`, and the upload ledger sits beside the configuration. The optional
start-at-sign-in feature is selected by default. Upgrade and uninstall preserve
all user data deliberately.

See [Installing AutoPierCam on Windows](docs/installation.md) for interactive
and silent installation, exact paths, diagnostics, upgrades, and removal.

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

Offline upload-ledger maintenance does not load the camera SDK. Stop the agent
first—even when uploads are disabled—then migrate an exact legacy v3 ledger
(or verify v4) with:

    cargo run --release -p autopiercam -- upload-ledger migrate --config autopiercam.toml

A fully drained v4 ledger can be archived and retired only with its exact
32-character ledger ID:

    cargo run --release -p autopiercam -- upload-ledger archive --config autopiercam.toml --expected-ledger-id <ledger-id>

The command prints the verified archive/retired paths and archive SHA-256. See
`docs/upload.md` for refusal conditions and partial-operation recovery.

The tray also accepts `AUTOPIERCAM_CONFIG` and `AUTOPIERCAM_ASI_SDK_PATH`.
Its menu can open the Viewer, capture and log folders, pause/resume scheduled
stills, capture one frame immediately, and perform an ordered shutdown. Logs use
UTC daily filenames and the newest 14 files are retained. Installers request an
orderly shutdown and wait up to 30 seconds before upgrade or uninstall; an
operator can issue the same request without loading the camera SDK:

    autopiercam shutdown-agent --if-running --timeout-seconds 30

`autopiercam run` remains available for a console-only installation or capture
test.

Set AUTOPIERCAM_ASI_SDK_PATH or pass --sdk when the SDK DLL is not in the
bundled location. The x64 runtime DLL is expected at:

    vendor/zwo/ASI SDK/lib/x64/ASICamera2.dll

The ZWO library is redistributed under its included MIT-style license; it is
not relicensed under AutoPierCam's Apache-2.0 license. The camera driver is
installed separately. See `THIRD_PARTY_NOTICES.md` for details.

## N.I.N.A. integration

The separately distributed AutoPierCam plugin requires N.I.N.A. 3.2 or newer.
It adds a **Pier Camera** dockable panel to Imaging, connects as the same Windows
user to the local preview stream, and retains a visibly stale last snapshot
during interruptions. It is strictly read-only and never opens or owns the ZWO
camera, so the N.I.N.A. panel and AutoPierCam Viewer can run together.

The plugin is not installed by the AutoPierCam MSI. Extract its archive to
`%LOCALAPPDATA%\NINA\Plugins\3.0.0\AutoPierCam` while N.I.N.A. is closed. See
[AutoPierCam for N.I.N.A.](integrations/AutoPierCam.NINA/README.md) for use,
troubleshooting, development, and package details.

## Workspace

- crates/autopiercam-asi: dynamically loaded ASICamera2 wrapper.
- crates/autopiercam-core: portable configuration and image processing.
- crates/autopiercam-protocol: control envelopes plus bounded preview-v1 framing.
- crates/autopiercam: reusable capture engine plus diagnostic CLI.
- crates/autopiercam-tray: Windows notification-area host and named-pipe server.
- apps/AutoPierCam.Viewer: unpackaged WinUI 3 status/settings application.
- installer and scripts/Build-Installer.ps1: per-user WiX 6 MSI and its guarded
  staging, packaging, and verification pipeline.
- integrations/AutoPierCam.NINA: N.I.N.A. Imaging-tab plugin, tests, and release
  archive/manifest builder.
- docs/installation.md: installed paths, start-at-sign-in, logs, diagnostics,
  silent setup, removal, and installer build/signing seam.
- docs/architecture.md: target agent, UI, storage, video, and upload design.
- docs/upload.md: implemented HTTP request, retry, and durability contract.
- docs/retention.md: implemented retention policy, upload-ledger safety, and
  storage-pressure behavior.
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
  does not lose a durable intent. The upload worker never deletes a local JPEG;
  the independent retention worker may delete only a separately authorized
  artifact.
- A ledger is bound to the canonical capture directory, normalized HTTP
  endpoint, and a one-way authorization identity. Root or endpoint changes
  fail closed. Credential rotation is accepted only after every upload is
  terminal and no crash-gap artifact awaits reconciliation; see
  `docs/upload.md` for the recovery and operator contract.
- Retention inventories only regular direct-child JPEGs with AutoPierCam's exact
  generated filename grammar. Age deletion is controlled by
  `capture.retention_days`; optional `capture.retention_max_bytes` and
  `capture.retention_min_free_bytes` limits reclaim the oldest eligible files
  until both byte targets are met. `capture.keep_latest` always protects the
  newest managed image. See `docs/retention.md` for the fail-closed ledger
  rules.
- Pending, active, retrying, permanently failed, unknown crash-gap, and otherwise
  unverified upload artifacts are protected from retention. Completed uploads
  and preactivation history can be reclaimed after an exact state and file
  identity recheck. With uploads disabled, any existing ledger or sidecar makes
  retention protect all managed captures.
- If protected data prevents a configured byte target from being met, status
  reports blocked storage pressure and scheduled still persistence pauses.
  Preview, camera draining, and an explicit Capture now request remain
  available.
- Preview candidates are sampled at most every 500 milliseconds even while
  scheduled still capture is paused. A one-slot latest-only queue feeds an
  off-camera-thread 1280-pixel-edge, JPEG-quality-75 encoder. Up to four
  independent active preview clients can coexist; a fifth retries when a slot
  becomes available. A slow client cannot stall capture or the other clients.
- Auto-exposure limits use microseconds in AutoPierCam configuration. SDK 1.41
  documentation calls control 11 microseconds, while the ASI676MC exposes
  AutoExpMaxExpMS. The implementation detects the runtime control name and
  converts accordingly.
- The Viewer renders the live preview and marks it stale after five seconds
  without a new frame. Its camera selector remains read-only. Max exposure,
  max gain, still interval, managed-image and minimum-free-space limits, upload
  endpoint/enable, and video enable are backed by versioned configuration
  replacement. Capability checks keep new retention fields safe when the Viewer
  talks to an older agent. It reports durable upload counts, offers newest-first
  outbox inspection and confirmed requeue of eligible permanent failures, and
  shows managed/protected/reclaimable bytes plus the latest retention sweep.
  Security video remains a designed seam rather than an active sink.
