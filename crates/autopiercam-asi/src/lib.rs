//! Narrow wrapper around ZWO's dynamically loaded ASICamera2 C API.
//!
//! Keep a Camera on one capture thread. The vendor SDK does not document that
//! concurrent control and frame calls are safe.

mod raw;

use libloading::Library;
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{CStr, c_char, c_int, c_long};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not load the ZWO ASI SDK; attempted: {attempted:?}; last error: {last_error}")]
    LibraryNotFound {
        attempted: Vec<PathBuf>,
        last_error: String,
    },
    #[error("failed to load ZWO ASI SDK at {path}: {source}")]
    LoadLibrary {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error("ZWO ASI SDK is missing symbol {name}: {source}")]
    MissingSymbol {
        name: &'static str,
        #[source]
        source: libloading::Error,
    },
    #[error("ASI SDK operation {operation} failed: {code}")]
    Sdk {
        operation: &'static str,
        code: ErrorCode,
    },
    #[error("ASI SDK returned invalid metadata: {0}")]
    InvalidMetadata(String),
    #[error("camera id {0} is already open in this process")]
    CameraAlreadyOpen(i32),
    #[error("internal camera-session registry is poisoned")]
    SessionRegistryPoisoned,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Sdk { code, .. } if code.0 == 11)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorCode(pub i32);

impl ErrorCode {
    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "success",
            1 => "invalid index",
            2 => "invalid camera id",
            3 => "invalid control type",
            4 => "camera closed",
            5 => "camera removed",
            6 => "invalid path",
            7 => "invalid file format",
            8 => "invalid size",
            9 => "invalid image type",
            10 => "out of boundary",
            11 => "timeout",
            12 => "invalid sequence",
            13 => "buffer too small",
            14 => "video mode active",
            15 => "exposure in progress",
            16 => "general error",
            17 => "invalid mode",
            18 => "GPS not supported",
            19 => "GPS version error",
            20 => "GPS FPGA error",
            21 => "GPS parameter out of range",
            22 => "GPS data invalid",
            _ => "unknown error",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name(), self.0)
    }
}

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
            other => Self::Unknown(other),
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
    fn raw(self) -> i32 {
        match self {
            Self::Raw8 => raw::ASI_IMG_RAW8,
            Self::Rgb24 => raw::ASI_IMG_RGB24,
            Self::Raw16 => raw::ASI_IMG_RAW16,
            Self::Y8 => raw::ASI_IMG_Y8,
            Self::Unknown(value) => value,
        }
    }

    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Raw8 | Self::Y8 => Some(1),
            Self::Raw16 => Some(2),
            Self::Rgb24 => Some(3),
            Self::Unknown(_) => None,
        }
    }
}

