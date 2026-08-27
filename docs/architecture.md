# AutoPierCam architecture

## Product boundary

The first product is a per-user desktop agent, not a Windows Service. A service
runs in noninteractive session 0 and cannot safely own a user's notification
area icon. If capture must later begin before login, split it into a service
that owns the camera and a small per-session tray/UI companion.

The intended process and data flow is:

    AutoPierCam WinUI viewer/configurator
                   |
        versioned local named-pipe IPC
                   |
      Rust tray agent and supervisor
                   |
       dedicated ASI camera thread
                   |
       bounded latest-frame fan-out
         |          |          |
      preview     stills     video sampler
         |          |          |
         +------ durable spool-+
                         |
                 durable upload queue

Only the camera thread calls ASICamera2. The UI never loads the vendor DLL or
opens a camera.

## Components

### ZWO adapter

autopiercam-asi loads an application-controlled absolute DLL path, resolves the
documented C ABI, and retains the module for every function pointer and camera
handle. It represents vendor enums as open Rust values so a future SDK cannot
create undefined behavior by returning a new value.

The lifecycle is enforced as:

    Disconnected -> Open -> Initialized -> Configured -> Streaming
          ^                                            |
          +------------ Stop -> Close -----------------+

Reconfiguration always transitions through Stopped. Hot-unplug errors trigger
best-effort stop/close, re-enumeration, and exponential reconnect backoff.
Persisted selection will use the camera serial number rather than the transient
SDK camera index.

### Portable core

autopiercam-core owns:

- schema-versioned configuration and validation;
- immutable frame metadata;
- raw luminance statistics and adaptive exposure decisions;
- Bayer phase handling and debayering;
- capture schedules and day/night hysteresis;
- artifact names, retention decisions, and sink interfaces.

It contains no WinUI, tray, or Windows SDK dependency and can be tested with a
mock camera backend.

### Tray agent

The agent is a normal per-user background executable. On Windows its primary
thread owns the notification icon and event loop. A supervisor owns cancellation,
camera reconnect state, sink worker health, config revisions, and IPC.

The tray menu will expose Status, Open AutoPierCam, Pause/Resume capture, Capture
now, and Exit. Exit performs an ordered shutdown rather than terminating the
process abruptly.

### WinUI application

The C#/XAML WinUI 3 app is deliberately thin. It displays:

- current preview and capture timestamp;
- connection, exposure, gain, temperature, queue, and disk status;
- camera and output settings with capability-aware ranges;
- upload and segmented-video settings;
- recent artifacts and recoverable errors.

Configuration writes include the revision the UI read. The agent validates and
atomically persists a replacement only when that revision still matches.

## Frame pipeline and backpressure

The ASI676MC produces about 12.6 MB per full-resolution RAW8 frame and about
37.9 MB per RGB24 frame. Processing every full-resolution RGB frame is neither
necessary nor a safe default.

The camera thread preallocates a small RAW buffer ring and drains video mode
continuously. Consumers have independent policies:

- Exposure statistics: latest frame wins; work on a sparse raw sample.
- Preview: capacity one, latest wins, debayer and downscale at 2 to 5 fps.
- Stills: bounded scheduled queue; a missed deadline is observable.
- Video: sample at configured output fps before conversion/encoding.
- Upload: accepts only durable completed files, never a borrowed frame buffer.

No filesystem, HTTP, database, UI, or encoder operation runs on the camera
thread.

## Day/night exposure strategy

Phase one uses the SDK's auto exposure and gain only when the camera reports
that both controls support automatic mode. AutoPierCam clamps them with maximum
exposure, maximum gain, and target-brightness controls discovered at runtime.

The deterministic controller in phase two operates in log exposure-value space:

1. Calculate raw p50, p90, and highlight-clipping fraction from a sparse sample.
2. Adjust exposure first within the current mode's bounds.
3. Add gain only after exposure reaches its mode limit.
4. Rate-limit changes and require several samples before reversing direction.
5. Enter or leave night mode only after sustained evidence and hysteresis.
6. Record requested and read-back exposure/gain with every selected frame.

Day mode favors short exposure, low gain, highlight protection, and faster
cadence. Night mode permits seconds-long exposure, higher gain, and a slower
still cadence. Solar elevation can later act as a prior when site coordinates
are configured, but image feedback remains authoritative for storms, enclosure
lights, and obstructions.

RAW8 is the preview/security path. RAW16 preserves night still dynamic range;
its valid-bit alignment will be characterized on both target cameras before
normalization. Debayer is applied only after a sink selects a frame. The current
bilinear implementation is the reference path; a higher-quality method can be
added behind the same interface.

## Still, video, and upload storage

Writers create a unique temporary file in the destination directory, explicitly
flush and sync it, and atomically publish it without replacing an existing
artifact. Only finalized artifacts enter the upload queue.

Security video starts with an FFmpeg child process receiving sampled raw or RGB
frames. It writes short H.264 Matroska segments because each completed segment
is independently usable after a crash. A segment is finalized and validated
before it is renamed and queued. Optional MP4 remuxing happens after close.

SQLite records artifacts, upload attempts, acknowledgements, retention, and
stable idempotency keys. One thread owns database writes. Upload retries use
bounded exponential backoff with jitter and never delay capture.

Retention is enforced by both age and disk quota. When space is critically low,
new recording pauses before it can fill the volume; an explicit health error is
shown rather than deleting unacknowledged data silently.

## Local IPC and security

The Windows transport is a named pipe restricted to the current logon SID with
remote clients rejected. Unix builds can use a Unix-domain socket behind the
same protocol.

Control messages are length-prefixed UTF-8 JSON envelopes containing protocol
version, request id, method, and payload. Preview data uses a separate
latest-only stream containing length-prefixed metadata followed by JPEG bytes.
Separating preview prevents an unresponsive image consumer from delaying
control responses.

Upload bearer material is never written into TOML. Configuration holds a
credential reference or environment-variable name; packaged Windows builds
will use Credential Manager or DPAPI.

## Shutdown and recovery

Ordered shutdown:

1. Stop accepting work and set cancellation.
2. Stop and close the camera on its owner thread.
3. Close sink queues.
4. Finish the current still and video segment.
5. Commit database and upload checkpoints.
6. Remove the tray icon and exit its event loop.

Startup reconciles database rows with the spool, quarantines incomplete stills,
validates recoverable video segments, resumes upload attempts, and begins camera
reconnect. An at-logon Scheduled Task with restart-on-failure is appropriate for
an unattended pier machine.

## Delivery sequence

1. Hardware vertical slice: enumerate, probe, RAW8 capture, debayer, encode.
2. Camera-owned worker, reusable buffers, mock backend, reconnect state machine.
3. Tray agent, versioned named pipe, status/config persistence.
4. WinUI preview and configuration.
5. Deterministic day/night controller and RAW16 characterization.
6. Atomic still spool and durable HTTP upload queue.
7. FFmpeg segmented recording, recovery, retention, and packaging.

The repository has completed item 1 plus the reusable-buffer, bounded-writer,
and auto-settling portion of item 2. It has also established a buildable WinUI
shell and the configuration/protocol seams needed by the remaining work.
