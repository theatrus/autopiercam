use anyhow::{Context, Result, anyhow, bail};
use autopiercam_asi::{
    BayerPattern as AsiBayerPattern, Camera, CameraInfo, ControlCaps, ControlType, FrameMeta,
    ImageType, Roi, Sdk,
};
use autopiercam_core::{
    config::{CameraConfig, Config},
    image::{BayerPattern, demosaic_bilinear, luma_stats, raw8_stats},
};
use autopiercam_protocol::{AgentState, AgentStatus, StatusCamera};
use image::{
    ColorType, ImageEncoder,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, RwLock, RwLockWriteGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Thread-safe controls shared by the tray, local IPC server, and camera owner.
///
/// The camera loop deliberately polls atomics instead of receiving commands on
/// a blocking channel: an IPC thread can request shutdown even while the SDK is
/// in a bounded frame wait or automatic-exposure settling pass.
#[derive(Clone, Debug, Default)]
pub struct AgentControl {
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    capture_generation: Arc<AtomicU64>,
}

impl AgentControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Queue one still from the next available frame, including while paused.
    pub fn capture_now(&self) {
        self.capture_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    fn capture_generation(&self) -> u64 {
        self.capture_generation.load(Ordering::Acquire)
    }
}

/// Read-only runtime status shared with the tray and local IPC server.
#[derive(Clone, Debug)]
pub struct AgentMonitor {
    inner: Arc<RwLock<AgentStatus>>,
    capturing_generation: Arc<AtomicU64>,
}

impl Default for AgentMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentMonitor {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(AgentStatus::new(AgentState::Starting))),
            capturing_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn snapshot(&self) -> AgentStatus {
        match self.inner.read() {
            Ok(status) => status.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Publish a host-level startup or transport failure before camera setup.
    pub fn report_fault(&self, message: impl Into<String>) {
        let mut status = self.write();
        status.state = AgentState::Faulted;
        status.last_error = Some(message.into());
    }

    pub fn mark_stopping(&self) {
        self.set_state(AgentState::Stopping);
    }

    /// Monotonically changes whenever an attempt enters the Capturing state.
    /// Supervisors use this handshake so a short-lived successful connection
    /// is not missed between status polls.
    pub fn capturing_generation(&self) -> u64 {
        self.capturing_generation.load(Ordering::Acquire)
    }

    fn write(&self) -> RwLockWriteGuard<'_, AgentStatus> {
        match self.inner.write() {
            Ok(status) => status,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn begin_attempt(&self) {
        let mut status = self.write();
        status.state = AgentState::Starting;
        status.camera = None;
        status.last_error = None;
    }

    fn set_camera(&self, info: &CameraInfo) {
        self.write().camera = Some(StatusCamera {
            id: info.camera_id,
            name: info.name.clone(),
        });
    }

    fn set_state(&self, state: AgentState) {
        let mut status = self.write();
        if state == AgentState::Capturing && status.state != AgentState::Capturing {
            self.capturing_generation.fetch_add(1, Ordering::AcqRel);
        }
        status.state = state;
    }

    fn frame_captured(&self, paused: bool) {
        let mut status = self.write();
        status.frames_captured = status.frames_captured.saturating_add(1);
        let state = if paused {
            AgentState::Paused
        } else {
            AgentState::Capturing
        };
        if state == AgentState::Capturing && status.state != AgentState::Capturing {
            self.capturing_generation.fetch_add(1, Ordering::AcqRel);
        }
        status.state = state;
    }

    fn artifact_saved(&self, path: &Path) {
        let mut status = self.write();
        status.frames_saved = status.frames_saved.saturating_add(1);
        status.last_artifact = Some(path.to_string_lossy().into_owned());
    }

    fn fault(&self, error: &anyhow::Error) {
        self.report_fault(format!("{error:#}"));
    }
}

pub fn list_cameras(sdk: &Arc<Sdk>, as_json: bool) -> Result<()> {
    let cameras = sdk.cameras()?;
    if as_json {
        let value = cameras.iter().map(camera_json).collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if cameras.is_empty() {
        println!("No ZWO ASI cameras found.");
    }
    for camera in cameras {
        println!(
            "{}: {} — {}x{}, {}-bit, {:?}, {:.2} µm pixels, USB3={}",
            camera.camera_id,
            camera.name,
            camera.max_width,
            camera.max_height,
            camera.bit_depth,
            camera.bayer_pattern,
            camera.pixel_size_um,
            camera.is_usb3_camera
        );
    }
    Ok(())
}

pub fn probe_camera(sdk: &Arc<Sdk>, camera_id: Option<i32>) -> Result<()> {
    let info = select_camera(sdk, camera_id)?;
    println!("Opening {} (id {})", info.name, info.camera_id);
    let camera = sdk.open(info)?;
    println!("Current ROI: {:?}", camera.roi()?);
    for caps in camera.controls()? {
        match camera.control_value(caps.control_type) {
            Ok(value) => println!(
                "{:>2} {:<24} value={:<10} auto={:<5} range={}..={} default={} writable={} auto_supported={}",
                caps.control_type.0,
                caps.name,
                value.value,
                value.automatic,
                caps.min_value,
                caps.max_value,
                caps.default_value,
                caps.writable,
                caps.auto_supported
            ),
            Err(error) => println!(
                "{:>2} {:<24} unavailable: {}",
                caps.control_type.0, caps.name, error
            ),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn snapshot(
    sdk: &Arc<Sdk>,
    camera_id: Option<i32>,
    output: &Path,
    settle_frames: u32,
    max_exposure_us: i64,
    max_gain: i64,
    target_brightness: i64,
    jpeg_quality: u8,
) -> Result<()> {
    if !(1..=100).contains(&jpeg_quality) {
        bail!("JPEG quality must be between 1 and 100");
    }
    let info = select_camera(sdk, camera_id)?;
    let bayer = core_bayer(info.bayer_pattern)?;
    let mut camera = sdk.open(info.clone())?;
    let controls = camera.controls()?;
    let auto_limits = configure_sdk_auto(
        &mut camera,
        &controls,
        max_exposure_us,
        max_gain,
        target_brightness,
    )?;

    camera.set_roi(Roi {
        width: info.max_width,
        height: info.max_height,
        bin: 1,
        image_type: ImageType::Raw8,
    })?;
    camera.start_video()?;
    let mut frame_data = Vec::new();
    let frame = wait_for_auto_settle(
        &mut camera,
        settle_frames,
        auto_limits,
        &mut frame_data,
        None,
    )?
    .context("snapshot was cancelled while automatic exposure was settling")?;
    camera.stop_video()?;
    let rgb = match frame.image_type {
        ImageType::Raw8 => demosaic_bilinear(&frame_data, frame.width, frame.height, bayer)?,
        ImageType::Rgb24 => frame_data,
        ImageType::Y8 => frame_data.iter().flat_map(|value| [*value; 3]).collect(),
        other => bail!("snapshot output does not yet support {other:?}"),
    };
    let stats = luma_stats(&rgb, 64)?;
    save_rgb(output, frame.width, frame.height, &rgb, jpeg_quality)?;
    println!(
        "Saved {}x{} image to {} (mean {:.1}, p50 {}, p90 {}, clipped {:.2}%)",
        frame.width,
        frame.height,
        output.display(),
        stats.mean,
        stats.p50,
        stats.p90,
        stats.clipped_fraction * 100.0
    );
    Ok(())
}

#[derive(Debug)]
struct CaptureJob {
    sequence: u64,
    width: u32,
    height: u32,
    bayer: BayerPattern,
    data: Vec<u8>,
    output: PathBuf,
    jpeg_quality: u8,
}

#[derive(Clone, Copy, Debug)]
struct AutoLimits {
    min_exposure_us: i64,
    max_exposure_us: i64,
    min_gain: i64,
    max_gain: i64,
    target_brightness: i64,
}

pub fn run_agent(sdk: &Arc<Sdk>, config_path: &Path, max_frames: Option<u64>) -> Result<()> {
    let control = AgentControl::new();
    let handler_control = control.clone();
    ctrlc::set_handler(move || handler_control.shutdown()).context("installing Ctrl-C handler")?;
    run_agent_with_control(sdk, config_path, max_frames, &control)
}

/// Run the camera-owning worker with controls supplied by a tray or host.
///
/// Unlike [`run_agent`], this function does not install a process-wide Ctrl-C
/// handler, so GUI hosts can own their shutdown policy and event loop.
pub fn run_agent_with_control(
    sdk: &Arc<Sdk>,
    config_path: &Path,
    max_frames: Option<u64>,
    control: &AgentControl,
) -> Result<()> {
    run_agent_with_monitor(sdk, config_path, max_frames, control, &AgentMonitor::new())
}

/// Run the worker while publishing snapshots for the tray and local clients.
pub fn run_agent_with_monitor(
    sdk: &Arc<Sdk>,
    config_path: &Path,
    max_frames: Option<u64>,
    control: &AgentControl,
    monitor: &AgentMonitor,
) -> Result<()> {
    monitor.begin_attempt();
    let result = run_agent_inner(sdk, config_path, max_frames, control, monitor);
    publish_attempt_result(&result, control, monitor);
    result
}

fn publish_attempt_result(result: &Result<()>, control: &AgentControl, monitor: &AgentMonitor) {
    match result {
        Ok(()) if control.is_shutdown() => monitor.mark_stopping(),
        Ok(()) => monitor.set_state(AgentState::Idle),
        Err(error) => monitor.fault(error),
    }
}

fn run_agent_inner(
    sdk: &Arc<Sdk>,
    config_path: &Path,
    max_frames: Option<u64>,
    control: &AgentControl,
    monitor: &AgentMonitor,
) -> Result<()> {
    if max_frames == Some(0) {
        bail!("--max-frames must be greater than zero");
    }
    let config = Config::load(config_path)?;
    let info = select_configured_camera(sdk, &config.camera)?;
    monitor.set_camera(&info);
    let bayer = core_bayer(info.bayer_pattern)?;
    let mut camera = sdk.open(info.clone())?;
    let controls = camera.controls()?;
    let auto_limits = configure_sdk_auto(
        &mut camera,
        &controls,
        config.camera.max_exposure_us,
        config.camera.max_gain,
        config.camera.target_brightness,
    )?;

    if !info.supported_bins.contains(&config.camera.bin) {
        bail!(
            "camera {} does not support bin {}; supported bins are {:?}",
            info.name,
            config.camera.bin,
            info.supported_bins
        );
    }
    if !info.supported_formats.contains(&ImageType::Raw8) {
        bail!("camera {} does not support RAW8 video", info.name);
    }
    let bin = u32::try_from(config.camera.bin).context("camera bin must be positive")?;
    let roi = Roi {
        width: config.camera.width.unwrap_or(info.max_width / bin),
        height: config.camera.height.unwrap_or(info.max_height / bin),
        bin: config.camera.bin,
        image_type: ImageType::Raw8,
    };
    camera.set_roi(roi)?;

    let capture_directory = if config.capture.directory.is_absolute() {
        config.capture.directory.clone()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&config.capture.directory)
    };
    std::fs::create_dir_all(&capture_directory)
        .with_context(|| format!("creating capture directory {}", capture_directory.display()))?;

    let (writer_tx, writer_rx) = sync_channel::<CaptureJob>(config.capture.writer_queue_capacity);
    let writer_monitor = monitor.clone();
    let writer = thread::Builder::new()
        .name("autopiercam-writer".to_owned())
        .spawn(move || writer_loop(writer_rx, &writer_monitor))
        .context("starting still writer")?;

    info!(
        camera = %info.name,
        width = roi.width,
        height = roi.height,
        bin = roi.bin,
        directory = %capture_directory.display(),
        "continuous capture worker started"
    );
    let capture_result = capture_loop(
        &mut camera,
        bayer,
        auto_limits,
        &config,
        &capture_directory,
        max_frames,
        control,
        monitor,
        &writer_tx,
    );
    monitor.set_state(AgentState::Stopping);
    drop(writer_tx);
    let writer_result = writer
        .join()
        .map_err(|_| anyhow!("still-writer thread panicked"))?;
    capture_result?;
    writer_result?;
    info!("continuous capture worker stopped cleanly");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    camera: &mut Camera,
    bayer: BayerPattern,
    auto_limits: AutoLimits,
    config: &Config,
    capture_directory: &Path,
    max_frames: Option<u64>,
    control: &AgentControl,
    monitor: &AgentMonitor,
    writer: &SyncSender<CaptureJob>,
) -> Result<()> {
    camera.start_video()?;
    let result = (|| {
        let mut frame_buffer = Vec::new();
        if wait_for_auto_settle(
            camera,
            config.camera.settle_frames,
            auto_limits,
            &mut frame_buffer,
            Some(control),
        )?
        .is_none()
        {
            return Ok(());
        }
        // Each AgentControl belongs to one camera attempt, so generation zero
        // preserves requests made during startup/auto-exposure settling.
        let mut seen_capture_generation = 0;
        monitor.set_state(if control.is_paused() {
            AgentState::Paused
        } else {
            AgentState::Capturing
        });
        let interval = Duration::from_millis(config.capture.interval_ms);
        let mut next_capture = Instant::now();
        let mut queued = 0_u64;

        while !control.is_shutdown() {
            let exposure_us = camera
                .control_value(ControlType::EXPOSURE)
                .map(|value| value.value)
                .unwrap_or(config.camera.max_exposure_us);
            let timeout_ms = (exposure_us / 1_000 + 500).clamp(500, 2_000) as i32;
            let meta = match camera.next_video_frame_into(&mut frame_buffer, timeout_ms) {
                Ok(meta) => meta,
                Err(error) if error.is_timeout() => continue,
                Err(error) => return Err(error.into()),
            };
            if control.is_shutdown() {
                break;
            }
            monitor.frame_captured(control.is_paused());

            let now = Instant::now();
            let capture_generation = control.capture_generation();
            let capture_requested = capture_generation != seen_capture_generation;
            if capture_requested {
                seen_capture_generation = seen_capture_generation.wrapping_add(1);
            }
            let periodic_capture_due = !control.is_paused() && now >= next_capture;
            if !capture_requested && !periodic_capture_due {
                continue;
            }
            let output = capture_directory.join(capture_filename(queued));
            let job = CaptureJob {
                sequence: queued,
                width: meta.width,
                height: meta.height,
                bayer,
                data: frame_buffer.clone(),
                output,
                jpeg_quality: config.capture.jpeg_quality,
            };
            match writer.try_send(job) {
                Ok(()) => {
                    queued += 1;
                    info!(sequence = queued, exposure_us, "queued still frame");
                }
                Err(TrySendError::Full(_)) => {
                    warn!("still writer is full; dropping scheduled frame");
                }
                Err(TrySendError::Disconnected(_)) => {
                    bail!("still writer stopped unexpectedly");
                }
            }
            if periodic_capture_due {
                next_capture = now + interval;
            }
            if max_frames.is_some_and(|limit| queued >= limit) {
                break;
            }
        }
        Ok(())
    })();
    let stop_result = camera.stop_video();
    result.and(stop_result.map_err(Into::into))
}

fn wait_for_auto_settle(
    camera: &mut Camera,
    minimum_frames: u32,
    limits: AutoLimits,
    frame_buffer: &mut Vec<u8>,
    control: Option<&AgentControl>,
) -> Result<Option<FrameMeta>> {
    let max_exposure_us = u64::try_from(limits.max_exposure_us.max(1)).unwrap_or(u64::MAX / 8);
    let overall_limit = Duration::from_micros(max_exposure_us.saturating_mul(4))
        .saturating_add(Duration::from_secs(1))
        .max(Duration::from_secs(5));
    let started = Instant::now();
    let mut previous: Option<(i64, i64, u8)> = None;
    let mut stable_samples = 0_u32;
    let mut received = 0_u32;

    loop {
        if control.is_some_and(AgentControl::is_shutdown) {
            return Ok(None);
        }
        let wait_exposure = camera
            .control_value(ControlType::EXPOSURE)
            .map(|value| value.value)
            .unwrap_or(max_exposure_us as i64);
        let timeout_ms = (wait_exposure / 1_000 + 500).clamp(500, 2_000) as i32;
        let meta = match camera.next_video_frame_into(frame_buffer, timeout_ms) {
            Ok(meta) => meta,
            Err(error) if error.is_timeout() && started.elapsed() < overall_limit => {
                if control.is_some_and(AgentControl::is_shutdown) {
                    return Ok(None);
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if control.is_some_and(AgentControl::is_shutdown) {
            return Ok(None);
        }
        received = received.saturating_add(1);
        // SDK telemetry is asynchronous, so read it after the successful frame
        // and use it only as a convergence signal, not frame-exact metadata.
        let exposure = camera
            .control_value(ControlType::EXPOSURE)
            .map(|value| value.value)
            .unwrap_or(wait_exposure);
        let gain = camera
            .control_value(ControlType::GAIN)
            .map(|value| value.value)
            .unwrap_or(limits.min_gain);
        let stats = raw8_stats(frame_buffer, 64)?;
        let exposure_tolerance = (exposure.unsigned_abs() / 20).max(32);
        let stable = previous.is_some_and(|(old_exposure, old_gain, old_p90)| {
            old_exposure.abs_diff(exposure) <= exposure_tolerance
                && old_gain.abs_diff(gain) <= 3
                && old_p90.abs_diff(stats.p90) <= 3
        });
        stable_samples = if stable {
            stable_samples.saturating_add(1)
        } else {
            0
        };
        previous = Some((exposure, gain, stats.p90));

        let dynamic_minimum = Duration::from_micros(
            exposure
                .unsigned_abs()
                .saturating_mul(2)
                .saturating_add(100_000),
        )
        .max(Duration::from_secs(5));
        let dark_threshold =
            u8::try_from((limits.target_brightness / 4).clamp(8, 64)).unwrap_or(32);
        let luma_acceptable = stats.p90 >= dark_threshold && stats.clipped_fraction <= 0.05;
        let at_dark_limit = exposure >= limits.max_exposure_us.saturating_mul(95) / 100
            && gain >= limits.max_gain.saturating_sub(3);
        let at_bright_limit = exposure
            <= limits
                .min_exposure_us
                .saturating_add((limits.min_exposure_us / 20).max(32))
            && gain <= limits.min_gain.saturating_add(3);
        if received >= minimum_frames.max(2)
            && stable_samples >= 3
            && started.elapsed() >= dynamic_minimum
            && (luma_acceptable || at_dark_limit || at_bright_limit)
        {
            if luma_acceptable {
                info!(
                    received,
                    exposure_us = exposure,
                    gain,
                    p90 = stats.p90,
                    elapsed_ms = started.elapsed().as_millis(),
                    "automatic exposure settled"
                );
            } else {
                warn!(
                    received,
                    exposure_us = exposure,
                    gain,
                    p90 = stats.p90,
                    at_dark_limit,
                    at_bright_limit,
                    elapsed_ms = started.elapsed().as_millis(),
                    "automatic exposure settled at a control limit"
                );
            }
            return Ok(Some(meta));
        }
        if started.elapsed() >= overall_limit {
            warn!(
                received,
                exposure_us = exposure,
                gain,
                p90 = stats.p90,
                elapsed_ms = started.elapsed().as_millis(),
                "automatic exposure reached its settling deadline"
            );
            return Ok(Some(meta));
        }
    }
}

fn writer_loop(receiver: Receiver<CaptureJob>, monitor: &AgentMonitor) -> Result<()> {
    for job in receiver {
        let rgb = demosaic_bilinear(&job.data, job.width, job.height, job.bayer)
            .with_context(|| format!("debayering frame {}", job.sequence))?;
        let stats = luma_stats(&rgb, 64)?;
        save_rgb(&job.output, job.width, job.height, &rgb, job.jpeg_quality)?;
        monitor.artifact_saved(&job.output);
        info!(
            sequence = job.sequence,
            path = %job.output.display(),
            mean = stats.mean,
            p50 = stats.p50,
            p90 = stats.p90,
            clipped_percent = stats.clipped_fraction * 100.0,
            "saved still frame"
        );
    }
    Ok(())
}

fn capture_filename(sequence: u64) -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    format!(
        "frame-{}-{:03}-{sequence:06}.jpg",
        elapsed.as_secs(),
        elapsed.subsec_millis()
    )
}

fn select_camera(sdk: &Arc<Sdk>, camera_id: Option<i32>) -> Result<CameraInfo> {
    let cameras = sdk.cameras()?;
    if let Some(camera_id) = camera_id {
        return cameras
            .into_iter()
            .find(|camera| camera.camera_id == camera_id)
            .ok_or_else(|| anyhow!("camera id {camera_id} is not connected"));
    }
    match cameras.len() {
        0 => bail!("no ZWO ASI camera is connected"),
        1 => Ok(cameras.into_iter().next().expect("length checked")),
        count => bail!("{count} cameras are connected; select one with --camera-id"),
    }
}

fn select_configured_camera(sdk: &Arc<Sdk>, config: &CameraConfig) -> Result<CameraInfo> {
    let mut cameras = sdk.cameras()?.into_iter().filter(|camera| {
        config
            .camera_id
            .is_none_or(|camera_id| camera.camera_id == camera_id)
            && config.name_contains.as_ref().is_none_or(|needle| {
                camera
                    .name
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            })
    });
    let selected = cameras
        .next()
        .ok_or_else(|| anyhow!("no connected camera matches the configuration"))?;
    if cameras.next().is_some() {
        bail!("more than one connected camera matches the configuration");
    }
    Ok(selected)
}

fn configure_sdk_auto(
    camera: &mut Camera,
    controls: &[ControlCaps],
    max_exposure_us: i64,
    max_gain: i64,
    target_brightness: i64,
) -> Result<AutoLimits> {
    let exposure_caps =
        control(controls, ControlType::EXPOSURE).context("camera has no exposure control")?;
    let gain_caps = control(controls, ControlType::GAIN).context("camera has no gain control")?;
    if !exposure_caps.writable || !exposure_caps.auto_supported {
        bail!("camera exposure control does not support automatic video mode");
    }
    if !gain_caps.writable || !gain_caps.auto_supported {
        bail!("camera gain control does not support automatic video mode");
    }

    // SDK documentation calls control 11 microseconds, while current cameras
    // expose AutoExpMaxExpMS. Honor the runtime capability name.
    let auto_max_exposure_caps = control(controls, ControlType::AUTO_MAX_EXPOSURE);
    let auto_max_exposure = auto_max_exposure_caps
        .map(|caps| {
            auto_exposure_limit_value(caps, max_exposure_us).clamp(caps.min_value, caps.max_value)
        })
        .unwrap_or(max_exposure_us);
    set_if_available(
        camera,
        controls,
        ControlType::AUTO_MAX_EXPOSURE,
        auto_max_exposure,
        false,
    )?;
    let effective_max_exposure_us = auto_max_exposure_caps
        .map(|caps| auto_exposure_limit_us(caps, auto_max_exposure))
        .unwrap_or(max_exposure_us)
        .clamp(exposure_caps.min_value, exposure_caps.max_value);

    let effective_max_gain = control(controls, ControlType::AUTO_MAX_GAIN)
        .map(|caps| max_gain.clamp(caps.min_value, caps.max_value))
        .unwrap_or(max_gain)
        .clamp(gain_caps.min_value, gain_caps.max_value);
    set_if_available(
        camera,
        controls,
        ControlType::AUTO_MAX_GAIN,
        effective_max_gain,
        false,
    )?;
    let effective_target = control(controls, ControlType::AUTO_TARGET_BRIGHTNESS)
        .map(|caps| target_brightness.clamp(caps.min_value, caps.max_value))
        .unwrap_or(target_brightness);
    set_if_available(
        camera,
        controls,
        ControlType::AUTO_TARGET_BRIGHTNESS,
        effective_target,
        false,
    )?;
    set_if_available(camera, controls, ControlType::FLIP, 0, false)?;
    let exposure = camera
        .control_value(ControlType::EXPOSURE)
        .map(|value| value.value)
        .ok()
        .or_else(|| control(controls, ControlType::EXPOSURE).map(|caps| caps.default_value))
        .context("camera has no exposure control")?;
    set_if_available(camera, controls, ControlType::EXPOSURE, exposure, true)?;
    let gain = camera
        .control_value(ControlType::GAIN)
        .map(|value| value.value)
        .ok()
        .or_else(|| control(controls, ControlType::GAIN).map(|caps| caps.default_value))
        .context("camera has no gain control")?;
    set_if_available(camera, controls, ControlType::GAIN, gain, true)?;
    Ok(AutoLimits {
        min_exposure_us: exposure_caps.min_value,
        max_exposure_us: effective_max_exposure_us,
        min_gain: gain_caps.min_value,
        max_gain: effective_max_gain,
        target_brightness: effective_target,
    })
}

fn set_if_available(
    camera: &mut Camera,
    controls: &[ControlCaps],
    control_type: ControlType,
    requested: i64,
    automatic: bool,
) -> Result<()> {
    let Some(caps) = control(controls, control_type) else {
        return Ok(());
    };
    if !caps.writable || (automatic && !caps.auto_supported) {
        return Ok(());
    }
    let value = requested.clamp(caps.min_value, caps.max_value);
    camera
        .set_control(control_type, value, automatic)
        .with_context(|| format!("setting camera control {}", caps.name))
}

fn control(controls: &[ControlCaps], control_type: ControlType) -> Option<&ControlCaps> {
    controls
        .iter()
        .find(|caps| caps.control_type == control_type)
}

fn auto_exposure_limit_value(caps: &ControlCaps, exposure_us: i64) -> i64 {
    if caps.name.to_ascii_lowercase().contains("ms") {
        exposure_us.saturating_add(999) / 1_000
    } else {
        exposure_us
    }
}

fn auto_exposure_limit_us(caps: &ControlCaps, sdk_value: i64) -> i64 {
    if caps.name.to_ascii_lowercase().contains("ms") {
        sdk_value.saturating_mul(1_000)
    } else {
        sdk_value
    }
}

fn core_bayer(pattern: AsiBayerPattern) -> Result<BayerPattern> {
    match pattern {
        AsiBayerPattern::Rg => Ok(BayerPattern::Rg),
        AsiBayerPattern::Bg => Ok(BayerPattern::Bg),
        AsiBayerPattern::Gr => Ok(BayerPattern::Gr),
        AsiBayerPattern::Gb => Ok(BayerPattern::Gb),
        AsiBayerPattern::Unknown(value) => bail!("unknown Bayer pattern {value}"),
    }
}

fn save_rgb(path: &Path, width: u32, height: u32, rgb: &[u8], quality: u8) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "jpg" | "jpeg" | "png") {
        bail!("output extension must be .jpg, .jpeg, or .png");
    }

    let temporary = temporary_artifact_path(path)?;
    let write_result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating temporary image {}", temporary.display()))?;
        let mut writer = BufWriter::new(file);
        match extension.as_str() {
            "jpg" | "jpeg" => {
                JpegEncoder::new_with_quality(&mut writer, quality)
                    .write_image(rgb, width, height, ColorType::Rgb8.into())
                    .context("encoding JPEG")?;
            }
            "png" => {
                PngEncoder::new(&mut writer)
                    .write_image(rgb, width, height, ColorType::Rgb8.into())
                    .context("encoding PNG")?;
            }
            _ => unreachable!("extension validated above"),
        }
        writer
            .flush()
            .with_context(|| format!("flushing temporary image {}", temporary.display()))?;
        writer
            .get_ref()
            .sync_all()
            .with_context(|| format!("syncing temporary image {}", temporary.display()))?;
        drop(writer);
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    // A same-directory hard link publishes the fully synced artifact atomically
    // and fails rather than replacing an existing final path.
    if let Err(error) = std::fs::hard_link(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("finalizing image without overwrite {}", path.display()));
    }
    std::fs::remove_file(&temporary)
        .with_context(|| format!("removing temporary image {}", temporary.display()))?;
    Ok(())
}

fn temporary_artifact_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("output path must have a UTF-8 file name")?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{file_name}.{}.{}.partial",
        std::process::id(),
        sequence
    )))
}

fn camera_json(camera: &CameraInfo) -> serde_json::Value {
    json!({
        "camera_id": camera.camera_id,
        "name": camera.name,
        "max_width": camera.max_width,
        "max_height": camera.max_height,
        "is_color": camera.is_color,
        "bayer_pattern": format!("{:?}", camera.bayer_pattern),
        "supported_bins": camera.supported_bins,
        "supported_formats": camera.supported_formats.iter().map(|value| format!("{value:?}")).collect::<Vec<_>>(),
        "pixel_size_um": camera.pixel_size_um,
        "bit_depth": camera.bit_depth,
        "is_usb3_camera": camera.is_usb3_camera,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_control_tracks_pause_capture_and_shutdown_requests() {
        let control = AgentControl::new();
        let worker_view = control.clone();

        assert!(!worker_view.is_paused());
        control.pause();
        assert!(worker_view.is_paused());
        control.resume();
        assert!(!worker_view.is_paused());

        let generation = worker_view.capture_generation();
        control.capture_now();
        assert_ne!(worker_view.capture_generation(), generation);

        assert!(!worker_view.is_shutdown());
        control.shutdown();
        assert!(worker_view.is_shutdown());
    }

    #[test]
    fn agent_monitor_publishes_cloneable_protocol_status() {
        let monitor = AgentMonitor::new();
        monitor.set_camera(&CameraInfo {
            camera_id: 7,
            name: "Test camera".to_owned(),
            max_width: 1,
            max_height: 1,
            is_color: true,
            bayer_pattern: AsiBayerPattern::Rg,
            supported_bins: vec![1],
            supported_formats: vec![ImageType::Raw8],
            pixel_size_um: 1.0,
            has_mechanical_shutter: false,
            has_st4_port: false,
            is_cooled: false,
            is_usb3_camera: true,
            bit_depth: 8,
            is_trigger_camera: false,
        });
        monitor.frame_captured(false);
        monitor.artifact_saved(Path::new("captures/test.jpg"));

        let status = monitor.snapshot();
        assert_eq!(status.state, AgentState::Capturing);
        assert_eq!(status.camera.expect("camera").id, 7);
        assert_eq!(status.frames_captured, 1);
        assert_eq!(status.frames_saved, 1);
        assert_eq!(status.last_artifact.as_deref(), Some("captures/test.jpg"));

        monitor.report_fault("camera disconnected");
        monitor.begin_attempt();
        let retry_status = monitor.snapshot();
        assert_eq!(retry_status.state, AgentState::Starting);
        assert!(retry_status.camera.is_none());
        assert!(retry_status.last_error.is_none());
        assert_eq!(retry_status.frames_captured, 1);
        assert_eq!(retry_status.frames_saved, 1);
        assert_eq!(
            retry_status.last_artifact.as_deref(),
            Some("captures/test.jpg")
        );
    }

    #[test]
    fn monitor_records_every_transition_into_capturing() {
        let monitor = AgentMonitor::new();
        assert_eq!(monitor.capturing_generation(), 0);

        monitor.set_state(AgentState::Capturing);
        assert_eq!(monitor.capturing_generation(), 1);
        monitor.frame_captured(false);
        assert_eq!(monitor.capturing_generation(), 1);
        monitor.frame_captured(true);
        assert_eq!(monitor.capturing_generation(), 1);
        monitor.frame_captured(false);
        assert_eq!(monitor.capturing_generation(), 2);
    }

    #[test]
    fn controlled_attempt_completion_remains_stopping() {
        let monitor = AgentMonitor::new();
        let control = AgentControl::new();
        monitor.set_state(AgentState::Capturing);
        control.shutdown();

        publish_attempt_result(&Ok(()), &control, &monitor);

        assert_eq!(monitor.snapshot().state, AgentState::Stopping);
    }

    #[test]
    fn artifact_publish_is_atomic_and_never_overwrites() {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "autopiercam-artifact-test-{}-{sequence}.jpg",
            std::process::id()
        ));
        let rgb = [10_u8, 20, 30];
        save_rgb(&path, 1, 1, &rgb, 80).unwrap();
        assert!(path.is_file());
        assert!(save_rgb(&path, 1, 1, &rgb, 80).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
