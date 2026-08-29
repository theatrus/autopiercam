#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod ipc;
#[cfg(target_os = "windows")]
mod preview;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
mod worker;

#[cfg(target_os = "windows")]
use clap::Parser;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

#[cfg(target_os = "windows")]
const PRODUCT_DATA_DIRECTORY: &str = "AutoPierCam";
#[cfg(target_os = "windows")]
const CONFIG_FILE_NAME: &str = "autopiercam.toml";

#[cfg(target_os = "windows")]
#[derive(Debug, Parser)]
#[command(version, about = "AutoPierCam Windows notification-area host")]
struct Options {
    /// Complete capture configuration used by the camera-owning worker.
    #[arg(long, env = "AUTOPIERCAM_CONFIG", default_value_os_t = default_config_path())]
    config: PathBuf,

    /// Explicit path to ASICamera2.dll.
    #[arg(long, env = "AUTOPIERCAM_ASI_SDK_PATH")]
    sdk: Option<PathBuf>,
}

#[cfg(target_os = "windows")]
fn main() {
    windows::run(Options::parse());
}

#[cfg(target_os = "windows")]
fn default_config_path() -> PathBuf {
    default_config_path_from(std::env::var_os("LOCALAPPDATA"))
}

#[cfg(target_os = "windows")]
fn default_config_path_from(local_app_data: Option<std::ffi::OsString>) -> PathBuf {
    local_app_data
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(PRODUCT_DATA_DIRECTORY)
        .join(CONFIG_FILE_NAME)
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "autopiercam-tray is currently Windows-only; use the headless `autopiercam` capture agent on this platform."
    );
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_kept_in_the_current_users_local_data_directory() {
        let path = default_config_path_from(Some(std::ffi::OsString::from(
            r"C:\Users\Alice\AppData\Local",
        )));

        assert_eq!(
            path,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\AutoPierCam\autopiercam.toml")
        );
    }

    #[test]
    fn missing_local_app_data_has_a_predictable_portable_fallback() {
        assert_eq!(
            default_config_path_from(None),
            PathBuf::from(".")
                .join("AutoPierCam")
                .join(CONFIG_FILE_NAME)
        );
    }
}
