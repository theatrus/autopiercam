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
camera reconnect state, config-driven restarts, and IPC. Camera startup and
runtime faults are retried with a bounded 1/2/5/10/30-second backoff; a healthy
capture session resets the delay.

The tray menu exposes Status, Open AutoPierCam, Pause/Resume capture, Capture
now, and Exit. Exit performs an ordered shutdown rather than terminating the
process abruptly.

### WinUI application

The C#/XAML WinUI 3 app is deliberately thin. The current application displays
agent/camera state, frame counters, and the most recent artifact; it can request
an immediate still, render the latest JPEG preview with exposure/gain metadata,
and edit max exposure, max gain, still cadence, HTTP upload, and video
enablement. It preserves all hidden configuration fields when it writes a
complete replacement. The preview UI reports connecting, waiting, live,
reconnecting, stale, and malformed-frame conditions. A separate upload card
shows durable pending, active, retrying, completed, and permanently failed
counts plus the most recent success and failure details. Planned additions
include:

- temperature and disk status;
- camera and output settings with capability-aware ranges;
- segmented-video settings;
- recent artifacts and recoverable errors.

Configuration writes include the revision the UI read. The agent validates and
atomically persists a replacement only when that revision still matches, then
schedules a controlled camera restart. Revision conflicts force a refresh.

## Frame pipeline and backpressure

The ASI676MC produces about 12.6 MB per full-resolution RAW8 frame and about
37.9 MB per RGB24 frame. Processing every full-resolution RGB frame is neither
necessary nor a safe default.

The camera thread preallocates a small RAW buffer ring and drains video mode
continuously. Consumers have independent policies:

- Exposure statistics: latest frame wins; work on a sparse raw sample.
- Preview: a capacity-one latest-only queue samples at most every 500 ms,
  including while scheduled stills are paused; a separate thread debayers,
  aspect-preserving downscales to a 1280-pixel edge, and encodes JPEG at quality
  75. Replaced work increments the session's dropped-frame counter.
- Stills: bounded scheduled queue; a missed deadline is observable.
- Video: sample at configured output fps before conversion/encoding.
- Upload: SQLite owns a durable outbox of finalized artifacts. A bounded
  nonblocking channel carries coalesced wake hints only; one dedicated thread
  claims due rows and performs HTTP.

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
artifact. Windows uses a write-through move. Only then does the writer record
the canonical path, size, SHA-256, and idempotency key in a SQLite ledger beside
the configuration file. A nonblocking channel wake follows the durable insert;
if that channel is full, the existing wake and periodic poll still drain every
due ledger row.

The ledger is created only when upload is enabled and records an activation
boundary, canonical capture root, normalized endpoint, and a domain-separated
SHA-256 authorization fingerprint. First startup does not adopt older captures.
Later startup reconciliation scans eligible direct generated JPEG children,
closing the publish/record crash window and recovering files created while
upload was temporarily disabled. A ledger refuses a different root,
destination, live authorization identity, schema, or concurrent owner.

Rows transition through `pending`, `in_progress`, `retrying`, `completed`, and
`permanently_failed`. Retry deadlines, attempt counts, acknowledgements, and
failure details persist. Startup returns abandoned `in_progress` claims to
`pending`; completed and terminal rows remain as history. Before HTTP, the
worker opens and verifies the exact recorded file size and SHA-256. Missing or
changed artifacts become permanent failures so different bytes cannot reuse an
existing identity.

The dedicated thread streams the verified file with `PUT` to the validated
endpoint after standard URL normalization, explicit JPEG content type and
length, filename and digest-bound idempotency headers, and optional bearer
authorization loaded from a named environment variable. It never appends a
filename. Redirects are disabled. HTTPS uses the platform certificate verifier;
bearer authentication requires HTTPS. Connect and total request timeouts are
five and fifteen seconds.

