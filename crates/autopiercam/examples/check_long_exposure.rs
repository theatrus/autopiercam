//! Opt-in hardware diagnostic. Close camera-owning applications before running.
//! Both modes restore the affected controls and ROI format after success,
//! cancellation, or failure. The SDK wrapper recenters a restored ROI; it cannot
//! restore a previous off-center ROI origin.
use anyhow::{Context, Result, bail, ensure};
use autopiercam::{AgentControl, AgentMonitor, PreviewHub, run_agent_with_monitor_and_preview};
use autopiercam_asi::{Camera, CameraInfo, ControlType, ControlValue, ImageType, Roi, Sdk};
use autopiercam_core::{ConfigStore, image::raw8_stats};
use clap::{Parser, Subcommand};
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(about = "Opt-in long-exposure camera and production-worker diagnostic")]
struct Options {
    #[arg(long, default_value_t = 0)]
    camera_id: i32,
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Verify real exposure cadence through short video-read polls, then cancel.
    Sdk {
        #[arg(long, value_delimiter = ',', default_value = "30,60")]
        seconds: Vec<u32>,
        #[arg(long)]
        raw16: bool,
    },
    /// Run the real auto worker with a temporary configuration and print progress.
    Agent {
        #[arg(long, default_value_t = 60)]
        max_seconds: u32,
        /// Stop the worker after this time (also tests cancellation while settling).
        #[arg(long, default_value_t = 720)]
        stop_after_seconds: u32,
        #[arg(long, default_value_t = 2)]
        stills: u64,
        #[arg(long)]
        adaptive: bool,
        #[arg(long)]
        raw16: bool,
        /// Also exercise production segmented recording using this installed FFmpeg.
        #[arg(long)]
        ffmpeg: Option<std::path::PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let options = Options::parse();
    let sdk = Arc::new(Sdk::load_default()?);
    let control = AgentControl::new();
    let cancel = control.clone();
    ctrlc::set_handler(move || cancel.shutdown())?;
    match options.mode {
        Mode::Sdk { seconds, raw16 } => {
            check_sdk(&sdk, options.camera_id, &seconds, raw16, &control)
        }
        Mode::Agent {
            max_seconds,
            stop_after_seconds,
            stills,
            adaptive,
            raw16,
            ffmpeg,
        } => check_agent(
            &sdk,
            options.camera_id,
            max_seconds,
            stop_after_seconds,
            stills,
            adaptive,
            raw16,
            ffmpeg.as_deref(),
            &control,
        ),
    }
}

struct SavedCameraSettings {
    roi: Roi,
    controls: Vec<(ControlType, ControlValue)>,
}

impl SavedCameraSettings {
    fn read(camera: &Camera, changed_controls: &[ControlType]) -> Result<Self> {
        let caps = camera.controls()?;
        let mut controls = Vec::new();
        for &control in changed_controls {
            if caps
                .iter()
                .any(|caps| caps.control_type == control && caps.writable)
            {
                controls.push((
                    control,
                    camera.control_value(control).with_context(|| {
                        format!("saving camera control {} before diagnostic", control.0)
                    })?,
                ));
            }
        }
        Ok(Self {
            roi: camera.roi()?,
            controls,
        })
    }

    fn restore(&self, camera: &mut Camera) -> Result<()> {
        let mut failures = Vec::new();
        if let Err(error) = camera.stop_video() {
            failures.push(format!("stopping video: {error}"));
        }
        if let Err(error) = camera.set_roi(self.roi) {
            failures.push(format!("restoring ROI format: {error}"));
        }
        for (control, value) in &self.controls {
            if let Err(error) = camera.set_control(*control, value.value, value.automatic) {
                failures.push(format!("restoring control {}: {error}", control.0));
            }
        }
        ensure!(
            failures.is_empty(),
            "camera restoration failed: {}",
            failures.join("; ")
        );
        Ok(())
    }

    fn restore_after_worker(&self, sdk: &Arc<Sdk>, info: CameraInfo) -> Result<()> {
        // The worker has been joined and its Camera dropped before this handle
        // is opened. Two handles never compete for the same physical camera.
        let mut camera = sdk
            .open(info)
            .context("reopening camera to restore diagnostic settings")?;
        self.restore(&mut camera)
    }
}

fn diagnostic_result(result: Result<()>, restoration: Result<()>) -> Result<()> {
    match (result, restoration) {
        (Err(error), Err(restoration)) => {
            Err(error.context(format!("camera cleanup also failed: {restoration:#}")))
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), restoration) => restoration,
    }
}

