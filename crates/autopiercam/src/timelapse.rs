//! Daily, fixed-rate timelapses of saved stills, independent of preview cadence.
//! Only the still writer samples pixels; native encoding runs in a killable child.
use crate::{
    parse_generated_capture_filename, publish_temporary_artifact,
    retention::RetentionSink,
    unix_time_millis,
    upload::{UploadEnqueueResult, UploadSink},
};
use anyhow::{Context, Result, anyhow, ensure};
use image::RgbImage;
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
    number.parse::<u32>().ok().filter(|n| *n < 1440)
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
    if name.len() != 8 || !name.bytes().all(|c| c.is_ascii_digit()) {
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
        })
    }

    fn record(
        &mut self,
        timestamp: u64,
        thumbnail: impl FnOnce() -> Result<RgbImage>,
    ) -> Result<()> {
        ensure!(
            regular(&self.root)?.is_dir(),
            "invalid timelapse cache root"
        );
        let day = timestamp / DAY_MS;
        ensure!(day < 100_000_000, "invalid timelapse day");
        let dir = self.root.join(format!("{day:08}"));
        // No back-fill into a sealed/completed day, including clock rollback.
        if dir.join("sealed").exists() || dir.join("done").exists() {
            return Ok(());
        }
        let slot = timestamp % DAY_MS / 60_000;
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
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&canvas)?;
        ensure!(
            jpeg.len() as u64 <= MAX_JPEG,
            "timelapse sample exceeds 512 KiB"
        );
        fs::create_dir_all(&dir)?;
        ensure!(regular(&dir)?.is_dir(), "invalid timelapse day directory");
        let mut staging = tempfile::NamedTempFile::new_in(&dir)?;
        staging.write_all(&jpeg)?;
        staging.as_file().sync_all()?;
        staging.persist_noclobber(&output)?;
        self.bytes += jpeg.len() as u64;
        Ok(())
    }

    fn pending(&self, now: u64) -> Result<Vec<(u64, PathBuf)>> {
        let mut result = Vec::new();
        for path in entries(&self.root)? {
            let day = day_number(&path).context("invalid timelapse cache directory")?;
            ensure!(regular(&path)?.is_dir(), "invalid timelapse day directory");
            if day < now / DAY_MS {
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
    pub(crate) fn record(&self, saved: &Path, thumbnail: impl FnOnce() -> Result<RgbImage>) {
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
            let result = cache.record(timestamp, thumbnail);
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
        upload: Option<UploadSink>,
        retention: Option<RetentionSink>,
    ) -> Result<Self> {
        let sink = DailySink(Arc::new(Mutex::new(Cache::open(directory)?)));
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
                            if !path.join("done").exists() {
                                finish_day(
                                    &executable,
                                    &path,
                                    &directory.join(output_name(day)),
                                    &cancelled,
                                    upload.as_ref(),
                                    retention.as_ref(),
                                )?;
                            }
                            cache
                                .0
                                .lock()
                                .map_err(|_| anyhow!("timelapse lock poisoned"))?
                                .clean_done(&path)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        tracing::warn!(%error, "daily timelapse pending; retrying in five minutes");
                    }
                    thread::park_timeout(Duration::from_secs(300));
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

fn finish_day(
    executable: &Path,
    input: &Path,
    output: &Path,
    stop: &AtomicBool,
    upload: Option<&UploadSink>,
    retention: Option<&RetentionSink>,
) -> Result<()> {
    finish_day_with(input, output, upload, retention, |input, temporary| {
        crate::native_video::run_encoder(
            executable,
            input,
            temporary,
            stop,
            Duration::from_secs(600),
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
        encode(input, &temporary)?;
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