Transport failures, HTTP 408/425/429, and 5xx responses retry with bounded
deterministic jitter; numeric `Retry-After` seconds act as a capped lower bound.
Other non-2xx responses are permanent. Neither completion nor failure removes
the local artifact. During shutdown, the active bounded request finishes and
its outcome is committed, an unsubmitted claim is released, retry waits are
interrupted, and all other intents remain durable. The full contract and
operator caveats are in `docs/upload.md`.

Planned security video starts with an FFmpeg child process receiving sampled
raw or RGB frames. It writes short H.264 Matroska segments because each
completed segment is independently usable after a crash. A segment is finalized
and validated before it is renamed and queued. Optional MP4 remuxing happens
after close.

Retention remains a later milestone. The current uploader never deletes a local
artifact or ledger row, and the configured age/keep-latest fields are not yet
enforced. Planned retention will account for both age and disk quota and must
never delete unacknowledged data silently.

## Local IPC and security

The Windows transport is a named pipe restricted to the current logon SID with
remote clients rejected. Unix builds can use a Unix-domain socket behind the
same protocol.

Control messages are length-prefixed UTF-8 JSON envelopes containing protocol
version, request id, method, and payload. Preview data uses the separate
outbound-only `autopiercam-preview-v1` pipe with the same current-user DACL,
remote-client rejection, and first-instance ownership. Each latest-only record
contains a bounded JSON metadata length and body (4 KiB maximum), followed by a
bounded JPEG length and body (4 MiB maximum). Dimensions are limited to a
1280-pixel edge and 1,638,400 pixels. A two-second write timeout evicts a stalled
consumer without delaying control responses. Camera-attempt generation,
process-wide sequence, and dropped-frame metadata describe resets, ordering,
and producer loss; clearing an established session stream causes a bounded
Viewer reconnect.

Upload bearer material is never written into TOML or SQLite. Configuration
names the environment variable from which the current worker loads it; the
ledger stores only a one-way authorization fingerprint. Credential rotation is
allowed when every row is terminal and fails closed while live work remains.
Packaged Windows builds will use Credential Manager or DPAPI.

## Shutdown and recovery

Ordered shutdown:

1. Stop accepting work and set cancellation.
2. Stop and close the camera on its owner thread.
3. Drain and close the still writer.
4. Finish and commit the active bounded HTTP result, release any claim not yet
   submitted, and leave pending/retrying rows in SQLite.
5. Close local IPC, remove the tray icon, and exit its event loop.

Future durable video work will also finalize the current video segment before
exit.

The implemented startup path validates configuration, begins camera connection,
and automatically retries startup or runtime faults. When upload is enabled it
opens and validates the bound ledger, recovers abandoned claims, reconciles the
capture directory against the activation watermark, and resumes due attempts.
Future startup work will quarantine incomplete stills and validate recoverable
video segments. An at-logon Scheduled Task with restart-on-failure is
appropriate for an unattended pier machine.

## Delivery sequence

1. Hardware vertical slice: enumerate, probe, RAW8 capture, debayer, encode.
2. Camera-owned worker, reusable buffers, mock backend, reconnect state machine.
3. Tray agent, versioned named pipe, status/config persistence.
4. WinUI preview and configuration.
5. Deterministic day/night controller and RAW16 characterization.
6. Atomic still spool and durable HTTP upload queue.
7. FFmpeg segmented recording, recovery, retention, and packaging.

The repository has completed item 1 and the reusable-buffer, bounded-writer,
auto-settling, host-control, and reconnect portions of item 2. Item 3 now has a
real tray host, secured one-request protocol-v1 control, atomic revisioned
configuration persistence, and restart application. Item 4 now has a live
WinUI preview, status, capture-now, and configuration client; artifact browsing
remains. Item 6 has atomic still publication and a durable SQLite HTTP outbox
with restart recovery, acknowledgements, retry state, terminal failure state,
and Viewer telemetry. Automated retention remains.
