use crate::protocol::{MAX_JPEG_BYTES, ServerMessage};
use std::time::{Duration, Instant};

/// Non-secret state supplied by the capture/consent owner, never by the Hub.
/// Increment privacy_generation on every consent/configuration change, including
/// off then on; an old request must not become valid again after re-enabling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalState {
    pub snapshots_allowed: bool,
    pub privacy_generation: u64,
    /// None while paused or disconnected. A new capture session uses a new ID.
    pub session_generation: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("snapshot unavailable")]
pub struct SnapshotUnavailable;

/// A request is fenced to both local consent and the current capture session.
/// Check again after encoding and immediately before handing bytes to transport.
/// The worker must additionally cancel socket writes on any local state change.
#[derive(Debug)]
pub struct SnapshotFence {
    request_id: String,
    state: LocalState,
    deadline: Instant,
    expires_at_unix_ms: u64,
    max_frame_age_ms: u64,
    max_jpeg_bytes: usize,
}

impl SnapshotFence {
    pub fn new(
        message: &ServerMessage,
        state: LocalState,
        now_unix_ms: u64,
        now: Instant,
    ) -> Result<Self, SnapshotUnavailable> {
        let ServerMessage::SnapshotRequest {
            request_id,
            expires_at,
            max_jpeg_bytes,
            max_frame_age_seconds,
        } = message
        else {
            return Err(SnapshotUnavailable);
        };
        let expires_at_unix_ms = u64::try_from(*expires_at)
            .ok()
            .and_then(|value| value.checked_mul(1000))
            .ok_or(SnapshotUnavailable)?;
        if !state.snapshots_allowed
            || state
                .session_generation
                .is_none_or(|generation| generation == 0)
            || expires_at_unix_ms <= now_unix_ms
            || *max_jpeg_bytes == 0
            || *max_frame_age_seconds == 0
        {
            return Err(SnapshotUnavailable);
        }
        // Remote input may tighten these limits but may never widen them.
        let timeout_ms = (expires_at_unix_ms - now_unix_ms).min(90_000);
        Ok(Self {
            request_id: request_id.clone(),
            state,
            deadline: now + Duration::from_millis(timeout_ms),
            expires_at_unix_ms,
            max_frame_age_ms: (*max_frame_age_seconds).min(120) * 1000,
            max_jpeg_bytes: (*max_jpeg_bytes).min(MAX_JPEG_BYTES),
        })
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn max_jpeg_bytes(&self) -> usize {
        self.max_jpeg_bytes
    }

    /// Uses the actual frame capture time, never the request/encoding time.
    /// Does not wait for or request a new exposure, and never resumes capture.
    pub fn check_frame(
        &self,
        current: LocalState,
        frame_session_generation: u64,
        captured_at_unix_ms: u64,
        now_unix_ms: u64,
        now: Instant,
    ) -> Result<(), SnapshotUnavailable> {
        if current != self.state
            || !current.snapshots_allowed
            || current.session_generation != Some(frame_session_generation)
            || now >= self.deadline
            || now_unix_ms >= self.expires_at_unix_ms
            || captured_at_unix_ms == 0
            || captured_at_unix_ms > now_unix_ms.saturating_add(30_000)
            || now_unix_ms.saturating_sub(captured_at_unix_ms) > self.max_frame_age_ms
        {
            return Err(SnapshotUnavailable);
        }
        Ok(())
    }

    /// Envelope validation only: the producer/encoder is responsible for pixels.
    pub fn check_jpeg(&self, jpeg: &[u8]) -> Result<(), SnapshotUnavailable> {
        if jpeg.len() < 4
            || jpeg.len() > self.max_jpeg_bytes
            || !jpeg.starts_with(&[0xff, 0xd8])
            || !jpeg.ends_with(&[0xff, 0xd9])
        {
            return Err(SnapshotUnavailable);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const WALL: u64 = 1_790_355_600_000;

    fn state() -> LocalState {
        LocalState {
            snapshots_allowed: true,
            privacy_generation: 1,
            session_generation: Some(10),
        }
    }

    fn request() -> ServerMessage {
        ServerMessage::SnapshotRequest {
            request_id: "5fe70313-0a01-43b2-97a8-bab512a45ba7".to_owned(),
            expires_at: (WALL / 1000 + 90) as i64,
            max_jpeg_bytes: MAX_JPEG_BYTES,
            max_frame_age_seconds: 120,
        }
    }

    #[test]
    fn long_exposure_frames_use_real_capture_timestamp() {
        let now = Instant::now();
        let fence = SnapshotFence::new(&request(), state(), WALL, now).unwrap();
        for age in [30_000, 60_000, 120_000] {
            assert_eq!(
                fence.check_frame(state(), 10, WALL - age, WALL, now),
                Ok(())
            );
        }
        assert!(
            fence
                .check_frame(state(), 10, WALL - 120_001, WALL, now)
                .is_err()
        );
        assert!(
            fence
                .check_frame(state(), 10, WALL + 30_001, WALL, now)
                .is_err()
        );
    }

    #[test]
    fn denied_paused_or_disconnected_never_create_a_fence() {
        for local in [
            LocalState::default(),
            LocalState {
                snapshots_allowed: false,
                ..state()
            },
            LocalState {
                session_generation: None,
                ..state()
            },
        ] {
            assert!(SnapshotFence::new(&request(), local, WALL, Instant::now()).is_err());
        }
    }

    #[test]
    fn privacy_changes_and_old_sessions_invalidate_prepared_frames() {
        let now = Instant::now();
        let fence = SnapshotFence::new(&request(), state(), WALL, now).unwrap();
        for current in [
            LocalState {
                snapshots_allowed: false,
                ..state()
            },
            LocalState {
                privacy_generation: 2,
                ..state()
            },
            LocalState {
                session_generation: Some(11),
                ..state()
            },
            LocalState {
                session_generation: None,
                ..state()
            },
        ] {
            assert!(fence.check_frame(current, 10, WALL, WALL, now).is_err());
        }
        assert!(fence.check_frame(state(), 9, WALL, WALL, now).is_err());
    }

    #[test]
    fn backward_clock_adjustment_does_not_extend_deadline() {
        let now = Instant::now();
        let fence = SnapshotFence::new(&request(), state(), WALL, now).unwrap();
        assert!(
            fence
                .check_frame(
                    state(),
                    10,
                    WALL,
                    WALL - 1000,
                    now + Duration::from_secs(90)
                )
                .is_err()
        );
        assert!(
            fence
                .check_frame(state(), 10, WALL, WALL + 90_000, now)
                .is_err()
        );
        assert!(SnapshotFence::new(&request(), state(), WALL + 90_000, now).is_err());
    }

    #[test]
    fn hub_cannot_expand_local_size_age_or_deadline_limits() {
        let now = Instant::now();
        let mut message = request();
        if let ServerMessage::SnapshotRequest {
            expires_at,
            max_jpeg_bytes,
            max_frame_age_seconds,
            ..
        } = &mut message
        {
            *expires_at += 9999;
            *max_jpeg_bytes = usize::MAX;
            *max_frame_age_seconds = u64::MAX;
        }
        let fence = SnapshotFence::new(&message, state(), WALL, now).unwrap();
        assert_eq!(fence.max_jpeg_bytes(), MAX_JPEG_BYTES);
        assert!(
            fence
                .check_frame(state(), 10, WALL - 120_001, WALL, now)
                .is_err()
        );
        assert!(
            fence
                .check_frame(state(), 10, WALL, WALL, now + Duration::from_secs(90))
                .is_err()
        );
    }

    #[test]
    fn jpeg_limits_accept_smaller_remote_caps() {
        let now = Instant::now();
        let mut message = request();
        if let ServerMessage::SnapshotRequest { max_jpeg_bytes, .. } = &mut message {
            *max_jpeg_bytes = 4;
        }
        let fence = SnapshotFence::new(&message, state(), WALL, now).unwrap();
        assert_eq!(fence.check_jpeg(&[0xff, 0xd8, 0xff, 0xd9]), Ok(()));
        assert!(fence.check_jpeg(&[0xff, 0xd8, 0, 0xff, 0xd9]).is_err());
        assert!(fence.check_jpeg(b"nope").is_err());
    }
}
