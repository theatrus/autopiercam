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
    limits.max_image_width = Some(1280);
    limits.max_image_height = Some(1280);
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

/// Conservative observations only: full-preview ROI, normalized brightness,
/// three distinct frames, exposure-change suppression and a mode dwell.
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
        if frame.mode != "day" && frame.mode != "night" {
            self.baseline = None;
            return Ok(None);
        }
        if self.mode.is_empty() {
            self.mode = frame.mode.clone();
        }
        if self.mode != frame.mode {
            self.baseline = None;
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
        let gray = decode(frame)?
            .resize_exact(32, 24, FilterType::Triangle)
            .to_luma8();
        let mean = gray.as_raw().iter().map(|v| *v as f32).sum::<f32>() / 768.0;
        // Very dark/noisy frames are not a reliable scene-change signal.
        if mean < 8.0 {
            self.baseline = None;
            self.changed_frames = 0;
            return Ok(None);
        }
        let grid: Vec<f32> = gray.as_raw().iter().map(|v| *v as f32 / mean).collect();
        if !stable_exposure {
            self.baseline = Some(grid);
            self.changed_frames = 0;
            return Ok(None);
        }
        let Some(baseline) = &self.baseline else {
            self.baseline = Some(grid);
            return Ok(None);
        };
        let changed = grid
            .iter()
            .zip(baseline)
            .filter(|(a, b)| (*a - *b).abs() > 0.3)
            .count();
        if changed * 100 >= 768 * usize::from(prefs.scene_threshold_percent) {
            self.changed_frames += 1;
            if self.changed_frames >= 3 {
                self.baseline = Some(grid);
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
