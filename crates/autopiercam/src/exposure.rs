//! Monotonic timing policy shared by startup settling and continuous capture.
//!
//! The SDK's short read timeout only bounds cancellation latency. It is not the
//! deadline for a complete exposure, which can span many such polls.

use std::time::Duration;

use super::AutoLimits;

pub(crate) fn poll_timeout_ms(exposure_us: i64) -> i32 {
    (exposure_us.max(1) / 1_000 + 500).clamp(500, 2_000) as i32
}

fn frame_timeout(exposure_us: i64) -> Duration {
    Duration::from_micros(exposure_us.max(1) as u64)
        .saturating_mul(2)
        .saturating_add(Duration::from_secs(5))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameWait {
    last_frame_at: Duration,
    exposure_us: i64,
    observed_exposure_us: i64,
    previous_frame_exposure_us: i64,
}

impl FrameWait {
    pub(crate) fn new(exposure_us: i64) -> Self {
        let exposure_us = exposure_us.max(1);
        Self {
            last_frame_at: Duration::ZERO,
            exposure_us,
            observed_exposure_us: exposure_us,
            previous_frame_exposure_us: exposure_us,
        }
    }

    /// Never shorten an in-flight deadline when asynchronous SDK telemetry
    /// switches from a dark exposure to a bright one.
    pub(crate) fn observe_exposure(&mut self, exposure_us: i64) {
        self.exposure_us = exposure_us.max(1);
        self.observed_exposure_us = self.observed_exposure_us.max(self.exposure_us);
    }

    pub(crate) fn frame_received(&mut self, now: Duration) {
        self.last_frame_at = now;
        // Retain one completed wait's telemetry to allow an already queued
        // long exposure to drain. Do not feed that allowance back into itself:
        // subsequent daylight frames must recover a short deadline.
        self.previous_frame_exposure_us = self.observed_exposure_us;
        self.observed_exposure_us = self.exposure_us;
    }

    pub(crate) fn elapsed(&self, now: Duration) -> Duration {
        now.saturating_sub(self.last_frame_at)
    }

    pub(crate) fn timeout(&self) -> Duration {
        frame_timeout(
            self.observed_exposure_us
                .max(self.previous_frame_exposure_us),
        )
    }

    pub(crate) fn expired(&self, now: Duration) -> bool {
        self.elapsed(now) >= self.timeout()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitDecision {
    Continue,
    Cancelled,
    Stalled,
    UseLatestFrame,
}

pub(crate) struct Settling {
    deadline: Duration,
    stable_anchor: Option<(i64, i64)>,
    stable_samples: u32,
    received: u32,
    minimum_frames: u32,
}

impl Settling {
    pub(crate) fn new(minimum_frames: u32, limits: AutoLimits) -> Self {
        let minimum_frames = minimum_frames.max(4);
        // The requested minimum must fit even if every frame uses the ceiling.
        // Four additional frames allow the SDK to converge after reaching it.
        let exposure_count = u64::from(minimum_frames).saturating_add(4);
        let deadline = Duration::from_micros(
            (limits.max_exposure_us.max(1) as u64).saturating_mul(exposure_count),
        )
        .saturating_add(Duration::from_secs(5))
        .max(Duration::from_secs(5));
        Self {
            deadline,
            stable_anchor: None,
            stable_samples: 0,
            received: 0,
            minimum_frames,
        }
    }

    pub(crate) fn minimum_frames(&self) -> u32 {
        self.minimum_frames
    }

    pub(crate) fn received(&self) -> u32 {
        self.received
    }

    /// Cancellation and a stopped frame stream take precedence over using a
    /// cached frame. An otherwise healthy but unconverged stream may fall back
    /// to its latest complete sample at the overall settling deadline.
    pub(crate) fn decision(
        &self,
        wait: &FrameWait,
        now: Duration,
        cancelled: bool,
    ) -> WaitDecision {
        if cancelled {
            WaitDecision::Cancelled
        } else if wait.expired(now) {
            WaitDecision::Stalled
        } else if now >= self.deadline && self.received != 0 {
            WaitDecision::UseLatestFrame
        } else {
            WaitDecision::Continue
        }
    }

    pub(crate) fn observe_frame(&mut self, now: Duration, exposure_us: i64, gain: i64) -> bool {
        self.received = self.received.saturating_add(1);
        let exposure_tolerance = (exposure_us.unsigned_abs() / 20).max(32);
        let stable = self.stable_anchor.is_some_and(|(old_exposure, old_gain)| {
            old_exposure.abs_diff(exposure_us) <= exposure_tolerance && old_gain.abs_diff(gain) <= 3
        });
        self.stable_samples = if stable {
            self.stable_samples.saturating_add(1)
        } else {
            self.stable_anchor = Some((exposure_us, gain));
            0
        };
        let dynamic_minimum = Duration::from_micros(
            exposure_us
                .unsigned_abs()
                .saturating_mul(2)
                .saturating_add(100_000),
        )
        .max(Duration::from_secs(5));
        // Settling means the controls have converged, not that the scene is
        // stationary or free of clipped pixels. Clouds/lights can change image
        // statistics indefinitely. The adaptive controller still vetoes exit
        // when it requests an adjustment; SDK mode is fenced by this window.
        self.received >= self.minimum_frames && self.stable_samples >= 3 && now >= dynamic_minimum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_exposure_us: i64) -> AutoLimits {
        AutoLimits {
            min_exposure_us: 32,
            max_exposure_us,
            min_gain: 0,
            max_gain: 400,
            target_brightness: 100,
        }
    }

    #[test]
    fn stable_controls_below_ceiling_settle_without_scene_brightness_gates() {
        // Regression: an 8.765s stream at gain 300 used to remain in startup
        // for ten minutes with a 60s ceiling and moving/clipped scenery.
        let mut settling = Settling::new(6, limits(60_000_000));
        for frame in 1..=6 {
            assert_eq!(
                settling.observe_frame(Duration::from_millis(frame * 8765), 8_765_000, 300),
                frame == 6
            );
        }
    }

    #[test]
    fn slow_sdk_ramp_is_measured_against_the_stable_window_not_adjacent_frames() {
        let mut settling = Settling::new(6, limits(60_000_000));
        for frame in 1..=20 {
            // Each step is below 5%, but the four-frame window is not stable.
            let exposure = (8_000_000f64 * 1.04f64.powi(frame)) as i64;
            assert!(!settling.observe_frame(Duration::from_secs(frame as u64 * 20), exposure, 300));
        }
    }

    #[test]
    fn gain_changes_reset_the_stable_window() {
        let mut settling = Settling::new(6, limits(60_000_000));
        for frame in 1..=10 {
            assert!(!settling.observe_frame(
                Duration::from_secs(frame * 9),
                8_765_000,
                frame as i64 * 10
            ));
        }
        for frame in 1..=3 {
            assert_eq!(
                settling.observe_frame(Duration::from_secs(90 + frame * 9), 8_765_000, 100),
                frame == 3
            );
        }
    }

    #[test]
    fn six_thirty_or_sixty_second_frames_fit_and_settle() {
        for seconds in [30, 60] {
            let exposure_us = seconds * 1_000_000;
            let mut settling = Settling::new(6, limits(exposure_us));
            let mut wait = FrameWait::new(exposure_us);
            for frame in 1..=6 {
                let now = Duration::from_secs(frame * seconds as u64);
                assert_eq!(settling.decision(&wait, now, false), WaitDecision::Continue);
                wait.frame_received(now);
                assert_eq!(settling.observe_frame(now, exposure_us, 400), frame == 6);
            }
            assert_eq!(settling.received(), 6);
        }
    }

    #[test]
    fn requested_minimum_and_four_convergence_frames_fit_the_budget() {
        let mut settling = Settling::new(20, limits(60_000_000));
        let mut wait = FrameWait::new(60_000_000);
        for frame in 1..=24 {
            let now = Duration::from_secs(frame * 60);
            assert_eq!(settling.decision(&wait, now, false), WaitDecision::Continue);
            wait.frame_received(now);
            settling.observe_frame(now, 60_000_000, (frame % 2) as i64 * 100);
        }
        assert_eq!(
            settling.decision(&wait, Duration::from_secs(1445), false),
            WaitDecision::UseLatestFrame
        );
    }

    #[test]
    fn daylight_deadline_expands_for_night_and_survives_bright_readback() {
        let mut wait = FrameWait::new(10_000);
        assert_eq!(wait.timeout(), Duration::from_millis(5020));
        wait.observe_exposure(30_000_000);
        assert_eq!(wait.timeout(), Duration::from_secs(65));
        wait.observe_exposure(60_000_000);
        wait.observe_exposure(10_000);
        assert_eq!(wait.timeout(), Duration::from_secs(125));
        assert!(!wait.expired(Duration::from_secs(60)));
        wait.frame_received(Duration::from_secs(60));
        assert_eq!(wait.timeout(), Duration::from_secs(125));
        wait.frame_received(Duration::from_secs(120));
        assert_eq!(wait.timeout(), Duration::from_millis(5020));
        assert!(wait.expired(Duration::from_secs(126)));
    }

    #[test]
    fn no_frames_and_stalled_stream_have_a_finite_deadline() {
        let mut settling = Settling::new(6, limits(60_000_000));
        let mut wait = FrameWait::new(60_000_000);
        assert_eq!(
            settling.decision(&wait, Duration::from_secs(125), false),
            WaitDecision::Stalled
        );
        wait.frame_received(Duration::from_secs(60));
        settling.observe_frame(Duration::from_secs(60), 60_000_000, 200);
        assert_eq!(
            settling.decision(&wait, Duration::from_secs(185), false),
            WaitDecision::Stalled
        );
    }

    #[test]
    fn cancellation_wins_even_at_fallback_and_stall_boundaries() {
        let mut settling = Settling::new(6, limits(60_000_000));
        let mut wait = FrameWait::new(60_000_000);
        wait.frame_received(Duration::from_secs(600));
        settling.observe_frame(Duration::from_secs(600), 60_000_000, 200);
        for seconds in [0, 30, 60, 605, 725] {
            assert_eq!(
                settling.decision(&wait, Duration::from_secs(seconds), true),
                WaitDecision::Cancelled
            );
        }
        for exposure in [0, 32, 10_000, 30_000_000, 60_000_000, i64::MAX] {
            assert!((500..=2000).contains(&poll_timeout_ms(exposure)));
        }
    }

    #[test]
    fn changing_night_to_day_requires_new_stable_samples() {
        let mut settling = Settling::new(6, limits(60_000_000));
        let mut now = Duration::ZERO;
        for frame in 0..4 {
            now += Duration::from_secs(60);
            assert!(!settling.observe_frame(now, 60_000_000, 400));
            assert_eq!(settling.received(), frame + 1);
        }
        for frame in 0..4 {
            now += Duration::from_secs(1);
            assert_eq!(settling.observe_frame(now, 10_000, 10), frame == 3);
        }
    }
}
