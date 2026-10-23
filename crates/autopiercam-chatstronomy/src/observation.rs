//! Opt-in Hub v4 full snapshots. This module never schedules image events.
use crate::protocol::{utc_milliseconds, valid_uuid};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const MAX_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OverlayOptions {
    pub telescope_id: Option<i64>,
    pub target: bool,
    pub exposure_count: bool,
    pub temperature: TemperatureSource,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureSource {
    #[default]
    Off,
    Focuser,
    External,
}

impl OverlayOptions {
    pub fn requested(&self) -> bool {
        self.target || self.exposure_count || self.temperature != TemperatureSource::Off
    }
    pub fn valid(&self) -> bool {
        self.telescope_id
            .is_none_or(|id| id > 0 && id as u64 <= MAX_INTEGER)
            && (!self.requested() || self.telescope_id.is_some())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObservationState {
    pub telescope_id: i64,
    pub telescope_name: Option<String>,
    pub state_epoch: Option<String>,
    pub revision: Option<u64>,
    pub observed_at: String,
    pub valid_until: String,
    pub available: bool,
    pub target: Option<Target>,
    pub temperatures: Option<Temperatures>,
    pub completed_exposures: Option<CompletedExposures>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub run_id: String,
    pub name: String,
    pub started_at: Option<String>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Temperatures {
    pub focuser: Option<Temperature>,
    pub external: Option<Temperature>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Temperature {
    pub celsius: f64,
    pub observed_at: String,
    pub valid_until: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CompletedExposures {
    pub count: u64,
    pub image_type: String,
    pub basis: String,
    pub as_of: String,
}

fn name(s: &str, limit: usize) -> bool {
    !s.trim().is_empty()
        && s.chars().count() <= limit
        && !s.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}
fn window(start: &str, end: &str, max: i64) -> Option<(i64, i64)> {
    let (a, b) = (utc_milliseconds(start)?, utc_milliseconds(end)?);
    (b > a && b.saturating_sub(a) <= max).then_some((a, b))
}
impl ObservationState {
    pub(crate) fn valid(&self) -> bool {
        let Some((observed, _)) = window(&self.observed_at, &self.valid_until, 30_000) else {
            return false;
        };
        if self.telescope_id <= 0
            || self.telescope_id as u64 > MAX_INTEGER
            || self.telescope_name.as_ref().is_some_and(|s| !name(s, 80))
            || self.state_epoch.as_ref().is_some_and(|s| !valid_uuid(s))
            || self.revision.is_some_and(|r| r > MAX_INTEGER)
        {
            return false;
        }
        if !self.available {
            return self.target.is_none()
                && self
                    .temperatures
                    .as_ref()
                    .is_none_or(|t| t.focuser.is_none() && t.external.is_none())
                && self.completed_exposures.is_none();
        }
        if self.state_epoch.is_none() || self.revision.is_none() {
            return false;
        }
        if let Some(t) = &self.target
            && (!valid_uuid(&t.run_id)
                || !name(&t.name, 120)
                || t.started_at
                    .as_ref()
                    .is_some_and(|s| utc_milliseconds(s).is_none_or(|ms| ms > observed)))
        {
            return false;
        }
        if let Some(temps) = &self.temperatures {
            for temp in [&temps.focuser, &temps.external].into_iter().flatten() {
                if !temp.celsius.is_finite()
                    || !(-273.15..=1000.0).contains(&temp.celsius)
                    || window(&temp.observed_at, &temp.valid_until, 120_000)
                        .is_none_or(|(a, _)| a > observed)
                {
                    return false;
                }
            }
        }
        if let Some(count) = &self.completed_exposures {
            let start = self
                .target
                .as_ref()
                .and_then(|t| t.started_at.as_deref())
                .and_then(utc_milliseconds);
            if count.count > MAX_INTEGER
                || count.image_type != "LIGHT"
                || count.basis != "successfully_saved"
                || utc_milliseconds(&count.as_of)
                    .is_none_or(|ms| start.is_none_or(|s| ms < s) || ms > observed)
            {
                return false;
            }
        }
        true
    }
}

/// Already selected, bounded text captured with the still; no later lookup by encoder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overlay {
    pub lines: Vec<String>,
}

struct Sample {
    state: ObservationState,
    received: i64,
    expires: Instant,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{Value, json};
    pub fn fixture() -> Value {
        json!({"type":"observation_state","telescope_id":17,"telescope_name":"UC131",
            "state_epoch":"6a9a7146-60eb-47ea-abec-43ad43af71e1","revision":42,
            "observed_at":"2026-10-22T05:10:00Z","valid_until":"2026-10-22T05:10:30Z","available":true,
            "target":{"run_id":"2d3768d4-3bf2-4a6e-a568-776aa6a22a39","name":"NGC 7023","started_at":"2026-10-22T04:00:00Z"},
            "temperatures":{"focuser":{"celsius":21.3,"observed_at":"2026-10-22T05:09:58Z","valid_until":"2026-10-22T05:11:58Z"},"external":null},
            "completed_exposures":{"count":505,"image_type":"LIGHT","basis":"successfully_saved","as_of":"2026-10-22T05:09:55Z"}})
    }
    fn state(value: Value) -> ObservationState {
        let crate::protocol::ServerMessage::ObservationState(state) =
            crate::protocol::ServerMessage::parse_for_version(
                &serde_json::to_vec(&value).unwrap(),
                4,
            )
            .unwrap()
        else {
            panic!("wrong message")
        };
        *state
    }
    fn options() -> OverlayOptions {
        OverlayOptions {
            telescope_id: Some(17),
            target: true,
            exposure_count: true,
            temperature: TemperatureSource::Focuser,
        }
    }
    fn millis() -> i64 {
        utc_milliseconds("2026-10-22T05:10:00Z").unwrap()
    }

    #[test]
    fn v4_is_separate_strict_and_keeps_v3_media_contract() {
        let bytes = serde_json::to_vec(&fixture()).unwrap();
        for version in 1..=3 {
            assert!(crate::protocol::ServerMessage::parse_for_version(&bytes, version).is_err());
        }
        assert!(state(fixture()).valid());
        let ready = br#"{"type":"observation_ready","telemetry":true,"interval_seconds":5,"max_age_seconds":30}"#;
        assert!(crate::protocol::ServerMessage::parse_for_version(ready, 4).is_ok());
        assert!(crate::protocol::ServerMessage::parse_for_version(ready, 3).is_err());
        assert!(
            crate::protocol::ServerMessage::parse_for_version(
                br#"{"type":"media_ready","max_timelapse_bytes":268435456}"#,
                4
            )
            .is_ok()
        );
        for (key, value) in [
            ("device_id", json!(42)),
            ("revision", json!(9007199254740992_u64)),
            ("valid_until", json!("2026-10-22T05:10:31Z")),
            ("observed_at", json!("2026-10-22T05:10:00")),
        ] {
            let mut bad = fixture();
            bad[key] = value;
            assert!(
                crate::protocol::ServerMessage::parse_for_version(
                    &serde_json::to_vec(&bad).unwrap(),
                    4
                )
                .is_err(),
                "{key}"
            );
        }
        let mut bad = fixture();
        bad["target"]["started_at"] = Value::Null;
        assert!(
            crate::protocol::ServerMessage::parse_for_version(
                &serde_json::to_vec(&bad).unwrap(),
                4
            )
            .is_err(),
            "cannot claim count with unobserved visit start"
        );
        bad["completed_exposures"] = Value::Null;
        assert!(state(bad).valid());
    }

    #[test]
    fn later_state_never_relabels_earlier_frames_and_telescope_ids_do_not_mix() {
        let (mut history, now, ms, options) =
            (History::default(), Instant::now(), millis(), options());
        history.accept(state(fixture()), &options, Some(1), ms + 10, now);
        assert!(history.overlay(&options, 1, ms + 9, now).is_none());
        let first = history.overlay(&options, 1, ms + 10, now).unwrap();
        assert_eq!(
            first.lines,
            vec![
                "Target: NGC 7023",
                "Saved LIGHT exposures: 505 · Focuser: 21.3 °C"
            ]
        );
        let mut next = fixture();
        next["revision"] = json!(43);
        next["observed_at"] = json!("2026-10-22T05:10:05Z");
        next["valid_until"] = json!("2026-10-22T05:10:35Z");
        next["target"]["name"] = json!("M31");
        next["completed_exposures"]["count"] = json!(506);
        history.accept(state(next.clone()), &options, Some(1), ms + 5000, now);
        assert_eq!(
            history.overlay(&options, 1, ms + 4999, now),
            Some(first.clone())
        );
        let second = history.overlay(&options, 1, ms + 5000, now).unwrap();
        assert_eq!(second.lines[0], "Target: M31");
        assert!(second.lines[1].contains("506"));
        next["telescope_id"] = json!(18);
        next["target"]["name"] = json!("WRONG SCOPE");
        history.accept(state(next), &options, Some(1), ms + 6000, now);
        assert_eq!(history.overlay(&options, 1, ms + 6000, now), Some(second));
        assert_eq!(
            first.lines[0], "Target: NGC 7023",
            "captured overlay owns its text"
        );
    }

    #[test]
    fn nulls_expiry_sensor_choice_and_epoch_changes_fail_closed() {
        let (mut history, now, ms, mut options) =
            (History::default(), Instant::now(), millis(), options());
        history.accept(state(fixture()), &options, Some(1), ms, now);
        options.temperature = TemperatureSource::External;
        assert_eq!(
            history.overlay(&options, 1, ms, now).unwrap().lines[1],
            "Saved LIGHT exposures: 505",
            "no fallback to focuser"
        );
        assert!(history.overlay(&options, 1, ms + 30_000, now).is_none());
        assert!(
            history
                .overlay(&options, 1, ms, now + Duration::from_secs(30))
                .is_none(),
            "clock rollback cannot extend validity"
        );
        let mut next = fixture();
        next["target"] = Value::Null;
        next["completed_exposures"] = Value::Null;
        next["temperatures"] = Value::Null;
        history.accept(state(next.clone()), &options, Some(1), ms + 100, now);
        assert!(
            history.overlay(&options, 1, ms + 100, now).is_none(),
            "null replaces, not merges"
        );
        next["available"] = json!(false);
        next["state_epoch"] = Value::Null;
        next["revision"] = Value::Null;
        history.accept(state(next), &options, Some(1), ms + 200, now);
        assert!(
            history.overlay(&options, 1, ms, now).is_none(),
            "unavailable clears old associations"
        );
        history.accept(state(fixture()), &options, Some(1), ms + 300, now);
        let mut next = fixture();
        next["state_epoch"] = json!("aaaaaaaa-60eb-47ea-abec-43ad43af71e1");
        history.accept(state(next), &options, Some(1), ms + 400, now);
        assert!(
            history.overlay(&options, 1, ms + 300, now).is_none(),
            "new epoch discards old history"
        );
        history.session(None);
        assert!(
            history.overlay(&options, 1, ms + 400, now).is_none(),
            "pause clears state"
        );
        history.accept(state(fixture()), &options, Some(1), ms + 500, now);
        assert!(
            history.overlay(&options, 2, ms + 500, now).is_none(),
            "session change clears state"
        );
    }

    #[test]
    fn history_is_bounded_and_sensor_timestamp_is_not_renewed() {
        let (mut history, now, ms, options) =
            (History::default(), Instant::now(), millis(), options());
        let mut stale_sensor = fixture();
        stale_sensor["temperatures"]["focuser"]["observed_at"] = json!("2026-10-22T05:08:00Z");
        stale_sensor["temperatures"]["focuser"]["valid_until"] = json!("2026-10-22T05:10:00Z");
        for i in 0..200 {
            history.accept(state(stale_sensor.clone()), &options, Some(1), ms + i, now);
        }
        assert_eq!(history.samples.len(), 128);
        assert_eq!(
            history.overlay(&options, 1, ms + 199, now).unwrap().lines[1],
            "Saved LIGHT exposures: 505"
        );
        assert!(
            !OverlayOptions {
                target: true,
                ..Default::default()
            }
            .valid()
        );
        assert!(!OverlayOptions::default().requested());
    }

    #[test]
    fn repeated_old_snapshots_cannot_revive_cleared_labels() {
        let (mut history, now, ms, options) =
            (History::default(), Instant::now(), millis(), options());
        history.accept(state(fixture()), &options, Some(1), ms, now);
        let mut cleared = fixture();
        cleared["revision"] = json!(43);
        cleared["observed_at"] = json!("2026-10-22T05:10:05Z");
        cleared["valid_until"] = json!("2026-10-22T05:10:35Z");
        cleared["target"] = Value::Null;
        cleared["completed_exposures"] = Value::Null;
        cleared["temperatures"] = Value::Null;
        history.accept(state(cleared.clone()), &options, Some(1), ms + 5000, now);
        for i in 1..=3 {
            history.accept(state(fixture()), &options, Some(1), ms + 5000 + i, now);
            assert!(
                history.overlay(&options, 1, ms + 5000 + i, now).is_none(),
                "old replay {i} revived cleared state"
            );
        }
        cleared["available"] = json!(false);
        cleared["state_epoch"] = Value::Null;
        cleared["revision"] = Value::Null;
        history.accept(state(cleared), &options, Some(1), ms + 6000, now);
        history.accept(state(fixture()), &options, Some(1), ms + 7000, now);
        assert!(
            history.overlay(&options, 1, ms + 7000, now).is_none(),
            "old epoch must not revive unavailable state"
        );
    }
}
#[derive(Default)]
pub(crate) struct History {
    session: Option<u64>,
    samples: VecDeque<Sample>,
}
impl History {
    pub fn clear(&mut self) {
        self.samples.clear();
        self.session = None;
    }
    pub fn session(&mut self, session: Option<u64>) {
        if self.session != session || session.is_none() {
            self.clear();
            self.session = session;
        }
    }
    pub fn accept(
        &mut self,
        state: ObservationState,
        options: &OverlayOptions,
        session: Option<u64>,
        now_ms: i64,
        now: Instant,
    ) {
        self.session(session);
        if session.is_none()
            || Some(state.telescope_id) != options.telescope_id
            || !options.requested()
        {
            return;
        }
        let Some((observed, until)) = window(&state.observed_at, &state.valid_until, 30_000) else {
            self.clear();
            return;
        };
        // Retain the most recent clearing snapshot as a watermark. Dropping it
        // for an older message would let a second replay revive stale labels.
        // Timestamp ordering also applies across unavailable/null epochs.
        if self.samples.back().is_some_and(|previous| {
            observed < utc_milliseconds(&previous.state.observed_at).unwrap()
                || now_ms < previous.received
                || (previous.state.state_epoch == state.state_epoch
                    && state.revision < previous.state.revision)
        }) {
            return;
        }
        if !state.valid() || observed > now_ms || until <= now_ms {
            self.clear();
            return;
        }
        if let Some(previous) = self.samples.back()
            && previous.state.state_epoch != state.state_epoch
        {
            self.samples.clear();
        }
        if !state.available {
            self.samples.clear();
        }
        self.samples.push_back(Sample {
            state,
            received: now_ms,
            expires: now + Duration::from_millis((until - now_ms) as u64),
        });
        while self.samples.len() > 128 {
            self.samples.pop_front();
        }
    }
    pub fn overlay(
        &mut self,
        options: &OverlayOptions,
        session: u64,
        capture_ms: i64,
        now: Instant,
    ) -> Option<Overlay> {
        self.session(Some(session));
        let sample = self
            .samples
            .iter()
            .rev()
            .find(|s| s.received <= capture_ms)?;
        let state = &sample.state;
        if now >= sample.expires
            || capture_ms >= utc_milliseconds(&state.valid_until)?
            || !state.available
            || Some(state.telescope_id) != options.telescope_id
        {
            return None;
        }
        let mut lines = Vec::new();
        if options.target
            && let Some(target) = &state.target
        {
            lines.push(format!("Target: {}", target.name));
        }
        let mut diagnostics = Vec::new();
        if options.exposure_count
            && let Some(count) = &state.completed_exposures
        {
            diagnostics.push(format!("Saved LIGHT exposures: {}", count.count));
        }
        if let Some(temps) = &state.temperatures {
            let (label, value) = match options.temperature {
                TemperatureSource::Off => ("", None),
                TemperatureSource::Focuser => ("Focuser", temps.focuser.as_ref()),
                TemperatureSource::External => ("External", temps.external.as_ref()),
            };
            if let Some(t) = value
                && capture_ms < utc_milliseconds(&t.valid_until)?
            {
                diagnostics.push(format!("{label}: {:.1} °C", t.celsius));
            }
        }
        if !diagnostics.is_empty() {
            lines.push(diagnostics.join(" · "));
        }
        (!lines.is_empty()).then_some(Overlay { lines })
    }
}
