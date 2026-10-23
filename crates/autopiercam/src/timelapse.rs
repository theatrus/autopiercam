//! Daily, fixed-rate timelapses of saved stills, independent of preview cadence.
//! Only the still writer samples pixels; native encoding runs in a killable child.
use crate::{
    parse_generated_capture_filename, publish_temporary_artifact,
    retention::RetentionSink,
    unix_time_millis,
    upload::{UploadEnqueueResult, UploadSink},
};
use anyhow::{Context, Result, anyhow, ensure};
use autopiercam_core::{
    config::CameraConfig,
    timelapse_schedule::{TimelapseSchedule, Window},
};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) const WIDTH: u32 = 1280;
pub(crate) const HEIGHT: u32 = 720;
const FPS: u32 = 24;
const DAY_MS: u64 = 86_400_000;
const MAX_JPEG: u64 = 512 * 1024;
const MAX_CACHE: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

fn regular(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "timelapse cache contains a link: {}",
        path.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "timelapse cache contains a reparse point"
        );
    }
    Ok(metadata)
}

fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    ensure!(regular(path)?.is_dir(), "expected timelapse directory");
    let mut result = Vec::new();
    for entry in fs::read_dir(path)? {
        ensure!(
            result.len() < MAX_ENTRIES,
            "too many timelapse cache entries"
        );
        result.push(entry?.path());
    }
    result.sort();
    Ok(result)
}

fn sample_slot(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let number = name.strip_suffix(".jpg")?;
    if number.len() != 4 || !number.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    number.parse::<u32>().ok().filter(|n| *n < 1560)
}

fn samples(path: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in entries(path)? {
        if sample_slot(&entry).is_some() {
            let meta = regular(&entry)?;
            ensure!(
                meta.is_file() && meta.len() <= MAX_JPEG,
                "invalid timelapse sample"
            );
            result.push(entry);
        }
    }
    Ok(result)
}

fn day_number(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    if ![8, 20].contains(&name.len()) || !name.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    name.parse::<u64>().ok()
}

// Uses the existing managed-artifact grammar for uploads and retention, with a
// fixed namespace nonce and the UTC day-end timestamp. A completed day is never
// rewritten, even after retention removes its output.
fn output_name(day: u64) -> String {
    format!(
        "frame-{}-000-{:032x}-000000.mp4",
        (day + 1) * 86400,
        0x74696d656c61707365_u128
    )
}

struct Cache {
    root: PathBuf,
    bytes: u64,
    last_warning_minute: Option<u64>,
    retention: Option<RetentionSink>,
    schedule: TimelapseSchedule,
    camera: CameraConfig,
    plan_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowRecord {
    window: Window,
    plan_id: String,
}

fn read_window(path: &Path) -> Result<Option<WindowRecord>> {
    ensure!(
        day_number(path).is_some() && regular(path)?.is_dir(),
        "invalid timelapse window directory"
    );
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() == 8)
    {
        return Ok(None);
    }
    let file = path.join("window.json");
    ensure!(
        regular(&file)?.is_file() && regular(&file)?.len() <= 4096,
        "invalid timelapse window metadata"
    );
    let record: WindowRecord = serde_json::from_reader(File::open(file)?)?;
    ensure!(
        record.window.valid()
            && record.plan_id.len() == 64
            && record.plan_id.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid timelapse boundaries"
    );
    Ok(Some(record))
}

// A crash between mkdir and the atomic metadata write leaves no samples. It
// must not block other windows; the next sample can initialize this directory.
fn uninitialized_window(path: &Path) -> Result<bool> {
    Ok(path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() == 20)
        && !path.join("window.json").exists()
        && entries(path)?.is_empty())
}

fn movie_path(directory: &Path, path: &Path) -> Result<PathBuf> {
    let key = day_number(path).context("invalid window key")?;
    Ok(if let Some(record) = read_window(path)? {
        directory.join(format!(
            "frame-{}-{:03}-{:032x}-000000.mp4",
            record.window.end_ms / 1000,
            record.window.end_ms % 1000,
            (u128::from(key) << 64) | 0x7363686564756c65
        ))
    } else {
        directory.join(output_name(key))
    })
}

