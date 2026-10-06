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

#[derive(Clone, Copy, PartialEq, Eq)]
enum TelescopeEvent {
    SlewStarted,
    SlewCompleted,
    SequenceStarted,
    SequenceFinished,
}

impl TelescopeEvent {
    fn parse(event: &str) -> Option<Self> {
        match event {
            "mount_slew_started" => Some(Self::SlewStarted),
            "mount_slewed" => Some(Self::SlewCompleted),
            "sequence_started" => Some(Self::SequenceStarted),
            "sequence_finished" => Some(Self::SequenceFinished),
            _ => None,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::SlewStarted => "mount slew started",
            Self::SlewCompleted => "mount slew completed",
            Self::SequenceStarted => "sequence started",
            Self::SequenceFinished => "sequence finished",
        }
    }
}

struct Burst {
    kind: &'static str,
    summary: &'static str,
    telescope: Option<TelescopeEvent>,
    total: u8,
    session: u64,
    after_sequence: u64,
    remaining: u8,
    due: Instant,
    deadline: Instant,
}

struct SlewFinish {
    session: u64,
    after_sequence: u64,
    after_unix_ms: u64,
    deadline: Instant,
}

pub(crate) struct Scheduler {
    slew_finish: Option<SlewFinish>,
    finish_ready: bool,
    last_queued_at: Option<Instant>,
    rules: TriggerRules,
    periodic_due: Instant,
    burst: Option<Burst>,
    last_queued: Option<(u64, u64)>,
}

