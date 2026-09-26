# Long exposures and night capture

New configurations and the snapshot command allow SDK auto exposure up to
**60 seconds** by default. This is a maximum: daylight exposures can still be
fractions of a second. Existing TOML files keep their explicit limit; upgrading
does not silently replace a previous five-second setting.

In the Viewer, set **Max exposure** (milliseconds) to `30000` for 30 seconds or `60000`
for one minute, then save. In TOML the units are microseconds:

```toml
[camera]
max_exposure_us = 60000000
max_gain = 300
settle_frames = 6
```

The effective limit appears in the Viewer and agent logs. Configuration changes
restart the camera session; an in-flight exposure is cancelled before applying
the new controls. Pause affects scheduled still persistence and video sampling, not exposure or
preview. Capture now requests the next eligible completed frame; it does not
force a long exposure to finish early.

## Camera limits

The attached ASI676MC with bundled SDK 1.41 advertises a manual exposure range
of 32 microseconds through 2,000 seconds, but its **SDK auto-exposure ceiling
is 60 seconds** (`AutoExpMaxExpMS`, range 1–60000 milliseconds). These are
different controls. The agent discovers capabilities, converts the SDK's
runtime units, clamps the requested ceiling, and reads back the effective
value. Another camera may report different limits; the ASI662MC has not been
hardware-tested here.

In SDK mode, requesting more than 60 seconds on this ASI676MC is clamped to
60 seconds. Enable **Application-controlled exposure** in the Viewer to use
the sensor's manual range instead. For a two-minute limit, set Max exposure to
`120000` milliseconds, or use:

```toml
[camera]
exposure_control = "adaptive"
max_exposure_us = 120000000
max_gain = 300
raw16 = true
```

The controller targets the sampled raw p90 brightness, prioritizes exposure
before increasing gain, shortens clipped frames aggressively, and uses a
brightness deadband plus three-frame day/night hysteresis. It stops and restarts
SDK video on control changes to discard queued frames with old settings.
Limits are clamped to the attached camera's manual capabilities; a high maximum
does not force a long exposure. The Viewer allows up to 2,000 seconds, subject
to the camera limit. Actual dark-to-daylight optical behavior still needs field
validation; the controller's transitions are covered by deterministic tests.

**RAW16 capture** is independently selectable. It saves lossless, debayered
16-bit RGB PNGs and retains the SDK samples without assuming an ADC bit shift.
The ASI676MC is a 12-bit sensor, not a 16-bit ADC. Its SDK output included
nonzero low bits during characterization. Preview/video use the high byte for
8-bit display; RAW16 doubles the input buffer size and PNGs can be large.
See [security recording](video.md) for optional MP4 segments.

## Startup, cadence, and recovery

