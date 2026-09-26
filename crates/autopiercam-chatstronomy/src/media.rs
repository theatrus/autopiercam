use crate::service::{Frame, Preferences};
use anyhow::{Result, bail};
use image::{ImageReader, codecs::jpeg::JpegEncoder, imageops::FilterType};
use std::{
    io::Cursor,
    time::{Duration, Instant},
};

fn decode(frame: &Frame) -> Result<image::DynamicImage> {
    if frame.jpeg.len() > 4 * 1024 * 1024 {
        bail!("Preview exceeds size limit");
    }
    let mut reader = ImageReader::with_format(Cursor::new(&frame.jpeg), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1920);
    limits.max_image_height = Some(1920);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}

pub(crate) fn jpeg(frame: &Frame, cap: usize) -> Result<Vec<u8>> {
    // Decode even small images so the network never forwards unvalidated bytes.
    let decoded = decode(frame)?;
    for dimension in [1280, 960, 640, 320, 160] {
        let rgb = decoded
            .resize(dimension, dimension, FilterType::Triangle)
            .to_rgb8();
        for quality in [75, 55, 35] {
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, quality).encode_image(&rgb)?;
            if bytes.len() <= cap {
                return Ok(bytes);
            }
        }
    }
    bail!("Preview cannot fit the Hub image limit")
}

/// A reference carried with the immutable outbound event until delivery is acknowledged.
pub(crate) struct SceneReference {
    session: u64,
    grid: Vec<f32>,
}

fn scene_grid(frame: &Frame) -> Result<Option<Vec<f32>>> {
    let gray = decode(frame)?
        .resize_exact(32, 24, FilterType::Triangle)
        .to_luma8();
    let mean = gray.as_raw().iter().map(|v| *v as f32).sum::<f32>() / 768.0;
    // Very dark/noisy frames are not a reliable scene-change signal.
    Ok((mean >= 8.0).then(|| gray.as_raw().iter().map(|v| *v as f32 / mean).collect()))
}

pub(crate) fn scene_reference(frame: &Frame) -> Result<Option<SceneReference>> {
    Ok(scene_grid(frame)?.map(|grid| SceneReference {
        session: frame.session,
        grid,
    }))
}

/// Compare against startup or the last delivered scene-change image, never an
/// adjacent frame. Suppression pauses observations without erasing slow drift.
#[derive(Default)]
pub(crate) struct Detector {
    session: u64,
    sequence: u64,
    baseline: Option<Vec<f32>>,
    exposure: Option<i64>,
    gain: Option<i64>,
    mode: String,
    candidate_mode: String,
    candidate_since: Option<Instant>,
    changed_frames: u8,
}
impl Detector {
    pub(crate) fn delivered(&mut self, reference: SceneReference) {
        if self.session == reference.session {
            self.baseline = Some(reference.grid);
            self.changed_frames = 0;
        }
    }

