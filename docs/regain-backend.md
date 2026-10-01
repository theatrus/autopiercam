# Regain camera backend

AutoPierCam 0.2.13 uses Regain 0.5.2 (Rust 1.89 or newer), pinned to release commit
`a13b0c7af07a5bd3e4016a944a1829dd0e7e2f06` in Cargo.toml and Cargo.lock.
ASI662MC Direct USB requires AutoPierCam 0.2.13 or newer.
There are no AutoPierCam SDK bindings. The camera-only `regain-device` entry
point calls the unchanged upstream drivers; `regain-core::Worker` owns the
framed transport, deadlines and process supervision.

## Selection

Settings has an explicit **Camera driver** choice:

- **Regain · ZWO SDK** (default): supports ASI662MC and ASI676MC through the
  vendor library. The installer still includes the reviewed SDK DLL.
- **Regain · ZWO Direct USB** (experimental): ASI662MC and ASI676MC support
  RAW16, bin 1, and exposures from 32 microseconds to 2,000 seconds.

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
Identical models require an exact serial; an ambiguous selection never silently
opens the first camera. SDK IDs remain useful for selecting distinct models
but are not durable USB identity.

ASI662MC Direct USB provides 1920×1080 RGGB frames, gain 0–600, and an
approximately 100 ms minimum frame interval for short exposures. AutoPierCam's
preview-rate cap still applies. Centered ROI origins are aligned to 8 pixels
on both axes. Bin 2 is not supported. Direct mode uses the installed ZWO USB
driver on Windows but does not load the ZWO SDK.

## Capture and safety

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
Faults terminate the worker. The tray stops automatic retries: restart from the
tray or save corrected settings to try again. No USB reset, port cycle, controller
reset, background rescan, or cross-backend recovery is performed.

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

Before release, still validate day/night convergence, native Bayer orientation,
rate limiting, live saves and USB interruption with operator approval. Simulator
success is not hardware validation.