fn check_sdk(
    sdk: &Arc<Sdk>,
    camera_id: i32,
    seconds: &[u32],
    raw16: bool,
    control: &AgentControl,
) -> Result<()> {
    ensure!(
        !seconds.is_empty() && seconds.iter().all(|s| (1..=2000).contains(s)),
        "SDK check durations must be in 1..=2000 seconds and supported by the camera"
    );
    let info = sdk
        .cameras()?
        .into_iter()
        .find(|c| c.camera_id == camera_id)
        .context("selected camera is not attached")?;
    println!("Testing {} with SDK {}", info.name, sdk.version());
    let mut camera = sdk.open(info.clone())?;
    let previous = SavedCameraSettings::read(&camera, &[ControlType::EXPOSURE, ControlType::GAIN])?;
    let result = (|| -> Result<()> {
        camera.set_roi(Roi {
            width: info.max_width,
            height: info.max_height,
            bin: 1,
            image_type: if raw16 {
                ImageType::Raw16
            } else {
                ImageType::Raw8
            },
        })?;
        camera.set_control(ControlType::GAIN, 0, false)?;
        for &seconds in seconds {
            if control.is_shutdown() {
                return Ok(());
            }
            let exposure_us = i64::from(seconds) * 1_000_000;
            camera.set_control(ControlType::EXPOSURE, exposure_us, false)?;
            let readback = camera.control_value(ControlType::EXPOSURE)?;
            ensure!(
                !readback.automatic && readback.value.abs_diff(exposure_us) < 10_000,
                "camera did not accept {seconds}s manual exposure: {readback:?}"
            );
            camera.start_video()?;
            let mut buffer = Vec::new();
            // Drain one frame, then measure a complete subsequent sensor interval.
            if receive_frame(&mut camera, &mut buffer, seconds, control)?.is_none() {
                return Ok(());
            }
            let started = Instant::now();
            let Some(polls) = receive_frame(&mut camera, &mut buffer, seconds, control)? else {
                return Ok(());
            };
            let elapsed = started.elapsed();
            let stats = if raw16 {
                let bitwise_or = buffer.chunks_exact(2).fold(0_u16, |bits, pixel| {
                    bits | u16::from_le_bytes([pixel[0], pixel[1]])
                });
                let maximum = buffer
                    .chunks_exact(2)
                    .map(|pixel| u16::from_le_bytes([pixel[0], pixel[1]]))
                    .max()
                    .unwrap_or(0);
                println!(
                    "RAW16 sensor_bits={} maximum={maximum} bitwise_or={bitwise_or:#06x} trailing_zero_bits={}",
                    info.bit_depth,
                    bitwise_or.trailing_zeros()
                );
                autopiercam_core::image::raw16_stats(&buffer, 64)?
            } else {
                raw8_stats(&buffer, 64)?
            };
            println!(
                "SDK exposure={seconds}s frame_interval={:.3}s timeouts={polls} bytes={} p90={}",
                elapsed.as_secs_f64(),
                buffer.len(),
                stats.p90
            );
            ensure!(
                elapsed >= Duration::from_millis(u64::from(seconds) * 900),
                "received a frame too quickly to verify a full {seconds}s exposure"
            );
            camera.stop_video()?;
        }
        if control.is_shutdown() {
            return Ok(());
        }
        let longest = *seconds.iter().max().unwrap();
        camera.set_control(ControlType::EXPOSURE, i64::from(longest) * 1_000_000, false)?;
        camera.start_video()?;
        let mut buffer = Vec::new();
        // One bounded poll leaves a long exposure in flight.
        match camera.next_video_frame_into(&mut buffer, 2_000) {
            Ok(_) => {}
            Err(error) if error.is_timeout() => {}
            Err(error) => return Err(error.into()),
        }
        let stopping = Instant::now();
        camera.stop_video()?;
        let elapsed = stopping.elapsed();
        println!(
            "SDK stop during {longest}s exposure completed in {:.3}s",
            elapsed.as_secs_f64()
        );
        ensure!(
            elapsed < Duration::from_secs(5),
            "SDK stop exceeded five seconds"
        );
        Ok(())
    })();
    // Attempt all restoration steps, even when a diagnostic failed.
    let restoration = previous.restore(&mut camera);
    if control.is_shutdown() {
        println!("SDK diagnostic cancelled; camera restoration attempted");
    }
    diagnostic_result(result, restoration)
}

