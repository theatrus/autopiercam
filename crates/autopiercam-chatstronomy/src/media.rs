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

const GRID_WIDTH: usize = 32;
const GRID_HEIGHT: usize = 24;
const GRID_CELLS: usize = GRID_WIDTH * GRID_HEIGHT;

fn usable_luminance(value: f32) -> bool {
    (8.0..247.0).contains(&value)
}

fn scene_grid(frame: &Frame) -> Result<Option<Vec<f32>>> {
    let gray = decode(frame)?
        .resize_exact(GRID_WIDTH as u32, GRID_HEIGHT as u32, FilterType::Triangle)
        .to_luma8();
    let grid: Vec<_> = gray.as_raw().iter().map(|v| *v as f32).collect();
    let mean = grid.iter().sum::<f32>() / GRID_CELLS as f32;
    // Do not seed an unusable startup reference that can never be compared.
    let usable = grid.iter().filter(|v| usable_luminance(**v)).count();
    Ok((mean >= 8.0 && usable * 5 >= GRID_CELLS).then_some(grid))
}

fn median(values: &mut [f32]) -> f32 {
    let middle = values.len() / 2;
    *values.select_nth_unstable_by(middle, f32::total_cmp).1
}

/// Fit a global gain and black-level shift from matching image locations. The
/// median pairwise slope (Theil-Sen) tolerates local foreground changes; unlike
/// mean normalization, neither a bright patch nor an offset rescales the whole
/// scene. Exclude clipped samples from the fit: their original values are lost.
fn scene_changes(baseline: &[f32], grid: &[f32]) -> Option<Vec<bool>> {
    let pairs: Vec<_> = baseline
        .iter()
        .zip(grid)
        .filter(|(a, b)| usable_luminance(**a) && usable_luminance(**b))
        .map(|(a, b)| (*a, *b))
        .collect();
    if pairs.len() * 5 < GRID_CELLS {
        return None;
    }
    // Bound work independently of preview resolution; sample across the grid.
    let samples: Vec<_> = (0..64).map(|i| pairs[i * pairs.len() / 64]).collect();
    let mut slopes = Vec::new();
    for (i, (a, b)) in samples.iter().enumerate() {
        for (c, d) in &samples[i + 1..] {
            if (a - c).abs() >= 24.0 {
                slopes.push((b - d) / (a - c));
            }
        }
    }
    let slope = if slopes.is_empty() {
        1.0
    } else {
        let fitted = median(&mut slopes);
        // A flat/reversed image is not an exposure transform. Do not fit away
        // a roof closing or an object obscuring the scene.
        if (0.125..=8.0).contains(&fitted) {
            fitted
        } else {
            1.0
        }
    };
    let mut offsets: Vec<_> = pairs.iter().map(|(a, b)| b - slope * a).collect();
    let offset = median(&mut offsets);
    let raw: Vec<_> = baseline
        .iter()
        .zip(grid)
        .map(|(a, b)| {
            let predicted = (slope * a + offset).clamp(0.0, 255.0);
            // A previously clipped pixel cannot be reconstructed. A newly
            // clipped pixel can still be compared with the fitted prediction.
            usable_luminance(*a) && (predicted - b).abs() > 16.0_f32.max(0.15 * predicted.max(*b))
        })
        .collect();
    // Isolated sensor/JPEG noise is not an area of changed scene. Keep cells
    // supported by at least two of their eight neighbors (no row wrapping).
    Some(
        (0..raw.len())
            .map(|i| {
                let x = i % GRID_WIDTH;
                let y = i / GRID_WIDTH;
                raw[i]
                    && (y.saturating_sub(1)..=(y + 1).min(GRID_HEIGHT - 1))
                        .flat_map(|row| {
                            (x.saturating_sub(1)..=(x + 1).min(GRID_WIDTH - 1))
                                .map(move |col| row * GRID_WIDTH + col)
                        })
                        .filter(|j| *j != i && raw[*j])
                        .count()
                        >= 2
            })
            .collect(),
    )
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
    mode: String,
    candidate_mode: String,
    candidate_since: Option<Instant>,
}
impl Detector {
    pub(crate) fn delivered(&mut self, reference: SceneReference) {
        if self.session == reference.session {
            self.baseline = Some(reference.grid);
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
        } else {
            self.candidate_mode.clear();
            self.candidate_since = None;
        }
        if !prefs.scene_changes {
            return Ok(None);
        }
        let Some(grid) = scene_grid(frame)? else {
            return Ok(None);
        };
        // Seed once. Changes in exposure, mode or darkness must not silently
        // replace the last image the user actually saw reported in chat.
        if self.baseline.is_none() {
            self.baseline = Some(grid);
            return Ok(None);
        }
        let baseline = self.baseline.as_ref().unwrap();
        let Some(area) = scene_changes(baseline, &grid) else {
            return Ok(None);
        };
        // One completed exposure is sufficient. Never average frames or wait
        // for confirmation exposures: a night frame may already take a minute.
        let changed = area.iter().filter(|v| **v).count();
        if changed * 100 >= GRID_CELLS * usize::from(prefs.scene_threshold_percent) {
            return Ok(Some(("scene_change", "Pier-camera scene changed")));
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

    fn textured(sequence: u64, transform: impl Fn(u32, u32, f32) -> f32) -> Frame {
        // Smooth illumination plus fixed large structures, rather than a flat
        // test image on which any brightness normalization trivially succeeds.
        let image = RgbImage::from_fn(320, 240, |x, y| {
            let value = 30.0
                + x as f32 * 0.35
                + y as f32 * 0.15
                + if (x / 50 + y / 60) % 2 == 0 {
                    35.0
                } else {
                    0.0
                };
            Rgb([transform(x, y, value).clamp(0.0, 255.0) as u8; 3])
        });
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 85)
            .encode_image(&image)
            .unwrap();
        Frame {
            jpeg: jpeg.into(),
            ..sample(sequence, 70, 70)
        }
    }

    #[test]
    fn exposure_gain_offsets_clipping_and_noise_do_not_report_a_static_scene() {
        for (scale, offset) in [(1.7, 25.0), (0.4, 30.0), (1.0, 65.0), (2.8, -50.0)] {
            let mut detector = Detector::default();
            let now = Instant::now();
            let prefs = preferences();
            detector
                .observe(&textured(1, |_, _, v| v), &prefs, now)
                .unwrap();
            for sequence in 2..=15 {
                let mut frame = textured(sequence, |x, y, v| {
                    let noise = ((x * 17 + y * 31 + sequence as u32 * 19) % 17) as f32 - 8.0;
                    v * scale + offset + noise
                });
                frame.exposure_us = Some(100_000);
                frame.gain = Some(200);
                assert!(
                    detector.observe(&frame, &prefs, now).unwrap().is_none(),
                    "static scene fired for scale={scale}, offset={offset}, frame={sequence}"
                );
            }
        }
    }

    #[test]
    fn moderate_gamma_change_is_tolerated() {
        for gamma in [0.75, 1.3] {
            let mut detector = Detector::default();
            let now = Instant::now();
            let prefs = preferences();
            detector
                .observe(&textured(1, |_, _, v| v), &prefs, now)
                .unwrap();
            for sequence in 2..=10 {
                let frame = textured(sequence, |_, _, v| 255.0 * (v / 255.0).powf(gamma));
                assert!(detector.observe(&frame, &prefs, now).unwrap().is_none());
            }
        }
    }

    #[test]
    fn one_long_exposure_detects_occlusion_even_when_exposure_and_gain_change() {
        for obscured in [0.0, 230.0] {
            let mut detector = Detector::default();
            let now = Instant::now();
            let prefs = preferences();
            detector
                .observe(&textured(1, |_, _, v| v), &prefs, now)
                .unwrap();
            let mut frame = textured(2, |x, _, v| if x < 90 { obscured } else { v * 1.2 + 15.0 });
            frame.exposure_us = Some(120_000_000);
            frame.gain = Some(200);
            assert!(
                detector
                    .observe(&frame, &prefs, now + Duration::from_secs(120))
                    .unwrap()
                    .is_some(),
                "first completed exposure missed occlusion at brightness {obscured}"
            );
        }
    }

    #[test]
    fn continuous_small_exposure_or_gain_steps_do_not_report_a_static_scene() {
        for gain_ramp in [false, true] {
            let mut detector = Detector::default();
            let now = Instant::now();
            let prefs = preferences();
            detector
                .observe(&textured(1, |_, _, v| v), &prefs, now)
                .unwrap();
            for sequence in 2..=20 {
                let mut frame = textured(sequence, |_, _, v| {
                    v * 1.04_f32.powi(sequence as i32) + 10.0
                });
                if gain_ramp {
                    frame.gain = Some(100 + sequence as i64 * 3);
                } else {
                    frame.exposure_us =
                        Some((60_000_000.0 * 1.06_f64.powi(sequence as i32)) as i64);
                }
                assert!(detector.observe(&frame, &prefs, now).unwrap().is_none());
            }
        }
    }

    #[test]
    fn missing_exposure_metadata_does_not_block_a_scene_change() {
        let mut detector = Detector::default();
        let now = Instant::now();
        let prefs = preferences();
        detector.observe(&sample(1, 70, 70), &prefs, now).unwrap();
        let mut changed = sample(2, 220, 70);
        changed.exposure_us = None;
        changed.gain = None;
        assert!(detector.observe(&changed, &prefs, now).unwrap().is_some());
    }

    #[test]
    fn badly_clipped_frames_pause_without_replacing_the_reference() {
        let mut detector = Detector::default();
        let now = Instant::now();
        let prefs = preferences();
        detector.observe(&sample(1, 70, 70), &prefs, now).unwrap();
        for sequence in 2..=8 {
            assert!(
                detector
                    .observe(&sample(sequence, 255, 255), &prefs, now)
                    .unwrap()
                    .is_none()
            );
        }
        for sequence in 9..=11 {
            assert!(
                detector
                    .observe(&sample(sequence, 220, 70), &prefs, now)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn unusable_startup_and_corrupt_frames_do_not_poison_reference() {
        let mut detector = Detector::default();
        let now = Instant::now();
        let prefs = preferences();
        for (sequence, value) in [(1, 0), (2, 255)] {
            detector
                .observe(&sample(sequence, value, value), &prefs, now)
                .unwrap();
            assert!(detector.baseline.is_none());
        }
        for sequence in 3..=7 {
            assert!(
                detector
                    .observe(&sample(sequence, 70, 70), &prefs, now)
                    .unwrap()
                    .is_none()
            );
        }
        let corrupt = Frame {
            jpeg: vec![0, 1, 2].into(),
            ..sample(10, 70, 70)
        };
        assert!(detector.observe(&corrupt, &prefs, now).is_err());
        for sequence in 11..=15 {
            assert!(
                detector
                    .observe(&sample(sequence, 220, 70), &prefs, now)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn scattered_noise_is_removed_and_threshold_still_controls_area() {
        let baseline = vec![70.0; GRID_CELLS];
        let mut noisy = baseline.clone();
        for y in (1..GRID_HEIGHT).step_by(3) {
            for x in (1..GRID_WIDTH).step_by(3) {
                noisy[y * GRID_WIDTH + x] = 160.0;
            }
        }
        assert!(scene_changes(&baseline, &noisy).unwrap().iter().all(|v| !v));
        for (threshold, expected) in [(10, true), (40, false)] {
            let mut detector = Detector::default();
            let prefs = Preferences {
                scene_threshold_percent: threshold,
                ..preferences()
            };
            let now = Instant::now();
            detector
                .observe(&textured(1, |_, _, v| v), &prefs, now)
                .unwrap();
            let mut detected = false;
            for sequence in 2..=8 {
                detected |= detector
                    .observe(
                        &textured(sequence, |x, _, v| if x < 70 { 230.0 } else { v }),
                        &prefs,
                        now,
                    )
                    .unwrap()
                    .is_some();
            }
            assert_eq!(detected, expected, "threshold {threshold}");
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
    fn dark_frames_pause_without_erasing_reference_or_delaying_next_good_frame() {
        let mut detector = Detector::default();
        let prefs = preferences();
        let now = Instant::now();
        detector.observe(&sample(1, 70, 70), &prefs, now).unwrap();
        let mut changed = sample(2, 220, 70);
        changed.exposure_us = Some(1_000_000);
        changed.gain = Some(300);
        assert!(detector.observe(&changed, &prefs, now).unwrap().is_some());
        let mut dark = sample(3, 0, 0);
        dark.exposure_us = changed.exposure_us;
        dark.gain = changed.gain;
        assert!(detector.observe(&dark, &prefs, now).unwrap().is_none());
        for sequence in 4..=8 {
            changed.sequence = sequence;
            assert!(detector.observe(&changed, &prefs, now).unwrap().is_some());
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
                .is_some()
        );
        for sequence in 4..=9 {
            changed.sequence = sequence;
            assert!(
                detector
                    .observe(&changed, &prefs, now + Duration::from_secs(31))
                    .unwrap()
                    .is_some()
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
            assert!(
                detector
                    .observe(&sample(sequence, 220, 70), &prefs, now)
                    .unwrap()
                    .is_some()
            );
        }
    }
}