fn fingerprint(schedule: &TimelapseSchedule, camera: &CameraConfig) -> String {
    let data = serde_json::to_vec(&(schedule, camera.latitude_deg, camera.longitude_deg)).unwrap();
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Cache {
    fn open(directory: &Path) -> Result<Self> {
        let root = directory.join(".autopiercam-timelapse");
        fs::create_dir_all(&root)?;
        let mut bytes = 0;
        for day in entries(&root)? {
            ensure!(
                day_number(&day).is_some(),
                "unexpected entry in timelapse cache"
            );
            for frame in samples(&day)? {
                bytes += regular(&frame)?.len();
            }
        }
        Ok(Self {
            root,
            bytes,
            last_warning_minute: None,
            retention: None,
            schedule: Default::default(),
            camera: Default::default(),
            plan_id: fingerprint(&TimelapseSchedule::default(), &CameraConfig::default()),
        })
    }

    #[cfg(test)]
    fn record(
        &mut self,
        timestamp: u64,
        thumbnail: impl FnOnce() -> Result<RgbImage>,
    ) -> Result<()> {
        self.record_annotated(timestamp, None, thumbnail)
    }

    fn record_annotated(
        &mut self,
        timestamp: u64,
        overlay: Option<&autopiercam_chatstronomy::observation::Overlay>,
        thumbnail: impl FnOnce() -> Result<RgbImage>,
    ) -> Result<()> {
        ensure!(
            regular(&self.root)?.is_dir(),
            "invalid timelapse cache root"
        );
        let Some(window) = self.schedule.at(timestamp, &self.camera) else {
            return Ok(());
        };
        let legacy = self.schedule == TimelapseSchedule::default();
        let dir = if legacy {
            let day = timestamp / DAY_MS;
            ensure!(day < 100_000_000, "invalid timelapse day");
            self.root.join(format!("{day:08}"))
        } else {
            let digest = Sha256::digest(serde_json::to_vec(&(&window, &self.plan_id))?);
            let key = u64::from_le_bytes(digest[..8].try_into().unwrap());
            self.root.join(format!("{key:020}"))
        };
        // No back-fill into a sealed/completed day, including clock rollback.
        if dir.join("sealed").exists() || dir.join("done").exists() {
            return Ok(());
        }
        let slot = (timestamp - window.start_ms) / 60_000;
        let output = dir.join(format!("{slot:04}.jpg"));
        if output.exists() {
            return Ok(());
        }
        ensure!(
            self.bytes + MAX_JPEG <= MAX_CACHE,
            "timelapse cache is full (1 GiB); samples skipped until completed clips free space"
        );
        let thumbnail = thumbnail()?;
        ensure!(
            thumbnail.width() <= WIDTH && thumbnail.height() <= HEIGHT,
            "oversized timelapse thumbnail"
        );
        let mut canvas = RgbImage::new(WIDTH, HEIGHT);
        image::imageops::overlay(
            &mut canvas,
            &thumbnail,
            i64::from((WIDTH - thumbnail.width()) / 2),
            i64::from((HEIGHT - thumbnail.height()) / 2),
        );
        if let Some(overlay) = overlay {
            crate::timelapse_overlay::draw(&mut canvas, overlay)?;
        }
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&canvas)?;
        ensure!(
            jpeg.len() as u64 <= MAX_JPEG,
            "timelapse sample exceeds 512 KiB"
        );
        if let Some(retention) = &self.retention {
            retention.check_video_space(jpeg.len() as u64)?;
        }
        fs::create_dir_all(&dir)?;
        ensure!(regular(&dir)?.is_dir(), "invalid timelapse day directory");
        if !legacy {
            let record = WindowRecord {
                window,
                plan_id: self.plan_id.clone(),
            };
            let data = serde_json::to_vec(&record)?;
            if dir.join("window.json").exists() {
                ensure!(
                    fs::read(dir.join("window.json"))? == data,
                    "timelapse window conflict"
                );
            } else {
                marker(&dir.join("window.json"), &data)?;
            }
        }
        let mut staging = tempfile::NamedTempFile::new_in(&dir)?;
        staging.write_all(&jpeg)?;
        staging.as_file().sync_all()?;
        staging.persist_noclobber(&output)?;
        self.bytes += jpeg.len() as u64;
        if let Some(retention) = &self.retention {
            let _ = retention.try_wake();
        }
        Ok(())
    }

    fn pending(&self, now: u64) -> Result<Vec<(u64, PathBuf)>> {
        let mut result = Vec::new();
        for path in entries(&self.root)? {
            let day = day_number(&path).context("invalid timelapse cache directory")?;
            ensure!(regular(&path)?.is_dir(), "invalid timelapse day directory");
            if uninitialized_window(&path)? {
                continue;
            }
            let end = read_window(&path)?.map_or_else(
                || day.saturating_add(1).saturating_mul(DAY_MS),
                |r| r.window.end_ms,
            );
            if end <= now {
                marker(&path.join("sealed"), b"1")?;
                result.push((day, path));
            }
        }
        Ok(result)
    }

    fn clean_done(&mut self, directory: &Path) -> Result<()> {
        ensure!(
            regular(&directory.join("done"))?.is_file(),
            "cannot discard unfinished timelapse samples"
        );
        for path in samples(directory)? {
            let size = regular(&path)?.len();
            fs::remove_file(path)?;
            self.bytes = self.bytes.saturating_sub(size);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scheduled_cache(root: &Path) -> Cache {
        use autopiercam_core::timelapse_schedule::{DeliveryMode, WindowMode};
        let mut cache = Cache::open(root).unwrap();
        cache.schedule = TimelapseSchedule {
            window: WindowMode::DuskToDawn,
            timezone: "America/Los_Angeles".into(),
            delivery: DeliveryMode::LocalTime,
            ..Default::default()
        };
        cache.camera.latitude_deg = Some(38.7);
        cache.camera.longitude_deg = Some(-121.1);
        cache.plan_id = fingerprint(&cache.schedule, &cache.camera);
        cache
    }
    fn timestamp(date: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(date)
            .unwrap()
            .timestamp_millis() as u64
    }

    #[test]
    fn sunset_window_survives_restart_seals_at_sunrise_and_keeps_waiting_movie() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = scheduled_cache(root.path());
        let evening = timestamp("2026-10-23T05:00:00Z");
        let morning = timestamp("2026-10-23T12:00:00Z");
        let window = cache.schedule.at(evening, &cache.camera).unwrap();
        cache.record(evening, thumbnail).unwrap();
        drop(cache);
        let mut cache = scheduled_cache(root.path());
        cache.record(morning, thumbnail).unwrap();
        cache
            .record(evening, || panic!("restart must not duplicate sample"))
            .unwrap();
        cache
            .record(window.start_ms - 1, || panic!("daylight must not render"))
            .unwrap();
        assert!(cache.pending(window.end_ms - 1).unwrap().is_empty());
        let jobs = cache.pending(window.end_ms).unwrap();
        assert_eq!(jobs.len(), 1);
        let path = &jobs[0].1;
        assert_eq!(samples(path).unwrap().len(), 2);
        let record = read_window(path).unwrap().unwrap();
        assert_eq!(record.window, window);
        cache
            .record(morning + 60_000, || {
                panic!("rollback cannot append to sealed night")
            })
            .unwrap();
        let output = movie_path(root.path(), path).unwrap();
        assert_eq!(
            parse_generated_capture_filename(output.file_name().unwrap().to_str().unwrap()),
            Some(window.end_ms)
        );
        fs::write(&output, b"finalized local movie").unwrap();
        marker(&path.join("sha256"), hash(&output).unwrap().as_bytes()).unwrap();
        marker(&path.join("send.pending"), b"consent").unwrap();
        finish_day(
            Path::new("must-not-execute"),
            path,
            &output,
            &AtomicBool::new(false),
            None,
            None,
        )
        .unwrap();
        cache.clean_done(path).unwrap();
        assert!(samples(path).unwrap().is_empty());
        assert_eq!(Cache::open(root.path()).unwrap().bytes, 0);
        assert_eq!(
            protected_outputs(root.path(), window.send_after_ms - 1).unwrap(),
            vec![output.clone()]
        );
        assert_eq!(
            protected_outputs(root.path(), window.end_ms + DAY_MS - 1).unwrap(),
            vec![output.clone()]
        );
        assert!(
            protected_outputs(root.path(), window.end_ms + DAY_MS)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&output).unwrap(), b"finalized local movie");
        // An interrupted offer is terminal on restart, not a second upload.
        marker(&path.join("send.claim"), b"1").unwrap();
        deliver_scheduled(
            path,
            &output,
            &record,
            &record.plan_id,
            window.send_after_ms,
            None,
            None,
        )
        .unwrap();
        assert!(path.join("send.done").exists());
        assert!(
            protected_outputs(root.path(), window.send_after_ms)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(output).unwrap(), b"finalized local movie");
    }

    #[test]
    fn scheduled_delivery_exact_fence_expiry_and_consent_are_bounded() {
        let record = WindowRecord {
            window: Window {
                start_ms: 0,
                end_ms: 100,
                send_after_ms: 1000,
            },
            plan_id: "plan".into(),
        };
        let decide = |now, consent, ready| {
            delivery_decision(&record, "plan", now, "consent", consent, ready)
        };
        for now in [0, 99, 100, 999] {
            assert_eq!(decide(now, Some("consent"), true), DeliveryDecision::Wait);
        }
        assert_eq!(decide(1000, Some("consent"), true), DeliveryDecision::Offer);
        assert_eq!(decide(1000, Some("consent"), false), DeliveryDecision::Wait);
        assert_eq!(
            decide(100 + DAY_MS - 1, Some("consent"), true),
            DeliveryDecision::Offer
        );
        assert_eq!(
            decide(100 + DAY_MS, Some("consent"), true),
            DeliveryDecision::Cancel
        );
        for consent in [None, Some("new revision"), Some("new pairing")] {
            assert_eq!(decide(999, consent, true), DeliveryDecision::Cancel);
        }
        assert_eq!(
            delivery_decision(&record, "new plan", 1000, "consent", Some("consent"), true),
            DeliveryDecision::Cancel
        );
    }

    #[test]
    fn incomplete_empty_window_is_recoverable_but_corrupt_metadata_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::open(root.path()).unwrap();
        let path = cache.root.join("00000000000000000001");
        fs::create_dir(&path).unwrap();
        assert!(cache.pending(2 * DAY_MS).unwrap().is_empty());
        assert!(
            protected_outputs(root.path(), 2 * DAY_MS)
                .unwrap()
                .is_empty()
        );
        fs::write(path.join("window.json"), b"invalid").unwrap();
        assert!(cache.pending(2 * DAY_MS).is_err());
        assert!(protected_outputs(root.path(), 2 * DAY_MS).is_err());
    }

    #[test]
    fn fallback_day_keeps_the_twenty_fifth_hour_in_order() {
        use autopiercam_core::timelapse_schedule::WindowMode;
        let root = tempfile::tempdir().unwrap();
        let mut cache = scheduled_cache(root.path());
        cache.schedule.window = WindowMode::LocalNoon;
        cache.plan_id = fingerprint(&cache.schedule, &cache.camera);
        let end = timestamp("2026-11-01T20:00:00Z");
        cache.record(end - 60_000, thumbnail).unwrap();
        let jobs = cache.pending(end).unwrap();
        let frames = samples(&jobs[0].1).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(sample_slot(&frames[0]), Some(1499));
        assert_eq!(read_window(&jobs[0].1).unwrap().unwrap().window.end_ms, end);
    }
    #[test]
    #[cfg(windows)]
    fn annotated_samples_stay_bounded_immutable_and_keep_unannotated_frames_clean() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(root.path()).unwrap();
        let label = autopiercam_chatstronomy::observation::Overlay {
            lines: vec![
                "Target: NGC 7023".into(),
                "Saved LIGHT exposures: 505".into(),
            ],
        };
        cache
            .record_annotated(DAY_MS, Some(&label), thumbnail)
            .unwrap();
        cache.record(DAY_MS + 60_000, thumbnail).unwrap();
        let path = cache.root.join("00000001/0000.jpg");
        let saved = fs::read(&path).unwrap();
        let plain = fs::read(cache.root.join("00000001/0001.jpg")).unwrap();
        assert!(saved.len() as u64 <= MAX_JPEG);
        let labeled = image::load_from_memory(&saved).unwrap().to_rgb8();
        let unlabeled = image::load_from_memory(&plain).unwrap().to_rgb8();
        assert_eq!(labeled.dimensions(), (WIDTH, HEIGHT));
        assert_eq!(labeled.get_pixel(30, 30), unlabeled.get_pixel(30, 30));
        assert_ne!(labeled, unlabeled);
        cache
            .record_annotated(DAY_MS, None, || {
                panic!("never re-render an occupied minute")
            })
            .unwrap();
        drop(cache);
        let mut reopened = Cache::open(root.path()).unwrap();
        reopened
            .record(DAY_MS, || panic!("never relabel after restart"))
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), saved);
    }

    #[test]
    fn shared_quota_skips_new_samples_and_defers_closed_day_without_losing_sources() {
        use crate::retention::{
            LocalOnlyRetentionAuthority, RetentionPolicy, RetentionWorker, VideoStoragePressure,
        };
        let root = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(root.path()).unwrap();
        cache.record(DAY_MS, thumbnail).unwrap();
        let (worker, sink) = RetentionWorker::start(
            RetentionPolicy::from_capture_config(&autopiercam_core::config::CaptureConfig {
                retention_max_bytes: Some(1),
                ..Default::default()
            }),
            root.path().into(),
            Arc::new(parse_generated_capture_filename),
            Arc::new(LocalOnlyRetentionAuthority),
            Arc::new(|_| {}),
        )
        .unwrap();
        assert!(sink.capture_suspended());
        cache.retention = Some(sink.clone());
        assert!(
            cache
                .record(DAY_MS + 60_000, thumbnail)
                .unwrap_err()
                .is::<VideoStoragePressure>()
        );
        let input = cache.pending(2 * DAY_MS).unwrap().remove(0).1;
        let original = fs::read(input.join("0000.jpg")).unwrap();
        let output = root.path().join(output_name(1));
        // The quota gate must run before launching this deliberately absent encoder.
        let error = finish_day(
            Path::new("must-not-launch.exe"),
            &input,
            &output,
            &AtomicBool::new(false),
            None,
            Some(&sink),
        )
        .unwrap_err();
        assert!(error.is::<VideoStoragePressure>());
        assert_eq!(samples(&input).unwrap().len(), 1);
        assert_eq!(fs::read(input.join("0000.jpg")).unwrap(), original);
        assert!(!output.exists());
        assert!(!input.join("encoding.mp4").exists());
        assert!(!input.join("done").exists());
        worker.stop_and_join().unwrap();
    }
    #[test]
    #[cfg(windows)]
    fn closed_daily_spool_retries_failed_encode_then_publishes_a_decodable_native_movie() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(root.path()).unwrap();
        cache.record(DAY_MS, thumbnail).unwrap();
        cache.record(DAY_MS + 120_000, thumbnail).unwrap();
        let day = cache.pending(2 * DAY_MS).unwrap().remove(0).1;
        let output = root.path().join(output_name(1));
        let error = finish_day_with(&day, &output, None, None, |_, temporary| {
            fs::write(temporary, b"partial movie")?;
            anyhow::bail!("injected encoder failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert!(!output.exists());
        assert!(!day.join("done").exists());
        assert!(!day.join("encoding.mp4").exists());
        assert_eq!(samples(&day).unwrap().len(), 2);
        // Resume from the persistent cache, replacing only the private partial.
        drop(cache);
        let mut cache = Cache::open(root.path()).unwrap();
        finish_day_with(&day, &output, None, None, crate::native_video::encode).unwrap();
        assert_eq!(crate::native_video::verify(&output).unwrap(), 3);
        assert_eq!(
            fs::read_to_string(day.join("sha256")).unwrap(),
            hash(&output).unwrap()
        );
        assert!(day.join("done").exists());
        assert!(!day.join("encoding.mp4").exists());
        cache.clean_done(&day).unwrap();
        assert_eq!(cache.bytes, 0);
        assert!(samples(&day).unwrap().is_empty());
        assert!(output.exists());
    }

    fn thumbnail() -> Result<RgbImage> {
        Ok(RgbImage::from_pixel(32, 24, image::Rgb([80, 90, 100])))
    }

    #[test]
    fn utc_rollover_restarts_and_rollback_never_duplicate_or_backfill_sealed_day() {
        let root = tempfile::tempdir().unwrap();
        let day = 20_000;
        let mut cache = Cache::open(root.path()).unwrap();
        cache.record(day * DAY_MS + 100, thumbnail).unwrap();
        cache
            .record(day * DAY_MS + 59_999, || {
                panic!("same slot must not resize again")
            })
            .unwrap();
        cache.record(day * DAY_MS + 180_000, thumbnail).unwrap();
        assert!(cache.pending((day + 1) * DAY_MS - 1).unwrap().is_empty());
        drop(cache);
        let mut cache = Cache::open(root.path()).unwrap();
        let jobs = cache.pending((day + 1) * DAY_MS).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, day);
        let files = samples(&jobs[0].1).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|p| sample_slot(p).unwrap())
                .collect::<Vec<_>>(),
            [0, 3]
        );
        // Known completed still pixels are private JPEG copies, not paths to
        // originals that the independent retention worker could delete.
        assert_eq!(image::open(&files[0]).unwrap().width(), WIDTH);
        cache
            .record(day * DAY_MS + 240_000, || {
                panic!("clock rollback cannot append")
            })
            .unwrap();
        assert!(cache.clean_done(&jobs[0].1).is_err());
        cache.record((day + 1) * DAY_MS, thumbnail).unwrap();
        assert_eq!(
            samples(&cache.root.join(format!("{:08}", day + 1)))
                .unwrap()
                .len(),
            1
        );
        marker(&jobs[0].1.join("done"), b"1").unwrap();
        cache.clean_done(&jobs[0].1).unwrap();
        cache
            .record(day * DAY_MS, || panic!("completed day must not replay"))
            .unwrap();
        assert!(samples(&jobs[0].1).unwrap().is_empty());
        assert!(cache.bytes > 0); // current day's sample remains
    }

    #[test]
    fn cache_pressure_is_bounded_and_does_not_touch_source_or_render_new_pixels() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(root.path()).unwrap();
        cache.bytes = MAX_CACHE;
        assert!(
            cache
                .record(DAY_MS, || panic!("no allocation when full"))
                .is_err()
        );
        assert_eq!(entries(&cache.root).unwrap().len(), 0);
        cache.bytes = 0;
        cache.record(DAY_MS, thumbnail).unwrap();
        assert!(cache.bytes <= MAX_JPEG);
        assert_eq!(Cache::open(root.path()).unwrap().bytes, cache.bytes);
    }

    #[test]
    fn publication_crash_recovery_verifies_receipt_before_marking_done() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(root.path()).unwrap();
        cache.record(DAY_MS, thumbnail).unwrap();
        let day = cache.pending(2 * DAY_MS).unwrap().remove(0).1;
        let output = root.path().join(output_name(1));
        assert_eq!(
            parse_generated_capture_filename(output.file_name().unwrap().to_str().unwrap()),
            Some(2 * DAY_MS)
        );
        fs::write(&output, b"mock finalized bytes").unwrap();
        marker(&day.join("sha256"), hash(&output).unwrap().as_bytes()).unwrap();
        let stop = AtomicBool::new(false);
        // Existing valid receipt must resume after publication without spawning.
        finish_day(
            Path::new("nonexistent encoder"),
            &day,
            &output,
            &stop,
            None,
            None,
        )
        .unwrap();
        assert!(day.join("done").exists());
        cache.clean_done(&day).unwrap();
        assert_eq!(cache.bytes, 0);
        assert_eq!(fs::read(&output).unwrap(), b"mock finalized bytes");
    }

    #[test]
    fn conflicting_publication_preserves_all_samples_and_existing_movie() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(root.path()).unwrap();
        cache.record(DAY_MS, thumbnail).unwrap();
        let day = cache.pending(2 * DAY_MS).unwrap().remove(0).1;
        let output = root.path().join(output_name(1));
        fs::write(&output, b"unrelated movie").unwrap();
        marker(&day.join("sha256"), &[b'0'; 64]).unwrap();
        let error = finish_day(
            Path::new("nonexistent encoder"),
            &day,
            &output,
            &AtomicBool::new(false),
            None,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("checksum conflict"));
        assert!(!day.join("done").exists());
        assert_eq!(samples(&day).unwrap().len(), 1);
        assert_eq!(fs::read(&output).unwrap(), b"unrelated movie");
    }
}

