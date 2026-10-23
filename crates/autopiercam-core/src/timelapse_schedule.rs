//! Pure, UTC-valued recording windows and local-clock delivery boundaries.
//! No wall-clock reads: restart and DST tests use the same production planner.
use crate::{config::CameraConfig, solar::solar_altitude};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    #[default]
    UtcDay,
    LocalNoon,
    DuskToDawn,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    #[default]
    Immediate,
    LocalTime,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Twilight {
    #[default]
    Sunset,
    Civil,
    Astronomical,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimelapseSchedule {
    pub window: WindowMode,
    pub timezone: String,
    pub twilight: Twilight,
    pub delivery: DeliveryMode,
    pub send_minute: u16,
}
impl Default for TimelapseSchedule {
    fn default() -> Self {
        Self {
            window: WindowMode::UtcDay,
            timezone: "UTC".into(),
            twilight: Twilight::Sunset,
            delivery: DeliveryMode::Immediate,
            send_minute: 720,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub start_ms: u64,
    pub end_ms: u64,
    pub send_after_ms: u64,
}
impl Window {
    pub fn valid(&self) -> bool {
        self.start_ms < self.end_ms
            && self.end_ms - self.start_ms <= 26 * 3_600_000
            && self.send_after_ms >= self.end_ms
            && self.send_after_ms - self.end_ms <= 27 * 3_600_000
    }
    pub fn contains(&self, timestamp: u64) -> bool {
        timestamp >= self.start_ms && timestamp < self.end_ms
    }
}

// Repeated local time: first occurrence. Missing local time: first valid minute
// after the gap, up to three hours. Exceptional skipped dates fail closed.
fn local_boundary(tz: Tz, date: NaiveDate, minute: u16) -> Option<DateTime<Utc>> {
    let naive = date.and_hms_opt(u32::from(minute / 60), u32::from(minute % 60), 0)?;
    for shift in 0..=180 {
        let candidate = naive.checked_add_signed(Duration::minutes(shift))?;
        match tz.from_local_datetime(&candidate) {
            LocalResult::Single(t) => return Some(t.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, b) => return Some(a.min(b).with_timezone(&Utc)),
            LocalResult::None => {}
        }
    }
    None
}
fn ms(t: DateTime<Utc>) -> Option<u64> {
    u64::try_from(t.timestamp_millis()).ok()
}

impl TimelapseSchedule {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self, camera: &CameraConfig) -> Result<(), &'static str> {
        if self.timezone.len() > 100 || self.timezone.parse::<Tz>().is_err() {
            return Err("timelapse timezone must be an IANA name, e.g. America/Los_Angeles");
        }
        if self.send_minute >= 1440 {
            return Err("timelapse send_minute must be in 0..1440");
        }
        if self.window == WindowMode::DuskToDawn
            && !camera
                .latitude_deg
                .zip(camera.longitude_deg)
                .is_some_and(|(a, b)| {
                    a.is_finite()
                        && b.is_finite()
                        && (-90.0..=90.0).contains(&a)
                        && (-180.0..=180.0).contains(&b)
                })
        {
            return Err("dusk-to-dawn timelapses require a valid observing latitude and longitude");
        }
        Ok(())
    }

    /// Window enclosing this timestamp, or None during daylight / polar periods
    /// without both twilight crossings. Solar transitions are minute-quantized
    /// estimates using the existing NOAA approximation, not sensor measurements.
    pub fn at(&self, timestamp: u64, camera: &CameraConfig) -> Option<Window> {
        self.validate(camera).ok()?;
        let tz = self.timezone.parse::<Tz>().ok()?;
        let now = DateTime::<Utc>::from_timestamp_millis(i64::try_from(timestamp).ok()?)?;
        let (start, end) = if self.window == WindowMode::UtcDay {
            let start = now.date_naive().and_hms_opt(0, 0, 0)?.and_utc();
            (start, start.checked_add_signed(Duration::days(1))?)
        } else {
            let date = now.with_timezone(&tz).date_naive();
            let noon = local_boundary(tz, date, 720)?;
            let date = if now < noon { date.pred_opt()? } else { date };
            let a = local_boundary(tz, date, 720)?;
            let b = local_boundary(tz, date.succ_opt()?, 720)?;
            if self.window == WindowMode::LocalNoon {
                (a, b)
            } else {
                let (lat, lon) = camera.latitude_deg.zip(camera.longitude_deg)?;
                // A night belongs to the observing location, not the display
                // timezone. Solar noon brackets both crossings even when the
                // operator deliberately uses UTC for a western observatory.
                let solar_noon = now
                    .date_naive()
                    .and_hms_opt(0, 0, 0)?
                    .and_utc()
                    .checked_add_signed(Duration::minutes((720.0 - lon * 4.0).round() as i64))?;
                let a = if now < solar_noon {
                    solar_noon.checked_sub_signed(Duration::days(1))?
                } else {
                    solar_noon
                };
                let b = a.checked_add_signed(Duration::days(1))?;
                let altitude = match self.twilight {
                    Twilight::Sunset => -0.833,
                    Twilight::Civil => -6.0,
                    Twilight::Astronomical => -18.0,
                };
                let mut previous = solar_altitude(lat, lon, a)?;
                let mut dusk = None;
                let mut night = None;
                let minutes = (b - a).num_minutes();
                if !(1..=1560).contains(&minutes) {
                    return None;
                }
                for minute in 1..=minutes {
                    let t = a.checked_add_signed(Duration::minutes(minute))?;
                    let current = solar_altitude(lat, lon, t)?;
                    if previous >= altitude && current < altitude {
                        dusk = Some(t);
                    }
                    if previous < altitude
                        && current >= altitude
                        && let Some(dusk) = dusk
                    {
                        night = Some((dusk, t));
                        break;
                    }
                    previous = current;
                }
                night?
            }
        };
        let send = if self.delivery == DeliveryMode::Immediate {
            end
        } else {
            let date = end.with_timezone(&tz).date_naive();
            let boundary = local_boundary(tz, date, self.send_minute)?;
            if boundary >= end {
                boundary
            } else {
                local_boundary(tz, date.succ_opt()?, self.send_minute)?
            }
        };
        let window = Window {
            start_ms: ms(start)?,
            end_ms: ms(end)?,
            send_after_ms: ms(send)?,
        };
        (window.valid() && window.contains(timestamp)).then_some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ts(s: &str) -> u64 {
        ms(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)).unwrap()
    }
    #[test]
    fn noon_windows_follow_dst_not_twenty_four_hour_countdowns() {
        let schedule = TimelapseSchedule {
            window: WindowMode::LocalNoon,
            timezone: "America/Los_Angeles".into(),
            ..Default::default()
        };
        for (input, start, end, hours) in [
            (
                "2026-03-08T09:00:00Z",
                "2026-03-07T20:00:00Z",
                "2026-03-08T19:00:00Z",
                23,
            ),
            (
                "2026-11-01T09:00:00Z",
                "2026-10-31T19:00:00Z",
                "2026-11-01T20:00:00Z",
                25,
            ),
        ] {
            let w = schedule.at(ts(input), &CameraConfig::default()).unwrap();
            assert_eq!((w.start_ms, w.end_ms), (ts(start), ts(end)));
            assert_eq!(w.end_ms - w.start_ms, hours * 3_600_000);
            assert!(!w.contains(w.end_ms));
            assert_eq!(
                schedule
                    .at(w.end_ms, &CameraConfig::default())
                    .unwrap()
                    .start_ms,
                w.end_ms
            );
        }
    }
    #[test]
    fn night_spans_midnight_and_delivery_is_noon_not_dawn() {
        let camera = CameraConfig {
            latitude_deg: Some(38.7),
            longitude_deg: Some(-121.1),
            ..Default::default()
        };
        let schedule = TimelapseSchedule {
            window: WindowMode::DuskToDawn,
            timezone: "America/Los_Angeles".into(),
            delivery: DeliveryMode::LocalTime,
            ..Default::default()
        };
        let evening = schedule.at(ts("2026-10-23T05:00:00Z"), &camera).unwrap();
        let morning = schedule.at(ts("2026-10-23T12:00:00Z"), &camera).unwrap();
        assert_eq!(evening, morning);
        assert_eq!(evening.send_after_ms, ts("2026-10-23T19:00:00Z"));
        assert!(
            evening.start_ms >= ts("2026-10-23T01:00:00Z")
                && evening.start_ms <= ts("2026-10-23T03:00:00Z")
        );
        assert!(
            evening.end_ms >= ts("2026-10-23T13:00:00Z")
                && evening.end_ms <= ts("2026-10-23T15:00:00Z")
        );
        assert!(schedule.at(ts("2026-10-23T18:00:00Z"), &camera).is_none());
        assert!(schedule.at(evening.start_ms - 1, &camera).is_none());
        assert!(schedule.at(evening.end_ms, &camera).is_none());
        assert_eq!(schedule.at(evening.start_ms, &camera).unwrap(), evening);
        let civil = TimelapseSchedule {
            twilight: Twilight::Civil,
            ..schedule.clone()
        }
        .at(ts("2026-10-23T05:00:00Z"), &camera)
        .unwrap();
        // Sunset/sunrise includes both twilight transitions, not just darkness.
        assert!(evening.start_ms < civil.start_ms);
        assert!(evening.end_ms > civil.end_ms);
        let utc = TimelapseSchedule {
            timezone: "UTC".into(),
            ..schedule.clone()
        }
        .at(ts("2026-10-23T05:00:00Z"), &camera)
        .unwrap();
        assert_eq!(
            (utc.start_ms, utc.end_ms),
            (evening.start_ms, evening.end_ms)
        );
        assert_eq!(utc.send_after_ms, ts("2026-10-24T12:00:00Z"));
    }
    #[test]
    fn ambiguous_and_missing_clock_times_have_one_deterministic_boundary() {
        let tz: Tz = "America/Los_Angeles".parse().unwrap();
        assert_eq!(
            ms(local_boundary(tz, NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(), 150).unwrap()),
            Some(ts("2026-03-08T10:00:00Z"))
        );
        assert_eq!(
            ms(local_boundary(tz, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), 90).unwrap()),
            Some(ts("2026-11-01T08:30:00Z"))
        );
    }
    #[test]
    fn missing_location_bad_timezone_and_polar_no_crossing_fail_closed() {
        let mut s = TimelapseSchedule {
            window: WindowMode::DuskToDawn,
            ..Default::default()
        };
        assert!(s.validate(&CameraConfig::default()).is_err());
        let c = CameraConfig {
            latitude_deg: Some(90.0),
            longitude_deg: Some(0.0),
            ..Default::default()
        };
        for date in ["2026-06-21T23:00:00Z", "2026-12-21T23:00:00Z"] {
            assert!(s.at(ts(date), &c).is_none());
        }
        s.timezone = "not/a/timezone".into();
        assert!(s.validate(&c).is_err());
    }
}
