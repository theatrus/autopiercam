use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub camera: CameraConfig,
    pub capture: CaptureConfig,
    pub upload: UploadConfig,
    pub video: VideoConfig,
    pub api: ApiConfig,
}

impl Config {
    /// Limits, cadence and JPEG quality can be applied between SDK polls. Other
    /// recording settings replace the recording services, not the camera.
    pub fn requires_recording_reload(&self, next: &Self) -> bool {
        let mut capture = self.capture.clone();
        capture.interval_ms = next.capture.interval_ms;
        capture.jpeg_quality = next.capture.jpeg_quality;
        capture != next.capture || self.upload != next.upload || self.video != next.video
    }

    /// Only acquisition layout/device changes need a new SDK session.
    pub fn requires_camera_restart(&self, next: &Self) -> bool {
        let a = &self.camera;
        let b = &next.camera;
        a.camera_id != b.camera_id
            || a.name_contains != b.name_contains
            || a.width != b.width
            || a.height != b.height
            || a.bin != b.bin
            || a.raw16 != b.raw16
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.camera.bin != 1 {
            return Err(ConfigError::Validation(
                "camera.bin must be 1 until color binning is characterized",
            ));
        }
        if self.camera.min_exposure_us < 0 || self.camera.max_exposure_us <= 0 {
            return Err(ConfigError::Validation(
                "camera exposure limits must be non-negative with a positive maximum",
            ));
        }
        if self.camera.max_exposure_us < self.camera.min_exposure_us {
            return Err(ConfigError::Validation(
                "camera.max_exposure_us must be >= camera.min_exposure_us",
            ));
        }
        if self.camera.max_gain < 0 || !(1..=250).contains(&self.camera.target_brightness) {
            return Err(ConfigError::Validation(
                "camera gain must be non-negative and target_brightness in 1..=250",
            ));
        }
        if !(1..=100).contains(&self.capture.jpeg_quality) {
            return Err(ConfigError::Validation(
                "capture.jpeg_quality must be between 1 and 100",
            ));
        }
        if self.capture.interval_ms == 0 {
            return Err(ConfigError::Validation(
                "capture.interval_ms must be greater than zero",
            ));
        }
        if self.capture.writer_queue_capacity == 0 {
            return Err(ConfigError::Validation(
                "capture.writer_queue_capacity must be greater than zero",
            ));
        }
        if self.capture.retention_max_bytes == Some(0) {
            return Err(ConfigError::Validation(
                "capture.retention_max_bytes must be greater than zero when set",
            ));
        }
        if self.capture.retention_min_free_bytes == Some(0) {
            return Err(ConfigError::Validation(
                "capture.retention_min_free_bytes must be greater than zero when set",
            ));
        }
        if self.upload.queue_capacity == 0 {
            return Err(ConfigError::Validation(
                "upload.queue_capacity must be greater than zero",
            ));
        }
        if !(1..=600).contains(&self.video.segment_seconds)
            || !(1..=30).contains(&self.video.frames_per_second)
        {
            return Err(ConfigError::Validation(
                "video segment_seconds must be in 1..=600 and frames_per_second in 1..=30",
            ));
        }
        if self.video.enabled
            && self
                .video
                .ffmpeg_path
                .as_ref()
                .is_none_or(|path| !path.is_absolute())
        {
            return Err(ConfigError::Validation(
                "video.ffmpeg_path must be an absolute FFmpeg executable path when video is enabled",
            ));
        }
        if self.upload.enabled && self.upload.endpoint.is_none() {
            return Err(ConfigError::Validation(
                "upload.endpoint is required when upload.enabled is true",
            ));
        }
        let upload_endpoint = self
            .upload
            .endpoint
            .as_deref()
            .map(validate_upload_endpoint)
            .transpose()?;
        if let Some(variable) = self.upload.bearer_token_env.as_deref() {
            if variable.is_empty()
                || variable.trim() != variable
                || variable.contains(['=', '\0'])
                || variable.chars().any(char::is_control)
            {
                return Err(ConfigError::Validation(
                    "upload.bearer_token_env must be a valid nonblank environment-variable name",
                ));
            }
            if upload_endpoint.is_some_and(|endpoint| endpoint.scheme() != "https") {
                return Err(ConfigError::Validation(
                    "upload bearer authentication requires an HTTPS endpoint",
                ));
            }
        }
        Ok(())
    }
}

