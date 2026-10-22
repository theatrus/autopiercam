//! Independent, bounded latest-preview sampler and segmented H.264 encoder.
//! Windows native encoding runs in an isolated, deadline-bounded worker process.
use crate::{
    AgentControl, CaptureSessionNonce, PreviewSession, capture_filename,
    retention::{RetentionSink, RetentionWakeResult},
    upload::{UploadEnqueueResult, UploadSink},
};
use anyhow::{Context, Result, anyhow, ensure};
use autopiercam_core::config::VideoConfig;
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_SPOOL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SAMPLES: usize = 1200;
const ENCODER_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) struct VideoWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl VideoWorker {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        config: &VideoConfig,
        directory: &Path,
        session: PreviewSession,
        nonce: CaptureSessionNonce,
        upload: Option<UploadSink>,
        retention: Option<RetentionSink>,
        control: AgentControl,
    ) -> Result<Self> {
        let executable = crate::native_video::executable()?;
        let directory = directory.to_owned();
        let config = config.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("autopiercam-video".into())
            .spawn(move || {
                let mut segment: Option<Segment> = None;
                let mut last_sequence = 0;
                let mut sequence = 0;
                let cadence = Duration::from_secs_f64(1.0 / f64::from(config.frames_per_second));
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    if !control.is_paused()
                        && !retention
                            .as_ref()
                            .is_some_and(RetentionSink::capture_suspended)
                        && let Some(frame) = session.snapshot().frame
                        && frame.metadata.sequence != last_sequence
                    {
                        last_sequence = frame.metadata.sequence;
                        if segment.is_none() {
                            segment = Some(Segment::new(&directory)?);
                        }
                        let active = segment.as_mut().expect("segment created");
                        // Rotation happens before the next allocation exceeds the byte bound.
                        if active.bytes + frame.jpeg.len() as u64 > MAX_SPOOL_BYTES
                            || active.frames.len() >= MAX_SAMPLES
                        {
                            finish_segment(
                                segment.take().unwrap(),
                                &executable,
                                &directory,
                                nonce,
                                sequence,
                                upload.as_ref(),
                                retention.as_ref(),
                            )?;
                            sequence += 1;
                            segment = Some(Segment::new(&directory)?);
                        }
                        segment.as_mut().unwrap().push(&frame.jpeg)?;
                    }
                    if segment.as_ref().is_some_and(|s| {
                        s.started.elapsed()
                            >= Duration::from_secs(u64::from(config.segment_seconds))
                    }) {
                        finish_segment(
                            segment.take().unwrap(),
                            &executable,
                            &directory,
                            nonce,
                            sequence,
                            upload.as_ref(),
                            retention.as_ref(),
                        )?;
                        sequence += 1;
                    }
                    thread::park_timeout(cadence);
                }
                if let Some(segment) = segment {
                    finish_segment(
                        segment,
                        &executable,
                        &directory,
                        nonce,
                        sequence,
                        upload.as_ref(),
                        retention.as_ref(),
                    )?;
                }
                Ok(())
            })
            .context("starting video sampler")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn stop_and_join(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread.thread().unpark();
        thread
            .join()
            .map_err(|_| anyhow!("video worker panicked"))?
    }
}

impl Drop for VideoWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

struct Segment {
    directory: tempfile::TempDir,
    started: Instant,
    frames: Vec<Duration>,
    bytes: u64,
}

impl Segment {
    fn new(parent: &Path) -> Result<Self> {
        Ok(Self {
            directory: tempfile::Builder::new()
                .prefix(".autopiercam-video-")
                .tempdir_in(parent)?,
            started: Instant::now(),
            frames: Vec::new(),
            bytes: 0,
        })
    }
    fn push(&mut self, jpeg: &[u8]) -> Result<()> {
        ensure!(
            self.bytes + jpeg.len() as u64 <= MAX_SPOOL_BYTES && self.frames.len() < MAX_SAMPLES,
            "video spool bound exceeded"
        );
        fs::write(
            self.directory
                .path()
                .join(format!("sample-{:06}.jpg", self.frames.len())),
            jpeg,
        )?;
        self.frames.push(self.started.elapsed());
        self.bytes += jpeg.len() as u64;
        Ok(())
    }
}

fn timed_samples(frames: &[Duration], end: Duration) -> Vec<crate::native_video::Sample> {
    frames
        .iter()
        .enumerate()
        .map(|(index, at)| {
            let duration = frames
                .get(index + 1)
                .copied()
                .unwrap_or(end)
                .saturating_sub(*at)
                .max(Duration::from_millis(40));
            crate::native_video::Sample {
                file: format!("sample-{index:06}.jpg"),
                duration: (duration.as_nanos() / 100).min(i64::MAX as u128) as i64,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn finish_segment(
    segment: Segment,
    executable: &Path,
    directory: &Path,
    nonce: CaptureSessionNonce,
    sequence: u64,
    upload: Option<&UploadSink>,
    retention: Option<&RetentionSink>,
) -> Result<()> {
    if segment.frames.is_empty() {
        return Ok(());
    }
    let output = directory
        .join(capture_filename(nonce, sequence))
        .with_extension("mp4");
    encode_segment(&segment, executable, &output)?;
    if let Some(upload) = upload {
        ensure!(
            upload.try_enqueue(output.clone())? != UploadEnqueueResult::WorkerStopped,
            "upload worker stopped after recording video"
        );
    }
    if let Some(retention) = retention {
        ensure!(
            retention.try_wake() != RetentionWakeResult::Stopped,
            "retention worker stopped after saving video"
        );
    }
    tracing::info!(path = %output.display(), frames = segment.frames.len(), "saved H.264 video segment");
    Ok(())
}

fn encode_segment(segment: &Segment, executable: &Path, output: &Path) -> Result<()> {
    encode_segment_with_timeout(segment, executable, output, ENCODER_TIMEOUT)
}

fn encode_segment_with_timeout(
    segment: &Segment,
    executable: &Path,
    output: &Path,
    timeout: Duration,
) -> Result<()> {
    ensure!(!segment.frames.is_empty(), "cannot encode an empty segment");
    crate::native_video::write_manifest(
        segment.directory.path(),
        &timed_samples(&segment.frames, segment.started.elapsed()),
    )?;
    crate::native_video::encode_atomic(executable, segment.directory.path(), output, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_preserves_elapsed_gaps_without_duplicate_last_frame() {
        let samples = timed_samples(
            &[Duration::ZERO, Duration::from_secs(30)],
            Duration::from_secs(90),
        );
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].duration, 300_000_000);
        assert_eq!(samples[1].duration, 600_000_000);
        assert_eq!(samples[1].file, "sample-000001.jpg");
    }
    #[test]
    fn spool_rejects_oversized_segments_before_writing() {
        let root = tempfile::tempdir().unwrap();
        let mut segment = Segment::new(root.path()).unwrap();
        segment.bytes = MAX_SPOOL_BYTES;
        assert!(segment.push(&[1]).is_err());
        assert_eq!(fs::read_dir(segment.directory.path()).unwrap().count(), 0);
    }
}
