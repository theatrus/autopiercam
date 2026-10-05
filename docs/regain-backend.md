# Regain camera backend

AutoPierCam 0.2.21 adopts Regain 0.5.8 continuous acquisition, with boundary-safe
scalar control changes and delivery-side FPS pacing. AutoPierCam 0.2.20 adopted
Regain 0.5.7 for bounded malformed-frame and SDK failed-exposure recovery. Published
AutoPierCam 0.2.19 adopted Regain 0.5.6 from its `v0.5.6.0` release, including
ASI676MC Direct USB video in [Regain PR #13](https://github.com/pulsarfab/regain/pull/13),
following the ASI662MC video support adopted in AutoPierCam 0.2.18.
Cargo.toml and Cargo.lock pin the exact upstream commit.
Published AutoPierCam 0.2.16 used targeted reconnect fixes from Regain PR #10
but still rejected USB 2 connections to these color cameras.
AutoPierCam 0.2.14 used the `v0.5.3.0` release; 0.2.13 used Regain 0.5.2.
ASI662MC Direct USB requires AutoPierCam 0.2.13 or newer.
There are no AutoPierCam SDK bindings. The `regain-device` entry
point calls the upstream drivers and USB recovery helper; `regain-core::Worker` owns the
framed transport, deadlines and process supervision.

## Selection

ASI662MC/ASI676MC Direct USB uses the worker's advertised video mode for exposures up to
30 seconds. Longer exposures use retained still capture without lowering the
configured exposure ceiling. SDK uses native video on its supported cameras.

The exclusive acquisition owner keeps draining independently of IPC, storage and
preview pacing. A bounded latest-frame slot replaces older frames; fractional FPS
limits delivery, not sensor output. Exposure/gain edits are applied after a frame
drains without restarting native video. Structural edits stop/reconfigure/start.
Transitions clear old buffers and discard at least two reads for old-plus-new
exposure time. This conservative fence is not optical proof of a sensor latch.
No extra discovery sweeps or implicit physical USB resets are added.

Watchdogs include delivery cadence, prior exposure, the transition fence and
advertised framing-recovery overhead. SDK read timeouts back off by 100 ms
without stop/start; no-frame waits and terminal errors remain bounded. Diagnostic
status distinguishes acquisition, delivery, replacement and pending settings.
Replaced frames are consumer decimation, not measured USB drops. Invalid pixels
are never accepted. The legacy single-exposure path remains available to older
workers and retains its recovery budget.

### Fault diagnosis and bounded recovery

The legacy single-exposure path retries SDK `ASI_EXP_FAILED` (status 3) once by stopping and
starting a fresh exposure on the same handle. This is an exposure status, not
an SDK API error code. The original outer frame deadline remains in force;
no configuration, disconnect, download or arbitrary SDK API error is silently
retried. If the fresh exposure also fails, normal 30-second session recovery
still applies. Direct video similarly discards one malformed frame and restarts
its stream once; it never delivers malformed pixels or substitutes a replay.

At normal INFO logging, session startup identifies the backend and model.
Direct USB reports negotiated transport/packet size; SDK reports its
`usb3Host` flag separately from the camera's USB 3 capability (a capable camera
can be attached via USB 2). These use already-opened camera information, not
extra discovery or USB probes. A health summary is emitted on successful frames
at most once per minute. Routine Regain phases remain DEBUG.

A failed request or interruption of an unfinished capture emits a diagnostic
snapshot: exposure, gain, ROI, FPS cap, capture mode, elapsed request time, age
of the last valid frame, frame/recovery counts and the last 24 operational
breadcrumbs with relative times. Each breadcrumb is capped at 1,024 characters;
frame pixels and inventory responses are excluded. Worker errors retain the
operation, parent request deadline and native error chain. Include startup and
the first fault/recovery lines when reporting a failure, not just the last
status line. Existing identity logs may contain camera serials: redact these
before public issue reports. Diagnostics require no special debug build on the
affected machine once this change is released.

These changes do not establish that USB interference caused the reported
failure. Local capped ASI662MC USB 2 checks passed seven fixed 60-second frames
through each backend, including settling and saving. Both the released and
candidate Direct workers passed the 6.4/25/25/6.4/25-second transition sequence
without reproducing the reported malformed frame. Synthetic fault tests cover
the recovery paths; these successful local captures are not evidence that the
problem machine's cable, hub, power or controller is reliable.

The updated AutoPierCam USB 2 Direct pipeline also passed a capped gain-300
adaptive ramp: 6.4 seconds, 25.6 seconds, then four 60-second frames. Exposure
settled after six total frames (277.8 seconds), saved one still, and exited
cleanly. This covered the video-to-still transition with a 0.5 FPS cap and USB
reset disabled. Post-test edits only clarified diagnostic field names and
preserved full error-chain display; recovery logic was unchanged.