fn validate_upload_endpoint(endpoint: &str) -> Result<url::Url, ConfigError> {
    if endpoint.is_empty() || endpoint.trim() != endpoint {
        return Err(ConfigError::Validation(
            "upload.endpoint must be a nonblank absolute HTTP or HTTPS URL",
        ));
    }
    let parsed = url::Url::parse(endpoint).map_err(|_| {
        ConfigError::Validation("upload.endpoint must be a nonblank absolute HTTP or HTTPS URL")
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ConfigError::Validation(
            "upload.endpoint must be a nonblank absolute HTTP or HTTPS URL",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ConfigError::Validation(
            "upload.endpoint must not contain embedded credentials",
        ));
    }
    if parsed.fragment().is_some() {
        return Err(ConfigError::Validation(
            "upload.endpoint must not contain a URL fragment",
        ));
    }
    Ok(parsed)
}

/// Return the validated endpoint in the canonical ASCII form consumed by HTTP
/// transports. This keeps configuration validation and runtime URI parsing on
/// one URL grammar, including internationalized hosts and escaped paths.
pub fn normalize_upload_endpoint(endpoint: &str) -> Result<String, ConfigError> {
    Ok(validate_upload_endpoint(endpoint)?.into())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    /// Opt-in application control uses the sensor's manual exposure limits.
    #[serde(skip_serializing_if = "ExposureControl::is_sdk")]
    pub exposure_control: ExposureControl,
    /// Preserve the SDK's full 16-bit Bayer samples in debayered PNG stills.
    #[serde(skip_serializing_if = "is_false")]
    pub raw16: bool,
    pub camera_id: Option<i32>,
    pub name_contains: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bin: i32,
    pub min_exposure_us: i64,
    pub max_exposure_us: i64,
    pub max_gain: i64,
    pub target_brightness: i64,
    pub settle_frames: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            exposure_control: ExposureControl::Sdk,
            raw16: false,
            camera_id: None,
            name_contains: None,
            width: None,
            height: None,
            bin: 1,
            min_exposure_us: 100,
            max_exposure_us: 60_000_000,
            max_gain: 300,
            target_brightness: 100,
            settle_frames: 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureControl {
    #[default]
    Sdk,
    Adaptive,
}

impl ExposureControl {
    fn is_sdk(&self) -> bool {
        *self == Self::Sdk
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub directory: PathBuf,
    pub interval_ms: u64,
    pub jpeg_quality: u8,
    pub writer_queue_capacity: usize,
    pub keep_latest: bool,
    /// Maximum managed capture bytes to retain. `None` disables this limit.
    pub retention_max_bytes: Option<u64>,
    /// Minimum free bytes to preserve on the capture volume. `None` disables this limit.
    pub retention_min_free_bytes: Option<u64>,
    /// Maximum capture age in days. Zero disables age-based deletion.
    pub retention_days: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("captures"),
            interval_ms: 10_000,
            jpeg_quality: 88,
            writer_queue_capacity: 2,
            keep_latest: true,
            retention_max_bytes: None,
            retention_min_free_bytes: None,
            retention_days: 14,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadConfig {
    pub enabled: bool,
    pub endpoint: Option<String>,
    /// Environment-variable name holding a bearer token; secrets stay out of TOML.
    pub bearer_token_env: Option<String>,
    pub queue_capacity: usize,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            bearer_token_env: None,
            queue_capacity: 32,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_path: Option<PathBuf>,
    pub enabled: bool,
    pub segment_seconds: u32,
    pub frames_per_second: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            ffmpeg_path: None,
            enabled: false,
            segment_seconds: 300,
            frames_per_second: 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub listen: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:4762".to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Validation(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_policy_only_reopens_for_device_or_image_layout_changes() {
        let original = Config::default();
        assert!(!original.requires_camera_restart(&original));
        assert!(!original.requires_recording_reload(&original));
        let mut next = original.clone();
        next.camera.max_exposure_us = 30_000_000;
        next.camera.max_gain = 200;
        next.camera.exposure_control = ExposureControl::Adaptive;
        next.capture.interval_ms = 5000;
        next.capture.jpeg_quality = 90;
        assert!(!original.requires_camera_restart(&next));
        assert!(!original.requires_recording_reload(&next));
        next.capture.retention_max_bytes = Some(1_000_000);
        next.upload.endpoint = Some("https://example.test/latest".into());
        next.upload.enabled = true;
        assert!(!original.requires_camera_restart(&next));
        assert!(original.requires_recording_reload(&next));
        for change in [
            |c: &mut Config| c.camera.camera_id = Some(1),
            |c: &mut Config| c.camera.name_contains = Some("ASI662MC".into()),
            |c: &mut Config| c.camera.raw16 = true,
            |c: &mut Config| c.camera.width = Some(1920),
            |c: &mut Config| c.camera.height = Some(1080),
            |c: &mut Config| c.camera.bin = 2,
        ] {
            let mut changed = original.clone();
            change(&mut changed);
            assert!(original.requires_camera_restart(&changed));
        }
    }

    #[test]
    fn video_requires_explicit_executable_and_bounded_sampling() {
        let mut config = Config::default();
        config.video.enabled = true;
        assert!(config.validate().is_err());
        config.video.ffmpeg_path = Some(PathBuf::from("ffmpeg.exe"));
        assert!(config.validate().is_err());
        config.video.ffmpeg_path = Some(std::env::current_dir().unwrap().join("ffmpeg.exe"));
        config.validate().unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.video.ffmpeg_path, config.video.ffmpeg_path);
        config.video.segment_seconds = 601;
        assert!(config.validate().is_err());
        config.video.segment_seconds = 60;
        config.video.frames_per_second = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn advanced_camera_fields_are_opt_in_and_roundtrip() {
        let defaults = toml::to_string(&Config::default()).unwrap();
        assert!(!defaults.contains("exposure_control"));
        assert!(!defaults.contains("raw16"));
        let config: Config = toml::from_str(
            "[camera]\nexposure_control = 'adaptive'\nraw16 = true\nmax_exposure_us = 120000000",
        )
        .unwrap();
        config.validate().unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert!(restored.camera.raw16);
        assert_eq!(restored.camera.exposure_control, ExposureControl::Adaptive);
        assert_eq!(restored.camera.max_exposure_us, 120_000_000);
        assert!(toml::from_str::<Config>("[camera]\nexposure_control='typo'").is_err());
    }

    #[test]
    fn defaults_are_valid() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.capture.retention_max_bytes, None);
        assert_eq!(config.capture.retention_min_free_bytes, None);
    }

    #[test]
    fn exposure_defaults_allow_long_nights_without_rewriting_explicit_limits() {
        let fresh: Config = toml::from_str("").unwrap();
        assert_eq!(fresh.camera.max_exposure_us, 60_000_000);
        for maximum in [5_000_000, 30_000_000, 60_000_000] {
            let config: Config =
                toml::from_str(&format!("[camera]\nmax_exposure_us = {maximum}\n")).unwrap();
            config.validate().unwrap();
            assert_eq!(config.camera.max_exposure_us, maximum);
        }
        for (minimum, maximum) in [(-1, 60_000_000), (0, 0), (-2, -1), (100, 32)] {
            let mut config = Config::default();
            config.camera.min_exposure_us = minimum;
            config.camera.max_exposure_us = maximum;
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn retention_limits_round_trip_and_zero_days_disable_the_age_rule() {
        let mut config = Config::default();
        config.capture.retention_days = 0;
        config.capture.retention_max_bytes = Some(50_000_000_000);
        config.capture.retention_min_free_bytes = Some(5_000_000_000);
        config.validate().unwrap();

        let serialized = toml::to_string(&config).unwrap();
        let round_trip: Config = toml::from_str(&serialized).unwrap();
        round_trip.validate().unwrap();
        assert_eq!(round_trip.capture.retention_days, 0);
        assert_eq!(round_trip.capture.retention_max_bytes, Some(50_000_000_000));
        assert_eq!(
            round_trip.capture.retention_min_free_bytes,
            Some(5_000_000_000)
        );
    }

    #[test]
    fn omitted_retention_byte_limits_default_to_none() {
        let config: Config = toml::from_str(
            r#"
            [capture]
            retention_days = 0
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.capture.retention_days, 0);
        assert_eq!(config.capture.retention_max_bytes, None);
        assert_eq!(config.capture.retention_min_free_bytes, None);
    }

    #[test]
    fn zero_retention_byte_limits_are_rejected() {
        let mut config = Config::default();
        config.capture.retention_max_bytes = Some(0);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation(
                "capture.retention_max_bytes must be greater than zero when set"
            ))
        ));

        config.capture.retention_max_bytes = Some(1);
        config.capture.retention_min_free_bytes = Some(0);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation(
                "capture.retention_min_free_bytes must be greater than zero when set"
            ))
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = toml::from_str::<Config>("mystery = true").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn uncharacterized_color_binning_is_rejected() {
        let mut config = Config::default();
        config.camera.bin = 2;
        assert!(config.validate().is_err());
    }

    #[test]
    fn enabled_upload_requires_a_bounded_http_endpoint() {
        let mut config = Config::default();
        config.upload.enabled = true;
        assert!(config.validate().is_err());

        config.upload.endpoint = Some("ftp://example.test/frame.jpg".to_owned());
        assert!(config.validate().is_err());

        config.upload.endpoint = Some("https://example.test/camera/latest#secret".to_owned());
        assert!(config.validate().is_err());

        config.upload.endpoint = Some("https://user:secret@example.test/camera/latest".to_owned());
        assert!(config.validate().is_err());

        config.upload.endpoint = Some("https://example.test/camera/latest".to_owned());
        config.validate().unwrap();
    }

    #[test]
    fn upload_endpoint_normalization_is_ascii_and_transport_safe() {
        let normalized = normalize_upload_endpoint("https://例え.テスト/snow camera").unwrap();
        assert!(normalized.is_ascii());
        assert!(normalized.starts_with("https://xn--"));
        assert!(normalized.ends_with("/snow%20camera"));
    }

    #[test]
    fn upload_queue_and_bearer_reference_are_validated() {
        let mut config = Config::default();
        config.upload.queue_capacity = 0;
        assert!(config.validate().is_err());

        config.upload.queue_capacity = 1;
        config.upload.endpoint = Some("https://example.test/camera/latest".to_owned());
        config.upload.bearer_token_env = Some(" AUTOPIERCAM_TOKEN".to_owned());
        assert!(config.validate().is_err());

        config.upload.bearer_token_env = Some("AUTOPIERCAM\nTOKEN".to_owned());
        assert!(config.validate().is_err());

        config.upload.bearer_token_env = Some("AUTOPIERCAM_TOKEN".to_owned());
        config.validate().unwrap();

        config.upload.endpoint = Some("http://127.0.0.1:4762/camera/latest".to_owned());
        assert!(config.validate().is_err());
    }
}