fn marker(path: &Path, contents: &[u8]) -> Result<()> {
    if path.exists() {
        ensure!(regular(path)?.is_file(), "invalid timelapse marker");
        return Ok(());
    }
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("missing marker parent")?)?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct DailySink(Arc<Mutex<Cache>>);
impl DailySink {
    pub(crate) fn record(
        &self,
        saved: &Path,
        overlay: Option<&autopiercam_chatstronomy::observation::Overlay>,
        thumbnail: impl FnOnce() -> Result<RgbImage>,
    ) {
        let result = (|| {
            let timestamp = saved
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_generated_capture_filename)
                .context("invalid saved frame timestamp")?;
            let mut cache = self
                .0
                .lock()
                .map_err(|_| anyhow!("timelapse cache lock poisoned"))?;
            let result = cache.record_annotated(timestamp, overlay, thumbnail);
            if result.is_err() {
                let minute = timestamp / 60_000;
                if cache.last_warning_minute == Some(minute) {
                    return Ok(());
                }
                cache.last_warning_minute = Some(minute);
            }
            result
        })();
        // Timelapse is optional: failure never aborts an exposure or loses the
        // already saved still. It is visible in the worker log, not retried busy.
        if let Err(error) = result {
            tracing::warn!(%error, "daily timelapse sample not saved");
        }
    }
}

