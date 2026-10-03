//! Synchronous capture-owner adapter to Regain's isolated camera workers.
//! Discovery and opt-in USB recovery require the camera owner to have released its handle.
use anyhow::{Context, Result, bail, ensure};
use autopiercam_core::config::{CameraDriver, WhiteBalanceConfig, WhiteBalanceMode};
use regain_core::{CancellationToken, Runtime, Worker};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BayerPattern {
    Rg,
    Bg,
    Gr,
    Gb,
    Unknown(i32),
}
impl From<i32> for BayerPattern {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Rg,
            1 => Self::Bg,
            2 => Self::Gr,
            3 => Self::Gb,
            v => Self::Unknown(v),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageType {
    Raw8,
    Rgb24,
    Raw16,
    Y8,
    Unknown(i32),
}
impl ImageType {
    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Raw8 | Self::Y8 => Some(1),
            Self::Raw16 => Some(2),
            Self::Rgb24 => Some(3),
            _ => None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct CameraInfo {
    /// Opaque Direct USB topology selector. Not persisted or exposed over IPC.
    pub locator: Option<String>,
    pub serial: Option<String>,
    pub discovery_error: Option<String>,
    pub name: String,
    pub camera_id: i32,
    pub max_width: u32,
    pub max_height: u32,
    pub is_color: bool,
    pub bayer_pattern: BayerPattern,
    pub supported_bins: Vec<i32>,
    pub supported_formats: Vec<ImageType>,
    pub pixel_size_um: f64,
    pub has_mechanical_shutter: bool,
    pub has_st4_port: bool,
    pub is_cooled: bool,
    pub is_usb3_camera: bool,
    pub bit_depth: i32,
    pub is_trigger_camera: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlType(pub i32);
impl ControlType {
    pub const GAIN: Self = Self(0);
    pub const EXPOSURE: Self = Self(1);
    pub const FLIP: Self = Self(9);
    pub const AUTO_MAX_GAIN: Self = Self(10);
    pub const AUTO_MAX_EXPOSURE: Self = Self(11);
    pub const AUTO_TARGET_BRIGHTNESS: Self = Self(12);
}
#[derive(Clone, Debug)]
pub struct ControlCaps {
    pub name: String,
    pub description: String,
    pub min_value: i64,
    pub max_value: i64,
    pub default_value: i64,
    pub auto_supported: bool,
    pub writable: bool,
    pub control_type: ControlType,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlValue {
    pub value: i64,
    pub automatic: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Roi {
    pub width: u32,
    pub height: u32,
    pub bin: i32,
    pub image_type: ImageType,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameMeta {
    pub width: u32,
    pub height: u32,
    pub image_type: ImageType,
}
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame is still exposing")]
    Pending,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}
impl FrameError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

fn diagnostic() -> regain_core::Diagnostic {
    Arc::new(|level, event, message| {
        tracing::info!(regain_level = level, regain_event = event, "{message}")
    })
}
fn executor() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn normalized_serial(serial: Option<String>) -> Option<String> {
    serial.and_then(|s| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_ascii_lowercase())
    })
}

pub struct Driver {
    runtime: Runtime,
    backend: CameraDriver,
    serial: Option<String>,
    // Held through worker teardown. Serializes list/open and rejects live discovery.
    owned: Mutex<bool>,
    recovery_target: Mutex<Option<String>>,
}
impl Driver {
    /// Test-only construction always passes --simulate; it cannot access hardware.
    #[cfg(feature = "simulator")]
    pub fn simulated(
        directory: PathBuf,
        backend: CameraDriver,
        settings: Value,
        serial: Option<String>,
    ) -> Self {
        Self {
            runtime: Runtime {
                directory,
                sdk: PathBuf::new(),
                simulate: true,
                sdk_simulation: Some(settings),
            },
            backend,
            serial: normalized_serial(serial),
            owned: Mutex::new(false),
            recovery_target: Mutex::new(None),
        }
    }
    pub fn new(sdk: Option<&Path>, backend: CameraDriver, serial: Option<String>) -> Result<Self> {
        let directory = std::env::current_exe()?
            .parent()
            .context("executable directory missing")?
            .to_owned();
        let directory = std::env::var_os("AUTOPIERCAM_REGAIN_DIRECTORY")
            .map(PathBuf::from)
            .unwrap_or(directory);
        let directory = std::fs::canonicalize(directory).context("Regain worker directory")?;
        ensure!(
            directory
                .join(format!("regain-device{}", std::env::consts::EXE_SUFFIX))
                .is_file(),
            "Regain camera worker is missing; install regain-device beside AutoPierCam"
        );
        let library = if cfg!(windows) {
            "ASICamera2.dll"
        } else if cfg!(target_os = "macos") {
            "libASICamera2.dylib"
        } else {
            "libASICamera2.so"
        };
        let sdk = sdk
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("AUTOPIERCAM_ASI_SDK_PATH").map(PathBuf::from))
            .unwrap_or_else(|| directory.join(library));
        // Resolve before the worker changes its working directory. Direct never loads this path.
        let sdk = if backend == CameraDriver::ZwoSdk {
            std::fs::canonicalize(sdk).context("Regain ZWO SDK library")?
        } else {
            sdk
        };
        Ok(Self {
            runtime: Runtime {
                directory,
                sdk,
                simulate: false,
                sdk_simulation: None,
            },
            backend,
            serial: normalized_serial(serial),
            owned: Mutex::new(false),
            recovery_target: Mutex::new(None),
        })
    }
    pub fn path(&self) -> &Path {
        &self.runtime.directory
    }
    pub fn selected_serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
    /// Called only by the capture owner before opening the selected camera.
    /// A missing/ambiguous binding must never turn into a guessed port reset.
    pub fn bind_usb_recovery(&self, info: &CameraInfo) -> Result<()> {
        let owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("camera ownership poisoned"))?;
        ensure!(!*owned, "Release the camera before binding USB recovery");
        *self
            .recovery_target
            .lock()
            .map_err(|_| anyhow::anyhow!("USB recovery state poisoned"))? = None;
        let serial = self
            .serial
            .as_deref()
            .context("USB recovery requires an explicit serial")?;
        ensure!(
            serial.len() == 16
                && serial.bytes().all(|c| c.is_ascii_hexdigit())
                && serial != "0000000000000000",
            "Invalid USB recovery serial"
        );
        let encoded = if self.runtime.simulate {
            "simulation".to_owned()
        } else {
            ensure!(
                cfg!(windows),
                "USB port recovery currently requires Windows"
            );
            let mut args = vec!["zwo", "camera-direct", "--usb-target", &info.name, serial];
            if let Some(locator) = &info.locator {
                args.push(locator);
            }
            let encoded = executor()?.block_on(self.runtime.usb_command(&args, 30))?;
            let target = regain_transport::usb::Target::decode(&encoded)?;
            ensure!(
                target.serial.eq_ignore_ascii_case(serial),
                "USB recovery serial mismatch"
            );
            encoded
        };
        *self
            .recovery_target
            .lock()
            .map_err(|_| anyhow::anyhow!("USB recovery state poisoned"))? = Some(encoded);
        Ok(())
    }

