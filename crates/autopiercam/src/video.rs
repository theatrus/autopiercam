//! Independent, bounded latest-preview sampler and segmented H.264 encoder.
//! FFmpeg is an explicitly configured external executable, never a shell command.
use crate::{
    AgentControl, CaptureSessionNonce, PreviewSession, capture_filename,
    publish_temporary_artifact,
    retention::{RetentionSink, RetentionWakeResult},
    temporary_artifact_path,
    upload::{UploadEnqueueResult, UploadSink},
};
use anyhow::{Context, Result, anyhow, ensure};
use autopiercam_core::config::VideoConfig;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
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
        let executable = config
            .ffmpeg_path
            .as_ref()
            .context("video.ffmpeg_path is required")?;
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "video.ffmpeg_path must identify an installed FFmpeg executable: {}",
            executable.display()
        );
        let executable = executable.clone();
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
    fn concat(&self) -> String {
        concat_manifest(&self.frames, self.started.elapsed())
    }
}

fn concat_manifest(frames: &[Duration], end: Duration) -> String {
    let mut text = String::from("ffconcat version 1.0\n");
    for (index, at) in frames.iter().enumerate() {
        let duration = frames
            .get(index + 1)
            .copied()
            .unwrap_or(end)
            .saturating_sub(*at)
            .max(Duration::from_millis(40));
        text.push_str(&format!(
            "file 'sample-{index:06}.jpg'\nduration {:.6}\n",
            duration.as_secs_f64()
        ));
    }
    if !frames.is_empty() {
        text.push_str(&format!("file 'sample-{:06}.jpg'\n", frames.len() - 1));
    }
    text
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
    ensure!(!segment.frames.is_empty(), "cannot encode an empty segment");
    let mut manifest = File::create(segment.directory.path().join("frames.ffconcat"))?;
    manifest.write_all(segment.concat().as_bytes())?;
    manifest.sync_all()?;
    drop(manifest);
    let temporary = temporary_artifact_path(output)?;
    let result = (|| -> Result<()> {
        let mut command = Command::new(executable);
        command
            .current_dir(segment.directory.path())
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-protocol_whitelist",
                "file",
                "-f",
                "concat",
                "-safe",
                "1",
                "-i",
                "frames.ffconcat",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "28",
                "-threads",
                "2",
                "-vf",
                "pad=ceil(iw/2)*2:ceil(ih/2)*2",
                "-pix_fmt",
                "yuv420p",
                "-fps_mode",
                "vfr",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "-n",
            ])
            .arg(&temporary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().context("starting configured FFmpeg")?;
        let stderr = child.stderr.take().context("FFmpeg stderr missing")?;
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.take(16 * 1024).read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if started.elapsed() < ENCODER_TIMEOUT => {
                    thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => {
                    break Err(anyhow!(
                        "FFmpeg exceeded the 20-second segment encoding deadline"
                    ));
                }
                Err(error) => break Err(error.into()),
            }
        };
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let errors = reader
            .join()
            .unwrap_or_else(|_| "stderr reader failed".into());
        ensure!(
            status?.success(),
            "FFmpeg encoding failed: {}",
            errors.trim()
        );
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temporary)?;
        ensure!(
            file.metadata()?.len() > 0,
            "FFmpeg produced an empty segment"
        );
        file.sync_all()?;
        drop(file);
        publish_temporary_artifact(&temporary, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn manifest_uses_safe_names_and_real_elapsed_gaps() {
        let manifest = concat_manifest(
            &[Duration::ZERO, Duration::from_secs(30)],
            Duration::from_secs(90),
        );
        assert!(manifest.contains("duration 30.000000"));
        assert!(manifest.contains("duration 60.000000"));
        assert!(manifest.ends_with("file 'sample-000001.jpg'\n"));
    }
    #[test]
    fn spool_rejects_oversized_segments_before_writing() {
        let root = tempfile::tempdir().unwrap();
        let mut segment = Segment::new(root.path()).unwrap();
        segment.bytes = MAX_SPOOL_BYTES;
        assert!(segment.push(&[1]).is_err());
        assert_eq!(fs::read_dir(segment.directory.path()).unwrap().count(), 0);
    }
    #[test]
    #[ignore = "requires AUTOPIERCAM_TEST_FFMPEG pointing to a verified FFmpeg executable"]
    fn real_encoder_finalizes_mp4_without_overwriting() {
        let executable = PathBuf::from(
            std::env::var_os("AUTOPIERCAM_TEST_FFMPEG").expect("set test FFmpeg path"),
        );
        let root = tempfile::tempdir().unwrap();
        let mut segment = Segment::new(root.path()).unwrap();
        for value in [30, 60, 90] {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
                .encode(
                    &[value; 32 * 32 * 3],
                    32,
                    32,
                    image::ExtendedColorType::Rgb8,
                )
                .unwrap();
            segment.push(&jpeg).unwrap();
            thread::sleep(Duration::from_millis(100));
        }
        let output = root.path().join("clip.mp4");
        encode_segment(&segment, &executable, &output).unwrap();
        let before = fs::read(&output).unwrap();
        assert!(before.windows(4).any(|bytes| bytes == b"ftyp"));
        assert!(before.windows(4).any(|bytes| bytes == b"moov"));
        let decoded = Command::new(&executable)
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&output)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert!(encode_segment(&segment, &executable, &output).is_err());
        assert_eq!(fs::read(output).unwrap(), before);
    }

    #[test]
    #[ignore = "requires AUTOPIERCAM_TEST_FFMPEG pointing to a verified FFmpeg executable"]
    fn encoder_failure_never_publishes_partial_recording() {
        let executable = PathBuf::from(
            std::env::var_os("AUTOPIERCAM_TEST_FFMPEG").expect("set test FFmpeg path"),
        );
        let root = tempfile::tempdir().unwrap();
        let mut segment = Segment::new(root.path()).unwrap();
        segment.push(b"invalid JPEG").unwrap();
        let output = root.path().join("clip.mp4");
        assert!(encode_segment(&segment, &executable, &output).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1); // private spool only
    }
}
