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

The pinned Regain direct backend supports ASI676MC up to **2,000 seconds**,
validated with a full-frame RAW16 capture. Set max_exposure_us to at most
2000000000 (2,000,000 ms in the Viewer). Higher ceilings
fail visibly; there is no silent direct-to-SDK fallback. Regain 0.5.2 also
supports ASI662MC Direct USB, advertising the same ceiling. Upstream Windows
ASI662MC evidence covers through 600 seconds, not 2,000 seconds.

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
aggressively, and uses a brightness deadband. After startup, lighting mode uses
exposure-based hysteresis, not a fixed sunrise/sunset time. Roof state and artificial lights
can legitimately differ from astronomical day/night. Chatstronomy applies its
own 30-second mode dwell before reporting a transition.

Startup settling waits for stable controls; previews appear during this phase.
Changing output/upload settings preserves the controller's history. Camera,
backend, serial or image-layout changes require a new session.

### Initial day/night selection

Set observing latitude and longitude in Viewer Settings, or `latitude_deg` and
`longitude_deg` under `[camera]` in TOML. Both are optional but must be provided
together: north/east positive, south/west negative, ranges -90..90 and -180..180.
No location is inferred or sent to a service. Leave both blank to preserve the
previous startup behavior.

On each new capture session, the current UTC instant and observing location
seed the lighting mode using the approximate
[NOAA solar-position equations](https://gml.noaa.gov/grad/solcalc/solareqns.PDF).
Below -6 degrees solar altitude is night; civil twilight and daylight are day.
This handles longitude, seasons and polar day/night without depending on the
computer's timezone or daylight-saving setting. An invalid system time falls
back to the previous behavior.

The first exposure is 1 second at night or 10 milliseconds by day, clamped to
the configured and camera-supported limits. Gain limits are unchanged. Normal
frame feedback then adjusts exposure and can override the initial mode. Saving
coordinates during capture does not restart or reseed it; the next new capture
session uses them. Recording pause/resume and output reloads preserve history.

Maximum preview frame rate paces acquisition and preview publication; it cannot
make a long exposure finish sooner. Save next frame requests the next eligible
completed frame. Pause recording stops scheduled stills and video sampling,
not the preview.

Frame progress has an exposure-aware deadline. Repeated 'exposing' replies do
not reset the time since the last completed frame. A worker timeout, invalid
frame or capture error faults the session. After the old camera owner and cleanup
have exited, the tray automatically retries after 30 seconds, retaining the saved
backend/selection and recording-pause state. Each failed attempt waits another
30 seconds. Use the tray's restart command or save corrected settings to retry
sooner; quitting cancels retries. Discovery occurs only before opening the new
session, never while a camera is owned. No automatic USB reset is attempted.

## Validation

Controller and Regain simulator tests run without physical cameras. The Regain
migration still requires operator-approved hardware tests for day/night
convergence, Bayer orientation, long exposures, live edits and USB interruption.
See [Regain backend](regain-backend.md) for the acceptance checklist.
