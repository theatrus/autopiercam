use anyhow::{Context, Result, anyhow, bail};
use autopiercam_core::image::{BayerPattern, demosaic_bilinear_preview};
use autopiercam_protocol::{
    MAX_PREVIEW_JPEG_SIZE, PREVIEW_MAX_DIMENSION, PROTOCOL_VERSION, PreviewContentType,
    PreviewMetadata, PreviewMode,
};
use image::{ColorType, ImageEncoder, codecs::jpeg::JpegEncoder};
use std::sync::{
    Arc, Condvar, Mutex, RwLock, TryLockError,
    atomic::{AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use tracing::warn;

const PREVIEW_JPEG_QUALITY: u8 = 75;
pub(crate) const PREVIEW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub metadata: PreviewMetadata,
    pub jpeg: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub struct PreviewSnapshot {
    pub change_generation: u64,
    pub frame: Option<Arc<PreviewFrame>>,
}

#[derive(Clone, Debug, Default)]
pub struct PreviewHub {
    inner: Arc<RwLock<PreviewHubState>>,
    next_session_generation: Arc<AtomicU64>,
    next_sequence: Arc<AtomicU64>,
}

#[derive(Debug, Default)]
struct PreviewHubState {
    change_generation: u64,
    current_session_generation: u64,
    frame: Option<Arc<PreviewFrame>>,
}

impl PreviewHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_session(&self) -> PreviewSession {
        let generation = next_nonzero(&self.next_session_generation);
        {
            let mut state = write_unpoisoned(&self.inner);
            state.change_generation = state.change_generation.wrapping_add(1);
            state.current_session_generation = generation;
            state.frame = None;
        }
        PreviewSession {
            inner: Arc::new(PreviewSessionInner {
                hub: self.clone(),
                generation,
            }),
        }
    }

    pub fn snapshot(&self) -> PreviewSnapshot {
        let state = read_unpoisoned(&self.inner);
        PreviewSnapshot {
            change_generation: state.change_generation,
            frame: state.frame.clone(),
        }
    }

    fn publish(
        &self,
        session_generation: u64,
        mut metadata: PreviewMetadata,
        jpeg: Vec<u8>,
    ) -> Result<bool> {
        if jpeg.is_empty() || jpeg.len() > MAX_PREVIEW_JPEG_SIZE {
            bail!(
                "preview JPEG length {} is outside the supported 1..={MAX_PREVIEW_JPEG_SIZE} byte range",
                jpeg.len()
            );
        }

        metadata.version = PROTOCOL_VERSION;
        metadata.session_generation = session_generation;
        metadata.sequence = next_nonzero(&self.next_sequence);
        metadata
            .validate()
            .context("validating generated preview metadata")?;

        let mut state = write_unpoisoned(&self.inner);
        if state.current_session_generation != session_generation {
            return Ok(false);
        }
        state.change_generation = state.change_generation.wrapping_add(1);
        state.frame = Some(Arc::new(PreviewFrame {
            metadata,
            jpeg: Arc::from(jpeg),
        }));
        Ok(true)
    }

    fn end_session(&self, session_generation: u64) {
        let mut state = write_unpoisoned(&self.inner);
        if state.current_session_generation != session_generation {
            return;
        }
        state.change_generation = state.change_generation.wrapping_add(1);
        state.current_session_generation = 0;
        state.frame = None;
    }
}

#[derive(Clone, Debug)]
pub struct PreviewSession {
    inner: Arc<PreviewSessionInner>,
}

#[derive(Debug)]
struct PreviewSessionInner {
    hub: PreviewHub,
    generation: u64,
}

impl PreviewSession {
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    fn publish(&self, metadata: PreviewMetadata, jpeg: Vec<u8>) -> Result<bool> {
        self.inner
            .hub
            .publish(self.inner.generation, metadata, jpeg)
    }
}

impl Drop for PreviewSessionInner {
    fn drop(&mut self) {
        self.hub.end_session(self.generation);
    }
}

#[derive(Debug)]
pub(crate) struct PreviewJob {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bayer: BayerPattern,
    pub(crate) data: Vec<u8>,
    pub(crate) captured_at_unix_ms: u64,
    pub(crate) exposure_us: i64,
    pub(crate) gain: i64,
    pub(crate) dropped_frames: u64,
}

pub(crate) struct PreviewEncoder {
    queue: Arc<LatestPreviewQueue>,
    thread: Option<JoinHandle<()>>,
}

impl PreviewEncoder {
    pub(crate) fn start(session: PreviewSession) -> Result<Self> {
        let queue = Arc::new(LatestPreviewQueue::default());
        let encoder_queue = Arc::clone(&queue);
        let thread = thread::Builder::new()
            .name("autopiercam-preview-encoder".to_owned())
            .spawn(move || preview_encoder_loop(&encoder_queue, &session))
            .context("starting preview encoder")?;
        Ok(Self {
            queue,
            thread: Some(thread),
        })
    }

    pub(crate) fn sink(&self) -> PreviewSink {
        PreviewSink {
            queue: Arc::clone(&self.queue),
        }
    }

    pub(crate) fn stop_and_join(mut self) -> Result<()> {
        self.queue.close();
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| anyhow!("preview encoder thread panicked"))
    }
}