impl From<i32> for ImageType {
    fn from(value: i32) -> Self {
        match value {
            raw::ASI_IMG_RAW8 => Self::Raw8,
            raw::ASI_IMG_RGB24 => Self::Rgb24,
            raw::ASI_IMG_RAW16 => Self::Raw16,
            raw::ASI_IMG_Y8 => Self::Y8,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CameraInfo {
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
    pub const GAIN: Self = Self(raw::ASI_GAIN);
    pub const EXPOSURE: Self = Self(raw::ASI_EXPOSURE);
    pub const FLIP: Self = Self(raw::ASI_FLIP);
    pub const AUTO_MAX_GAIN: Self = Self(raw::ASI_AUTO_MAX_GAIN);
    pub const AUTO_MAX_EXPOSURE: Self = Self(raw::ASI_AUTO_MAX_EXP);
    pub const AUTO_TARGET_BRIGHTNESS: Self = Self(raw::ASI_AUTO_TARGET_BRIGHTNESS);
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

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub image_type: ImageType,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameMeta {
    pub width: u32,
    pub height: u32,
    pub image_type: ImageType,
}

struct Functions {
    get_num_of_connected_cameras: raw::GetNumOfConnectedCameras,
    get_camera_property: raw::GetCameraProperty,
    open_camera: raw::OpenCamera,
    init_camera: raw::InitCamera,
    close_camera: raw::CloseCamera,
    get_num_of_controls: raw::GetNumOfControls,
    get_control_caps: raw::GetControlCaps,
    get_control_value: raw::GetControlValue,
    set_control_value: raw::SetControlValue,
    set_roi_format: raw::SetRoiFormat,
    get_roi_format: raw::GetRoiFormat,
    set_start_pos: raw::SetStartPos,
    get_start_pos: raw::GetStartPos,
    disable_dark_subtract: raw::DisableDarkSubtract,
    start_video_capture: raw::StartVideoCapture,
    stop_video_capture: raw::StopVideoCapture,
    get_video_data: raw::GetVideoData,
    get_sdk_version: raw::GetSdkVersion,
}

pub struct Sdk {
    functions: Functions,
    _library: Library,
    path: PathBuf,
    open_camera_ids: Mutex<HashSet<i32>>,
}

impl fmt::Debug for Sdk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sdk")
            .field("path", &self.path)
            .finish()
    }
}

impl Sdk {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // SAFETY: We resolve only the documented C ABI and keep the module loaded.
        let library = unsafe { Library::new(&path) }.map_err(|source| Error::LoadLibrary {
            path: path.clone(),
            source,
        })?;

        macro_rules! load {
            ($ty:ty, $symbol:literal) => {{
                // SAFETY: Each type mirrors SDK v1.41 ASICamera2.h.
                let symbol =
                    unsafe { library.get::<$ty>($symbol.as_bytes()) }.map_err(|source| {
                        Error::MissingSymbol {
                            name: $symbol,
                            source,
                        }
                    })?;
                *symbol
            }};
        }

        let functions = Functions {
            get_num_of_connected_cameras: load!(
                raw::GetNumOfConnectedCameras,
                "ASIGetNumOfConnectedCameras"
            ),
            get_camera_property: load!(raw::GetCameraProperty, "ASIGetCameraProperty"),
            open_camera: load!(raw::OpenCamera, "ASIOpenCamera"),
            init_camera: load!(raw::InitCamera, "ASIInitCamera"),
            close_camera: load!(raw::CloseCamera, "ASICloseCamera"),
            get_num_of_controls: load!(raw::GetNumOfControls, "ASIGetNumOfControls"),
            get_control_caps: load!(raw::GetControlCaps, "ASIGetControlCaps"),
            get_control_value: load!(raw::GetControlValue, "ASIGetControlValue"),
            set_control_value: load!(raw::SetControlValue, "ASISetControlValue"),
            set_roi_format: load!(raw::SetRoiFormat, "ASISetROIFormat"),
            get_roi_format: load!(raw::GetRoiFormat, "ASIGetROIFormat"),
            set_start_pos: load!(raw::SetStartPos, "ASISetStartPos"),
            get_start_pos: load!(raw::GetStartPos, "ASIGetStartPos"),
            disable_dark_subtract: load!(raw::DisableDarkSubtract, "ASIDisableDarkSubtract"),
            start_video_capture: load!(raw::StartVideoCapture, "ASIStartVideoCapture"),
            stop_video_capture: load!(raw::StopVideoCapture, "ASIStopVideoCapture"),
            get_video_data: load!(raw::GetVideoData, "ASIGetVideoData"),
            get_sdk_version: load!(raw::GetSdkVersion, "ASIGetSDKVersion"),
        };

        Ok(Self {
            functions,
            _library: library,
            path,
            open_camera_ids: Mutex::new(HashSet::new()),
        })
    }

    pub fn load_default() -> Result<Self> {
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("AUTOPIERCAM_ASI_SDK_PATH") {
            candidates.push(PathBuf::from(path));
        }
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            candidates.push(directory.join(platform_library_name()));
        }
        candidates.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(bundled_relative_path()),
        );
        candidates.push(PathBuf::from(platform_library_name()));

