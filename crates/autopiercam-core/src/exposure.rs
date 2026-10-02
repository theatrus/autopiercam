//! Deterministic frame-feedback controller; hardware I/O remains on the camera owner.
use crate::image::LumaStats;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightMode {
    Day,
    Night,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExposureSetting {
    pub exposure_us: i64,
    pub gain: i64,
}

pub struct AdaptiveExposure {
    min_us: i64,
    max_us: i64,
    min_gain: i64,
    max_gain: i64,
    target: f64,
    mode: LightMode,
    votes: u8,
    prefer_short_exposures: bool,
}

impl AdaptiveExposure {
    /// Inputs must be positive, capability-clamped exposure bounds and ordered gain bounds.
    pub fn new(min_us: i64, max_us: i64, min_gain: i64, max_gain: i64, target: u8) -> Self {
        assert!(
            min_us > 0 && max_us >= min_us && min_gain >= 0 && max_gain >= min_gain && target > 0
        );
        Self {
            min_us,
            max_us,
            min_gain,
            max_gain,
            target: f64::from(target),
            mode: LightMode::Day,
            votes: 0,
            prefer_short_exposures: false,
        }
    }

    pub fn mode(&self) -> LightMode {
        self.mode
    }

    /// Seed a fresh session, leaving subsequent frame-feedback transitions intact.
    pub fn with_initial_mode(mut self, mode: LightMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn initial_exposure_us(&self) -> i64 {
        match self.mode {
            LightMode::Day => 10_000,
            LightMode::Night => 1_000_000,
        }
        .clamp(self.min_us, self.max_us)
    }

    /// Preserve controller history while changing the operator's noise/cadence tradeoff.
    pub fn set_prefer_short_exposures(&mut self, prefer: bool) {
        self.prefer_short_exposures = prefer;
    }

    /// Keep day/night history when an operator changes the permitted range.
    pub fn update_limits(
        &mut self,
        min_us: i64,
        max_us: i64,
        min_gain: i64,
        max_gain: i64,
        target: u8,
    ) {
        let next = Self::new(min_us, max_us, min_gain, max_gain, target);
        self.min_us = next.min_us;
        self.max_us = next.max_us;
        self.min_gain = next.min_gain;
        self.max_gain = next.max_gain;
        self.target = next.target;
    }

    pub fn observe(&mut self, current: ExposureSetting, stats: LumaStats) -> ExposureSetting {
        // Three frames of evidence plus separated thresholds prevent twilight chatter.
        let opposite = match self.mode {
            LightMode::Day => current.exposure_us >= 1_000_000,
            LightMode::Night => current.exposure_us <= 250_000,
        };
        self.votes = if opposite {
            self.votes.saturating_add(1)
        } else {
            0
        };
        if self.votes >= 3 {
            self.mode = match self.mode {
                LightMode::Day => LightMode::Night,
                LightMode::Night => LightMode::Day,
            };
            self.votes = 0;
        }
        let mut next = current;
        let measured = f64::from(stats.p90.max(1));
        let clipped = stats.clipped_fraction > 0.05;
        let ratio = if clipped {
            0.25
        } else {
            (self.target / measured).clamp(0.0625, 4.0)
        };
        // Also rebalance a previously converged long exposure when gain-first
        // is enabled live. Use subsequent image feedback to shorten it rather
        // than assuming a camera-specific gain-to-brightness conversion.
        if self.prefer_short_exposures
            && !clipped
            && ratio >= 0.90
            && current.gain < self.max_gain
            && (ratio > 1.10 || current.exposure_us > self.min_us)
        {
            next.gain = current.gain.saturating_add(20).min(self.max_gain);
        } else if clipped || !(0.90..=1.10).contains(&ratio) {
            if !self.prefer_short_exposures
                && ratio < 1.0
                && current.exposure_us <= 250_000
                && current.gain > self.min_gain
            {
                next.gain = current.gain.saturating_sub(30).max(self.min_gain);
            } else {
                next.exposure_us = ((current.exposure_us as f64 * ratio).round() as i64)
                    .clamp(self.min_us, self.max_us);
                if ratio > 1.0 && current.exposure_us >= self.max_us {
                    next.gain = current.gain.saturating_add(20).min(self.max_gain);
                } else if ratio < 1.0 && current.exposure_us <= self.min_us {
                    next.gain = current.gain.saturating_sub(30).max(self.min_gain);
                }
            }
        }
        next.exposure_us = next.exposure_us.clamp(self.min_us, self.max_us);
        next.gain = next.gain.clamp(self.min_gain, self.max_gain);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_mode_respects_limits_and_can_yield_to_frames() {
        let mut c =
            AdaptiveExposure::new(100, 100_000, 0, 300, 100).with_initial_mode(LightMode::Night);
        assert_eq!(c.initial_exposure_us(), 100_000);
        let setting = ExposureSetting {
            exposure_us: 100_000,
            gain: 0,
        };
        c.observe(setting, stats(100, 0.0));
        c.observe(setting, stats(100, 0.0));
        assert_eq!(c.mode(), LightMode::Night);
        c.observe(setting, stats(100, 0.0));
        assert_eq!(c.mode(), LightMode::Day);
        assert_eq!(
            AdaptiveExposure::new(20_000, 20_000, 0, 0, 100).initial_exposure_us(),
            20_000
        );
    }
    fn stats(p90: u8, clipped: f32) -> LumaStats {
        LumaStats {
            mean: p90 as f32,
            p50: p90,
            p90,
            clipped_fraction: clipped,
        }
    }
    #[test]
    fn gain_first_raises_gain_before_exposure_and_rebalances_a_stable_long_frame() {
        let mut c = AdaptiveExposure::new(100, 60_000_000, 200, 300, 100);
        c.set_prefer_short_exposures(true);
        let mut setting = ExposureSetting {
            exposure_us: 1_000_000,
            gain: 200,
        };
        let stable = c.observe(setting, stats(100, 0.0));
        assert_eq!(stable.gain, 220);
        assert_eq!(stable.exposure_us, setting.exposure_us);
        for expected in [220, 240, 260, 280, 300] {
            setting = c.observe(setting, stats(10, 0.0));
            assert_eq!(setting.gain, expected);
            assert_eq!(setting.exposure_us, 1_000_000);
        }
        setting = c.observe(setting, stats(10, 0.0));
        assert_eq!(setting.gain, 300);
        assert_eq!(setting.exposure_us, 4_000_000);
        let bright = c.observe(setting, stats(255, 0.8));
        assert_eq!(bright.gain, 300);
        assert_eq!(bright.exposure_us, 1_000_000);
    }

    #[test]
    fn floor_and_fixed_gain_survive_day_night_and_clipped_frames() {
        for ceiling in [200, 300] {
            for prefer in [false, true] {
                let mut c = AdaptiveExposure::new(100, 60_000_000, 200, ceiling, 100);
                c.set_prefer_short_exposures(prefer);
                let mut setting = ExposureSetting {
                    exposure_us: 60_000_000,
                    gain: ceiling,
                };
                for (p90, clipped) in [(255, 0.8), (0, 0.0), (100, 0.0)] {
                    for _ in 0..60 {
                        setting = c.observe(setting, stats(p90, clipped));
                        assert!((200..=ceiling).contains(&setting.gain));
                        assert!((100..=60_000_000).contains(&setting.exposure_us));
                    }
                }
            }
        }
    }

    #[test]
    fn gain_first_uses_gain_floor_when_minimum_exposure_is_still_too_bright() {
        let mut c = AdaptiveExposure::new(100, 60_000_000, 200, 300, 100);
        c.set_prefer_short_exposures(true);
        let mut setting = ExposureSetting {
            exposure_us: 100,
            gain: 300,
        };
        for _ in 0..10 {
            setting = c.observe(setting, stats(255, 0.8));
        }
        assert_eq!(
            setting,
            ExposureSetting {
                exposure_us: 100,
                gain: 200
            }
        );
        assert_eq!(c.observe(setting, stats(100, 0.0)), setting);
    }

    #[test]
    fn updated_gain_floor_is_enforced_even_inside_brightness_deadband() {
        let mut c = AdaptiveExposure::new(100, 60_000_000, 0, 300, 100);
        c.update_limits(100, 60_000_000, 200, 250, 100);
        for (gain, expected) in [(48, 200), (300, 250)] {
            assert_eq!(
                c.observe(
                    ExposureSetting {
                        exposure_us: 1000,
                        gain
                    },
                    stats(100, 0.0)
                )
                .gain,
                expected
            );
        }
    }
    #[test]
    fn dark_frames_reach_manual_ceiling_beyond_sixty_then_add_gain() {
        let mut c = AdaptiveExposure::new(100, 120_000_000, 0, 300, 100);
        let mut setting = ExposureSetting {
            exposure_us: 100_000,
            gain: 0,
        };
        for _ in 0..30 {
            setting = c.observe(setting, stats(0, 0.0));
        }
        assert_eq!(
            setting,
            ExposureSetting {
                exposure_us: 120_000_000,
                gain: 300
            }
        );
        assert_eq!(c.mode(), LightMode::Night);
    }
    #[test]
    fn abrupt_daylight_reduces_long_exposure_and_eventually_gain() {
        let mut c = AdaptiveExposure::new(100, 120_000_000, 0, 300, 100);
        let mut setting = ExposureSetting {
            exposure_us: 120_000_000,
            gain: 300,
        };
        for _ in 0..30 {
            setting = c.observe(setting, stats(255, 0.8));
        }
        assert_eq!(
            setting,
            ExposureSetting {
                exposure_us: 100,
                gain: 0
            }
        );
        assert_eq!(c.mode(), LightMode::Day);
    }
    #[test]
    fn deadband_and_hysteresis_prevent_small_fluctuation_chatter() {
        let mut c = AdaptiveExposure::new(100, 120_000_000, 0, 300, 100);
        let setting = ExposureSetting {
            exposure_us: 2_000_000,
            gain: 0,
        };
        for value in [98, 102] {
            assert_eq!(c.observe(setting, stats(value, 0.0)), setting);
        }
        assert_eq!(c.mode(), LightMode::Day);
        c.observe(setting, stats(100, 0.0));
        assert_eq!(c.mode(), LightMode::Night);
        for _ in 0..10 {
            c.observe(
                ExposureSetting {
                    exposure_us: 500_000,
                    gain: 0,
                },
                stats(100, 0.0),
            );
        }
        assert_eq!(c.mode(), LightMode::Night);
    }
}