impl Drop for PreviewEncoder {
    fn drop(&mut self) {
        self.queue.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct PreviewSink {
    queue: Arc<LatestPreviewQueue>,
}

impl PreviewSink {
    pub(crate) fn try_publish<F>(&self, build: F) -> bool
    where
        F: FnOnce(u64) -> PreviewJob,
    {
        let mut state = match self.queue.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                self.queue.dropped_frames.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        if state.closed {
            return false;
        }
        if state.pending.is_some() {
            self.queue.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
        let dropped_frames = self.queue.dropped_frames.load(Ordering::Relaxed);
        state.pending = Some(build(dropped_frames));
        self.queue.available.notify_one();
        true
    }
}

#[derive(Debug, Default)]
struct LatestPreviewQueue {
    state: Mutex<LatestPreviewState>,
    available: Condvar,
    dropped_frames: AtomicU64,
}

#[derive(Debug, Default)]
struct LatestPreviewState {
    pending: Option<PreviewJob>,
    closed: bool,
}

impl LatestPreviewQueue {
    fn next(&self) -> Option<PreviewJob> {
        let mut state = lock_unpoisoned(&self.state);
        loop {
            if let Some(job) = state.pending.take() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        let mut state = lock_unpoisoned(&self.state);
        state.closed = true;
        state.pending = None;
        self.available.notify_all();
    }
}

fn preview_encoder_loop(queue: &LatestPreviewQueue, session: &PreviewSession) {
    while let Some(job) = queue.next() {
        if let Err(error) = encode_and_publish(job, session) {
            warn!(%error, "dropping preview frame after an encoding failure");
        }
    }
}

fn encode_and_publish(job: PreviewJob, session: &PreviewSession) -> Result<()> {
    let (width, height, rgb) = demosaic_bilinear_preview(
        &job.data,
        job.width,
        job.height,
        job.bayer,
        PREVIEW_MAX_DIMENSION,
        PREVIEW_MAX_DIMENSION,
    )
    .context("debayering downscaled preview")?;
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, PREVIEW_JPEG_QUALITY)
        .write_image(&rgb, width, height, ColorType::Rgb8.into())
        .context("encoding preview JPEG")?;
    if jpeg.len() > MAX_PREVIEW_JPEG_SIZE {
        bail!(
            "encoded preview is {} bytes; maximum is {MAX_PREVIEW_JPEG_SIZE}",
            jpeg.len()
        );
    }

    let metadata = PreviewMetadata {
        version: PROTOCOL_VERSION,
        session_generation: session.generation(),
        sequence: 1,
        captured_at_unix_ms: job.captured_at_unix_ms,
        width,
        height,
        exposure_us: Some(job.exposure_us),
        gain: Some(job.gain),
        content_type: PreviewContentType::Jpeg,
        mode: PreviewMode::Unknown,
        dropped_frames: job.dropped_frames,
    };
    let _ = session.publish(metadata, jpeg)?;
    Ok(())
}

fn next_nonzero(counter: &AtomicU64) -> u64 {
    loop {
        let value = counter.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        if value != 0 {
            return value;
        }
    }
}

fn read_unpoisoned<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_unpoisoned<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_unpoisoned<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> PreviewMetadata {
        PreviewMetadata {
            version: PROTOCOL_VERSION,
            session_generation: 1,
            sequence: 1,
            captured_at_unix_ms: 1,
            width: 1,
            height: 1,
            exposure_us: Some(1_000),
            gain: Some(10),
            content_type: PreviewContentType::Jpeg,
            mode: PreviewMode::Unknown,
            dropped_frames: 0,
        }
    }

    fn job(value: u8, dropped_frames: u64) -> PreviewJob {
        PreviewJob {
            width: 2,
            height: 2,
            bayer: BayerPattern::Rg,
            data: vec![value; 4],
            captured_at_unix_ms: u64::from(value),
            exposure_us: 1_000,
            gain: 10,
            dropped_frames,
        }
    }

    #[test]
    fn sessions_clear_stale_frames_and_reject_old_publishers() {
        let hub = PreviewHub::new();
        let first = hub.begin_session();
        assert!(
            first
                .publish(metadata(), vec![0xff, 0xd8, 0xff, 0xd9])
                .unwrap()
        );
        let first_snapshot = hub.snapshot();
        assert_eq!(
            first_snapshot
                .frame
                .as_ref()
                .unwrap()
                .metadata
                .session_generation,
            first.generation()
        );

        let second = hub.begin_session();
        assert!(hub.snapshot().frame.is_none());
        assert!(
            !first
                .publish(metadata(), vec![0xff, 0xd8, 0xff, 0xd9])
                .unwrap()
        );
        assert!(
            second
                .publish(metadata(), vec![0xff, 0xd8, 0xff, 0xd9])
                .unwrap()
        );
        drop(second);
        assert!(hub.snapshot().frame.is_none());
    }

    #[test]
    fn pending_preview_is_replaced_and_drop_count_is_carried_forward() {
        let queue = Arc::new(LatestPreviewQueue::default());
        let sink = PreviewSink {
            queue: Arc::clone(&queue),
        };
        assert!(sink.try_publish(|dropped| job(1, dropped)));
        assert!(sink.try_publish(|dropped| job(2, dropped)));
        let latest = queue.next().unwrap();
        assert_eq!(latest.data, vec![2; 4]);
        assert_eq!(latest.dropped_frames, 1);
        queue.close();
        assert!(queue.next().is_none());
    }
}