    pub fn has_usb_recovery_target(&self) -> bool {
        self.recovery_target
            .lock()
            .is_ok_and(|target| target.is_some())
    }

    /// Consume the binding once, only after camera teardown. The helper verifies
    /// the original device generation again immediately before cycling its port.
    pub fn reset_bound_usb(&self, cancelled: impl Fn() -> bool) -> Result<()> {
        let owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("camera ownership poisoned"))?;
        ensure!(!*owned, "Cannot reset USB while the camera is owned");
        ensure!(!cancelled(), "USB recovery cancelled before dispatch");
        let target = self
            .recovery_target
            .lock()
            .map_err(|_| anyhow::anyhow!("USB recovery state poisoned"))?
            .take()
            .context("No verified USB recovery target")?;
        if !self.runtime.simulate {
            executor()?.block_on(self.runtime.usb_command(&["usb", "reset", &target], 80))?;
        }
        // Do not start replacement capture here; the supervisor's normal retry
        // reopens with the explicit saved serial and restores all settings.
        Ok(())
    }
    pub fn version(&self) -> String {
        format!("Regain 0.5.3 / {:?}", self.backend)
    }
    pub fn is_direct(&self) -> bool {
        self.backend == CameraDriver::ZwoDirect
    }
    pub fn cameras(&self) -> Result<Vec<CameraInfo>> {
        let owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("camera ownership poisoned"))?;
        ensure!(
            !*owned,
            "Camera discovery is unavailable while acquiring; use the cached inventory"
        );
        let runtime = executor()?;
        let list = runtime.block_on(async {
            let mut worker = self.runtime.spawn(self.is_direct(), diagnostic()).await?;
            let result = worker
                .call("list", json!({}), 30., &CancellationToken::new())
                .await;
            worker.kill().await;
            result.map(|r| r.0)
        })?;
        list.as_array()
            .context("invalid Regain inventory")?
            .iter()
            .map(parse_info)
            .collect()
    }
    pub fn open(self: &Arc<Self>, info: CameraInfo) -> Result<Camera> {
        ensure!(
            !self.is_direct() || self.runtime.simulate || info.locator.is_some(),
            "Direct USB requires a selected interface; refusing a serial sweep"
        );
        let mut owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("camera ownership poisoned"))?;
        ensure!(!*owned, "A camera already owns this driver");
        let runtime = executor()?;
        let mut worker = runtime.block_on(self.runtime.spawn(self.is_direct(), diagnostic()))?;
        let mut selection =
            json!({"name":info.name,"serial":self.serial.as_deref().or(info.serial.as_deref())});
        if self.is_direct() {
            if let Some(locator) = &info.locator {
                selection["locator"] = json!(locator);
            }
        } else {
            selection["id"] = json!(info.camera_id);
        }
        let result =
            runtime.block_on(worker.call("open", selection, 15., &CancellationToken::new()));
        let parsed = result.and_then(|(v, _)| {
            let mut actual = parse_info(&v["info"])?;
            ensure!(
                actual.name == info.name
                    && (self.serial.is_some() || actual.camera_id == info.camera_id)
                    && actual.max_width == info.max_width
                    && actual.max_height == info.max_height
                    && actual.bayer_pattern == info.bayer_pattern,
                "Camera identity or geometry changed; refresh discovery and select again"
            );
            let serial = v["serial"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Regain did not report a camera serial")?;
            ensure!(
                self.serial
                    .as_deref()
                    .is_none_or(|s| s.eq_ignore_ascii_case(serial))
                    && info
                        .serial
                        .as_deref()
                        .is_none_or(|s| s.eq_ignore_ascii_case(serial)),
                "Camera serial changed"
            );
            actual.serial = Some(serial.to_owned());
            let controls: Vec<regain_core::Control> =
                serde_json::from_value(v["controls"].clone())?;
            let controls: BTreeMap<_, _> = controls.into_iter().map(|c| (c.kind, c)).collect();
            for kind in [0, 1] {
                let cap = controls
                    .get(&kind)
                    .context("missing gain/exposure control")?;
                ensure!(
                    cap.min <= cap.value && cap.value <= cap.max && cap.writable,
                    "invalid camera control bounds"
                );
            }
            Ok((v["info"].clone(), controls, serial.to_owned(), actual))
        });
        let (descriptor, controls, serial, info) = match parsed {
            Ok(v) => v,
            Err(e) => {
                runtime.block_on(worker.kill());
                return Err(e);
            }
        };
        // Regain's SDK control list contains values, not the persisted auto flag.
        // Explicitly disable SDK auto gain even if the desired numeric value is
        // unchanged. Regain's start command similarly forces manual exposure.
        let gain = controls[&0].value;
        let manual_gain = runtime.block_on(async {
            worker
                .call(
                    "set",
                    json!({"control":0,"value":gain}),
                    5.,
                    &CancellationToken::new(),
                )
                .await?;
            let actual = worker
                .call("get", json!({"control":0}), 5., &CancellationToken::new())
                .await?
                .0;
            ensure!(
                actual.as_i64() == Some(gain),
                "Regain initial gain readback mismatch"
            );
            Ok::<_, anyhow::Error>(())
        });
        if let Err(error) = manual_gain {
            runtime.block_on(worker.kill());
            return Err(error);
        }
        *owned = true;
        drop(owned);
        Ok(Camera {
            driver: Arc::clone(self),
            runtime,
            worker: RefCell::new(worker),
            values: controls.iter().map(|(&k, c)| (k, c.value)).collect(),
            controls,
            descriptor,
            queued: BTreeMap::new(),
            roi: Roi {
                width: info.max_width,
                height: info.max_height,
                bin: 1,
                image_type: ImageType::Raw16,
            },
            info,
            serial,
            active: false,
            pending: false,
            failed: false,
        })
    }
}

