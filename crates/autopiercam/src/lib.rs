use anyhow::{Context, Result, anyhow, bail};
use autopiercam_asi::{
    BayerPattern as AsiBayerPattern, Camera, CameraInfo, ControlCaps, ControlType, FrameMeta,
    ImageType, Roi, Sdk,
};
use autopiercam_core::{
    config::ExposureControl,
    exposure::{AdaptiveExposure, ExposureSetting, LightMode},
};
use autopiercam_core::{
    config::{CameraConfig, Config, UploadConfig, normalize_upload_endpoint},
    image::{BayerPattern, demosaic_bilinear, luma_stats, raw8_stats},
};
use autopiercam_protocol::{
    AgentState, AgentStatus, CAPABILITY_EXPOSURE_PROGRESS, CAPABILITY_STORAGE_RETENTION,
    CAPABILITY_UPLOADS_LIST, CAPABILITY_UPLOADS_REQUEUE, StatusCamera, StatusExposure,
    StatusStorage, StatusUpload, StoragePressure, UploadListRequest, UploadListResponse,
    UploadRequeueRequest, UploadRequeueResult,
};
use image::{
    ColorType, ImageEncoder,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex, RwLock, RwLockWriteGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

#[cfg(test)]
mod auto_limits_tests;
mod exposure;
mod ledger_maintenance;
mod preview;
mod retention;
mod upload;
mod video;

use exposure::{FrameWait, Settling, WaitDecision, poll_timeout_ms};
use ledger_maintenance::LedgerLease;
pub use ledger_maintenance::{
    LedgerArchiveReport, LedgerMaintenanceError, LedgerMigrationReport, archive_upload_ledger,
    migrate_upload_ledger,
};
use preview::{PREVIEW_INTERVAL, PreviewEncoder, PreviewJob, PreviewSink};
pub use preview::{PreviewFrame, PreviewHub, PreviewSession, PreviewSnapshot};
use retention::{
    LocalOnlyRetentionAuthority, ProtectAllRetentionAuthority, RetentionAuthority,
    RetentionObserver, RetentionPolicy, RetentionPressure, RetentionSink, RetentionTelemetry,
    RetentionWakeResult, RetentionWorker,
};
pub use upload::UploadAdminError;
use upload::{
    BearerAuthorization, UploadAdmin, UploadEnqueueResult, UploadHealth, UploadObserver,
    UploadOptions, UploadSink, UploadTelemetry, UploadWorker, bearer_authorization,
    parse_generated_capture_filename,
};

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const CAPTURE_SESSION_NONCE_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureSessionNonce([u8; CAPTURE_SESSION_NONCE_BYTES]);

impl CaptureSessionNonce {
    fn random() -> Result<Self> {
        let mut bytes = [0_u8; CAPTURE_SESSION_NONCE_BYTES];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow!("generating capture-session filename nonce: {error}"))?;
        Ok(Self(bytes))
    }

    fn hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut value = String::with_capacity(CAPTURE_SESSION_NONCE_BYTES * 2);
        for byte in self.0 {
            value.push(char::from(HEX[usize::from(byte >> 4)]));
            value.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        value
    }
}

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
    upload_admin: Arc<RwLock<Option<RegisteredUploadAdmin>>>,
    upload_admin_generation: Arc<AtomicU64>,
}

#[derive(Clone, Debug)]
struct RegisteredUploadAdmin {
    generation: u64,
    session: Arc<UploadAdminSession>,
}

struct UploadAdminSession {
    admin: UploadAdmin,
    state: Mutex<UploadAdminSessionState>,
    idle: Condvar,
}

#[derive(Debug)]
struct UploadAdminSessionState {
    accepting: bool,
    active: usize,
}

impl std::fmt::Debug for UploadAdminSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        formatter
            .debug_struct("UploadAdminSession")
            .field("admin", &self.admin)
            .field("accepting", &state.accepting)
            .field("active", &state.active)
            .finish()
    }
}

