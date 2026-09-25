//! Bounded, completed-frame scheduling. No camera commands or image backlog.
use crate::service::{Frame, Preferences};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TriggerRules {
    pub interval_minutes: u16,
    pub scene_changes: bool,
    pub day_night: bool,
    pub telescope_events: bool,
    pub burst_count: u8,
    pub spacing_seconds: u16,
}

impl TriggerRules {
    pub fn local(p: &Preferences) -> Self {
        Self {
            interval_minutes: p.interval_minutes,
            scene_changes: p.scene_changes,
            day_night: p.day_night,
            telescope_events: p.telescope_events,
            burst_count: p.burst_count,
            spacing_seconds: p.spacing_seconds,
        }
    }

    /// Chat may narrow local consent, never enable a locally disabled source,
    /// increase burst size, or shorten the locally selected intervals.
    pub fn allowed_by(&self, p: &Preferences) -> bool {
        self.interval_minutes <= 1440
            && (self.interval_minutes == 0
                || (p.interval_minutes > 0 && self.interval_minutes >= p.interval_minutes))
            && (!self.scene_changes || p.scene_changes)
            && (!self.day_night || p.day_night)
            && (!self.telescope_events || p.telescope_events)
            && (1..=p.burst_count).contains(&self.burst_count)
            && (p.spacing_seconds..=600).contains(&self.spacing_seconds)
    }
}

pub(crate) fn telescope_summary(event: &str) -> Option<&'static str> {
    match event {
        "mount_slew_started" => Some("Pier camera: mount slew started"),
        "mount_slewed" => Some("Pier camera: mount slew ended"),
        "sequence_started" => Some("Pier camera: sequence started"),
        "sequence_finished" => Some("Pier camera: sequence finished"),
        _ => None,
    }
}

struct Burst {
    kind: &'static str,
    summary: &'static str,
    session: u64,
    after_sequence: u64,
    remaining: u8,
    due: Instant,
    deadline: Instant,
}

pub(crate) struct Scheduler {
    rules: TriggerRules,
    periodic_due: Instant,
    burst: Option<Burst>,
    last_queued: Option<(u64, u64)>,
}

impl Scheduler {
    pub fn new(rules: TriggerRules, now: Instant) -> Self {
        let periodic_due = now + Duration::from_secs(u64::from(rules.interval_minutes) * 60);
        Self {
            rules,
            periodic_due,
            burst: None,
            last_queued: None,
        }
    }

    pub fn trigger(
        &mut self,
        kind: &'static str,
        summary: &'static str,
        frame: &Frame,
        now: Instant,
        wait_new: bool,
    ) {
        if self.burst.is_some() {
            return;
        } // coalesce; never queue an event storm
        self.burst = Some(Burst {
            kind,
            summary,
            session: frame.session,
            after_sequence: if wait_new {
                frame.sequence
            } else {
                frame.sequence.saturating_sub(1)
            },
            remaining: self.rules.burst_count,
            due: now,
            deadline: now
                + Duration::from_secs(
                    180 + u64::from(self.rules.spacing_seconds)
                        * u64::from(self.rules.burst_count - 1),
                ),
        });
    }

    pub fn due(
        &mut self,
        frame: Option<&Frame>,
        now: Instant,
        wall_ms: u64,
    ) -> Option<(&'static str, &'static str)> {
        let Some(frame) = frame else {
            self.burst = None;
            return None;
        };
        if self
            .burst
            .as_ref()
            .is_some_and(|b| b.session != frame.session || now >= b.deadline)
        {
            self.burst = None;
        }
        if self.rules.interval_minutes > 0 && now >= self.periodic_due {
            self.periodic_due =
                now + Duration::from_secs(u64::from(self.rules.interval_minutes) * 60);
            // Periodic sends are single frames; bursts are only for observations/events.
            if self.burst.is_none() {
                self.trigger("periodic", "Scheduled pier-camera image", frame, now, false);
                self.burst.as_mut().unwrap().remaining = 1;
            }
        }
        let b = self.burst.as_ref()?;
        (now >= b.due
            && self.last_queued != Some((frame.session, frame.sequence))
            && frame.sequence > b.after_sequence
            && wall_ms.saturating_sub(frame.captured_at_unix_ms) <= 120_000
            && frame.captured_at_unix_ms <= wall_ms.saturating_add(30_000))
        .then_some((b.kind, b.summary))
    }

