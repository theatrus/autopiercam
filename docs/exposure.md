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
the new controls. Pause affects scheduled still persistence, not exposure or
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

Requesting more than 60 seconds on this ASI676MC therefore does not enable
longer automatic exposures. That needs the planned application-owned
day/night controller using manual exposure commands. RAW16 night stills and
segmented video also remain separate, unimplemented milestones.

## Startup, cadence, and recovery

The SDK adapts exposure and gain from completed frames. Dark startup can take
several minutes: with six minimum frames and a 60-second ceiling, the overall
settling budget is 605 seconds (ten exposures plus five seconds of allowance).
It can finish sooner when exposure, gain, and luminance stabilize. A healthy
stream that does not converge before that budget uses its latest complete
frame. It does not reuse a buffer modified by a failed SDK read.

Preview starts with the first completed frame, even during settling. The
Viewer and N.I.N.A. panel show settling counts or estimated exposing progress
between frames. Once settling completes, its last frame can be saved as the
first still immediately, without waiting for another full exposure. The still
interval is a sampling interval, not a shorter shutter time: a five-second
still interval with 60-second exposure cannot produce a new still every five
seconds.

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
