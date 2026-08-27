use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub camera: CameraConfig,
    pub capture: CaptureConfig,
    pub upload: UploadConfig,
    pub video: VideoConfig,
    pub api: ApiConfig,
}

impl Config {
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
        if self.camera.max_exposure_us < self.camera.min_exposure_us {
            return Err(ConfigError::Validation(
                "camera.max_exposure_us must be >= camera.min_exposure_us",
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
        if self.upload.queue_capacity == 0 {
            return Err(ConfigError::Validation(
                "upload.queue_capacity must be greater than zero",
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
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
            camera_id: None,
            name_contains: None,
            width: None,
            height: None,
            bin: 1,
            min_exposure_us: 100,
            max_exposure_us: 5_000_000,
            max_gain: 300,
            target_brightness: 100,
            settle_frames: 6,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub directory: PathBuf,
    pub interval_ms: u64,
    pub jpeg_quality: u8,
    pub writer_queue_capacity: usize,
    pub keep_latest: bool,
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
            retention_days: 14,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    pub enabled: bool,
    pub segment_seconds: u32,
    pub frames_per_second: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            segment_seconds: 300,
            frames_per_second: 4,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
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