    pub fn queued(&mut self, frame: &Frame, now: Instant) {
        self.last_queued = Some((frame.session, frame.sequence));
        if let Some(b) = &mut self.burst {
            b.remaining -= 1;
            b.after_sequence = frame.sequence;
            b.due = now + Duration::from_secs(u64::from(self.rules.spacing_seconds));
            if b.remaining == 0 {
                self.burst = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn burst_storm_is_coalesced_and_sends_at_most_three_distinct_images() {
        let now = Instant::now();
        let mut f = crate::tests::frame(1, false);
        let mut s = Scheduler::new(
            TriggerRules::local(&Preferences {
                burst_count: 3,
                ..Default::default()
            }),
            now,
        );
        s.trigger("scene_change", "original", &f, now, false);
        for index in 0..3 {
            let at = now + Duration::from_secs(index * 60);
            s.trigger("telescope_event", "coalesced", &f, at, false);
            assert_eq!(
                s.due(Some(&f), at, f.captured_at_unix_ms),
                Some(("scene_change", "original"))
            );
            s.queued(&f, at);
            assert!(
                s.due(
                    Some(&f),
                    at + Duration::from_secs(60),
                    f.captured_at_unix_ms
                )
                .is_none()
            );
            f.sequence += 1;
        }
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(180),
                f.captured_at_unix_ms
            )
            .is_none()
        );
    }
    #[test]
    fn chat_cannot_expand_consent_or_rate() {
        let p = Preferences {
            interval_minutes: 5,
            burst_count: 2,
            telescope_events: true,
            ..Default::default()
        };
        let rules = TriggerRules::local(&p);
        assert!(rules.allowed_by(&p));
        for bad in [
            TriggerRules {
                interval_minutes: 1,
                ..rules.clone()
            },
            TriggerRules {
                burst_count: 3,
                ..rules.clone()
            },
            TriggerRules {
                spacing_seconds: 59,
                ..rules.clone()
            },
            TriggerRules {
                scene_changes: true,
                ..rules.clone()
            },
        ] {
            assert!(!bad.allowed_by(&p));
        }
        assert!(!rules.allowed_by(&Preferences::default()));
    }
    #[test]
    fn bursts_wait_for_new_frames_and_expire_without_backlog() {
        let now = Instant::now();
        let mut f = crate::tests::frame(1, false);
        let mut s = Scheduler::new(
            TriggerRules::local(&Preferences {
                burst_count: 3,
                ..Default::default()
            }),
            now,
        );
        s.trigger("telescope_event", "slew", &f, now, true);
        assert!(s.due(Some(&f), now, f.captured_at_unix_ms).is_none());
        f.sequence += 1;
        assert!(s.due(Some(&f), now, f.captured_at_unix_ms).is_some());
        s.queued(&f, now);
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(60),
                f.captured_at_unix_ms
            )
            .is_none()
        );
        f.sequence += 1;
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(59),
                f.captured_at_unix_ms
            )
            .is_none()
        );
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(60),
                f.captured_at_unix_ms
            )
            .is_some()
        );
        f.session += 1;
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(61),
                f.captured_at_unix_ms
            )
            .is_none()
        );
        s.trigger("telescope_event", "slew", &f, now, false);
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(301),
                f.captured_at_unix_ms
            )
            .is_none()
        );
    }
    #[test]
    fn periodic_is_delayed_single_and_stale_frames_are_not_sent() {
        let now = Instant::now();
        let f = crate::tests::frame(1, false);
        let mut s = Scheduler::new(
            TriggerRules::local(&Preferences {
                interval_minutes: 5,
                burst_count: 3,
                ..Default::default()
            }),
            now,
        );
        assert!(s.due(Some(&f), now, f.captured_at_unix_ms).is_none());
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(300),
                f.captured_at_unix_ms + 121_000
            )
            .is_none()
        );
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(301),
                f.captured_at_unix_ms
            )
            .is_some()
        );
        s.queued(&f, now + Duration::from_secs(301));
        assert!(
            s.due(
                Some(&f),
                now + Duration::from_secs(400),
                f.captured_at_unix_ms
            )
            .is_none()
        );
        assert!(telescope_summary("slew_to_coordinates").is_none());
    }
}
