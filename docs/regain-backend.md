# Regain camera backend

AutoPierCam uses Regain 0.5.0, pinned to commit
`3011d788e2757158fba2e9945809ed499eda704c` in Cargo.toml and Cargo.lock.
There are no AutoPierCam SDK bindings. The camera-only `regain-device` entry
point calls the unchanged upstream drivers; `regain-core::Worker` owns the
framed transport, deadlines and process supervision.

## Selection

Settings has an explicit **Camera driver** choice:

- **Regain · ZWO SDK** (default): supports ASI662MC and ASI676MC through the
  vendor library. The installer still includes the reviewed SDK DLL.
- **Regain · ZWO Direct USB** (experimental): ASI676MC is supported, with a
  maximum exposure of 30 seconds. ASI662MC is not supported yet.

There is no automatic fallback. Adding ASI662MC direct support requires a
reviewed Regain pin update and hardware validation, not a local driver.
Other camera vendors can be added behind the adapter as Regain implements them;
this version does not claim support for non-ZWO or monochrome cameras.

```toml
[camera]
driver = "zwo_sdk" # or "zwo_direct", explicitly
# serial = "exact-camera-serial"
exposure_control = "adaptive"
max_exposure_us = 60000000 # use <= 30000000 for ASI676MC direct
```

Keep the selected model filter. Backend changes clear the transient camera ID
in the Viewer and restart capture. A saved serial is verified on open.
Identical models require an exact serial; an ambiguous selection never silently
opens the first camera. SDK IDs remain useful for selecting distinct models
but are not durable USB identity.

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

Before release, validate an isolated physical camera with operator approval:
day/night convergence, native Bayer orientation and geometry, short/30/60-second
exposures where supported, rate limiting, live saves, USB interruption, and
bounded quit. Simulator success is not hardware validation.
