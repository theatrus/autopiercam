//! Startup lighting estimate, not a replacement for measured exposure feedback.
use crate::{config::CameraConfig, exposure::LightMode};
use chrono::{DateTime, Datelike, Timelike, Utc};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn startup_mode(config: &CameraConfig, now: SystemTime) -> Option<LightMode> {
    let seconds = i64::try_from(now.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    let time = DateTime::<Utc>::from_timestamp(seconds, 0)?;
    let altitude = solar_altitude(config.latitude_deg?, config.longitude_deg?, time)?;
    // Civil twilight remains day; night starts once the Sun is six degrees below the horizon.
    Some(if altitude < -6.0 {
        LightMode::Night
    } else {
        LightMode::Day
    })
}

/// NOAA's fractional-year approximation. UTC plus east-positive longitude avoids
/// local timezone/DST assumptions and also works during polar day/night.
/// https://gml.noaa.gov/grad/solcalc/solareqns.PDF
fn solar_altitude(latitude: f64, longitude: f64, time: DateTime<Utc>) -> Option<f64> {
    if !latitude.is_finite()
        || !longitude.is_finite()
        || !(-90.0..=90.0).contains(&latitude)
        || !(-180.0..=180.0).contains(&longitude)
    {
        return None;
    }
    let hour = f64::from(time.num_seconds_from_midnight()) / 3600.0;
    let year_length = if time.date_naive().leap_year() {
        366.0
    } else {
        365.0
    };
    let gamma =
        std::f64::consts::TAU / year_length * (f64::from(time.ordinal0()) + (hour - 12.0) / 24.0);
    let equation = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());
    let declination = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2.0 * gamma).cos()
        + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos()
        + 0.00148 * (3.0 * gamma).sin();
    let angle =
        ((hour * 60.0 + equation + 4.0 * longitude).rem_euclid(1440.0) / 4.0 - 180.0).to_radians();
    let lat = latitude.to_radians();
    Some(
        (lat.sin() * declination.sin() + lat.cos() * declination.cos() * angle.cos())
            .clamp(-1.0, 1.0)
            .asin()
            .to_degrees(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mode(lat: f64, lon: f64, date: &str) -> LightMode {
        let time = DateTime::parse_from_rfc3339(date)
            .unwrap()
            .with_timezone(&Utc);
        startup_mode(
            &CameraConfig {
                latitude_deg: Some(lat),
                longitude_deg: Some(lon),
                ..CameraConfig::default()
            },
            time.into(),
        )
        .unwrap()
    }
    #[test]
    fn longitude_and_utc_determine_day_not_host_timezone() {
        assert_eq!(mode(0.0, 0.0, "2026-03-20T12:00:00Z"), LightMode::Day);
        assert_eq!(mode(0.0, 180.0, "2026-03-20T12:00:00Z"), LightMode::Night);
        assert_eq!(mode(0.0, -180.0, "2026-03-20T12:00:00Z"), LightMode::Night);
        assert_eq!(mode(40.0, -120.0, "2026-10-02T03:00:00Z"), LightMode::Night);
        assert_eq!(mode(40.0, -120.0, "2026-10-01T20:00:00Z"), LightMode::Day);
    }
    #[test]
    fn seasons_poles_and_leap_day() {
        assert_eq!(mode(90.0, 0.0, "2024-06-21T00:00:00Z"), LightMode::Day);
        assert_eq!(mode(-90.0, 0.0, "2024-06-21T12:00:00Z"), LightMode::Night);
        assert_eq!(mode(90.0, 0.0, "2024-12-21T12:00:00Z"), LightMode::Night);
        assert_eq!(mode(-90.0, 0.0, "2024-12-21T00:00:00Z"), LightMode::Day);
        assert_eq!(mode(0.0, 0.0, "2024-02-29T12:00:00Z"), LightMode::Day);
    }
    #[test]
    fn twilight_threshold_and_missing_location() {
        assert_eq!(mode(0.0, 0.0, "2026-03-20T18:20:00Z"), LightMode::Day);
        assert_eq!(mode(0.0, 0.0, "2026-03-20T18:40:00Z"), LightMode::Night);
        assert_eq!(startup_mode(&CameraConfig::default(), UNIX_EPOCH), None);
        let mut config = CameraConfig {
            latitude_deg: Some(f64::NAN),
            longitude_deg: Some(0.0),
            ..CameraConfig::default()
        };
        assert_eq!(startup_mode(&config, UNIX_EPOCH), None);
        config.latitude_deg = Some(91.0);
        assert_eq!(startup_mode(&config, UNIX_EPOCH), None);
    }
}
