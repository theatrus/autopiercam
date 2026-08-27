#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod ipc;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
mod worker;

#[cfg(target_os = "windows")]
use clap::Parser;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

#[cfg(target_os = "windows")]
#[derive(Debug, Parser)]
#[command(version, about = "AutoPierCam Windows notification-area host")]
struct Options {
    /// Complete capture configuration used by the camera-owning worker.
    #[arg(long, env = "AUTOPIERCAM_CONFIG", default_value = "autopiercam.toml")]
    config: PathBuf,

    /// Explicit path to ASICamera2.dll.
    #[arg(long, env = "AUTOPIERCAM_ASI_SDK_PATH")]
    sdk: Option<PathBuf>,
}

#[cfg(target_os = "windows")]
fn main() {
    windows::run(Options::parse());
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "autopiercam-tray is currently Windows-only; use the headless `autopiercam` capture agent on this platform."
    );
}
