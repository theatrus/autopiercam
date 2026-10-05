# Capture timing retrospective and test plan

This is the post-release review for AutoPierCam 0.2.22 and Regain 0.5.9.0.
It is for maintainers changing acquisition, worker IPC, frame delivery or
preview. We qualified eventual frame delivery too broadly and did not qualify
latency at each boundary. The fixes and regressions below address observed
failures; the remaining plan is a backlog, not a claim of executed coverage.
This document does not authorize more hardware tests or installation.

## Why the earlier tests missed the failures

The earlier checks asked whether two valid frames eventually arrived. That is
necessary, but it does not establish correct cadence or a responsive worker.
We incorrectly treated those passes as adequate timing qualification.

- The [old Regain hardware check](https://github.com/pulsarfab/regain/blob/987760fb55b7f48e7d5be855ae4a4beb423936cf/scripts/inspection/check_continuous.py)
  allowed `4 * max(current exposure, earlier exposures) + 60 seconds` for two
  frames. Its spacing assertion was only `>= 1.9 seconds` for a 0.5 FPS cap.
  That catches delivery that is too fast, not every-other-frame delivery or
  excessive IPC latency. It waited out `settingsPending` and `settling`, so
  could not detect the missing transition preview.
- The [old AutoPierCam adapter check](https://github.com/theatrus/autopiercam/blob/ff2968230ede874f5bbea0da47192cbdffcbcde5/crates/autopiercam/examples/check_continuous.rs)
  checked geometry and two frames within exposure/pacing budgets. It printed
  elapsed times but did not assert delivered cadence against raw acquisition.
- Slow output had coverage, but the reverse dependency did not: a blocked
  acquisition call could delay status and frame IPC. Instant synthetic frames
  did not represent the native read's blocking contract.
- Earlier hardware runs covered short exposures and 6.4/25/60-second sequences,
  not the observed adaptive ramp near 47.6 ms, 190 ms, 761 ms, 3.045 s,
  12.181 s and 20 s. Nominal endpoint coverage missed the path between them.

The confirmed application mechanisms were serialized IPC behind acquisition,
separate readiness/download operations, and withholding all uncertain frames
from preview. The original SDK fault was our no-progress watchdog after
repeated native timeout 11, not evidence of a native terminal error or a
physical lock. Raising the video read cap from one to five seconds passed the
previously failing ramp. The SDK's internal cause remains unproven; this is
not a firmware or interference diagnosis.

The earlier MSI staging failure was separate: packaging expected an obsolete
worker version label. [PR 34](https://github.com/theatrus/autopiercam/pull/34)
added a shared assertion and a built-worker integration contract. Do not count
that packaging fix as camera timing coverage.

## Released evidence and its limits

Released source: AutoPierCam `98b9ec6587454ed80d758d43f14df415a2f274b6`
(tree `c1ee008eb87f958f00e78edb2e46dc9b477cae26`) pins Regain
`76e649816382e1ccd877b1f97d42bdf92cef76c5`
(tree `0c762cad5d58076941b00f39682fb02a1e67b0b9`). Merge trees matched their
checked candidates. Both signed release workflows and both public NINA feeds
were verified before this documentation branch was created.

Operator-authorized ASI662MC USB 2, full RAW16/bin 1 runs passed with SDK and
Direct. Regain exercised 47.584 ms → 190.336 ms → 761.344 ms → 3.045376 s →
12.181504 s → 20 s → 20 s → 234 ms with gain 300/270, two settled frames per
stage and transition previews. The actual AutoPierCam adapter passed 234 ms →
3.045376 s → 12.181504 s → 20 s → 234 ms with both backends. Observed final
20-second cadence was approximately 20.30 s SDK / 20.34 s Direct; empty cached
polls were at most 16 ms in these samples. Three-second frames reached delivery
at raw cadence rather than every other frame. These are observations, not
universal performance limits or a statistically adequate soak.

All sessions restored manual 234 ms/gain 300 and closed. ASI676MC was unavailable
for this patch. ASI585MM Pro was not substituted or tested. Earlier 676 evidence
does not qualify this patch on 676. No optical settings-latch accuracy,
disconnect, long-soak or Linux/macOS hardware validation is claimed. Timing-only
evidence remains local; camera identifiers, pixels and private traces are not
repository artifacts.

## Regressions implemented for this release

Names below were checked in the released source. They describe assertions, not
just test counts. Upstream links are pinned to the released commit.

- [Regain continuous unit tests](https://github.com/pulsarfab/regain/blob/76e649816382e1ccd877b1f97d42bdf92cef76c5/crates/regain-zwo/src/asi/continuous.rs):
  `atomic_poll_delivers_ready_frame_without_an_intervening_backend_read`
  rejects any backend call during cached polling;
  `cached_ipc_latches_unresponsive_owner_and_invalidates_buffer` ages publication
  to 11 seconds, requires a latched error and cleared slot, rejects a late
  revival, then checks explicit stop/restart;
  `publication_preserves_decimation_clears_on_edits_and_latches_faults`
  checks cached publication and terminal state.
- The same module's `drains_and_retains_only_latest_even_below_one_fps`,
  `long_transition_preview_flows_without_claiming_settled_settings`,
  `transition_opt_out_clears_uncertain_slot_and_keeps_delivery_limit`, and
  `scalar_changes_are_coalesced_at_boundary_without_restart` cover slot bounds,
  selected counter accounting, provenance, opt-out and coalescing. These use
  synthetic backend progress and manipulated time state, not realistic SDK waits.
- [Regain real-pipe tests](https://github.com/pulsarfab/regain/blob/76e649816382e1ccd877b1f97d42bdf92cef76c5/scripts/test-rust.py),
  `continuous_sdk_fixture`: a marker confirms entry into a 1.5-second native
  fixture block; status, one ready atomic frame poll and five empty polls must
  complete together in under 750 ms. Another case fills output with a
  512 × 512 × 2-byte frame and checks that acquisition advances despite
  backpressure. It also checks normal timeout handling and terminal SDK code 5.
  `continuous` checks independent draining for SDK and every Direct simulator.
  The 750 ms threshold is a fixture regression bound, not the product latency target.
- [SDK wait calculation](https://github.com/pulsarfab/regain/blob/76e649816382e1ccd877b1f97d42bdf92cef76c5/crates/regain-zwo/src/asi/sdk/library.rs),
  `video_wait_tracks_exposure_with_five_second_cap`, checks the argument
  calculation including saturation. It cannot establish actual vendor wait behavior.
- [AutoPierCam worker integration tests](../crates/autopiercam-regain-worker/tests/capture.rs),
  `continuous_transition_frames_reach_preview_on_both_backends`, checks adapter
  delivery of valid uncertain frames on both simulated backends;
  `failed_download_faults_instead_of_retrying_or_reopening` covers injected
  download failure, process crash and hang.
- [Preview tests](../crates/autopiercam/src/preview.rs),
  `settling_observer_publishes_preview_before_capture_transition`, checks encoded
  preview and null exposure/gain plus Unknown mode for unsettled input, without
  advancing the monitor to Capturing. It is not a full AE/snapshot exclusion test.
  Those production guards are in [capture processing](../crates/autopiercam/src/lib.rs);
  combined negative regressions remain a P0 item below.
- [Packaging contract](../scripts/Test-RegainWorkerVersion.ps1) invokes only
  built-worker `--version` and rejects old label, wrong app version, empty output
  and nonzero exit. It shares [the installer assertion](../scripts/RegainWorkerVersion.ps1).

## Timing contracts for future qualification

Use monotonic clocks for durations. Keep wall-clock timestamps only for
correlation. Record raw sequence, settings generation and session generation
alongside each timestamp; an IPC response is not proof of camera progress.

For steady-state samples define `R` as observed raw acquisition interval and
`D = 1 / max_fps`. Delivery should track `max(R, D)` within a predeclared,
calibrated transport/encoding/scheduler allowance. Exposure alone is not exact
sensor cadence. Test both lower and upper bounds: an upper bound must reject an
unexplained extra whole frame interval when the consumer can keep up. When
decimation or a slow consumer is intentional, account for skipped sequences;
never accept duplicate, stale-session or falsely settled frames.

| Clock or outcome | Required pass or fail assertion |
| --- | --- |
| Raw acquisition | Intervals and progress remain within the selected backend/model/transport baseline; record timeouts separately. |
| Owner heartbeat | Healthy 0.5–5 s native waits keep publication alive. A permanently blocked owner trips the 10 s latch within one poll plus declared scheduler allowance; buffered pixels are invalidated. |
| IPC round trip | Cached status/empty poll remains bounded independently of native read duration. Measure payload transfer separately; blocking output must not block acquisition. |
| Acquisition to preview | Track the same raw sequence through worker receipt, adapter receipt and encoded preview. Its age stays within a declared copy/encoding/queue allowance; no growing queue. |
| Delivered cadence | Bound both sides of `max(R,D)`; correlate gaps with raw and replacement counters, not only the requested FPS. |
| Settings | Separate request acknowledgment from owner application and settled provenance. Bound first transition frame after the next eligible raw arrival; bound first settled frame using prior/new exposure and the conservative fence. |
| Stop and recovery | Measure acknowledgment, native return, worker exit and replacement separately. A deadline faults/reaps the worker; no replacement owner overlaps it and no late frame revives it. |

Before a run, fill in numerical allowances per model, USB transport, format,
resolution and load. Calibrate on a separate baseline, justify the overhead,
then freeze thresholds; never enlarge them after seeing a failure. An unset
threshold means unqualified, not passed. Do not demand a preview before any new
raw frame exists, especially after a long-to-short exposure transition.

Proposed minimum samples: deterministic boundary cases use explicit event
ordering and exact counts; real-pipe cases use 30 independent blocked-read
cycles per backend fixture; steady-state operator runs use 30 delivered intervals
per chosen stage, with five transitions in each direction. Record every attempt.
Long-exposure runs need an approved duration budget or an explicit coverage
exception, not a silent reduction. Report median, p95 and maximum; do not infer
tail reliability from two frames or report p99 from these small samples.

Use virtual clocks and barriers rather than sleeps for deterministic tests.
Use bounded real-pipe tests for scheduling/transport behavior, including an
outer deadline that kills only the owned fixture if the protocol blocks.
Every new regression needs a negative control: reintroduce serialized polling,
double cadence, stale generations or missing exclusion in an isolated fixture
and demonstrate failure. A green test that also accepts old behavior is not
evidence that the requirement is covered.

## Prioritized remaining work

These are proposed tests, not implemented coverage or authorization to run
hardware. Extend existing regressions where possible.

### P0 Blocking and correctness

1. Test both directions independently: blocked native acquisition with responsive
   cached IPC, and full stdout backpressure with continuing acquisition. Add
   concurrent control, stop and shutdown at each barrier. Verify bounded queues
   and RSS under sustained blockage, not only counter advancement.
2. Exercise a truly hung owner over a real pipe while cached responses continue.
   Verify latch detection, stale-buffer invalidation, late-return rejection,
   bounded parent termination, cleanup and restart without overlapping owners.
3. Model realistic 0.5–5 s native return timing, timeout 11, busy and terminal
   results. Assert wait arguments, backoff, independent no-progress budget and
   prior-exposure grace. Immediate errors alone do not model the SDK contract.
4. Force status/download time-of-check/time-of-use races; verify atomic polling
   returns matching metadata/pixels once or empty, never an old generation.
5. Drive uncertain frames through the actual adapter and capture/preview pipeline.
   Verify visible preview while AE state, startup convergence, explicit snapshot
   completion, saved metadata and periodic saving remain unchanged. Then prove
   a settled frame resumes each consumer. Mutate each guard as a negative control.
6. Bound settings/stop/close/restart under an active read and blocked output.
   Check terminal errors remain terminal and counters reconcile raw acquisition,
   transitions, accepted frames, replacements, delivery, clearing and pending slot.
   Do not assume `raw = delivered + replaced` across transitions or session resets.

### P1 Production shaped timing and load

Use deterministic and seeded adaptive ramps, including the exact reported ramp.
Cover both directions around the 1 s long-mode boundary, 5 s read cap and 30 s
Direct video/still boundary; rapid/coalesced exposure and gain changes;
20 s → 20 s gain-only changes; and 60 s → 234 ms. At FPS 0.1/0.5/1, test consumers
faster and slower than acquisition and rates just around each decimation boundary.

Repeat under full-resolution RAW16 and RAW8 conversion, CPU/copy pressure,
preview encoding, recording, slow storage and IPC load. Measure each stage's
latency and memory bound so a fast worker cannot conceal a slow preview queue.
Seeded cases must print a reproducible seed and retain the first failing trace.

### P2 Operator approved hardware coverage

Plan long soaks, cold starts, optical/settings-latch measurements, physical
disconnect recovery and multi-camera isolation separately. Each needs explicit
operator permission, camera selection, duration, stop condition and restoration
plan. Do not substitute 585 for an absent 676 or extrapolate 662 results to 676.
Do not claim USB/firmware recovery from successful software timeout tests.

## Qualification stages and release review

Progress from simulator/unit and ABI fixtures, to the actual AutoPierCam adapter,
to capture/preview integration, then to signed installed-artifact validation.
CI remains hardware-free and never sends live images/messages. Existing CI
installer lifecycle coverage is not a substitute for installed end-to-end timing
qualification. Local installation and every new physical test require fresh
explicit permission; do not run them merely because they appear in this plan.

For every behavioral timing change, the release review must identify a named
timing/blocking test and its lower/upper bounds, sample count, failure control and
remaining gap. A reviewer must derive the expected behavior independently from
the requirement and inspect the assertions, not just accept test counts or green
CI. In particular ask whether the test would reject an extra whole-frame delay,
a responsive cache hiding a dead owner, or correctly sized but stale pixels.

## Evidence record template

Copy this into the run record before qualification:

- App and upstream commit/tree; build configuration; SDK binary SHA256; signed
  artifact version/checksum when applicable.
- Backend, exact model, OS, USB transport, ROI, binning, pixel format and FPS;
  redact serials, locators and other identifying data from shared records.
- Requirement, scenario, sequence/gain changes, deterministic seed and load.
- Predeclared thresholds and calibration source; sample/transition counts,
  timeout/termination bounds, operator authorization if needed.
- Every attempt and failure, expected versus observed result, median/p95/max;
  raw and delivered timestamps/sequences/counters, owner heartbeat and IPC RTT,
  settings acknowledgment/application and transition/settled provenance.
- Queue/memory bounds, stop/cleanup/restore result, process ownership checks;
  limitations and skipped cases with reasons.
- Timing-only artifact references with identifiers and pixels removed. Retain
  failures and distinguish a new fixed candidate from a rerun; never retry until
  green and discard the failing evidence.

This plan improves detection and makes limits explicit. It does not guarantee
that all future timing bugs are prevented.