    pub(crate) fn observe(
        &mut self,
        frame: &Frame,
        prefs: &Preferences,
        now: Instant,
    ) -> Result<Option<(&'static str, &'static str)>> {
        if self.session != frame.session {
            *self = Self {
                session: frame.session,
                ..Self::default()
            };
        }
        if self.sequence == frame.sequence {
            return Ok(None);
        }
        self.sequence = frame.sequence;
        let known_mode = frame.mode == "day" || frame.mode == "night";
        if known_mode && self.mode.is_empty() {
            self.mode = frame.mode.clone();
        }
        if known_mode && self.mode != frame.mode {
            self.changed_frames = 0;
            if self.candidate_mode != frame.mode {
                self.candidate_mode = frame.mode.clone();
                self.candidate_since = Some(now);
            }
            if self
                .candidate_since
                .is_some_and(|since| now.duration_since(since) >= Duration::from_secs(30))
            {
                self.mode = frame.mode.clone();
                self.candidate_since = None;
                if prefs.day_night {
                    return Ok(Some((
                        "day_night_transition",
                        "Camera day/night mode changed",
                    )));
                }
            }
            return Ok(None);
        }
        self.candidate_mode.clear();
        self.candidate_since = None;
        if !prefs.scene_changes {
            return Ok(None);
        }
        let stable_exposure = match (self.exposure, frame.exposure_us, self.gain, frame.gain) {
            (Some(a), Some(b), Some(g), Some(h)) if a > 0 && b > 0 => {
                (a as f64 / b as f64 - 1.0).abs() <= 0.2 && g.abs_diff(h) <= 10
            }
            _ => false,
        };
        self.exposure = frame.exposure_us;
        self.gain = frame.gain;
        let Some(grid) = scene_grid(frame)? else {
            self.changed_frames = 0;
            return Ok(None);
        };
        // Seed once. Changes in exposure, mode or darkness must not silently
        // replace the last image the user actually saw reported in chat.
        if self.baseline.is_none() {
            self.baseline = Some(grid.clone());
        }
        if !stable_exposure {
            self.changed_frames = 0;
            return Ok(None);
        }
        let baseline = self.baseline.as_ref().unwrap();
        let changed = grid
            .iter()
            .zip(baseline)
            .filter(|(a, b)| (*a - *b).abs() > 0.3)
            .count();
        if changed * 100 >= 768 * usize::from(prefs.scene_threshold_percent) {
            self.changed_frames += 1;
            if self.changed_frames >= 3 {
                self.changed_frames = 0;
                return Ok(Some((
                    "scene_change",
                    "Persistent scene change in the full pier-camera preview",
                )));
            }
        } else {
            self.changed_frames = 0;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn sample(sequence: u64, left: u8, right: u8) -> Frame {
        let image = RgbImage::from_fn(96, 64, |x, _| Rgb([if x < 48 { left } else { right }; 3]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&image)
            .unwrap();
        Frame {
            jpeg: jpeg.into(),
            mode: "unknown".into(),
            ..crate::tests::frame(sequence, false)
        }
    }

    fn preferences() -> Preferences {
        Preferences {
            scene_changes: true,
            ..Default::default()
        }
    }

    #[test]
    fn slow_drift_accumulates_until_delivery_then_uses_the_reported_image() {
        let mut detector = Detector::default();
        let now = Instant::now();
        let prefs = preferences();
        assert!(
            detector
                .observe(&sample(1, 70, 70), &prefs, now)
                .unwrap()
                .is_none()
        );
        // Adjacent samples differ by only 5/255. No rolling baseline.
        let mut sequence = 1;
        let mut detected = false;
        for left in (75..=220).step_by(5) {
            sequence += 1;
            detected |= detector
                .observe(&sample(sequence, left, 70), &prefs, now)
                .unwrap()
                .is_some();
        }
        assert!(detected);
        // Detection alone (including a coalesced/rate-limited send) cannot commit.
        for _ in 0..3 {
            sequence += 1;
            detected = detector
                .observe(&sample(sequence, 220, 70), &prefs, now)
                .unwrap()
                .is_some();
        }
        assert!(detected);
        // The queued image may differ from the original detection.
        let reported = sample(sequence, 200, 100);
        detector.delivered(scene_reference(&reported).unwrap().unwrap());
        for _ in 0..6 {
            sequence += 1;
            assert!(
                detector
                    .observe(&sample(sequence, 200, 100), &prefs, now)
                    .unwrap()
                    .is_none()
            );
        }
        for _ in 0..3 {
            sequence += 1;
            detected = detector
                .observe(&sample(sequence, 70, 70), &prefs, now)
                .unwrap()
                .is_some();
        }
        assert!(
            detected,
            "return to the startup scene differs from the last report"
        );
    }

    #[test]
    fn exposure_changes_and_dark_frames_pause_without_erasing_reference() {
        let mut detector = Detector::default();
        let prefs = preferences();
        let now = Instant::now();
        detector.observe(&sample(1, 70, 70), &prefs, now).unwrap();
        let mut changed = sample(2, 220, 70);
        changed.exposure_us = Some(1_000_000);
        changed.gain = Some(300);
        assert!(detector.observe(&changed, &prefs, now).unwrap().is_none());
        let mut dark = sample(3, 0, 0);
        dark.exposure_us = changed.exposure_us;
        dark.gain = changed.gain;
        assert!(detector.observe(&dark, &prefs, now).unwrap().is_none());
        for sequence in 4..=6 {
            changed.sequence = sequence;
            assert_eq!(
                detector.observe(&changed, &prefs, now).unwrap().is_some(),
                sequence == 6
            );
            assert!(
                detector.observe(&changed, &prefs, now).unwrap().is_none(),
                "duplicate is not evidence"
            );
        }
    }

    #[test]
    fn mode_transition_preserves_reference_and_session_change_reseeds() {
        let mut detector = Detector::default();
        let prefs = preferences();
        let now = Instant::now();
        let mut first = sample(1, 70, 70);
        first.mode = "night".into();
        detector.observe(&first, &prefs, now).unwrap();
        let mut changed = sample(2, 220, 70);
        changed.mode = "day".into();
        detector.observe(&changed, &prefs, now).unwrap();
        changed.sequence = 3;
        assert!(
            detector
                .observe(&changed, &prefs, now + Duration::from_secs(30))
                .unwrap()
                .is_none()
        );
        for sequence in 4..=6 {
            changed.sequence = sequence;
            assert_eq!(
                detector
                    .observe(&changed, &prefs, now + Duration::from_secs(31))
                    .unwrap()
                    .is_some(),
                sequence == 6
            );
        }
        let old_report = scene_reference(&first).unwrap().unwrap();
        changed.session = 2;
        changed.sequence = 1;
        detector.observe(&changed, &prefs, now).unwrap();
        detector.delivered(old_report); // late ACK from old camera session is ignored
        for sequence in 2..=5 {
            changed.sequence = sequence;
            assert!(detector.observe(&changed, &prefs, now).unwrap().is_none());
        }
    }

    #[test]
    fn global_brightness_drift_is_normalized_and_unknown_mode_does_not_gate_scenes() {
        let mut detector = Detector::default();
        let prefs = preferences();
        let now = Instant::now();
        for sequence in 1..=10 {
            assert!(
                detector
                    .observe(
                        &sample(sequence, 50 + sequence as u8 * 10, 50 + sequence as u8 * 10),
                        &prefs,
                        now
                    )
                    .unwrap()
                    .is_none()
            );
        }
        for sequence in 11..=13 {
            assert_eq!(
                detector
                    .observe(&sample(sequence, 220, 70), &prefs, now)
                    .unwrap()
                    .is_some(),
                sequence == 13
            );
        }
    }
}
