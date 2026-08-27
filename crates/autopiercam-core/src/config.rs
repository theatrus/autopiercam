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
        if self.upload.enabled && self.upload.endpoint.is_none() {
            return Err(ConfigError::Validation(
                "upload.endpoint is required when upload.enabled is true",
            ));
        }
        Ok(())
    }
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
}