Upstream Windows USB 2 and USB 3 video matrices for each model passed 21 frames covering
ROI/full-frame, exposures through 30 seconds, fractional FPS, cancellation,
restart and a still regression. This is not a long-running stability or optical
accuracy guarantee. AutoPierCam adapter/pipeline tests use isolated simulators.

Settings has an explicit **Camera driver** choice:

- **Regain · ZWO SDK** (default): supports ASI662MC and ASI676MC through the
  vendor library. The installer still includes the reviewed SDK DLL.
- **Regain · ZWO Direct USB** (experimental): ASI662MC and ASI676MC support
  RAW16, bin 1, and exposures from 32 microseconds to 2,000 seconds.

Current source supports USB 2 high-speed as well as USB 3 for both models.
The bus negotiates the link; this does not switch backends or reset a port.
Regain checks the model, serial and bulk endpoint layout, and logs the selected
transport. USB 2 uses 512-byte bulk packets. Very short Bayer exposures use a
roughly 100 ms sensor frame interval while preserving integration time; this
prevents the observed ASI676 short-ROI retained-frame mismatch. USB 3 and
host-timed long-exposure timing remain unchanged.

Operator-authorized Windows USB 2 checks passed on ASI662MC and ASI676MC:
full-resolution frames, 30-second exposures, byte-identical retained replay,
deliberately interrupted host reads and non-packet-aligned ROI. The ASI676
production worker also passed close/reopen with cached identity. These are
not tests of physical USB disconnect/reset, optical accuracy, cold power-up,
or the installed Viewer's complete SDK-to-Direct handoff. Upstream 2600 P25 and
Duo USB 2 matrices also passed, with a Duo USB 3 regression after complete-readout
and tiny-ROI fixes. Both ASI6200 revisions have qualified transfer/recovery
results: extreme-gain row uniformity remains uncharacterized. These upstream
results do not expand AutoPierCam's supported color-camera scope.
See [upstream USB 2 evidence](https://github.com/pulsarfab/regain/blob/v0.5.4.0/docs/usb2-coverage-spike.md).

SDK remains the default; existing configurations do not switch drivers.
There is no automatic fallback.
Other camera vendors can be added behind the adapter as Regain implements them;
this version does not claim support for non-ZWO or monochrome cameras.

```toml
[camera]
driver = "zwo_sdk" # or "zwo_direct", explicitly
# serial = "exact-camera-serial"
exposure_control = "adaptive"
max_exposure_us = 60000000 # Both direct models support up to 2000000000
```

Keep the selected model filter. Backend changes clear the transient camera ID
in the Viewer and restart capture. A saved serial is verified on open.
An ambiguous selection never silently opens the first camera. A selected SDK ID
can identify one initial candidate; it is not durable identity. The verified
serial is retained in memory across retries even when the optional field is blank.
Identical Direct USB models cannot be distinguished by product ID; if their
serials are unknown, automatic capture refuses to probe them to find a match.

0.2.15's automatic serial-aware scan opened every SDK camera. **0.2.16 removes
that scan.** Initial discovery requests descriptors only, and the selected
camera's verified serial appears in cached Viewer choices after connecting.
Blank serial is allowed for one unambiguous candidate; hexadecimal serials match
case-insensitively. Serial verification precedes SDK initialization on reconnect.
Direct USB retains an opaque interface locator internally, opens only that
interface, and verifies its serial. It never falls back to another interface.

Inventory and selected identity survive capture worker replacement and exposure
settings changes. Capture retries stay at 30 seconds; failed/empty discovery has
a separate 5/10/20/30-minute capped backoff. No successful inventory is refreshed
automatically on fault. Reload camera list is still cache-only. To deliberately
rediscover changed hardware, stop/restart the agent when other cameras are idle.
An SDK ID or USB interface that changes after replug/reset may need this operator
action rather than an automatic scan. Camera driver changes invalidate that
backend's cached selection. No identity paths are persisted or sent over IPC.

SDK initial descriptor discovery still calls `ASIGetCameraProperty`, which can
internally open devices. Targeted SDK reopen calls the count API once to populate
the new process's device table but does not sweep properties or initialize other
cameras. This is not a system-wide interlock or proof of vendor SDK safety with
another application's camera. Synthetic call-count tests cover the reconnect
policy; the reported hardware deadlock has not been reproduced or proven fixed.

ASI662MC Direct USB provides 1920×1080 RGGB frames, gain 0–600, and an
approximately 100 ms minimum frame interval for short exposures. AutoPierCam's
preview-rate cap still applies. Centered ROI origins are aligned to 8 pixels
on both axes. Bin 2 is not supported. Direct mode uses the installed ZWO USB
driver on Windows but does not load the ZWO SDK.

## White balance

AutoPierCam 0.2.14 exposes Regain 0.5.3's shared software white balance in
Settings for both SDK and Direct USB. Published AutoPierCam 0.2.13 does not have
these controls. Disabled is the default and sends no WB command, retaining the
backend's existing behavior; SDK and Direct are not guaranteed to have matching
unmanaged colors.

Manual uses red/blue linear multipliers relative to green=1 (0.125–8), **not** the
ZWO SDK slider scale. Auto once estimates until it has a valid frame and then
holds the gains for that session. Continuous AWB smooths valid gray-world
estimates. Red/blue fields provide initial gains while AWB waits for suitable
signal. Dark or clipped/insufficient signal holds the preceding gains. Strongly
colored, sparse night-sky or narrowband scenes can defeat gray-world assumptions;
use manual gains when color consistency matters.

Regain applies the correction to RAW16 Bayer data before AutoPierCam's RAW8
conversion or debayering. Thus previews, JPEG/video output and 16-bit PNGs all
include the correction; an enabled WB PNG is not unmodified sensor data. The
application does not implement another estimator or apply WB a second time.
Managed SDK capture neutralizes native WB first and restores the previous values
on orderly close. A crash, hung worker or forced termination cannot guarantee
restoration; close failures are logged. No physical-camera WB calibration is
claimed.

```toml
[camera]
white_balance = { mode = "manual", red = 1.0, blue = 1.0 }
# Or mode = "once" / "continuous". Omit white_balance to disable management.
```

Changing WB restarts the capture session, including when disabling it. An
automatic fault retry starts from the saved gains: Auto once estimates again,
and Continuous starts smoothing again. Effective AWB estimates are not written
back to the user's configuration. Only the capture owner commands the worker;
the Viewer/IPC never opens an extra handle. Older agents do not advertise
`camera.white_balance`, so their Viewer controls stay disabled.

## Capture ownership and safety

Only the capture owner starts or commands a worker. Discovery runs before open,
never while acquiring, settling or paused. IPC and the Viewer use cached
inventory and previews; they do not open camera handles.

Frames use Regain's start/status/download API, paced to the configured preview
rate and still schedule. RAW16 is acquired for both output modes; RAW8 is
derived from the high byte locally. The adapter validates dimensions, byte
length, ROI alignment, RAW16 support and Bayer phase. Cleanup errors reject
the frame.

AutoPierCam's application exposure/gain controller is always active, including
for legacy configurations whose exposure_control is omitted or sdk. Existing
limits and gain preferences remain unchanged. SDK auto exposure is not used.
Edits during exposure wait for the next frame, without resetting settling.
A direct-backend exposure ceiling above its capabilities faults visibly rather
than silently shortening night exposures.

Regain calls have parent-enforced deadlines (15 s open, 5 s controls/start,
2 s status/stop/close, 10 s download); frame progress retains AutoPierCam's
exposure-aware deadline. A poll reporting 'exposing' is not a completed frame.
Faults terminate the worker. Once the camera-owning thread and its cleanup have
finished, the tray waits 30 seconds and automatically starts a fresh session with
the saved backend and camera selection. Repeated failures each wait 30 seconds;
the original error remains visible with a retry notice. Restart from the tray
or save corrected settings to retry sooner. Recording pause and queued capture
requests survive reconnection; quitting cancels the pending retry.
No replacement starts while the old owner is alive. Discovery runs only at
the start of the new session, never during active acquisition. USB recovery is
disabled unless explicitly configured as described below. Hub/controller reset
and cross-backend fallback are never performed. The
one-shot/headless CLI still returns its failure to the caller; automatic session
retry belongs to the tray supervisor.

### Opt-in USB port recovery (0.2.15)

Enable **Reset this camera's USB port after capture failure** in Viewer Settings,
or set `camera.usb_reset_on_fault = true`. Save the camera's explicit, nonzero
16-digit hexadecimal serial first. Changing this option restarts the camera
session so recovery can bind the physical device before acquisition begins.

On Windows, run `autopiercam-tray.exe` persistently with administrator rights,
under the same Windows account used for the existing configuration. Launch Viewer
normally to keep it unelevated; launching it from an elevated tray inherits that
tray's elevation. AutoPierCam does not install a service or scheduled task, change
the existing logon setup, or prompt for elevation during automatic recovery.
An unelevated reset attempt fails visibly and ordinary capture retry continues.
Windows requires elevation for the [hub port-cycle request](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/usbioctl/ni-usbioctl-ioctl_usb_hub_cycle_port).

Regain binds the selected camera serial to a physical device before opening it.
On a fatal frame-read/exposure failure or an exposure deadline, AutoPierCam first
closes and reaps the camera worker, then invokes Regain's bounded reset helper.
The helper rechecks device generation, identity and the immediate parent port;
it never resets an entire hub/controller or guesses among cameras. Unknown
models, missing/ambiguous identity, changed device generation and busy devices
fail closed. Binding failure leaves ordinary capture available without USB reset.
The pinned helper supports ASI662MC, ASI676MC, ASI2600MM Pro/Duo, ASI6200MM Pro
and ASI220MM Mini identities; this is not reset support for every SDK model.

There is at most one attempt per failed session and a five-minute cooldown per
running agent, including unsuccessful/permission-denied resets. Storage, encoder,
configuration and discovery failures do not trigger recovery. Shutdown or settings
reload before dispatch cancels it. Once dispatched, the bounded helper completes;
an abrupt host termination can interrupt recovery. The normal 30-second tray retry
then reopens the explicitly selected serial and reapplies camera/WB settings.
The headless one-shot CLI reports the failed capture even after a successful reset.

Port cycling does not guarantee removal of USB power and never switches an
external camera power supply. This integration is simulator-tested only; physical
AutoPierCam reset/reconnect behavior and multi-camera isolation need an explicit
hardware validation pass. Linux/macOS automatic reset is not enabled here.

Process isolation bounds a hung user-space call. It does not guarantee recovery
from a wedged kernel driver, USB controller, firmware or another application's
camera ownership. The earlier SDK discovery incident and shared hardening are
recorded in [AutoPierCam PR #12](https://github.com/theatrus/autopiercam/pull/12)
and [Regain PR #1](https://github.com/pulsarfab/regain/pull/1).

## Build and validation

```powershell
cargo build -p autopiercam -p autopiercam-tray -p autopiercam-regain-worker
cargo test -p autopiercam-camera -p autopiercam-regain-worker
```

Keep regain-device beside the capture executable. For development only,
AUTOPIERCAM_REGAIN_DIRECTORY may point to its directory. SDK paths are resolved
before worker launch; direct mode never loads the SDK DLL. The installer stages,
validates and signs the worker with the other programs. Driver source and
dependency licensing are included in the generated notice bundle.

Tests use Regain's --simulate backend, never hardware. They cover SDK and direct
frames, ownership, live-discovery rejection, serial mismatch, RAW16 conversion,
in-flight edits, long-exposure cancellation, download faults, process crashes and
hung downloads. CI runs these on Windows, Linux x64/ARM64 and macOS ARM64.

Physical ASI676MC validation on Windows passed SDK exposures at 1, 30 and 60
seconds, and full-frame Direct USB RAW16 exposures at 60, 120 and 2,000 seconds.
The 2,000-second integration took 2,000.190 seconds, returned 25,233,408 bytes,
and replayed the retained frame byte-for-byte without another exposure. The SDK
was not loaded; no read retries or cleanup errors occurred.

Both backends saved full-resolution 3552×3552 16-bit RGB PNGs. Active capture
cancellation took 0.385 seconds through the SDK and 0.005 seconds through direct
worker termination. A new direct capture saved successfully afterward, and all
test workers exited. The short post-cancellation run reached its bounded settling
deadline; it was not a convergence test. See
[Regain PR #3](https://github.com/pulsarfab/regain/pull/3) for upstream evidence.

For ASI662MC, [Regain's upstream validation](https://github.com/pulsarfab/regain/blob/a13b0c7af07a5bd3e4016a944a1829dd0e7e2f06/docs/asi662mc.md)
covers Windows captures from 32 microseconds through 600 seconds, retained-frame
replay, interrupted reads and bounded cancellation. Its advertised 2,000-second
limit has not been hardware-validated on the 662. Cold startup, optical accuracy
and Linux/macOS hardware also remain unvalidated. AutoPierCam's pin-update tests
use simulated ASI662MC full frames, centered ROIs, controls and the save pipeline;
they are not additional physical-camera validation.

Subsequent operator-authorized Windows checks captured from ASI662MC and
ASI676MC concurrently with opposite SDK/Direct backends. Both assignments
succeeded at 1-second exposure, including full-resolution saves. See the
[mixed-backend diagnostic report](asi662-coexistence.md) for startup-order
coverage, build provenance and limitations; the reported original no-capture
incident has not been reproduced.

Before release, still validate day/night convergence, native Bayer orientation,
rate limiting, live saves and USB interruption with operator approval. Simulator
success is not hardware validation.