fn receive_frame(
    camera: &mut Camera,
    buffer: &mut Vec<u8>,
    seconds: u32,
    control: &AgentControl,
) -> Result<Option<u32>> {
    let started = Instant::now();
    let deadline = Duration::from_secs(u64::from(seconds) * 2 + 5);
    let mut polls = 0;
    loop {
        if control.is_shutdown() {
            return Ok(None);
        }
        let result = camera.next_video_frame_into(buffer, 2_000);
        if control.is_shutdown() {
            return Ok(None);
        }
        match result {
            Ok(_) => return Ok(Some(polls)),
            Err(error) if error.is_timeout() && started.elapsed() < deadline => polls += 1,
            Err(error) => return Err(error.into()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_agent(
    sdk: &Arc<Sdk>,
    camera_id: i32,
    max_seconds: u32,
    stop_after_seconds: u32,
    stills: u64,
    adaptive: bool,
    raw16: bool,
    ffmpeg: Option<&std::path::Path>,
    control: &AgentControl,
) -> Result<()> {
    ensure!(
        (1..=if adaptive { 2000 } else { 60 }).contains(&max_seconds)
            && stop_after_seconds > 0
            && stills > 0,
        "max-seconds must be in 1..=60 (1..=2000 with --adaptive); stop-after-seconds and stills must be positive"
    );
    let temporary = tempfile::Builder::new()
        .prefix("autopiercam-exposure-check-")
        .tempdir()?;
    let store = ConfigStore::open(temporary.path().join("autopiercam.toml"))?;
    let snapshot = store.snapshot()?;
    let mut config = snapshot.config;
    config.camera.camera_id = Some(camera_id);
    config.camera.exposure_control = if adaptive {
        autopiercam_core::config::ExposureControl::Adaptive
    } else {
        autopiercam_core::config::ExposureControl::Sdk
    };
    config.camera.raw16 = raw16;
    config.camera.max_exposure_us = i64::from(max_seconds) * 1_000_000;
    config.capture.interval_ms = 1;
    config.capture.retention_days = 0;
    if let Some(ffmpeg) = ffmpeg {
        config.video.enabled = true;
        config.video.ffmpeg_path = Some(ffmpeg.to_path_buf());
        config.video.segment_seconds = 10;
    }
    let capture_directory = temporary.path().join(&config.capture.directory);
    store.replace(snapshot.revision, config)?;
    let info = sdk
        .cameras()?
        .into_iter()
        .find(|info| info.camera_id == camera_id)
        .context("selected camera is not attached")?;
    let previous = {
        let camera = sdk.open(info.clone())?;
        SavedCameraSettings::read(
            &camera,
            &[
                ControlType::AUTO_MAX_EXPOSURE,
                ControlType::AUTO_MAX_GAIN,
                ControlType::AUTO_TARGET_BRIGHTNESS,
                ControlType::FLIP,
                ControlType::EXPOSURE,
                ControlType::GAIN,
            ],
        )?
    };
    let monitor = AgentMonitor::new();
    let preview = PreviewHub::new();
    let session = preview.begin_session();
    let worker_control = control.clone();
    let worker_monitor = monitor.clone();
    let worker_sdk = Arc::clone(sdk);
    let config_path = store.path().to_path_buf();
    let worker = thread::spawn(move || {
        run_agent_with_monitor_and_preview(
            &worker_sdk,
            &config_path,
            Some(stills),
            &worker_control,
            &worker_monitor,
            &session,
        )
    });
    let started = Instant::now();
    let mut stop_started = None;
    let mut saw_settling_preview = false;
    let mut saw_progress = false;
    let mut peak_exposure_us = 0;
    let mut last_sequence = 0;
    let mut warned_shutdown = false;
    let mut next_status = Duration::ZERO;
    let observation_result = (|| -> Result<()> {
        while !worker.is_finished() {
            let status = monitor.snapshot();
            saw_progress |= status.exposure.is_some();
            if let Some(exposure) = &status.exposure {
                peak_exposure_us = peak_exposure_us.max(exposure.exposure_us);
            }
            if let Some(frame) = preview.snapshot().frame
                && frame.metadata.sequence != last_sequence
            {
                last_sequence = frame.metadata.sequence;
                let settling = status.exposure.as_ref().is_some_and(|e| e.settling);
                saw_settling_preview |= settling;
                println!(
                    "Preview sequence={last_sequence} settling={settling} exposure_us={:?}",
                    frame.metadata.exposure_us
                );
            }
            if started.elapsed() >= next_status {
                println!("{}", serde_json::to_string(&status)?);
                next_status = started.elapsed() + Duration::from_secs(2);
            }
            if started.elapsed() >= Duration::from_secs(u64::from(stop_after_seconds)) {
                control.shutdown();
            }
            if control.is_shutdown() && stop_started.is_none() {
                stop_started = Some(Instant::now());
            }
            if !warned_shutdown
                && stop_started.is_some_and(|start| start.elapsed() > Duration::from_secs(15))
            {
                // Keep the owner thread joined; report the failed bound after it exits.
                eprintln!("Worker shutdown exceeded 15 seconds; waiting for camera cleanup");
                warned_shutdown = true;
            }
            thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    })();
    if observation_result.is_err() {
        control.shutdown();
    }
    let worker_result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("camera owner panicked"))
        .and_then(|result| result);
    let stop_elapsed = stop_started.map(|started| started.elapsed());
    let restoration = previous.restore_after_worker(sdk, info);
    let result = (|| -> Result<()> {
        observation_result?;
        worker_result?;
        if let Some(ffmpeg) = ffmpeg {
            let clips: Vec<_> = std::fs::read_dir(&capture_directory)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
                .collect();
            ensure!(
                !clips.is_empty(),
                "video enabled but no MP4 segments were published"
            );
            for clip in &clips {
                let decoded = std::process::Command::new(ffmpeg)
                    .args(["-nostdin", "-v", "error", "-i"])
                    .arg(clip)
                    .args(["-f", "null", "-"])
                    .output()?;
                ensure!(
                    decoded.status.success(),
                    "recorded MP4 could not be decoded: {}",
                    String::from_utf8_lossy(&decoded.stderr)
                );
            }
            println!(
                "Validated {} published H.264 video segments by decoding them",
                clips.len()
            );
        }
        let final_status = monitor.snapshot();
        println!(
            "Finished: saved={} saw_progress={saw_progress} saw_settling_preview={saw_settling_preview} peak_exposure_us={peak_exposure_us}",
            final_status.frames_saved
        );
        ensure!(saw_progress, "worker never published exposure progress");
        ensure!(
            final_status.exposure.is_none(),
            "finished worker retained exposure progress"
        );
        if let Some(elapsed) = stop_elapsed {
            println!("Worker shutdown completed in {:.3}s", elapsed.as_secs_f64());
            ensure!(
                elapsed < Duration::from_secs(5),
                "worker shutdown exceeded five seconds"
            );
        } else {
            ensure!(
                saw_settling_preview,
                "worker completed without a preview observed during settling"
            );
            if final_status.frames_saved != stills {
                bail!("worker ended before requested stills were saved");
            }
        }
        Ok(())
    })();
    diagnostic_result(result, restoration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_and_restoration_failures_are_both_reported() {
        let result = diagnostic_result(
            Err(anyhow::anyhow!("frame deadline expired")),
            Err(anyhow::anyhow!("exposure restore failed")),
        );
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("frame deadline expired"));
        assert!(error.contains("exposure restore failed"));
    }

    #[test]
    fn successful_diagnostic_does_not_hide_failed_restoration() {
        let result = diagnostic_result(Ok(()), Err(anyhow::anyhow!("gain restore failed")));
        assert_eq!(result.unwrap_err().to_string(), "gain restore failed");
        assert!(diagnostic_result(Ok(()), Ok(())).is_ok());
    }
}