pub struct Camera {
    driver: Arc<Driver>,
    runtime: tokio::runtime::Runtime,
    worker: RefCell<Worker>,
    info: CameraInfo,
    serial: String,
    descriptor: Value,
    controls: BTreeMap<i32, regain_core::Control>,
    values: BTreeMap<i32, i64>,
    queued: BTreeMap<i32, i64>,
    roi: Roi,
    active: bool,
    pending: bool,
    failed: bool,
}
impl Camera {
    /// Called only by the capture owner before acquisition; disable by reopening.
    /// All estimation/correction stays in Regain, before RAW8 conversion/debayering.
    pub fn configure_white_balance(&mut self, config: &WhiteBalanceConfig) -> Result<()> {
        ensure!(
            !self.active && !self.pending && !self.failed,
            "configure white balance before capture starts"
        );
        ensure!(
            self.info.is_color && self.roi.bin == 1,
            "white balance requires a color camera at bin 1"
        );
        use regain_core::white_balance::{Gains, Mode, Output, Settings};
        let settings = Settings {
            mode: match config.mode {
                WhiteBalanceMode::Manual => Mode::Manual,
                WhiteBalanceMode::Once => Mode::Once,
                WhiteBalanceMode::Continuous => Mode::Continuous,
            },
            gains: Gains {
                red: config.red,
                blue: config.blue,
            },
            output: Output::Corrected,
        };
        settings.gains.validate()?;
        let response = self.runtime.block_on(
            self.worker
                .borrow_mut()
                .white_balance(Some(settings), &CancellationToken::new()),
        )?;
        ensure!(
            response["managed"] == true,
            "Regain did not enable managed white balance"
        );
        let actual: Settings = serde_json::from_value(response["settings"].clone())?;
        ensure!(
            actual == settings,
            "Regain white-balance configuration readback mismatch"
        );
        Ok(())
    }
    pub fn info(&self) -> &CameraInfo {
        &self.info
    }
    pub fn serial(&self) -> &str {
        &self.serial
    }
    pub fn is_direct(&self) -> bool {
        self.driver.is_direct()
    }
    fn call(&self, method: &str, params: Value, seconds: f64) -> Result<(Value, Vec<u8>)> {
        self.runtime.block_on(self.worker.borrow_mut().call(
            method,
            params,
            seconds,
            &CancellationToken::new(),
        ))
    }
    pub fn controls(&self) -> Result<Vec<ControlCaps>> {
        Ok(self
            .controls
            .values()
            .map(|c| ControlCaps {
                name: format!("Control {}", c.kind),
                description: String::new(),
                min_value: c.min,
                max_value: c.max,
                default_value: c.value,
                auto_supported: false,
                writable: c.writable,
                control_type: ControlType(c.kind),
            })
            .collect())
    }
    pub fn control_value(&self, kind: ControlType) -> Result<ControlValue> {
        // Cached exposure/gain belong to the in-flight frame. Direct workers reject
        // controls during capture, and status polling must not trigger extra USB I/O.
        Ok(ControlValue {
            value: *self.values.get(&kind.0).context("control unavailable")?,
            automatic: false,
        })
    }
    pub fn set_control(&mut self, kind: ControlType, value: i64, automatic: bool) -> Result<()> {
        ensure!(
            !automatic,
            "Regain capture requires application-controlled exposure"
        );
        let cap = self.controls.get(&kind.0).context("control unavailable")?;
        ensure!(
            cap.writable && value >= cap.min && value <= cap.max,
            "control outside camera capabilities"
        );
        if self.pending {
            self.queued.insert(kind.0, value);
            return Ok(());
        }
        self.call("set", json!({"control":kind.0,"value":value}), 5.)?;
        let actual = self
            .call("get", json!({"control":kind.0}), 5.)?
            .0
            .as_i64()
            .context("invalid control readback")?;
        ensure!(
            actual == value,
            "Regain control readback differs from requested value"
        );
        self.values.insert(kind.0, actual);
        self.queued.remove(&kind.0);
        Ok(())
    }
    fn exposure(&self) -> Result<regain_core::Exposure> {
        let bin = u32::try_from(self.roi.bin)?;
        ensure!(bin > 0, "bin must be positive");
        // Preserve AutoPierCam's centered ROI and Bayer phase, with the selected
        // driver's stricter alignment where required. No register assumptions.
        let align_x = self.descriptor["originAlignment"]
            .as_u64()
            .or_else(|| self.descriptor["originAlignmentX"].as_u64())
            .unwrap_or(2)
            .max(2);
        let align_y = self.descriptor["originAlignmentY"]
            .as_u64()
            .or_else(|| self.descriptor["originAlignment"].as_u64())
            .unwrap_or(2)
            .max(2);
        let centered = |sensor: u32, size: u32, alignment: u64| -> u32 {
            let origin = u64::from((sensor / bin).saturating_sub(size) / 2);
            // Round down to an even multiple of alignment, retaining Bayer phase.
            let alignment = if alignment.is_multiple_of(2) {
                alignment
            } else {
                alignment.saturating_mul(2)
            };
            (origin / alignment * alignment) as u32
        };
        Ok(regain_core::Exposure {
            width: self.roi.width,
            height: self.roi.height,
            bin,
            x: centered(self.info.max_width, self.roi.width, align_x),
            y: centered(self.info.max_height, self.roi.height, align_y),
            microseconds: u64::try_from(self.control_value(ControlType::EXPOSURE)?.value)?,
            dark: false,
        })
    }
    pub fn set_roi(&mut self, roi: Roi) -> Result<()> {
        ensure!(
            !self.active && !self.pending,
            "stop acquisition before changing ROI"
        );
        ensure!(
            matches!(roi.image_type, ImageType::Raw8 | ImageType::Raw16),
            "Regain capture supports RAW16; RAW8 is derived locally"
        );
        let previous = self.roi;
        self.roi = roi;
        let result = self
            .exposure()
            .and_then(|e| regain_core::validate_exposure(&self.descriptor, &self.controls, &e));
        if result.is_err() {
            self.roi = previous;
        }
        result
    }
    pub fn roi(&self) -> Result<Roi> {
        Ok(self.roi)
    }
    pub fn start_capture(&mut self) -> Result<()> {
        ensure!(
            !self.failed,
            "Regain worker faulted; a new camera session is required"
        );
        self.active = true;
        Ok(())
    }
    pub fn stop_capture(&mut self) -> Result<()> {
        self.active = false;
        if self.pending {
            // Direct stop may wait for USB completion; impose a short parent deadline.
            let result = self.call("stop", Value::Null, 2.).map(|_| ());
            self.pending = false;
            if result.is_err() {
                self.failed = true;
            }
            result
        } else {
            Ok(())
        }
    }
    pub fn poll_frame(
        &mut self,
        data: &mut Vec<u8>,
        timeout_ms: i32,
    ) -> std::result::Result<FrameMeta, FrameError> {
        let result = self.next_frame(data, timeout_ms);
        if matches!(result, Err(FrameError::Failed(_))) {
            self.failed = true;
            self.runtime.block_on(self.worker.borrow_mut().kill());
        }
        result
    }
    fn next_frame(
        &mut self,
        data: &mut Vec<u8>,
        timeout_ms: i32,
    ) -> std::result::Result<FrameMeta, FrameError> {
        if !self.active || self.failed {
            return Err(anyhow::anyhow!("Regain acquisition is not active").into());
        }
        if !self.pending {
            for (kind, value) in std::mem::take(&mut self.queued) {
                self.set_control(ControlType(kind), value, false)?;
            }
        }
        let exposure = self.exposure()?;
        if !self.pending {
            regain_core::validate_exposure(&self.descriptor, &self.controls, &exposure)?;
            self.call(
                "start",
                serde_json::to_value(&exposure).map_err(anyhow::Error::from)?,
                5.,
            )?;
            self.pending = true;
        }
        let until = Instant::now() + Duration::from_millis(timeout_ms.clamp(1, 250) as u64);
        loop {
            match self.call("status", Value::Null, 2.)?.0.as_i64() {
                Some(1) => {}
                Some(2) => break,
                state => {
                    return Err(anyhow::anyhow!("Regain exposure failed: state {state:?}").into());
                }
            }
            if Instant::now() >= until {
                return Err(FrameError::Pending);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (meta, pixels) = self.call("download", Value::Null, 10.)?;
        decode_frame(&meta, &pixels, &exposure, self.roi.image_type, data)?;
        self.pending = false;
        Ok(FrameMeta {
            width: self.roi.width,
            height: self.roi.height,
            image_type: self.roi.image_type,
        })
    }
}
impl Drop for Camera {
    fn drop(&mut self) {
        // Kill on error rather than call back into a poisoned driver. No reopening
        // or inventory refresh occurs here. Regain owns/reaps its process tree.
        if !self.failed
            && let Err(error) = self.call("close", Value::Null, 2.)
        {
            tracing::warn!(%error, "Regain close failed; camera controls may not have been restored");
        }
        self.runtime.block_on(self.worker.borrow_mut().kill());
        if let Ok(mut owned) = self.driver.owned.lock() {
            *owned = false;
        }
    }
}
fn decode_frame(
    meta: &Value,
    pixels: &[u8],
    exposure: &regain_core::Exposure,
    format: ImageType,
    out: &mut Vec<u8>,
) -> Result<()> {
    ensure!(
        meta.get("cleanupError").is_none(),
        "Regain frame cleanup failed; camera session must restart"
    );
    ensure!(
        meta["width"] == exposure.width
            && meta["height"] == exposure.height
            && pixels.len() == exposure.bytes()?,
        "Regain returned an invalid RAW16 frame"
    );
    match format {
        ImageType::Raw16 => {
            out.clear();
            out.extend_from_slice(pixels)
        }
        ImageType::Raw8 => {
            out.clear();
            out.extend(pixels.as_chunks::<2>().0.iter().map(|p| p[1]))
        }
        _ => bail!("unsupported output format"),
    }
    Ok(())
}
fn parse_info(v: &Value) -> Result<CameraInfo> {
    let integer = |key: &str| -> Result<i32> {
        Ok(i32::try_from(
            v[key]
                .as_i64()
                .with_context(|| format!("missing camera {key}"))?,
        )?)
    };
    let width = u32::try_from(integer("width")?)?;
    let height = u32::try_from(integer("height")?)?;
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) * 2 <= 512 * 1024 * 1024,
        "invalid camera dimensions"
    );
    ensure!(
        v["formats"]
            .as_array()
            .is_some_and(|a| a.contains(&json!(2))),
        "camera does not support RAW16"
    );
    let bins = v["bins"]
        .as_array()
        .context("missing camera bins")?
        .iter()
        .map(|b| Ok(i32::try_from(b.as_u64().context("invalid bin")?)?))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !bins.is_empty() && bins.iter().all(|b| *b > 0),
        "invalid camera bins"
    );
    Ok(CameraInfo {
        locator: v["locator"].as_str().map(str::to_owned),
        serial: v["serial"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        discovery_error: v["discoveryError"].as_str().map(str::to_owned),
        name: v["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("camera name missing")?
            .into(),
        camera_id: integer("id")?,
        max_width: width,
        max_height: height,
        is_color: v["color"].as_bool().context("missing color flag")?,
        bayer_pattern: integer("bayer")?.into(),
        supported_bins: bins,
        supported_formats: vec![ImageType::Raw8, ImageType::Raw16],
        pixel_size_um: v["pixelSize"].as_f64().unwrap_or(0.),
        has_mechanical_shutter: v["shutter"] == true,
        has_st4_port: v["st4"] == true,
        is_cooled: v["cooled"] == true,
        is_usb3_camera: v["usb3Camera"] == true,
        bit_depth: integer("bitDepth")?,
        is_trigger_camera: v["triggerCamera"] == true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_inventory_is_rejected() {
        let valid = json!({"name":"camera","id":0,"width":64,"height":64,
            "formats":[2],"bins":[1],"color":true,"bayer":0,"bitDepth":12});
        assert!(parse_info(&valid).is_ok());
        for (key, value) in [
            ("width", json!(-1)),
            ("height", json!(0)),
            ("formats", json!([0])),
            ("bins", json!([0])),
            ("color", Value::Null),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(parse_info(&invalid).is_err(), "accepted invalid {key}");
        }
    }
    #[test]
    fn raw16_is_little_endian_and_raw8_uses_high_byte() {
        let e = regain_core::Exposure {
            width: 8,
            height: 2,
            bin: 1,
            x: 0,
            y: 0,
            microseconds: 100,
            dark: false,
        };
        let pixels = [0x34, 0xab].repeat(16);
        let mut out = Vec::new();
        decode_frame(
            &json!({"width":8,"height":2}),
            &pixels,
            &e,
            ImageType::Raw8,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, vec![0xab; 16]);
        decode_frame(
            &json!({"width":8,"height":2}),
            &pixels,
            &e,
            ImageType::Raw16,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, pixels);
        assert!(
            decode_frame(
                &json!({"width":8,"height":2,"cleanupError":"stop failed"}),
                &pixels,
                &e,
                ImageType::Raw16,
                &mut out
            )
            .is_err()
        );
        assert!(
            decode_frame(
                &json!({"width":16,"height":2}),
                &pixels,
                &e,
                ImageType::Raw16,
                &mut out
            )
            .is_err()
        );
        assert!(
            decode_frame(
                &json!({"width":8,"height":2}),
                &pixels[..31],
                &e,
                ImageType::Raw16,
                &mut out
            )
            .is_err()
        );
    }
}