impl Scheduler {
    pub fn new(rules: TriggerRules, now: Instant) -> Self {
        let periodic_due = now + Duration::from_secs(u64::from(rules.interval_minutes) * 60);
        Self {
            slew_finish: None,
            finish_ready: false,
            last_queued_at: None,
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
            telescope: None,
            total: self.rules.burst_count,
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

    /// Motion images stay available. Completion reserves one separate post-slew
    /// image; it never stops/restarts capture or relabels an in-flight exposure.
    pub fn telescope_trigger(&mut self, event: &str, frame: &Frame, now: Instant, wall_ms: u64) {
        let Some(event) = TelescopeEvent::parse(event) else {
            return;
        };
        if event == TelescopeEvent::SlewCompleted {
            // Duplicate notifications cannot postpone the pending image forever.
            if self
                .slew_finish
                .as_ref()
                .is_none_or(|p| p.session != frame.session || now >= p.deadline)
            {
                let exposure_seconds = frame
                    .exposure_us
                    .and_then(|v| u64::try_from(v).ok())
                    .unwrap_or(60_000_000)
                    .div_ceil(1_000_000);
                self.slew_finish = Some(SlewFinish {
                    session: frame.session,
                    after_sequence: frame.sequence,
                    after_unix_ms: wall_ms,
                    // Several 30-60s integrations may be needed. Still bounded
                    // if capture or timing provenance never becomes usable.
                    deadline: now
                        + Duration::from_secs(
                            exposure_seconds
                                .saturating_mul(4)
                                .saturating_add(180)
                                .clamp(300, 3600),
                        ),
                });
            }
            return;
        }
        if event == TelescopeEvent::SlewStarted {
            // A genuinely new slew supersedes the previous unfinished request.
            // This only changes sharing state, never camera acquisition.
            self.slew_finish = None;
        }
        if self
            .burst
            .as_ref()
            .is_some_and(|b| b.session != frame.session || now >= b.deadline)
        {
            self.burst = None;
        }
        if self.burst.is_none() {
            self.trigger("telescope_event", "", frame, now, true);
            self.burst.as_mut().unwrap().telescope = Some(event);
        } else if let Some(b) = &mut self.burst
            && b.telescope.is_some_and(|previous| previous != event)
        {
            b.telescope = Some(event);
            // A new caption must not be attached to a frame already available
            // before that event. Duplicate events must not starve delivery.
            b.after_sequence = frame.sequence;
        }
    }

    pub fn due(
        &mut self,
        frame: Option<&Frame>,
        now: Instant,
        wall_ms: u64,
    ) -> Option<(&'static str, String)> {
        self.finish_ready = false;
        let Some(frame) = frame else {
            self.burst = None;
            self.slew_finish = None;
            return None;
        };
        if self
            .slew_finish
            .as_ref()
            .is_some_and(|p| p.session != frame.session || now >= p.deadline)
        {
            self.slew_finish = None;
        }
        if self.slew_finish.as_ref().is_some_and(|p| {
            frame.sequence > p.after_sequence
                && self.last_queued_at.is_none_or(|at| {
                    now.saturating_duration_since(at)
                        >= Duration::from_secs(u64::from(self.rules.spacing_seconds))
                })
                && self.last_queued != Some((frame.session, frame.sequence))
                && frame
                    .conservative_start_unix_ms
                    .is_some_and(|start| start > p.after_unix_ms && start <= wall_ms)
                && wall_ms.saturating_sub(frame.captured_at_unix_ms) <= 120_000
                && frame.captured_at_unix_ms <= wall_ms
        }) {
            self.finish_ready = true;
            return Some(("telescope_event", "Pier camera: Post-slew image after mount slew completed (exposure timing estimated)".into()));
        }
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
        .then(|| {
            let summary = match b.telescope {
                Some(event) => {
                    let index = b.total - b.remaining + 1;
                    let label = if index == 1 { "Update" } else { "Follow-up" };
                    format!(
                        "Pier camera: {label} after {} (image {index} of {})",
                        event.description(),
                        b.total
                    )
                }
                None => b.summary.to_owned(),
            };
            (b.kind, summary)
        })
    }

    pub fn queued(&mut self, frame: &Frame, now: Instant) {
        self.last_queued = Some((frame.session, frame.sequence));
        self.last_queued_at = Some(now);
        if self.finish_ready {
            self.finish_ready = false;
            self.slew_finish = None;
            // No more start-followups after the post-slew image. Already queued
            // bytes retain their caption for retries; no exposure is cancelled.
            if self
                .burst
                .as_ref()
                .is_some_and(|b| b.telescope == Some(TelescopeEvent::SlewStarted))
            {
                self.burst = None;
            }
            return;
        }
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
    fn long_exposure_finish_waits_without_suppressing_motion_images() {
        for seconds in [30, 60] {
            let now = Instant::now();
            let wall = 1_000_000;
            let mut f = crate::tests::frame(1, false);
            f.captured_at_unix_ms = wall;
            f.exposure_us = Some(seconds * 1_000_000);
            let mut s = Scheduler::new(
                TriggerRules::local(&Preferences {
                    burst_count: 3,
                    ..Default::default()
                }),
                now,
            );
            s.telescope_trigger("mount_slew_started", &f, now, wall);
            s.telescope_trigger("mount_slewed", &f, now, wall);
            let deadline = s.slew_finish.as_ref().unwrap().deadline;
            // The in-flight frame may be blurred and is still a useful motion image.
            f.sequence += 1;
            f.conservative_start_unix_ms = Some(wall - 1);
            assert_eq!(
                s.due(Some(&f), now, wall).unwrap().1,
                "Pier camera: Update after mount slew started (image 1 of 3)"
            );
            s.queued(&f, now);
            // Delivery after completion is insufficient: an old/buffered frame
            // remains ineligible even once the send spacing has elapsed.
            let later = now + Duration::from_secs(120);
            f.sequence += 1;
            f.captured_at_unix_ms = wall + 120_000;
            s.telescope_trigger("mount_slewed", &f, later, wall + 120_000);
            assert_eq!(s.slew_finish.as_ref().unwrap().deadline, deadline);
            assert!(
                s.due(Some(&f), later, wall + 120_000)
                    .unwrap()
                    .1
                    .contains("Follow-up after mount slew started")
            );
            s.queued(&f, later);
            f.sequence += 1;
            f.conservative_start_unix_ms = Some(wall + 1);
            f.captured_at_unix_ms = wall + 180_000;
            assert!(
                s.due(Some(&f), later + Duration::from_secs(59), wall + 180_000)
                    .is_none()
            );
            let ready = later + Duration::from_secs(60);
            assert_eq!(
                s.due(Some(&f), ready, wall + 180_000).unwrap().1,
                "Pier camera: Post-slew image after mount slew completed (exposure timing estimated)"
            );
            s.queued(&f, ready);
            f.sequence += 1;
            assert!(
                s.due(Some(&f), ready + Duration::from_secs(60), wall + 180_000)
                    .is_none()
            );
        }
    }

    #[test]
    fn finish_requires_known_timing_and_survives_exhausted_motion_burst() {
        let now = Instant::now();
        let wall = 1_000_000;
        let mut f = crate::tests::frame(1, false);
        f.captured_at_unix_ms = wall;
        let mut s = Scheduler::new(TriggerRules::local(&Preferences::default()), now);
        s.telescope_trigger("mount_slew_started", &f, now, wall);
        f.sequence += 1;
        s.due(Some(&f), now, wall).unwrap();
        s.queued(&f, now); // one-image burst is exhausted
        s.telescope_trigger("mount_slewed", &f, now, wall);
        f.sequence += 1;
        for timing in [None, Some(wall - 1), Some(wall), Some(wall + 999_999)] {
            f.conservative_start_unix_ms = timing;
            assert!(
                s.due(Some(&f), now + Duration::from_secs(60), wall + 60_000)
                    .is_none()
            );
            assert!(s.slew_finish.is_some());
        }
        f.conservative_start_unix_ms = Some(wall + 1);
        assert!(
            s.due(Some(&f), now + Duration::from_secs(60), wall + 60_000)
                .is_some()
        );
        f.session += 1;
        assert!(
            s.due(Some(&f), now + Duration::from_secs(60), wall + 60_000)
                .is_none()
        );
        assert!(s.slew_finish.is_none());
    }

    #[test]
    fn completion_expiry_and_other_bursts_remain_bounded() {
        let now = Instant::now();
        let wall = 1_000_000;
        let mut f = crate::tests::frame(1, false);
        f.captured_at_unix_ms = wall;
        let mut s = Scheduler::new(TriggerRules::local(&Preferences::default()), now);
        s.trigger("scene_change", "Scene changed", &f, now, false);
        s.telescope_trigger("mount_slewed", &f, now, wall);
        assert_eq!(
            s.due(Some(&f), now, wall),
            Some(("scene_change", "Scene changed".into()))
        );
        s.queued(&f, now);
        f.sequence += 1;
        f.conservative_start_unix_ms = None;
        assert!(
            s.due(Some(&f), now + Duration::from_secs(421), wall)
                .is_none()
        );
        assert!(s.slew_finish.is_none());
        s.telescope_trigger("unknown", &f, now, wall);
        assert!(s.slew_finish.is_none());
        for (event, description) in [
            ("sequence_started", "sequence started"),
            ("sequence_finished", "sequence finished"),
        ] {
            let mut s = Scheduler::new(TriggerRules::local(&Preferences::default()), now);
            s.telescope_trigger(event, &f, now, wall);
            f.sequence += 1;
            assert!(s.due(Some(&f), now, wall).unwrap().1.contains(description));
        }
    }

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
                Some(("scene_change", "original".into()))
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
        assert!(TelescopeEvent::parse("slew_to_coordinates").is_none());
    }
}