        let mut last_error = "no candidate paths".to_owned();
        for candidate in &candidates {
            match Self::load(candidate) {
                Ok(sdk) => return Ok(sdk),
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(Error::LibraryNotFound {
            attempted: candidates,
            last_error,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> String {
        // SAFETY: SDK owns the returned null-terminated static string.
        let pointer = unsafe { (self.functions.get_sdk_version)() };
        if pointer.is_null() {
            return "unknown".to_owned();
        }
        // SAFETY: The SDK contract promises a valid null-terminated string.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn cameras(&self) -> Result<Vec<CameraInfo>> {
        // SAFETY: No arguments or ownership transfer.
        let count = unsafe { (self.functions.get_num_of_connected_cameras)() };
        if count < 0 {
            return Err(Error::InvalidMetadata(format!(
                "negative connected-camera count {count}"
            )));
        }
        (0..count)
            .map(|index| {
                // SAFETY: Zero is a valid initial state for this C output struct.
                let mut raw_info: raw::CameraInfo = unsafe { std::mem::zeroed() };
                // SAFETY: raw_info has the exact writable C layout.
                let code =
                    unsafe { (self.functions.get_camera_property)(&mut raw_info as *mut _, index) };
                check("ASIGetCameraProperty", code)?;
                CameraInfo::try_from(raw_info)
            })
            .collect()
    }

    pub fn open(self: &Arc<Self>, info: CameraInfo) -> Result<Camera> {
        {
            let mut open_ids = self
                .open_camera_ids
                .lock()
                .map_err(|_| Error::SessionRegistryPoisoned)?;
            if !open_ids.insert(info.camera_id) {
                return Err(Error::CameraAlreadyOpen(info.camera_id));
            }
        }
        // SAFETY: Camera id came from ASIGetCameraProperty.
        let code = unsafe { (self.functions.open_camera)(info.camera_id) };
        if let Err(error) = check("ASIOpenCamera", code) {
            self.release_camera_id(info.camera_id);
            return Err(error);
        }
        // SAFETY: Camera was opened successfully immediately above.
        let init_code = unsafe { (self.functions.init_camera)(info.camera_id) };
        if let Err(error) = check("ASIInitCamera", init_code) {
            // SAFETY: Best-effort cleanup of the camera opened above.
            unsafe { (self.functions.close_camera)(info.camera_id) };
            self.release_camera_id(info.camera_id);
            return Err(error);
        }
        // The vendor setting persists in the Windows Registry. AutoPierCam performs
        // calibration in its own pipeline, so normalize every owned session.
        let dark_code = unsafe { (self.functions.disable_dark_subtract)(info.camera_id) };
        if let Err(error) = check("ASIDisableDarkSubtract", dark_code) {
            // SAFETY: Best-effort cleanup after successful open and init.
            unsafe { (self.functions.close_camera)(info.camera_id) };
            self.release_camera_id(info.camera_id);
            return Err(error);
        }
        Ok(Camera {
            sdk: Arc::clone(self),
            info,
            video_active: false,
            _not_sync: PhantomData,
        })
    }

    fn release_camera_id(&self, camera_id: i32) {
        if let Ok(mut open_ids) = self.open_camera_ids.lock() {
            open_ids.remove(&camera_id);
        }
    }
}

pub struct Camera {
    sdk: Arc<Sdk>,
    info: CameraInfo,
    video_active: bool,
    // Camera may move to its owner thread, but safe Rust cannot share references
    // across threads and make concurrent vendor calls.
    _not_sync: PhantomData<Cell<()>>,
}

impl fmt::Debug for Camera {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Camera")
            .field("info", &self.info)
            .field("video_active", &self.video_active)
            .finish()
    }
}

impl Camera {
    pub fn info(&self) -> &CameraInfo {
        &self.info
    }

    pub fn controls(&self) -> Result<Vec<ControlCaps>> {
        let mut count: c_int = 0;
        // SAFETY: Camera remains open and count is a writable output pointer.
        let code =
            unsafe { (self.sdk.functions.get_num_of_controls)(self.info.camera_id, &mut count) };
        check("ASIGetNumOfControls", code)?;
        if count < 0 {
            return Err(Error::InvalidMetadata(format!(
                "negative control count {count}"
            )));
        }
        (0..count)
            .map(|index| {
                // SAFETY: Zero is a valid initial state for this C output struct.
                let mut raw_caps: raw::ControlCaps = unsafe { std::mem::zeroed() };
                // SAFETY: Camera is open and raw_caps has the exact C layout.
                let code = unsafe {
                    (self.sdk.functions.get_control_caps)(self.info.camera_id, index, &mut raw_caps)
                };
                check("ASIGetControlCaps", code)?;
                Ok(ControlCaps::from(raw_caps))
            })
            .collect()
    }

    pub fn control_value(&self, control_type: ControlType) -> Result<ControlValue> {
        let mut value: c_long = 0;
        let mut automatic: c_int = 0;
        // SAFETY: Camera is open and output pointers are writable.
        let code = unsafe {
            (self.sdk.functions.get_control_value)(
                self.info.camera_id,
                control_type.0,
                &mut value,
                &mut automatic,
            )
        };
        check("ASIGetControlValue", code)?;
        Ok(ControlValue {
            value: value as i64,
            automatic: automatic != 0,
        })
    }

    pub fn set_control(
        &mut self,
        control_type: ControlType,
        value: i64,
        automatic: bool,
    ) -> Result<()> {
        let value = c_long::try_from(value).map_err(|_| {
            Error::InvalidMetadata(format!("control value {value} does not fit C long"))
        })?;
        // SAFETY: Camera is open and the SDK validates type and bounds.
        let code = unsafe {
            (self.sdk.functions.set_control_value)(
                self.info.camera_id,
                control_type.0,
                value,
                i32::from(automatic),
            )
        };
        check("ASISetControlValue", code)
    }

    pub fn set_roi(&mut self, roi: Roi) -> Result<()> {
        if self.video_active {
            return Err(Error::InvalidMetadata(
                "ROI cannot be changed while video capture is active".to_owned(),
            ));
        }
        let width = i32::try_from(roi.width)
            .map_err(|_| Error::InvalidMetadata("ROI width exceeds i32".to_owned()))?;
        let height = i32::try_from(roi.height)
            .map_err(|_| Error::InvalidMetadata("ROI height exceeds i32".to_owned()))?;
        if width == 0 || height == 0 {
            return Err(Error::InvalidMetadata(
                "ROI dimensions must be non-zero".to_owned(),
            ));
        }
        if width % 8 != 0 || height % 2 != 0 {
            return Err(Error::InvalidMetadata(format!(
                "ROI must have width divisible by 8 and height divisible by 2; got {width}x{height}"
            )));
        }
        if !self.info.supported_bins.contains(&roi.bin) {
            return Err(Error::InvalidMetadata(format!(
                "unsupported bin {}; supported bins are {:?}",
                roi.bin, self.info.supported_bins
            )));
        }
        if matches!(roi.image_type, ImageType::Unknown(_))
            || !self.info.supported_formats.contains(&roi.image_type)
        {
            return Err(Error::InvalidMetadata(format!(
                "unsupported image type {:?}; supported formats are {:?}",
                roi.image_type, self.info.supported_formats
            )));
        }
        let bin = u32::try_from(roi.bin)
            .map_err(|_| Error::InvalidMetadata("bin must be positive".to_owned()))?;
        let sensor_width = roi
            .width
            .checked_mul(bin)
            .ok_or_else(|| Error::InvalidMetadata("ROI width overflow".to_owned()))?;
        let sensor_height = roi
            .height
            .checked_mul(bin)
            .ok_or_else(|| Error::InvalidMetadata("ROI height overflow".to_owned()))?;
        if sensor_width > self.info.max_width || sensor_height > self.info.max_height {
            return Err(Error::InvalidMetadata(format!(
                "ROI {}x{} at bin {} exceeds sensor {}x{}",
                roi.width, roi.height, roi.bin, self.info.max_width, self.info.max_height
            )));
        }
        // SAFETY: Camera is open and the SDK validates ROI parameters.
        let code = unsafe {
            (self.sdk.functions.set_roi_format)(
                self.info.camera_id,
                width,
                height,
                roi.bin,
                roi.image_type.raw(),
            )
        };
        check("ASISetROIFormat", code)?;

        // Setting ROI recenters it. Keep both origin coordinates even so the
        // camera-reported Bayer phase remains valid for application debayering.
        let (mut start_x, mut start_y) = (0, 0);
        let position_code = unsafe {
            (self.sdk.functions.get_start_pos)(self.info.camera_id, &mut start_x, &mut start_y)
        };
        check("ASIGetStartPos", position_code)?;
        let even_x = start_x & !1;
        let even_y = start_y & !1;
        if even_x != start_x || even_y != start_y {
            let position_code =
                unsafe { (self.sdk.functions.set_start_pos)(self.info.camera_id, even_x, even_y) };
            check("ASISetStartPos", position_code)?;
        }
        Ok(())
    }

    pub fn roi(&self) -> Result<Roi> {
        let (mut width, mut height, mut bin, mut image_type) = (0, 0, 0, 0);
        // SAFETY: Camera is open and all output pointers are writable.
        let code = unsafe {
            (self.sdk.functions.get_roi_format)(
                self.info.camera_id,
                &mut width,
                &mut height,
                &mut bin,
                &mut image_type,
            )
        };
        check("ASIGetROIFormat", code)?;
        Ok(Roi {
            width: u32::try_from(width)
                .map_err(|_| Error::InvalidMetadata(format!("negative ROI width {width}")))?,
            height: u32::try_from(height)
                .map_err(|_| Error::InvalidMetadata(format!("negative ROI height {height}")))?,
            bin,
            image_type: ImageType::from(image_type),
        })
    }

    pub fn start_video(&mut self) -> Result<()> {
        if self.video_active {
            return Ok(());
        }
        // SAFETY: Camera is open and configured.
        let code = unsafe { (self.sdk.functions.start_video_capture)(self.info.camera_id) };
        check("ASIStartVideoCapture", code)?;
        self.video_active = true;
        Ok(())
    }

    pub fn stop_video(&mut self) -> Result<()> {
        if !self.video_active {
            return Ok(());
        }
        // SAFETY: Camera is open. The SDK documents this as idempotent.
        let code = unsafe { (self.sdk.functions.stop_video_capture)(self.info.camera_id) };
        check("ASIStopVideoCapture", code)?;
        self.video_active = false;
        Ok(())
    }

    pub fn next_video_frame(&mut self, timeout_ms: i32) -> Result<Frame> {
        let mut data = Vec::new();
        let meta = self.next_video_frame_into(&mut data, timeout_ms)?;
        Ok(Frame {
            width: meta.width,
            height: meta.height,
            image_type: meta.image_type,
            data,
        })
    }

    /// Reads into a reusable allocation. The buffer length is adjusted to the
    /// exact current ROI size before entering the vendor API.
    pub fn next_video_frame_into(
        &mut self,
        data: &mut Vec<u8>,
        timeout_ms: i32,
    ) -> Result<FrameMeta> {
        if !self.video_active {
            return Err(Error::InvalidMetadata(
                "video capture has not been started".to_owned(),
            ));
        }
        let roi = self.roi()?;
        let bytes_per_pixel = roi.image_type.bytes_per_pixel().ok_or_else(|| {
            Error::InvalidMetadata(format!("unknown image type {:?}", roi.image_type))
        })?;
        let buffer_size = (roi.width as usize)
            .checked_mul(roi.height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or_else(|| Error::InvalidMetadata("frame buffer size overflow".to_owned()))?;
        let sdk_buffer_size = c_long::try_from(buffer_size).map_err(|_| {
            Error::InvalidMetadata(format!("frame buffer {buffer_size} does not fit C long"))
        })?;
        data.resize(buffer_size, 0);
        // SAFETY: Buffer is writable for its exact advertised length.
        let code = unsafe {
            (self.sdk.functions.get_video_data)(
                self.info.camera_id,
                data.as_mut_ptr(),
                sdk_buffer_size,
                timeout_ms,
            )
        };
        check("ASIGetVideoData", code)?;
        Ok(FrameMeta {
            width: roi.width,
            height: roi.height,
            image_type: roi.image_type,
        })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        if self.video_active {
            // SAFETY: Best-effort cleanup; this object owns the camera session.
            unsafe { (self.sdk.functions.stop_video_capture)(self.info.camera_id) };
        }
        // SAFETY: This object owns the open camera session.
        unsafe { (self.sdk.functions.close_camera)(self.info.camera_id) };
        self.sdk.release_camera_id(self.info.camera_id);
    }
}

impl TryFrom<raw::CameraInfo> for CameraInfo {
    type Error = Error;

    fn try_from(value: raw::CameraInfo) -> Result<Self> {
        let max_width = u32::try_from(value.max_width).map_err(|_| {
            Error::InvalidMetadata(format!("negative maximum width {}", value.max_width))
        })?;
        let max_height = u32::try_from(value.max_height).map_err(|_| {
            Error::InvalidMetadata(format!("negative maximum height {}", value.max_height))
        })?;
        let supported_bins = value
            .supported_bins
            .into_iter()
            .take_while(|value| *value != 0)
            .collect();
        let supported_formats = value
            .supported_video_formats
            .into_iter()
            .take_while(|format| *format != raw::ASI_IMG_END)
            .map(ImageType::from)
            .collect();
        Ok(Self {
            name: c_char_array_to_string(&value.name),
            camera_id: value.camera_id,
            max_width,
            max_height,
            is_color: value.is_color_camera != 0,
            bayer_pattern: BayerPattern::from(value.bayer_pattern),
            supported_bins,
            supported_formats,
            pixel_size_um: value.pixel_size,
            has_mechanical_shutter: value.has_mechanical_shutter != 0,
            has_st4_port: value.has_st4_port != 0,
            is_cooled: value.is_cooled_camera != 0,
            is_usb3_camera: value.is_usb3_camera != 0,
            bit_depth: value.bit_depth,
            is_trigger_camera: value.is_trigger_camera != 0,
        })
    }
}

impl From<raw::ControlCaps> for ControlCaps {
    fn from(value: raw::ControlCaps) -> Self {
        Self {
            name: c_char_array_to_string(&value.name),
            description: c_char_array_to_string(&value.description),
            min_value: value.min_value as i64,
            max_value: value.max_value as i64,
            default_value: value.default_value as i64,
            auto_supported: value.is_auto_supported != 0,
            writable: value.is_writable != 0,
            control_type: ControlType(value.control_type),
        }
    }
}

fn c_char_array_to_string(value: &[c_char]) -> String {
    let length = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    let bytes = value[..length]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn check(operation: &'static str, code: i32) -> Result<()> {
    if code == raw::ASI_SUCCESS {
        Ok(())
    } else {
        Err(Error::Sdk {
            operation,
            code: ErrorCode(code),
        })
    }
}

fn platform_library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ASICamera2.dll"
    } else if cfg!(target_os = "macos") {
        "libASICamera2.dylib"
    } else {
        "libASICamera2.so"
    }
}

fn bundled_relative_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        PathBuf::from("vendor/zwo/ASI SDK/lib/x64/ASICamera2.dll")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("vendor/zwo/ASI SDK/lib/mac/libASICamera2.dylib")
    } else {
        PathBuf::from("vendor/zwo/ASI SDK/lib/x64/libASICamera2.so")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(target_os = "windows")]
    fn ffi_layout_matches_64_bit_windows_sdk() {
        assert_eq!(std::mem::size_of::<std::ffi::c_long>(), 4);
        assert_eq!(std::mem::size_of::<super::raw::CameraInfo>(), 240);
        assert_eq!(std::mem::size_of::<super::raw::ControlCaps>(), 248);
        assert_eq!(
            std::mem::offset_of!(super::raw::CameraInfo, pixel_size),
            184
        );
        assert_eq!(std::mem::offset_of!(super::raw::CameraInfo, bit_depth), 216);
        assert_eq!(
            std::mem::offset_of!(super::raw::ControlCaps, max_value),
            192
        );
        assert_eq!(
            std::mem::offset_of!(super::raw::ControlCaps, control_type),
            212
        );
    }
}
