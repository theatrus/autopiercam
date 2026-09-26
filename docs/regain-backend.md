# Regain direct-driver migration

Decision (2026-09-26): prefer the SDK-free Regain camera backend for AutoPierCam
once ASI662MC support and pier-camera requirements are validated. Do not change
the installed backend or silently fall back between SDK and Direct USB today.

## Why

The incident investigated in [PR #12](https://github.com/theatrus/autopiercam/pull/12)
stalled acquisition inside SDK 1.41's camera-property lookup, which internally
opened a different camera during periodic discovery. The operator also reported
other ZWO cameras stopped working on the same system. The dump proves the
user-space SDK loop; it does not establish whether the wider failure was in a
USB controller, kernel driver, firmware, or another shared component.

No live SDK inventory polling is allowed during acquisition, settling, or paused
recording. Worker process isolation contains a hung process but does not guarantee
hardware or system-wide USB recovery. Recovery must not reset unrelated cameras,
hubs or controllers, or repeatedly enumerate them after a fault.

## Current support gate

Inspected [pulsarfab/regain](https://github.com/pulsarfab/regain) at
`00e042dd9309b95bacefacc13a03209d920119ee`:

- Direct capture includes ASI676MC, not ASI662MC. Do not alias these models or
  infer register compatibility from similar sensors/resolution.
- The ASI676MC direct settings currently enforce 32 microseconds through
  **30 seconds**, RAW16 and bin 1. This is not yet a drop-in replacement for
  AutoPierCam's 60-second-and-longer night exposures.
- `regain-core::Session` supervises per-camera worker processes, and
  `regain-device zwo camera-direct --serve` provides the framed worker protocol.
  Reuse these ownership/recovery boundaries, rather than loading another native
  camera implementation into the tray/UI process.
- Regain's SDK mode still loads ZWO's SDK. It may improve process containment,
  but switching to it is not the requested SDK-free migration.

Recheck capabilities on the exact pinned version before implementation. No new
runtime dependency, backend selector, or claim of ASI662MC support is added by
this design decision.

## Integration boundary

Introduce a capture-provider boundary where the current ASI-specific acquisition
loop produces completed frames. Keep camera handles and register I/O in the
selected worker; keep debayering, preview, stills/video, storage, sharing and
configuration in AutoPierCam.

The provider contract must supply:

- Explicit, stable model/serial selection and a cached discovery snapshot.
  Translate existing ID/model selection only with operator confirmation; USB
  enumeration indexes are not stable identity. No first-match fallback.
- Actual RAW16 layout, Bayer pattern, ROI, dimensions, exposure/gain bounds and
  effective applied settings. Preserve full-resolution stills and full-HD previews.
- One completed frame with its capture timestamp, sequence and worker generation.
  Regain's still-oriented start/status/download workflow needs paced successive
  captures, not an assumption that it implements the ASI video API.
- Cancellation and operation deadlines enforced by the parent process. Report
  distinct capture, transfer and worker-stall phases; transport responsiveness
  alone cannot renew the frame-progress clock.
- Bounded worker teardown and verified exit before reopening the same identity.
  Stop automatic retries and ask for operator intervention when recovery fails;
  no automatic USB hub/controller reset.

Use AutoPierCam's application-controlled exposure/gain loop for direct capture;
there is no ZWO SDK auto-exposure engine in this mode. Keep its day/night state,
settling state, scene-change reference and pause intent through ordinary settings
saves. Reject unsupported exposure requests visibly; never silently cap a night
configuration to 30 seconds or fall back to SDK discovery.

## Acceptance before enabling ASI662MC

1. Regain implements and identifies ASI662MC directly, with tested frame parsing,
   Bayer phase, ROI and manual controls. Unsupported models fail before register I/O.
2. Capabilities expose the validated exposure ceiling; validate 30/60-second and
   longer exposures required by the installation rather than assuming SDK parity.
3. Simulated/fixture tests cover stuck list/open/control/read/close, stale status,
   malformed frames, clock changes, reconnect identity and quit during every phase.
4. Operator-approved isolated-hardware tests cover sustained night/day transitions,
   USB interruption and long-running acquisition. No discovery during multi-camera
   imaging. Confirm other cameras are unaffected; simulator success is not evidence.
5. Ship a pinned worker with licensing, installer lifecycle handling and release
   tests. Continue publishing every released AutoPierCam NINA plugin through the
   shared registry, not only as a GitHub ZIP.

## Shared hardening

[Regain PR #1](https://github.com/pulsarfab/regain/pull/1) shares a connected-worker
SDK discovery guard, simulator/fixture tests and malformed-descriptor regressions.
Its `docs/sdk-lifecycle.md` carries the sanitized incident findings, the
per-worker no-live-discovery rule, the limits of process isolation, and remaining
cross-process discovery risk. Share code/tests at that boundary; do not publish
the full-memory dump, private log paths, credentials or observatory images.