impl UploadAdminSession {
    fn new(admin: UploadAdmin) -> Self {
        Self {
            admin,
            state: Mutex::new(UploadAdminSessionState {
                accepting: true,
                active: 0,
            }),
            idle: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Option<UploadAdminLease> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !state.accepting {
            return None;
        }
        state.active = state
            .active
            .checked_add(1)
            .expect("upload administration operation counter overflowed");
        drop(state);
        Some(UploadAdminLease {
            session: Arc::clone(self),
        })
    }

    fn revoke(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.accepting = false;
    }

    fn wait_until_idle(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        while state.active != 0 {
            state = match self.idle.wait(state) {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }
}

#[derive(Debug)]
struct UploadAdminLease {
    session: Arc<UploadAdminSession>,
}

impl UploadAdminLease {
    fn admin(&self) -> &UploadAdmin {
        &self.session.admin
    }
}

impl Drop for UploadAdminLease {
    fn drop(&mut self) {
        let mut state = match self.session.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active = state
            .active
            .checked_sub(1)
            .expect("upload administration operation counter underflowed");
        if state.active == 0 {
            self.session.idle.notify_all();
        }
    }
}

#[derive(Debug)]
struct UploadAdminRegistration {
    registry: Arc<RwLock<Option<RegisteredUploadAdmin>>>,
    generation: u64,
    session: Arc<UploadAdminSession>,
}

impl Drop for UploadAdminRegistration {
    fn drop(&mut self) {
        let should_drain = {
            let mut registration = match self.registry.write() {
                Ok(registration) => registration,
                Err(poisoned) => poisoned.into_inner(),
            };
            if registration
                .as_ref()
                .is_some_and(|current| current.generation == self.generation)
            {
                self.session.revoke();
                *registration = None;
                true
            } else {
                false
            }
        };
        if should_drain {
            self.session.wait_until_idle();
        }
    }
}

impl Default for AgentMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentMonitor {
    pub fn new() -> Self {
        let mut status = AgentStatus::new(AgentState::Starting);
        status.capabilities = vec![
            CAPABILITY_UPLOADS_LIST.to_owned(),
            CAPABILITY_UPLOADS_REQUEUE.to_owned(),
            CAPABILITY_STORAGE_RETENTION.to_owned(),
            CAPABILITY_EXPOSURE_PROGRESS.to_owned(),
            "camera.adaptive_exposure".to_owned(),
            "camera.raw16".to_owned(),
            "video.ffmpeg".to_owned(),
        ];
        Self {
            inner: Arc::new(RwLock::new(status)),
            capturing_generation: Arc::new(AtomicU64::new(0)),
            upload_admin: Arc::new(RwLock::new(None)),
            upload_admin_generation: Arc::new(AtomicU64::new(0)),
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
        status.exposure = None;
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

    /// List one revision-stable page from the currently owned durable upload
    /// ledger. The operation is unavailable while upload is disabled or the
    /// supervised camera/upload attempt is restarting.
    pub fn list_uploads(
        &self,
        request: &UploadListRequest,
    ) -> Result<UploadListResponse, UploadAdminError> {
        self.current_upload_admin()?.admin().list(request)
    }

    /// Requeue one revision-fenced terminal failure after verifying its exact
    /// recorded artifact and current delivery binding.
    pub fn requeue_upload(
        &self,
        request: &UploadRequeueRequest,
    ) -> Result<UploadRequeueResult, UploadAdminError> {
        self.current_upload_admin()?.admin().requeue(request)
    }

    fn current_upload_admin(&self) -> Result<UploadAdminLease, UploadAdminError> {
        let registration = match self.upload_admin.read() {
            Ok(registration) => registration,
            Err(poisoned) => poisoned.into_inner(),
        };
        registration
            .as_ref()
            .and_then(|registration| registration.session.acquire())
            .ok_or(UploadAdminError::ServiceUnavailable)
    }

    fn register_upload_admin(&self, admin: UploadAdmin) -> UploadAdminRegistration {
        let generation = self
            .upload_admin_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let session = Arc::new(UploadAdminSession::new(admin));
        let previous = {
            let mut registration = match self.upload_admin.write() {
                Ok(registration) => registration,
                Err(poisoned) => poisoned.into_inner(),
            };
            let previous = registration.take();
            if let Some(previous) = &previous {
                previous.session.revoke();
            }
            *registration = Some(RegisteredUploadAdmin {
                generation,
                session: Arc::clone(&session),
            });
            previous
        };
        if let Some(previous) = previous {
            previous.session.wait_until_idle();
        }
        UploadAdminRegistration {
            registry: Arc::clone(&self.upload_admin),
            generation,
            session,
        }
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
        status.upload = None;
        status.storage = None;
        status.exposure = None;
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
        if matches!(
            status.state,
            AgentState::Idle | AgentState::Stopping | AgentState::Faulted
        ) {
            status.exposure = None;
        }
    }

    fn exposure_progress(&self, exposure: StatusExposure) {
        self.write().exposure = Some(exposure);
    }

    fn settling_frame_captured(&self) {
        let mut status = self.write();
        status.frames_captured = status.frames_captured.saturating_add(1);
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

    fn upload_telemetry(&self, telemetry: UploadTelemetry) {
        self.write().upload = Some(StatusUpload {
            pending: telemetry.pending,
            active: telemetry.active,
            retrying: telemetry.retrying,
            completed: telemetry.completed,
            permanently_failed: telemetry.permanently_failed,
            last_success_unix_ms: telemetry.last_success_unix_ms,
            last_failure_unix_ms: telemetry.last_failure_unix_ms,
            last_error: telemetry.last_error,
        });
    }

    fn retention_telemetry(&self, telemetry: RetentionTelemetry) {
        self.write().storage = Some(StatusStorage {
            managed_bytes: telemetry.managed_bytes,
            protected_bytes: telemetry.protected_bytes,
            reclaimable_bytes: telemetry.reclaimable_bytes,
            free_bytes: telemetry.free_bytes,
            last_sweep_unix_ms: Some(telemetry.swept_at_unix_ms),
            last_reclaimed_files: telemetry.reclaimed_file_count,
            last_reclaimed_bytes: telemetry.reclaimed_bytes,
            pressure: match telemetry.pressure {
                RetentionPressure::Ok => StoragePressure::Ok,
                RetentionPressure::CleanupNeeded => StoragePressure::CleanupNeeded,
                RetentionPressure::Blocked => StoragePressure::Blocked,
            },
            capture_suspended: telemetry.blocked_pressure,
            last_error: telemetry.error,
        });
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
    let mut progress = CaptureProgress::new(&camera, auto_limits, 0, settle_frames);
    let mut observer = CaptureObserver::new(bayer, None, None);
    let frame = wait_for_auto_settle(
        &mut camera,
        settle_frames,
        auto_limits,
        None,
        &mut progress,
        &mut observer,
    )?
    .context("snapshot was cancelled while automatic exposure was settling")?;
    camera.stop_video()?;
    let meta = frame.meta;
    let rgb = match meta.image_type {
        ImageType::Raw8 => demosaic_bilinear(&frame.data, meta.width, meta.height, bayer)?,
        ImageType::Rgb24 => frame.data,
        ImageType::Y8 => frame.data.iter().flat_map(|value| [*value; 3]).collect(),
        other => bail!("snapshot output does not yet support {other:?}"),
    };
    let stats = luma_stats(&rgb, 64)?;
    save_rgb(output, meta.width, meta.height, &rgb, jpeg_quality)?;
    println!(
        "Saved {}x{} image to {} (mean {:.1}, p50 {}, p90 {}, clipped {:.2}%)",
        meta.width,
        meta.height,
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
    raw16: bool,
}

#[derive(Clone, Copy, Debug)]
struct AutoLimits {
    min_exposure_us: i64,
    max_exposure_us: i64,
    min_gain: i64,
    max_gain: i64,
    target_brightness: i64,
}

/// A successful SDK sample owns its bytes so a subsequent timed-out read can
/// never replace the image while leaving its old dimensions and telemetry.
#[derive(Debug)]
struct CompletedFrame {
    meta: FrameMeta,
    data: Vec<u8>,
    captured_at_unix_ms: u64,
    exposure_us: i64,
    gain: i64,
}

fn frame_stats(meta: FrameMeta, data: &[u8]) -> Result<autopiercam_core::image::LumaStats> {
    Ok(if meta.image_type == ImageType::Raw16 {
        autopiercam_core::image::raw16_stats(data, 64)?
    } else {
        raw8_stats(data, 64)?
    })
}

struct CaptureProgress {
    started: Instant,
    wait: FrameWait,
    status: StatusExposure,
}

impl CaptureProgress {
    fn new(
        camera: &Camera,
        limits: AutoLimits,
        session_generation: u64,
        minimum_frames: u32,
    ) -> Self {
        let exposure_us = current_exposure(camera, limits.max_exposure_us);
        let gain = current_gain(camera, limits.min_gain);
        Self {
            started: Instant::now(),
            wait: FrameWait::new(exposure_us),
            status: StatusExposure {
                session_generation,
                settling: true,
                exposure_us,
                gain,
                max_exposure_us: limits.max_exposure_us,
                settling_frames: 0,
                settling_min_frames: minimum_frames.max(4),
                wait_elapsed_ms: 0,
                frame_timeout_ms: 0,
            },
        }
    }

    fn refresh(&mut self, camera: &Camera, limits: AutoLimits) {
        self.status.exposure_us = current_exposure(camera, limits.max_exposure_us);
        self.status.gain = current_gain(camera, self.status.gain);
        self.wait.observe_exposure(self.status.exposure_us);
    }

    fn publish(&self, observer: &CaptureObserver<'_>) {
        if let Some(monitor) = observer.monitor {
            let mut status = self.status.clone();
            status.wait_elapsed_ms = duration_millis(self.wait.elapsed(self.started.elapsed()));
            status.frame_timeout_ms = duration_millis(self.wait.timeout());
            monitor.exposure_progress(status);
        }
    }

    fn completed_frame(&mut self, meta: FrameMeta, data: &mut Vec<u8>) -> CompletedFrame {
        self.wait.frame_received(self.started.elapsed());
        CompletedFrame {
            meta,
            data: std::mem::take(data),
            captured_at_unix_ms: unix_time_millis(),
            exposure_us: self.status.exposure_us,
            gain: self.status.gain,
        }
    }
}

fn current_exposure(camera: &Camera, fallback_us: i64) -> i64 {
    camera
        .control_value(ControlType::EXPOSURE)
        .map(|value| value.value)
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(fallback_us.max(1))
}

fn current_gain(camera: &Camera, fallback: i64) -> i64 {
    camera
        .control_value(ControlType::GAIN)
        .map(|value| value.value)
        .unwrap_or(fallback)
}

fn duration_millis(value: Duration) -> u64 {
    value.as_millis().try_into().unwrap_or(u64::MAX)
}

struct CaptureObserver<'a> {
    bayer: BayerPattern,
    monitor: Option<&'a AgentMonitor>,
    preview: Option<&'a PreviewSink>,
    next_preview: Instant,
    adaptive: Option<AdaptiveExposure>,
    video: Option<&'a video::VideoWorker>,
}

impl<'a> CaptureObserver<'a> {
    fn new(
        bayer: BayerPattern,
        monitor: Option<&'a AgentMonitor>,
        preview: Option<&'a PreviewSink>,
    ) -> Self {
        Self {
            bayer,
            monitor,
            preview,
            next_preview: Instant::now(),
            adaptive: None,
            video: None,
        }
    }

    fn adapt(&mut self, camera: &mut Camera, frame: &CompletedFrame) -> Result<bool> {
        let Some(controller) = &mut self.adaptive else {
            return Ok(false);
        };
        let current = ExposureSetting {
            exposure_us: frame.exposure_us,
            gain: frame.gain,
        };
        let next = controller.observe(current, frame_stats(frame.meta, &frame.data)?);
        if next == current {
            return Ok(false);
        }
        // Discard the SDK pipeline before changing manual controls, so the next
        // sample cannot drive feedback using a queued image with old settings.
        camera.stop_video()?;
        camera.set_control(ControlType::EXPOSURE, next.exposure_us, false)?;
        camera.set_control(ControlType::GAIN, next.gain, false)?;
        camera.start_video()?;
        info!(exposure_us = next.exposure_us, gain = next.gain, mode = ?controller.mode(), "adaptive exposure updated");
        Ok(true)
    }

    fn frame_received(&mut self, frame: &CompletedFrame, settling: bool, paused: bool) {
        if let Some(monitor) = self.monitor {
            if settling {
                monitor.settling_frame_captured();
            } else {
                monitor.frame_captured(paused);
            }
        }
        let now = Instant::now();
        if let Some(preview) = self.preview
            && now >= self.next_preview
        {
            let _ = preview.try_publish(|dropped_frames| PreviewJob {
                width: frame.meta.width,
                height: frame.meta.height,
                bayer: self.bayer,
                data: frame.data.clone(),
                raw16: frame.meta.image_type == ImageType::Raw16,
                captured_at_unix_ms: frame.captured_at_unix_ms,
                exposure_us: frame.exposure_us,
                gain: frame.gain,
                mode: match self.adaptive.as_ref().map(AdaptiveExposure::mode) {
                    None => autopiercam_protocol::PreviewMode::Unknown,
                    Some(LightMode::Day) => autopiercam_protocol::PreviewMode::Day,
                    Some(LightMode::Night) => autopiercam_protocol::PreviewMode::Night,
                },
                dropped_frames,
            });
            self.next_preview = now + PREVIEW_INTERVAL;
        }
    }
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
    run_agent_with_optional_preview(sdk, config_path, max_frames, control, monitor, None)
}

/// Run the worker while publishing bounded, latest-only preview frames.
pub fn run_agent_with_monitor_and_preview(
    sdk: &Arc<Sdk>,
    config_path: &Path,
    max_frames: Option<u64>,
    control: &AgentControl,
    monitor: &AgentMonitor,
    preview: &PreviewSession,
) -> Result<()> {
    run_agent_with_optional_preview(
        sdk,
        config_path,
        max_frames,
        control,
        monitor,
        Some(preview),
    )
}

fn run_agent_with_optional_preview(
    sdk: &Arc<Sdk>,
    config_path: &Path,
    max_frames: Option<u64>,
    control: &AgentControl,
    monitor: &AgentMonitor,
    preview: Option<&PreviewSession>,
) -> Result<()> {
    monitor.begin_attempt();
    let result = run_agent_inner(sdk, config_path, max_frames, control, monitor, preview);
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
    preview: Option<&PreviewSession>,
) -> Result<()> {
    if max_frames == Some(0) {
        bail!("--max-frames must be greater than zero");
    }
    let config_path = std::path::absolute(config_path)
        .with_context(|| format!("resolving configuration path {}", config_path.display()))?;
    let config = Config::load(&config_path)?;
    // Headless recording uses the same bounded preview encoder as the tray.
    let video_preview =
        (config.video.enabled && preview.is_none()).then(|| PreviewHub::new().begin_session());
    let preview = preview.or(video_preview.as_ref());
    let configured_capture_directory = if config.capture.directory.is_absolute() {
        config.capture.directory.clone()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&config.capture.directory)
    };
    let capture_directory =
        std::path::absolute(&configured_capture_directory).with_context(|| {
            format!(
                "resolving capture directory {}",
                configured_capture_directory.display()
            )
        })?;
    std::fs::create_dir_all(&capture_directory)
        .with_context(|| format!("creating capture directory {}", capture_directory.display()))?;
    let capture_session_nonce = CaptureSessionNonce::random()?;

    let upload_ledger_path = config_path.with_extension("upload.sqlite3");
    // UploadStore owns this shared lifecycle lease while uploads are enabled.
    // Disabled runs still publish captures and may delete them via retention,
    // so they must independently exclude offline migration/archive for the
    // complete writer/retention lifetime.
    let disabled_upload_ledger_lease =
        acquire_disabled_upload_ledger_lease(&config.upload, &upload_ledger_path)?;
    let (upload_worker, upload_sink) = match start_upload_worker(
        &config.upload,
        &upload_ledger_path,
        &capture_directory,
        monitor,
    )? {
        Some((worker, sink)) => (Some(worker), Some(sink)),
        None => (None, None),
    };
    let upload_admin_registration = upload_sink
        .as_ref()
        .map(|sink| monitor.register_upload_admin(sink.admin()));
    let (retention_worker, retention_sink) = match start_retention_worker(
        &config,
        &upload_ledger_path,
        &capture_directory,
        upload_sink.as_ref(),
        monitor,
    )? {
        Some((worker, sink)) => (Some(worker), Some(sink)),
        None => (None, None),
    };
    let info = select_configured_camera(sdk, &config.camera)?;
    if !info.is_color {
        bail!(
            "{} is not a color camera; AutoPierCam requires a color ASI camera",
            info.name
        );
    }
    monitor.set_camera(&info);
    let bayer = core_bayer(info.bayer_pattern)?;
    let mut camera = sdk.open(info.clone())?;
    let controls = camera.controls()?;
    let auto_limits = if config.camera.exposure_control == ExposureControl::Adaptive {
        configure_adaptive(&mut camera, &controls, &config.camera)?
    } else {
        configure_sdk_auto(
            &mut camera,
            &controls,
            config.camera.max_exposure_us,
            config.camera.max_gain,
            config.camera.target_brightness,
        )?
    };
    CaptureProgress::new(
        &camera,
        auto_limits,
        preview.map(PreviewSession::generation).unwrap_or(0),
        config.camera.settle_frames,
    )
    .publish(&CaptureObserver::new(bayer, Some(monitor), None));

    if !info.supported_bins.contains(&config.camera.bin) {
        bail!(
            "camera {} does not support bin {}; supported bins are {:?}",
            info.name,
            config.camera.bin,
            info.supported_bins
        );
    }
    let image_type = if config.camera.raw16 {
        ImageType::Raw16
    } else {
        ImageType::Raw8
    };
    if !info.is_color {
        bail!(
            "{} is not a color camera; AutoPierCam's current debayer pipeline requires a color ASI camera",
            info.name
        );
    }
    if !info.supported_formats.contains(&image_type) {
        bail!(
            "camera {} does not support {:?} video",
            info.name,
            image_type
        );
    }
    let bin = u32::try_from(config.camera.bin).context("camera bin must be positive")?;
    let roi = Roi {
        width: config.camera.width.unwrap_or(info.max_width / bin),
        height: config.camera.height.unwrap_or(info.max_height / bin),
        bin: config.camera.bin,
        image_type,
    };
    camera.set_roi(roi)?;

    // Start every remaining fallible helper before the writer. Once the writer
    // is running, execution always reaches the explicit join sequence below,
    // keeping the ledger lifecycle lease held until publication has drained.
    let preview_encoder = preview.cloned().map(PreviewEncoder::start).transpose()?;
    let preview_sink = preview_encoder.as_ref().map(PreviewEncoder::sink);
    let video_worker = if config.video.enabled {
        Some(video::VideoWorker::start(
            &config.video,
            &capture_directory,
            preview.context("video preview session missing")?.clone(),
            capture_session_nonce,
            upload_sink.clone(),
            retention_sink.clone(),
            control.clone(),
        )?)
    } else {
        None
    };
    let (writer_tx, writer_rx) = sync_channel::<CaptureJob>(config.capture.writer_queue_capacity);
    let upload_health = upload_sink.as_ref().map(UploadSink::health);
    let writer_retention_sink = retention_sink.clone();
    let writer_ledger_lease = disabled_upload_ledger_lease.clone();
    let writer_monitor = monitor.clone();
    let writer = thread::Builder::new()
        .name("autopiercam-writer".to_owned())
        .spawn(move || {
            let result = writer_loop(
                writer_rx,
                &writer_monitor,
                upload_sink.as_ref(),
                writer_retention_sink.as_ref(),
            );
            // Also fence publication if an unwinding camera thread detaches
            // this writer before the normal join sequence can run.
            drop(writer_ledger_lease);
            result
        })
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
        upload_health.as_ref(),
        retention_sink.as_ref(),
        preview_sink.as_ref(),
        preview.map(PreviewSession::generation).unwrap_or(0),
        capture_session_nonce,
        video_worker.as_ref(),
    );
    monitor.set_state(AgentState::Stopping);
    drop(writer_tx);
    let writer_result = writer
        .join()
        .map_err(|_| anyhow!("still-writer thread panicked"));
    let preview_result = preview_encoder
        .map(PreviewEncoder::stop_and_join)
        .unwrap_or(Ok(()));
    let video_result = video_worker
        .map(video::VideoWorker::stop_and_join)
        .unwrap_or(Ok(()));
    let retention_result = retention_worker
        .map(RetentionWorker::stop_and_join)
        .unwrap_or(Ok(()));
    drop(upload_admin_registration);
    let upload_result = upload_worker
        .map(UploadWorker::stop_and_join)
        .unwrap_or(Ok(()));
    drop(disabled_upload_ledger_lease);
    video_result.context("recording video")?;
    capture_result?;
    writer_result??;
    retention_result.context("stopping capture retention worker")?;
    upload_result.context("stopping HTTP upload worker")?;
    preview_result?;
    info!("continuous capture worker stopped cleanly");
    Ok(())
}

fn acquire_disabled_upload_ledger_lease(
    config: &UploadConfig,
    database_path: &Path,
) -> Result<Option<Arc<LedgerLease>>> {
    if config.enabled {
        return Ok(None);
    }

    LedgerLease::acquire_live(database_path)
        .map(Arc::new)
        .map(Some)
        .with_context(|| {
            format!(
                "acquiring upload-ledger lifecycle lease for disabled uploads at {}",
                database_path.display()
            )
        })
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
    upload_health: Option<&UploadHealth>,
    retention: Option<&RetentionSink>,
    preview: Option<&PreviewSink>,
    preview_session_generation: u64,
    capture_session_nonce: CaptureSessionNonce,
    video: Option<&video::VideoWorker>,
) -> Result<()> {
    camera.start_video()?;
    let result = (|| {
        let mut progress = CaptureProgress::new(
            camera,
            auto_limits,
            preview_session_generation,
            config.camera.settle_frames,
        );
        let mut observer = CaptureObserver::new(bayer, Some(monitor), preview);
        observer.video = video;
        if config.camera.exposure_control == ExposureControl::Adaptive {
            observer.adaptive = Some(AdaptiveExposure::new(
                auto_limits.min_exposure_us,
                auto_limits.max_exposure_us,
                auto_limits.min_gain,
                auto_limits.max_gain,
                auto_limits.target_brightness as u8,
            ));
        }
        progress.publish(&observer);
        let Some(settling_frame) = wait_for_auto_settle(
            camera,
            config.camera.settle_frames,
            auto_limits,
            Some(control),
            &mut progress,
            &mut observer,
        )?
        else {
            return Ok(());
        };
        progress.status.settling = false;
        progress.publish(&observer);
        // Settling already observed and published this frame. Reuse its owned
        // sample for the first still without another exposure or double count.
        let mut pending_frame = Some(settling_frame);
        let mut frame_buffer = Vec::new();
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
            if video.is_some_and(video::VideoWorker::is_finished) {
                bail!("video worker stopped unexpectedly");
            }
            if upload_health.is_some_and(UploadHealth::is_stopped) {
                bail!("durable upload worker stopped unexpectedly");
            }
            if retention.is_some_and(RetentionSink::is_stopped) {
                bail!("capture retention worker stopped unexpectedly");
            }
            let frame = if let Some(frame) = pending_frame.take() {
                frame
            } else {
                progress.refresh(camera, auto_limits);
                progress.publish(&observer);
                if progress.wait.expired(progress.started.elapsed()) {
                    bail!(
                        "camera produced no frame within its exposure deadline ({} ms)",
                        duration_millis(progress.wait.timeout())
                    );
                }
                let result = camera.next_video_frame_into(
                    &mut frame_buffer,
                    poll_timeout_ms(progress.status.exposure_us),
                );
                if control.is_shutdown() {
                    break;
                }
                progress.refresh(camera, auto_limits);
                progress.publish(&observer);
                let meta = match result {
                    Ok(meta) => meta,
                    Err(error) if error.is_timeout() => {
                        if progress.wait.expired(progress.started.elapsed()) {
                            bail!(
                                "camera produced no frame within its exposure deadline ({} ms)",
                                duration_millis(progress.wait.timeout())
                            );
                        }
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                let frame = progress.completed_frame(meta, &mut frame_buffer);
                observer.adapt(camera, &frame)?;
                progress.refresh(camera, auto_limits);
                observer.frame_received(&frame, false, control.is_paused());
                progress.publish(&observer);
                frame
            };
            let meta = frame.meta;
            let exposure_us = frame.exposure_us;
            frame_buffer = frame.data;
            let now = Instant::now();
            let capture_generation = control.capture_generation();
            let capture_requested = capture_generation != seen_capture_generation;
            if capture_requested {
                seen_capture_generation = seen_capture_generation.wrapping_add(1);
            }
            let periodic_capture_due = !control.is_paused()
                && !retention.is_some_and(RetentionSink::capture_suspended)
                && now >= next_capture;
            if !capture_requested && !periodic_capture_due {
                continue;
            }
            let mut output =
                capture_directory.join(capture_filename(capture_session_nonce, queued));
            if config.camera.raw16 {
                output.set_extension("png");
            }
            let job = CaptureJob {
                sequence: queued,
                width: meta.width,
                height: meta.height,
                bayer,
                data: frame_buffer.clone(),
                output,
                jpeg_quality: config.capture.jpeg_quality,
                raw16: config.camera.raw16,
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
    control: Option<&AgentControl>,
    progress: &mut CaptureProgress,
    observer: &mut CaptureObserver<'_>,
) -> Result<Option<CompletedFrame>> {
    let mut settling = Settling::new(minimum_frames, limits);
    progress.status.settling_min_frames = settling.minimum_frames();
    let mut frame_buffer = Vec::new();
    let mut latest: Option<CompletedFrame> = None;
    loop {
        if observer.video.is_some_and(video::VideoWorker::is_finished) {
            bail!("video worker stopped while exposure was settling");
        }
        progress.refresh(camera, limits);
        progress.publish(observer);
        match settling.decision(
            &progress.wait,
            progress.started.elapsed(),
            control.is_some_and(AgentControl::is_shutdown),
        ) {
            WaitDecision::Cancelled => return Ok(None),
            WaitDecision::Stalled => bail!(
                "camera produced no frame while automatic exposure was settling within its exposure deadline ({} ms; {} frames received)",
                duration_millis(progress.wait.timeout()),
                settling.received()
            ),
            WaitDecision::UseLatestFrame => {
                warn!(
                    received = settling.received(),
                    elapsed_ms = duration_millis(progress.started.elapsed()),
                    "automatic exposure reached its settling deadline; using latest complete frame"
                );
                return Ok(latest);
            }
            WaitDecision::Continue => {}
        }
        let result = camera.next_video_frame_into(
            &mut frame_buffer,
            poll_timeout_ms(progress.status.exposure_us),
        );
        if control.is_some_and(AgentControl::is_shutdown) {
            return Ok(None);
        }
        progress.refresh(camera, limits);
        progress.publish(observer);
        let meta = match result {
            Ok(meta) => meta,
            // Deadline handling at the top of the loop also applies when the
            // final SDK read times out. `latest` is never the SDK scratch buffer.
            Err(error) if error.is_timeout() => continue,
            Err(error) => return Err(error.into()),
        };
        // The SDK readback is asynchronous. It is a convergence/progress signal,
        // not an assertion that exposure/gain are exact for these sensor bytes.
        let stats = frame_stats(meta, &frame_buffer)?;
        let frame = progress.completed_frame(meta, &mut frame_buffer);
        let settled = settling.observe_frame(
            progress.started.elapsed(),
            frame.exposure_us,
            frame.gain,
            stats.p90,
            stats.clipped_fraction,
        );
        progress.status.settling_frames = settling.received();
        let adjusted = observer.adapt(camera, &frame)?;
        progress.refresh(camera, limits);
        observer.frame_received(&frame, true, control.is_some_and(AgentControl::is_paused));
        progress.publish(observer);
        if settled && !adjusted {
            info!(
                received = settling.received(),
                exposure_us = frame.exposure_us,
                gain = frame.gain,
                p90 = stats.p90,
                elapsed_ms = duration_millis(progress.started.elapsed()),
                "automatic exposure settled"
            );
            return Ok(Some(frame));
        }
        if let Some(previous) = latest.replace(frame) {
            frame_buffer = previous.data;
        }
    }
}

fn start_upload_worker(
    config: &UploadConfig,
    database_path: &Path,
    capture_directory: &Path,
    monitor: &AgentMonitor,
) -> Result<Option<(UploadWorker, UploadSink)>> {
    if !config.enabled {
        return Ok(None);
    }

    let endpoint = config
        .endpoint
        .as_deref()
        .context("upload endpoint is missing despite validated configuration")?;
    let endpoint = normalize_upload_endpoint(endpoint)
        .context("normalizing validated upload endpoint")?
        .parse::<ureq::http::Uri>()
        .context("parsing validated upload endpoint")?;
    let authorization = config
        .bearer_token_env
        .as_deref()
        .map(load_bearer_authorization)
        .transpose()?;
    let upload_monitor = monitor.clone();
    let observer: UploadObserver = Arc::new(move |telemetry| {
        upload_monitor.upload_telemetry(telemetry);
    });
    UploadWorker::start(
        UploadOptions::new(endpoint, authorization, config.queue_capacity),
        database_path,
        capture_directory,
        observer,
    )
    .map(Some)
    .with_context(|| {
        format!(
            "starting durable HTTP upload worker with ledger {}",
            database_path.display()
        )
    })
}

fn start_retention_worker(
    config: &Config,
    upload_ledger_path: &Path,
    capture_directory: &Path,
    upload_sink: Option<&UploadSink>,
    monitor: &AgentMonitor,
) -> Result<Option<(RetentionWorker, RetentionSink)>> {
    let policy = RetentionPolicy::from_capture_config(&config.capture);
    if !policy.is_enabled() {
        return Ok(None);
    }

    let authority: Arc<dyn RetentionAuthority> = if let Some(upload_sink) = upload_sink {
        upload_sink.retention_authority()
    } else if upload_ledger_artifacts_exist(upload_ledger_path)? {
        warn!(
            ledger = %upload_ledger_path.display(),
            "uploads are disabled but a prior ledger exists; retention will protect all managed captures"
        );
        Arc::new(ProtectAllRetentionAuthority)
    } else {
        Arc::new(LocalOnlyRetentionAuthority)
    };
    let retention_monitor = monitor.clone();
    let observer: RetentionObserver = Arc::new(move |telemetry| {
        retention_monitor.retention_telemetry(telemetry);
    });
    RetentionWorker::start(
        policy,
        capture_directory.to_path_buf(),
        Arc::new(parse_generated_capture_filename),
        authority,
        observer,
    )
    .map(Some)
    .with_context(|| {
        format!(
            "starting capture retention worker for {}",
            capture_directory.display()
        )
    })
}

fn upload_ledger_artifacts_exist(database_path: &Path) -> Result<bool> {
    let mut candidates = vec![database_path.to_path_buf()];
    for suffix in ["-wal", "-shm", ".maintenance"] {
        let mut path = database_path.as_os_str().to_os_string();
        path.push(suffix);
        candidates.push(PathBuf::from(path));
    }
    for candidate in candidates {
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "checking for prior upload-ledger artifact {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    Ok(false)
}

fn load_bearer_authorization(variable: &str) -> Result<BearerAuthorization> {
    let token = match std::env::var(variable) {
        Ok(token) => token,
        Err(std::env::VarError::NotPresent) => {
            bail!("upload bearer-token environment variable {variable} is not set")
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("upload bearer-token environment variable {variable} is not valid Unicode")
        }
    };
    if token.is_empty() || token.trim() != token {
        bail!(
            "upload bearer-token environment variable {variable} is empty or has surrounding whitespace"
        );
    }
    bearer_authorization(&token).map_err(|_| {
        anyhow!(
            "upload bearer-token environment variable {variable} cannot be represented safely in an HTTP header"
        )
    })
}

fn writer_loop(
    receiver: Receiver<CaptureJob>,
    monitor: &AgentMonitor,
    upload: Option<&UploadSink>,
    retention: Option<&RetentionSink>,
) -> Result<()> {
    for job in receiver {
        let stats = if job.raw16 {
            let rgb = autopiercam_core::image::demosaic_bilinear16(
                &job.data, job.width, job.height, job.bayer,
            )?;
            let bytes: Vec<u8> = rgb.iter().flat_map(|value| value.to_ne_bytes()).collect();
            save_rgb_samples(
                &job.output,
                job.width,
                job.height,
                &bytes,
                job.jpeg_quality,
                ColorType::Rgb16,
            )?;
            autopiercam_core::image::raw16_stats(&job.data, 64)?
        } else {
            let rgb = demosaic_bilinear(&job.data, job.width, job.height, job.bayer)
                .with_context(|| format!("debayering frame {}", job.sequence))?;
            save_rgb(&job.output, job.width, job.height, &rgb, job.jpeg_quality)?;
            luma_stats(&rgb, 64)?
        };
        monitor.artifact_saved(&job.output);
        if let Some(upload) = upload {
            match upload.try_enqueue(job.output.clone()) {
                Ok(UploadEnqueueResult::Recorded | UploadEnqueueResult::AlreadyRecorded) => {}
                Ok(UploadEnqueueResult::WorkerStopped) => bail!(
                    "upload worker stopped after recording {} durably",
                    job.output.display()
                ),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "recording finalized artifact {} in the upload ledger",
                            job.output.display()
                        )
                    });
                }
            }
        }
        if let Some(retention) = retention
            && retention.try_wake() == RetentionWakeResult::Stopped
        {
            bail!(
                "capture retention worker stopped after finalizing {}",
                job.output.display()
            );
        }
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

fn capture_filename(session_nonce: CaptureSessionNonce, sequence: u64) -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    capture_filename_at(elapsed, session_nonce, sequence)
}

fn capture_filename_at(
    captured_at: Duration,
    session_nonce: CaptureSessionNonce,
    sequence: u64,
) -> String {
    format!(
        "frame-{}-{:03}-{}-{sequence:06}.jpg",
        captured_at.as_secs(),
        captured_at.subsec_millis(),
        session_nonce.hex()
    )
}

fn unix_time_millis() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
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

fn configure_adaptive(
    camera: &mut Camera,
    controls: &[ControlCaps],
    config: &CameraConfig,
) -> Result<AutoLimits> {
    let exposure = control(controls, ControlType::EXPOSURE)
        .filter(|c| c.writable)
        .context("camera needs a writable manual exposure control")?;
    let gain = control(controls, ControlType::GAIN)
        .filter(|c| c.writable)
        .context("camera needs a writable manual gain control")?;
    let max_exposure_us = config
        .max_exposure_us
        .clamp(exposure.min_value.max(1), exposure.max_value);
    let min_exposure_us = config
        .min_exposure_us
        .clamp(exposure.min_value.max(1), max_exposure_us);
    let max_gain = config.max_gain.clamp(gain.min_value, gain.max_value);
    camera.set_control(
        ControlType::EXPOSURE,
        100_000_i64.clamp(min_exposure_us, max_exposure_us),
        false,
    )?;
    camera.set_control(ControlType::GAIN, gain.min_value, false)?;
    set_if_available(camera, controls, ControlType::FLIP, 0, false)?;
    info!(
        min_exposure_us,
        max_exposure_us,
        max_gain,
        "configured application-controlled exposure within manual camera limits"
    );
    Ok(AutoLimits {
        min_exposure_us,
        max_exposure_us,
        min_gain: gain.min_value,
        max_gain,
        target_brightness: config.target_brightness,
    })
}

fn configure_sdk_auto(
    camera: &mut Camera,
    controls: &[ControlCaps],
    max_exposure_us: i64,
    max_gain: i64,
    target_brightness: i64,
) -> Result<AutoLimits> {
    if max_exposure_us <= 0 || max_gain < 0 {
        bail!("maximum exposure must be positive and maximum gain non-negative");
    }
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
    let auto_max_exposure_caps = control(controls, ControlType::AUTO_MAX_EXPOSURE)
        .filter(|caps| caps.writable)
        .context("camera has no writable automatic exposure ceiling; bounded SDK auto mode is unavailable")?;
    let auto_max_exposure = auto_exposure_limit_value(
        auto_max_exposure_caps,
        max_exposure_us.clamp(exposure_caps.min_value, exposure_caps.max_value),
    )
    .clamp(
        auto_max_exposure_caps.min_value,
        auto_max_exposure_caps.max_value,
    );
    set_if_available(
        camera,
        controls,
        ControlType::AUTO_MAX_EXPOSURE,
        auto_max_exposure,
        false,
    )?;
    let readback = camera
        .control_value(ControlType::AUTO_MAX_EXPOSURE)
        .context("reading back the SDK automatic exposure ceiling")?;
    if !(auto_max_exposure_caps.min_value..=auto_max_exposure_caps.max_value)
        .contains(&readback.value)
    {
        bail!("SDK returned an automatic exposure ceiling outside its advertised limits");
    }
    let effective_max_exposure_us = auto_exposure_limit_us(auto_max_exposure_caps, readback.value)
        .clamp(exposure_caps.min_value, exposure_caps.max_value);
    if effective_max_exposure_us <= 0 {
        bail!("SDK returned a non-positive automatic exposure ceiling");
    }
    if effective_max_exposure_us != max_exposure_us {
        warn!(
            requested_max_exposure_us = max_exposure_us,
            effective_max_exposure_us, "camera adjusted the requested automatic exposure ceiling"
        );
    }
    info!(
        requested_max_exposure_us = max_exposure_us,
        effective_max_exposure_us,
        sdk_auto_max_exposure_us =
            auto_exposure_limit_us(auto_max_exposure_caps, auto_max_exposure_caps.max_value),
        "configured automatic exposure limits"
    );

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
    // A camera can retain an exposure from a previous application/session.
    // Seed auto mode within the newly confirmed ceiling, including on restart
    // after the operator lowers the maximum for daylight.
    let exposure = exposure.clamp(exposure_caps.min_value, effective_max_exposure_us);
    set_if_available(camera, controls, ControlType::EXPOSURE, exposure, true)?;
    let gain = camera
        .control_value(ControlType::GAIN)
        .map(|value| value.value)
        .ok()
        .or_else(|| control(controls, ControlType::GAIN).map(|caps| caps.default_value))
        .context("camera has no gain control")?;
    let gain = gain.clamp(gain_caps.min_value, effective_max_gain);
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
    save_rgb_samples(path, width, height, rgb, quality, ColorType::Rgb8)
}

fn save_rgb_samples(
    path: &Path,
    width: u32,
    height: u32,
    rgb: &[u8],
    quality: u8,
    color: ColorType,
) -> Result<()> {
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
                    .write_image(rgb, width, height, color.into())
                    .context("encoding JPEG")?;
            }
            "png" => {
                PngEncoder::new(&mut writer)
                    .write_image(rgb, width, height, color.into())
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

    // Publish the fully synced artifact atomically and fail rather than
    // replacing an existing final path. Windows uses a write-through move;
    // other platforms flush both directory-entry transitions explicitly.
    if let Err(error) = publish_temporary_artifact(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("finalizing image without overwrite {}", path.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn publish_temporary_artifact(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // MOVEFILE_REPLACE_EXISTING is deliberately omitted.
    let moved = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn publish_temporary_artifact(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::hard_link(temporary, destination)?;
    sync_parent_directory(destination)?;
    std::fs::remove_file(temporary)?;
    sync_parent_directory(destination)
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut options = OpenOptions::new();
    options.read(true);
    let directory = options.open(parent)?;
    directory.sync_all()
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
    fn raw16_png_roundtrip_preserves_low_sensor_bits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("precision.png");
        let samples: Vec<u16> = (0..48).map(|n| 12_345 + n * 17).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|v| v.to_ne_bytes()).collect();
        save_rgb_samples(&path, 4, 4, &bytes, 88, ColorType::Rgb16).unwrap();
        let decoded = image::open(&path).unwrap();
        assert_eq!(decoded.color(), ColorType::Rgb16);
        assert_eq!(decoded.to_rgb16().into_raw(), samples);
        assert!(save_rgb_samples(&path, 4, 4, &bytes, 88, ColorType::Rgb16).is_err());
    }

    fn test_exposure_progress(session_generation: u64) -> CaptureProgress {
        CaptureProgress {
            started: Instant::now(),
            wait: FrameWait::new(60_000_000),
            status: StatusExposure {
                session_generation,
                settling: true,
                exposure_us: 60_000_000,
                gain: 400,
                max_exposure_us: 60_000_000,
                settling_frames: 0,
                settling_min_frames: 6,
                wait_elapsed_ms: 0,
                frame_timeout_ms: 0,
            },
        }
    }

    #[test]
    fn timeout_fallback_retains_matching_complete_bytes_and_metadata() {
        let mut progress = test_exposure_progress(7);
        progress.started = Instant::now() - Duration::from_secs(600);
        let mut scratch = vec![1, 2, 3, 4];
        let meta = FrameMeta {
            width: 2,
            height: 2,
            image_type: ImageType::Raw8,
        };
        let frame = progress.completed_frame(meta, &mut scratch);
        let mut settling = Settling::new(
            6,
            AutoLimits {
                min_exposure_us: 32,
                max_exposure_us: 60_000_000,
                min_gain: 0,
                max_gain: 400,
                target_brightness: 100,
            },
        );
        settling.observe_frame(
            Duration::from_secs(600),
            frame.exposure_us,
            frame.gain,
            4,
            0.0,
        );
        // The final SDK read times out after touching its destination buffer.
        scratch.resize(16, 255);
        assert_eq!(
            settling.decision(&progress.wait, Duration::from_secs(605), false),
            WaitDecision::UseLatestFrame
        );
        assert_eq!(frame.meta, meta);
        assert_eq!(frame.data, [1, 2, 3, 4]);
        assert_eq!(frame.exposure_us, 60_000_000);
        assert_eq!(frame.gain, 400);
        assert!(frame.captured_at_unix_ms > 0);
    }

    #[test]
    fn settling_counts_frames_without_entering_capturing() {
        let monitor = AgentMonitor::new();
        let hub = PreviewHub::new();
        let session = hub.begin_session();
        let mut observer = CaptureObserver::new(BayerPattern::Rg, Some(&monitor), None);
        let mut progress = test_exposure_progress(session.generation());
        let mut raw = vec![100; 16];
        let frame = progress.completed_frame(
            FrameMeta {
                width: 4,
                height: 4,
                image_type: ImageType::Raw8,
            },
            &mut raw,
        );
        progress.status.settling_frames = 1;
        observer.frame_received(&frame, true, false);
        progress.publish(&observer);
        let status = monitor.snapshot();
        assert_eq!(status.state, AgentState::Starting);
        assert_eq!(status.frames_captured, 1);
        assert_eq!(status.frames_saved, 0);
        assert_eq!(monitor.capturing_generation(), 0);
        let exposure = status.exposure.unwrap();
        assert!(exposure.settling);
        assert_eq!(exposure.settling_frames, 1);
        assert_eq!(exposure.session_generation, session.generation());
        assert_eq!(exposure.frame_timeout_ms, 125_000);

        progress.status.settling = false;
        progress.publish(&observer);
        monitor.set_state(AgentState::Capturing);
        assert_eq!(monitor.snapshot().frames_captured, 1);
        assert_eq!(monitor.capturing_generation(), 1);
    }

    #[test]
    fn exposure_progress_clears_on_stop_fault_and_restart() {
        let monitor = AgentMonitor::new();
        let observer = CaptureObserver::new(BayerPattern::Rg, Some(&monitor), None);
        let progress = test_exposure_progress(42);
        for state in [AgentState::Stopping, AgentState::Idle, AgentState::Faulted] {
            progress.publish(&observer);
            assert!(monitor.snapshot().exposure.is_some());
            monitor.set_state(state);
            assert!(monitor.snapshot().exposure.is_none());
        }
        progress.publish(&observer);
        monitor.report_fault("camera disconnected");
        assert!(monitor.snapshot().exposure.is_none());
        progress.publish(&observer);
        monitor.begin_attempt();
        assert!(monitor.snapshot().exposure.is_none());
    }

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
        monitor.upload_telemetry(UploadTelemetry {
            pending: 2,
            active: 1,
            retrying: 3,
            completed: 4,
            permanently_failed: 5,
            last_success_unix_ms: Some(10),
            last_failure_unix_ms: Some(20),
            last_error: Some("HTTP endpoint requested a retry".to_owned()),
        });
        monitor.retention_telemetry(RetentionTelemetry {
            swept_at_unix_ms: 30,
            managed_bytes: 1_000,
            protected_bytes: 600,
            reclaimable_bytes: 400,
            free_bytes: Some(2_000),
            reclaimed_file_count: 2,
            reclaimed_bytes: 300,
            blocked_pressure: true,
            pressure: RetentionPressure::Blocked,
            error: Some("protected uploads prevent cleanup".to_owned()),
        });

        let status = monitor.snapshot();
        assert_eq!(status.state, AgentState::Capturing);
        assert_eq!(status.camera.expect("camera").id, 7);
        assert_eq!(status.frames_captured, 1);
        assert_eq!(status.frames_saved, 1);
        assert_eq!(status.last_artifact.as_deref(), Some("captures/test.jpg"));
        assert_eq!(
            status.capabilities,
            [
                CAPABILITY_UPLOADS_LIST.to_owned(),
                CAPABILITY_UPLOADS_REQUEUE.to_owned(),
                CAPABILITY_STORAGE_RETENTION.to_owned(),
                CAPABILITY_EXPOSURE_PROGRESS.to_owned(),
                "camera.adaptive_exposure".to_owned(),
                "camera.raw16".to_owned(),
                "video.ffmpeg".to_owned()
            ]
        );
        let upload = status.upload.expect("upload telemetry");
        assert_eq!(upload.pending, 2);
        assert_eq!(upload.active, 1);
        assert_eq!(upload.retrying, 3);
        assert_eq!(upload.completed, 4);
        assert_eq!(upload.permanently_failed, 5);
        assert_eq!(upload.last_success_unix_ms, Some(10));
        assert_eq!(upload.last_failure_unix_ms, Some(20));
        assert_eq!(
            upload.last_error.as_deref(),
            Some("HTTP endpoint requested a retry")
        );
        let storage = status.storage.expect("storage telemetry");
        assert_eq!(storage.managed_bytes, 1_000);
        assert_eq!(storage.protected_bytes, 600);
        assert_eq!(storage.reclaimable_bytes, 400);
        assert_eq!(storage.free_bytes, Some(2_000));
        assert_eq!(storage.last_sweep_unix_ms, Some(30));
        assert_eq!(storage.last_reclaimed_files, 2);
        assert_eq!(storage.last_reclaimed_bytes, 300);
        assert_eq!(storage.pressure, StoragePressure::Blocked);
        assert!(storage.capture_suspended);
        assert_eq!(
            storage.last_error.as_deref(),
            Some("protected uploads prevent cleanup")
        );

        monitor.report_fault("camera disconnected");
        monitor.begin_attempt();
        let retry_status = monitor.snapshot();
        assert_eq!(retry_status.state, AgentState::Starting);
        assert!(retry_status.camera.is_none());
        assert!(retry_status.last_error.is_none());
        assert!(retry_status.upload.is_none());
        assert!(retry_status.storage.is_none());
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
    fn stale_upload_registration_cannot_clear_its_replacement() {
        let root = tempfile::tempdir().unwrap();
        let capture = root.path().join("captures");
        std::fs::create_dir(&capture).unwrap();
        let database = root.path().join("upload.sqlite3");
        let monitor = AgentMonitor::new();
        let observer: UploadObserver = Arc::new(|_| {});
        let (worker, sink) = UploadWorker::start(
            UploadOptions::new(
                ureq::http::Uri::from_static("http://127.0.0.1:9/upload"),
                None,
                1,
            ),
            &database,
            &capture,
            observer,
        )
        .unwrap();

        let first = monitor.register_upload_admin(sink.admin());
        let replacement = monitor.register_upload_admin(sink.admin());
        drop(first);
        assert_eq!(
            monitor
                .list_uploads(&UploadListRequest::default())
                .unwrap()
                .jobs,
            []
        );

        drop(replacement);
        assert_eq!(
            monitor.list_uploads(&UploadListRequest::default()),
            Err(UploadAdminError::ServiceUnavailable)
        );
        worker.stop_and_join().unwrap();
    }

    #[test]
    fn upload_registration_drains_inflight_leases_before_ledger_reopen() {
        let root = tempfile::tempdir().unwrap();
        let capture = root.path().join("captures");
        std::fs::create_dir(&capture).unwrap();
        let database = root.path().join("upload.sqlite3");
        let monitor = AgentMonitor::new();
        let observer: UploadObserver = Arc::new(|_| {});
        let (worker, sink) = UploadWorker::start(
            UploadOptions::new(
                ureq::http::Uri::from_static("http://127.0.0.1:9/upload"),
                None,
                1,
            ),
            &database,
            &capture,
            Arc::clone(&observer),
        )
        .unwrap();
        let registration = monitor.register_upload_admin(sink.admin());
        let lease = monitor.current_upload_admin().unwrap();

        worker.stop_and_join().unwrap();
        drop(sink);
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();
        let drain = thread::spawn(move || {
            drop(registration);
            drained_tx.send(()).unwrap();
        });
        assert!(
            drained_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "registration teardown ignored an in-flight operation"
        );

        drop(lease);
        drained_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drain.join().unwrap();

        let (replacement, replacement_sink) = UploadWorker::start(
            UploadOptions::new(
                ureq::http::Uri::from_static("http://127.0.0.1:9/upload"),
                None,
                1,
            ),
            &database,
            &capture,
            observer,
        )
        .unwrap();
        replacement.stop_and_join().unwrap();
        drop(replacement_sink);
    }

    #[test]
    fn disabled_upload_run_lease_blocks_concurrent_offline_maintenance_until_release() {
        const CONTENDERS: usize = 8;

        let root = tempfile::tempdir().unwrap();
        let capture = root.path().join("captures");
        std::fs::create_dir(&capture).unwrap();
        let config_path = root.path().join("autopiercam.toml");
        std::fs::write(
            &config_path,
            "[capture]\ndirectory = \"captures\"\n\n[upload]\nenabled = false\n",
        )
        .unwrap();
        let database = config_path.with_extension("upload.sqlite3");
        let lease = acquire_disabled_upload_ledger_lease(&UploadConfig::default(), &database)
            .unwrap()
            .expect("disabled runs must own the live ledger lease");

        let barrier = Arc::new(std::sync::Barrier::new(CONTENDERS + 1));
        let contenders = (0..CONTENDERS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let config_path = config_path.clone();
                thread::spawn(move || {
                    barrier.wait();
                    match migrate_upload_ledger(&config_path) {
                        Err(LedgerMaintenanceError::Lease(message))
                            if message.contains("upload ledger is active") =>
                        {
                            Ok(())
                        }
                        result => Err(format!(
                            "offline maintenance was not excluded by the disabled run: {result:?}"
                        )),
                    }
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for contender in contenders {
            contender.join().unwrap().unwrap();
        }

        // Production releases this only after the still writer and retention
        // worker have both joined, so maintenance cannot inspect a capture set
        // while either publisher is still active.
        drop(lease);
        assert!(matches!(
            migrate_upload_ledger(&config_path),
            Err(LedgerMaintenanceError::MissingLedger(path)) if path == database
        ));
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
    fn capture_filename_formats_time_session_nonce_and_sequence_exactly() {
        let nonce = CaptureSessionNonce([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let captured_at = Duration::new(1_700_000_000, 123_000_000);
        assert_eq!(
            capture_filename_at(captured_at, nonce, 42),
            "frame-1700000000-123-00112233445566778899aabbccddeeff-000042.jpg"
        );
        assert_ne!(
            capture_filename_at(captured_at, nonce, 42),
            capture_filename_at(captured_at, CaptureSessionNonce([0xff; 16]), 42)
        );
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