pub(crate) struct DailyWorker {
    sink: DailySink,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DailyWorker {
    pub(crate) fn start(
        directory: &Path,
        schedule: &TimelapseSchedule,
        camera: &CameraConfig,
        upload: Option<UploadSink>,
        retention: Option<RetentionSink>,
        sharing: Option<crate::TimelapseSink>,
    ) -> Result<Self> {
        let mut initial_cache = Cache::open(directory)?;
        initial_cache.retention = retention.clone();
        initial_cache.schedule = schedule.clone();
        initial_cache.camera = camera.clone();
        initial_cache.plan_id = fingerprint(schedule, camera);
        let plan_id = initial_cache.plan_id.clone();
        let legacy_delivery = schedule == &TimelapseSchedule::default();
        let sink = DailySink(Arc::new(Mutex::new(initial_cache)));
        let cache = sink.clone();
        let directory = directory.to_owned();
        let executable = crate::native_video::executable()?;
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let thread = thread::Builder::new()
            .name("autopiercam-timelapse".into())
            .spawn(move || {
                while !cancelled.load(Ordering::Acquire) {
                    let result = (|| -> Result<()> {
                        let jobs = cache
                            .0
                            .lock()
                            .map_err(|_| anyhow!("timelapse lock poisoned"))?
                            .pending(unix_time_millis())?;
                        for (day, path) in jobs {
                            if cancelled.load(Ordering::Acquire) {
                                break;
                            }
                            let job = (|| -> Result<()> {
                            let output = movie_path(&directory,&path)?;
                            let record = read_window(&path)?;
                            if !path.join("done").exists() {
                                // Capture consent before publishing; enabling sharing later
                                // must not turn old local movies into an upload backlog.
                                if record.is_some() && !path.join("send.pending").exists() && !path.join("send.done").exists() {
                                    if let Some(key) = sharing.as_ref().and_then(|s| s.consent_key()) {
                                        marker(&path.join("send.pending"),key.as_bytes())?;
                                    } else { marker(&path.join("send.done"),b"not consented")?; }
                                }
                                finish_day(
                                    &executable,
                                    &path,
                                    &output,
                                    &cancelled,
                                    upload.as_ref(),
                                    retention.as_ref(),
                                )?;
                                if record.is_none() && legacy_delivery && let Some(sharing) = &sharing
                                    && let Err(error) = sharing.offer_checked(&output, (day + 1).saturating_mul(DAY_MS),
                                        |additional| retention.as_ref().map_or(Ok(()), |sink| sink.check_video_space(additional))) {
                                        tracing::warn!(%error, "Timelapse not queued for Chatstronomy; local movie is unchanged");
                                    }
                            }
                            if let Some(record) = record {
                                deliver_scheduled(&path,&output,&record,&plan_id,unix_time_millis(),sharing.as_ref(),retention.as_ref())?;
                            }
                            cache
                                .0
                                .lock()
                                .map_err(|_| anyhow!("timelapse lock poisoned"))?
                                .clean_done(&path)?;
                            Ok(())
                            })();
                            if let Err(error) = job { tracing::warn!(%error, path=%path.display(), "timelapse window pending; local files kept"); }
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        tracing::warn!(%error, "daily timelapse pending; checking again in thirty seconds");
                    }
                    thread::park_timeout(Duration::from_secs(30));
                }
            })?;
        Ok(Self {
            sink,
            stop,
            thread: Some(thread),
        })
    }
    pub(crate) fn sink(&self) -> DailySink {
        self.sink.clone()
    }
    pub(crate) fn stop_and_join(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
impl Drop for DailyWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[derive(Debug, PartialEq, Eq)]
enum DeliveryDecision {
    Wait,
    Cancel,
    Offer,
}
fn delivery_decision(
    record: &WindowRecord,
    plan: &str,
    now: u64,
    expected: &str,
    consent: Option<&str>,
    ready: bool,
) -> DeliveryDecision {
    if record.plan_id != plan
        || consent != Some(expected)
        // Existing Hub contract accepts a movie timestamp at most 24 h old.
        // Do not turn a delayed schedule into a permanently rejected backlog.
        || now >= record.window.end_ms.saturating_add(DAY_MS)
    {
        DeliveryDecision::Cancel
    } else if now < record.window.send_after_ms || !ready {
        DeliveryDecision::Wait
    } else {
        DeliveryDecision::Offer
    }
}

fn deliver_scheduled(
    path: &Path,
    output: &Path,
    record: &WindowRecord,
    plan: &str,
    now: u64,
    sharing: Option<&crate::TimelapseSink>,
    retention: Option<&RetentionSink>,
) -> Result<()> {
    if path.join("send.done").exists() {
        return Ok(());
    }
    if path.join("send.claim").exists() {
        // A crash after offering is ambiguous. Never replay an accepted post.
        return marker(
            &path.join("send.done"),
            b"interrupted attempt; not replayed",
        );
    }
    let pending = path.join("send.pending");
    if !pending.exists() {
        return Ok(());
    }
    ensure!(
        regular(&pending)?.is_file() && regular(&pending)?.len() <= 2048,
        "invalid timelapse consent record"
    );
    let expected = fs::read_to_string(pending)?;
    let consent = sharing.and_then(|s| s.consent_key());
    match delivery_decision(
        record,
        plan,
        now,
        &expected,
        consent.as_deref(),
        sharing.is_some_and(|s| s.ready_for_movie()),
    ) {
        DeliveryDecision::Wait => Ok(()),
        DeliveryDecision::Cancel => {
            tracing::warn!(path=%output.display(), "scheduled timelapse expired or consent/schedule changed; local movie kept");
            marker(&path.join("send.done"), b"expired or cancelled")
        }
        DeliveryDecision::Offer => {
            // Verify immutable local bytes before the one durable claim. HTTP
            // retries still use the existing snapshot/UUID/timestamp contract.
            ensure!(regular(output)?.is_file(), "scheduled movie missing");
            ensure!(
                fs::read_to_string(path.join("sha256"))? == hash(output)?,
                "scheduled movie checksum conflict"
            );
            marker(&path.join("send.claim"), b"1")?;
            let offered = sharing.unwrap().offer_scheduled_checked(
                output,
                record.window.end_ms,
                &expected,
                |additional| retention.map_or(Ok(()), |s| s.check_video_space(additional)),
            );
            marker(&path.join("send.done"), b"attempted")?;
            match offered {
                Ok(true) => {
                    tracing::info!(path=%output.display(),"scheduled timelapse queued for Chatstronomy")
                }
                Ok(false) => {
                    tracing::warn!(path=%output.display(),"scheduled timelapse consent/session changed; local movie kept")
                }
                Err(error) => {
                    tracing::warn!(%error,"scheduled timelapse not queued; local movie kept")
                }
            }
            Ok(())
        }
    }
}

/// Retention must not purge a finalized movie while its bounded send window
/// is pending. Corrupt metadata fails the sweep closed, never authorizes deletion.
pub(crate) fn protected_outputs(directory: &Path, now: u64) -> Result<Vec<PathBuf>> {
    let root = directory.join(".autopiercam-timelapse");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut protected = Vec::new();
    for path in entries(&root)? {
        if uninitialized_window(&path)? {
            continue;
        }
        if let Some(record) = read_window(&path)?
            && path.join("send.pending").exists()
            && !path.join("send.done").exists()
            && now < record.window.end_ms.saturating_add(DAY_MS)
        {
            protected.push(movie_path(directory, &path)?);
        }
    }
    Ok(protected)
}

fn finish_day(
    executable: &Path,
    input: &Path,
    output: &Path,
    stop: &AtomicBool,
    upload: Option<&UploadSink>,
    retention: Option<&RetentionSink>,
) -> Result<()> {
    finish_day_with(input, output, upload, retention, |input, temporary| {
        crate::native_video::run_encoder_checked(
            executable,
            input,
            temporary,
            stop,
            Duration::from_secs(600),
            || retention.map_or(Ok(()), |sink| sink.check_video_space(0)),
        )
    })
}

fn finish_day_with(
    input: &Path,
    output: &Path,
    upload: Option<&UploadSink>,
    retention: Option<&RetentionSink>,
    encode: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    if samples(input)?.is_empty() {
        return marker(&input.join("done"), b"empty");
    }
    let receipt = input.join("sha256");
    if !output.exists() {
        let temporary = input.join("encoding.mp4");
        if temporary.exists() {
            ensure!(regular(&temporary)?.is_file(), "invalid temporary movie");
            fs::remove_file(&temporary)?;
        }
        if receipt.exists() {
            ensure!(regular(&receipt)?.is_file(), "invalid timelapse receipt");
            fs::remove_file(&receipt)?;
        }
        let manifest = samples(input)?
            .into_iter()
            .enumerate()
            .map(|(i, path)| crate::native_video::Sample {
                file: path.file_name().unwrap().to_str().unwrap().to_owned(),
                duration: ((i as i64 + 1) * 10_000_000 / i64::from(FPS))
                    - (i as i64 * 10_000_000 / i64::from(FPS)),
            })
            .collect::<Vec<_>>();
        crate::native_video::write_manifest(input, &manifest)?;
        if let Err(error) = encode(input, &temporary) {
            // Reaped encoder no longer owns the partial. Keep samples, but do
            // not let a failed output consume the space needed by the retry.
            if temporary.exists() && regular(&temporary)?.is_file() {
                fs::remove_file(&temporary)?;
            }
            return Err(error);
        }
        let file = OpenOptions::new().read(true).write(true).open(&temporary)?;
        ensure!(
            file.metadata()?.len() > 0,
            "native encoder produced an empty movie"
        );
        file.sync_all()?;
        drop(file);
        marker(&receipt, hash(&temporary)?.as_bytes())?;
        publish_temporary_artifact(&temporary, output)?;
    }
    ensure!(regular(output)?.is_file(), "invalid timelapse output");
    ensure!(
        regular(&receipt)?.len() == 64,
        "missing or invalid timelapse publication receipt"
    );
    ensure!(
        fs::read_to_string(&receipt)? == hash(output)?,
        "timelapse output checksum conflict; refusing replacement"
    );
    if let Some(upload) = upload {
        ensure!(
            upload.try_enqueue(output.to_owned())? != UploadEnqueueResult::WorkerStopped,
            "timelapse upload worker stopped"
        );
    }
    marker(&input.join("done"), b"1")?;
    if let Some(retention) = retention {
        let _ = retention.try_wake();
    }
    tracing::info!(path = %output.display(), "saved native daily timelapse");
    Ok(())
}
