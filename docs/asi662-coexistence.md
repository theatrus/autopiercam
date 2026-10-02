# ASI662MC / ASI676MC mixed-backend diagnosis

Operator-authorized Windows hardware checks on 2026-10-02 UTC investigated
"no capture" with ASI662MC Direct USB, specifically whether an SDK session on
the other attached camera interferes with it. No USB reset, installation,
saved user-configuration change, image upload or live message was performed.
All captures were explicitly selected by model, with one unit of each model.

## Evidence

The initial standalone ASI662MC Direct USB snapshot returned six complete
1920x1080 frames and saved a JPEG. Its deliberately short 100 ms exposure cap
hit the bounded settling deadline; this was acquisition evidence, not proof of
exposure convergence.

The coexistence matrix used separate capture processes and isolated temporary
configurations, with the second process starting three seconds after the first.
Both had fixed 1-second exposure, gain 100, bin 1, uploads/video disabled.

| First process | Second process | 640x480 RAW16 result |
| --- | --- | --- |
| ASI662MC Direct | ASI676MC SDK | 8 PNGs each, both exit 0 |
| ASI676MC SDK | ASI662MC Direct | 8 PNGs each, both exit 0 |
| ASI676MC Direct | ASI662MC SDK | 8 PNGs each, both exit 0 |
| ASI662MC SDK | ASI676MC Direct | 8 PNGs each, both exit 0 |

Those initial matrix runs used existing debug executables labeled 0.2.12 with
the Regain `a13b0c7af07a` worker. Full-resolution RAW8/JPEG runs with a five-second
still interval also saved three frames per camera in both backend assignments.
Current-source executables were then rebuilt and verified as AutoPierCam 0.2.13
and worker 0.2.13 / Regain `a13b0c7af07a` for full-resolution confirmation.
Those rebuilt runs also passed: ASI662MC Direct followed by ASI676MC SDK,
and ASI662MC SDK followed by ASI676MC Direct, each saving three full-resolution
JPEGs per camera at 1-second exposure. All four processes exited 0, with no
capture/USB errors in either paired run.
The SDK DLL was version 1.41.0.0, byte-identical to the repository's vendored DLL.
These are development builds, not a test of the installed Viewer/MSI.

An initial full-resolution RAW16/debug run saturated the still-writer queue:
acquisition continued, but scheduled frames were dropped. ASI662MC saved eight
PNGs and exited normally; the slow ASI676MC writer process was terminated at
the diagnostic time limit. That run is not a clean completion result and must
not be classified as a USB timeout. Cropped RAW16 and full-resolution JPEG
checks avoided that bottleneck.

## Interpretation and limits

The tests have not reproduced a general SDK/Direct USB conflict between these
two different cameras. They do not exclude intermittent USB contention,
different installed SDK/worker versions, another application's discovery
behavior, cold startup, or long exposures. An SDK inventory operation can
touch devices other than the selected camera; these results do not justify
adding live enumeration to capture or Viewer code.

The original no-capture incident remains unexplained. The next useful evidence
is the failing agent's error/phase log, exact executable/worker/SDK versions,
selected camera/backend and exposure/ROI settings, plus the other application's
camera and startup order. Do not infer a driver fix from these successful runs.

Raw logs and images remain local ignored diagnostic artifacts; no images,
serials, private dataset paths or installed configuration are included here.
