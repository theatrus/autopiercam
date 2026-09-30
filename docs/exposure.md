# Exposure and gain

AutoPierCam controls exposure and gain through Regain. SDK auto-exposure is no
longer used, even when an older configuration omits exposure_control or says
sdk. The Viewer always uses application control. Saved exposure/gain limits and
the shorter-exposure preference are preserved.

The default maximum is 60 seconds. This is a ceiling, not a fixed shutter time;
daylight exposures can be much shorter. Viewer units are milliseconds, TOML
units are microseconds.

```toml
[camera]
driver = "zwo_sdk"
exposure_control = "adaptive"
min_exposure_us = 100
max_exposure_us = 60000000
min_gain = 200
max_gain = 300
prefer_short_exposures = true
```

## Driver limits

The SDK backend uses the camera's manual exposure range, not its SDK-auto
ceiling. The previously tested ASI676MC SDK advertises up to 2,000 seconds.
Actual limits come from the opened camera.

The pinned Regain direct backend supports ASI676MC up to **30 seconds**.
Set max_exposure_us to at most 30000000 before selecting direct. Higher ceilings
fail visibly; there is no silent direct-to-SDK fallback. ASI662MC must use
Regain's SDK backend until upstream adds direct support.

## Gain and shorter exposures

In Settings, set Minimum gain to 200, Maximum gain to 300 and enable
Prefer shorter exposures to raise gain before extending dark exposures.
Setting both limits to 200 fixes gain while exposure remains automatic.

Gain increases in bounded steps. A steady long exposure is gradually rebalanced
toward higher gain and shorter time; the controller does not assume a
sensor-specific gain scale. Bright frames shorten exposure first. At minimum
exposure, gain can decrease, but not below the configured floor. An unattainable
minimum fails explicitly. If the image still clips at the shortest exposure,
lower the gain floor.

Exposure and gain changes apply between frames. Saving these limits does not
reopen the camera or reset the settling gate. A running long exposure must
finish before edits can take effect. There is no video backlog to flush.

## Cadence, settling and lighting mode

The controller targets sampled p90 brightness, shortens clipped frames
aggressively, and uses a brightness deadband. Lighting mode uses exposure-based
hysteresis, not a fixed sunrise/sunset time. Roof state and artificial lights
can legitimately differ from astronomical day/night. Chatstronomy applies its
own 30-second mode dwell before reporting a transition.

Startup settling waits for stable controls; previews appear during this phase.
Changing output/upload settings preserves the controller's history. Camera,
backend, serial or image-layout changes require a new session.

Maximum preview frame rate paces acquisition and preview publication; it cannot
make a long exposure finish sooner. Save next frame requests the next eligible
completed frame. Pause recording stops scheduled stills and video sampling,
not the preview.

Frame progress has an exposure-aware deadline. Repeated 'exposing' replies do
not reset the time since the last completed frame. A worker timeout, invalid
frame or capture error faults the session; use the tray's restart command or
save corrected settings to retry. No automatic USB reset or repeated discovery
is attempted.

## Validation

Controller and Regain simulator tests run without physical cameras. The Regain
migration still requires operator-approved hardware tests for day/night
convergence, Bayer orientation, long exposures, live edits and USB interruption.
See [Regain backend](regain-backend.md) for the acceptance checklist.