The SDK adapts exposure and gain from completed frames. Dark startup can take
several minutes: with six minimum frames and a 60-second ceiling, the overall
settling budget is 605 seconds (ten exposures plus five seconds of allowance).
It finishes sooner after the minimum frames and a four-sample stable exposure/gain
window (within 5% exposure and three gain units of the window's initial sample).
Slow cumulative ramps reset that window. Moving clouds, scene luminance, and
clipped pixels do not prevent completion once the controls are stable. The
adaptive controller still prevents completion on a frame where it adjusts controls.
A healthy
stream that does not converge before that budget uses its latest complete
frame. It does not reuse a buffer modified by a failed SDK read.

Preview starts with the first completed frame, even during settling. The
Viewer and N.I.N.A. panel show received-frame counts or estimated exposing progress
between frames. Once settling completes, its last frame can be saved as the
first still immediately, without waiting for another full exposure. The still
interval is a sampling interval, not a shorter shutter time: a five-second
still interval with 60-second exposure cannot produce a new still every five
seconds.

The Viewer labels startup frames as **Acquiring preview · stabilizing exposure**,
with recording pending, rather than implying that no capture has started. Full
status (including the header, counters, and pause controls) refreshes every two
seconds independently of the settings form. Live polls do not reload configuration
or discard edits; a response started before a command cannot overwrite its newer
UI state. Unsaved-change detection compares normalized editable values against
the loaded configuration, so control initialization/formatting is not an edit,
and reverting an edit clears the warning.

Each SDK read waits at most two seconds before checking cancellation. A
separate no-frame deadline is twice the longest relevant observed exposure
plus five seconds: normally 65 seconds at 30-second exposure, or 125 seconds
at 60-second exposure. The deadline can grow as SDK auto exposure increases;
it never shrinks partway through a wait, and retains one completed frame's
allowance while queued night exposures drain. This prevents a bright
readback from prematurely faulting an older dark frame. Once daylight frames
flow, the deadline becomes short again. A stalled stream faults and enters
the existing supervised reconnect path instead of retrying timeouts forever.

Fresh same-session status progress and the last frame's exposure determine
image freshness. Missing status falls back to the frame's exposure. Repeated
status polls or cached-frame delivery cannot keep an indefinitely frozen image
fresh. SDK exposure/gain readback is asynchronous and approximate; the progress
timer is elapsed time since a completed frame, **not an exact sensor shutter
countdown**. Computer-clock corrections can affect the initial age estimate
of a cached image; ongoing age uses a monotonic clock.

## Opt-in hardware checks

Close other camera-owning applications before running these commands. Both
checks temporarily change camera settings and attempt to restore the affected
controls and ROI format after success, failure, or Ctrl+C. Restoring the ROI
format recenters it; the diagnostic cannot restore a previously off-center
ROI origin. Opening the camera also normalizes the SDK's persisted
dark-subtraction flag, as in ordinary capture. The SDK check acquires
full-resolution RAW8 frames at forced 30- and 60-second
exposures, measures a complete subsequent interval, and tests stopping during
a long exposure:

```powershell
cargo run --release -p autopiercam --example check_long_exposure -- sdk
```

The production-worker check uses a temporary configuration and capture folder,
disables upload and age retention, prints status and preview sequences, and
removes its diagnostic images when finished. It does not edit the installed
configuration. With a covered lens or genuinely dark scene, use:

```powershell
cargo run --release -p autopiercam --example check_long_exposure -- agent --max-seconds 60
```

The default check requests two saved stills and requests orderly shutdown
after 720 seconds if still running. To check cancellation during settling,
use `--stop-after-seconds 2`.
For optical recovery, cover the lens until exposure rises, then uncover it
while observing exposure, gain, frame intervals, and the image. A bright
scene alone can verify a configured 60-second ceiling but cannot demonstrate
that SDK auto exposure actually converges at that ceiling.

## Validation on the attached ASI676MC

On 2026-09-09, full-resolution RAW8 SDK checks measured **30.078 seconds** and
**60.084 seconds** between consecutive frames at the corresponding manual
exposures. Those frames arrived through 14 and 29 short timeout polls,
respectively. Stopping video during an in-flight 60-second exposure took
**0.281 seconds**.

The production auto worker read back the full 60-second ceiling, delivered
previews while settling, saved two stills, and cleared exposure status on
shutdown. In the available scene it converged near **0.672 seconds**, so this
does not validate automatic convergence at the 60-second ceiling or an actual
covered-lens-to-daylight transition. Starting from a pre-set manual exposure
did not force a long auto exposure: the SDK began auto mode around 100 ms.
Those optical tests still require a covered lens or genuinely dark scene.

A separate production run confirmed the 30-second ceiling and cancelled during
settling in **0.500 seconds** at the diagnostic's 500 ms observation resolution.
Both worker checks restored the saved controls and ROI format successfully.
Automated tests cover six complete 30/60-second settling frames, exposure
transitions, missing-frame deadlines, cancellation, fallback-buffer ownership,
and both clients' stale-frame and progress handling without requiring hardware.

On 2026-09-21, the same ASI676MC delivered a forced 90-second exposure at a
measured **90.136-second** frame interval; stopping a subsequent exposure took
**0.331 seconds**. RAW16 returned 25,233,408 bytes at full resolution. A production
adaptive run with a 120-second ceiling converged to **12.308 seconds** in the
available scene, saved two full-resolution 16-bit PNGs and seven MP4 segments,
and restored camera controls after shutdown. Every MP4 decoded successfully.
This demonstrates the pipeline, not automatic convergence at 120 seconds.

To repeat the combined test (select the correct ID from camera enumeration):

```powershell
cargo run --release -p autopiercam --example check_long_exposure -- --camera-id 1 agent --adaptive --raw16 --max-seconds 120 --ffmpeg C:\Tools\ffmpeg\bin\ffmpeg.exe
```
