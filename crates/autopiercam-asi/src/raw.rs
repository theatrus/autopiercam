use std::ffi::{c_char, c_int, c_long, c_uchar};

pub const ASI_SUCCESS: c_int = 0;
pub const ASI_IMG_RAW8: c_int = 0;
pub const ASI_IMG_RGB24: c_int = 1;
pub const ASI_IMG_RAW16: c_int = 2;
pub const ASI_IMG_Y8: c_int = 3;
pub const ASI_IMG_END: c_int = -1;

pub const ASI_GAIN: c_int = 0;
pub const ASI_EXPOSURE: c_int = 1;
pub const ASI_FLIP: c_int = 9;
pub const ASI_AUTO_MAX_GAIN: c_int = 10;
pub const ASI_AUTO_MAX_EXP: c_int = 11;
pub const ASI_AUTO_TARGET_BRIGHTNESS: c_int = 12;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CameraInfo {
    pub name: [c_char; 64],
    pub camera_id: c_int,
    pub max_height: c_long,
    pub max_width: c_long,
    pub is_color_camera: c_int,
    pub bayer_pattern: c_int,
    pub supported_bins: [c_int; 16],
    pub supported_video_formats: [c_int; 8],
    pub pixel_size: f64,
    pub has_mechanical_shutter: c_int,
    pub has_st4_port: c_int,
    pub is_cooled_camera: c_int,
    pub is_usb3_host: c_int,
    pub is_usb3_camera: c_int,
    pub electrons_per_adu: f32,
    pub bit_depth: c_int,
    pub is_trigger_camera: c_int,
    pub unused: [c_char; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ControlCaps {
    pub name: [c_char; 64],
    pub description: [c_char; 128],
    pub max_value: c_long,
    pub min_value: c_long,
    pub default_value: c_long,
    pub is_auto_supported: c_int,
    pub is_writable: c_int,
    pub control_type: c_int,
    pub unused: [c_char; 32],
}

pub type GetNumOfConnectedCameras = unsafe extern "C" fn() -> c_int;
pub type GetCameraProperty = unsafe extern "C" fn(*mut CameraInfo, c_int) -> c_int;
pub type OpenCamera = unsafe extern "C" fn(c_int) -> c_int;
pub type InitCamera = unsafe extern "C" fn(c_int) -> c_int;
pub type CloseCamera = unsafe extern "C" fn(c_int) -> c_int;
pub type GetNumOfControls = unsafe extern "C" fn(c_int, *mut c_int) -> c_int;
pub type GetControlCaps = unsafe extern "C" fn(c_int, c_int, *mut ControlCaps) -> c_int;
pub type GetControlValue = unsafe extern "C" fn(c_int, c_int, *mut c_long, *mut c_int) -> c_int;
pub type SetControlValue = unsafe extern "C" fn(c_int, c_int, c_long, c_int) -> c_int;
pub type SetRoiFormat = unsafe extern "C" fn(c_int, c_int, c_int, c_int, c_int) -> c_int;
pub type GetRoiFormat =
    unsafe extern "C" fn(c_int, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> c_int;
pub type SetStartPos = unsafe extern "C" fn(c_int, c_int, c_int) -> c_int;
pub type GetStartPos = unsafe extern "C" fn(c_int, *mut c_int, *mut c_int) -> c_int;
pub type DisableDarkSubtract = unsafe extern "C" fn(c_int) -> c_int;
pub type StartVideoCapture = unsafe extern "C" fn(c_int) -> c_int;
pub type StopVideoCapture = unsafe extern "C" fn(c_int) -> c_int;
pub type GetVideoData = unsafe extern "C" fn(c_int, *mut c_uchar, c_long, c_int) -> c_int;
pub type GetSdkVersion = unsafe extern "C" fn() -> *const c_char;
