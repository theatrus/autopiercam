#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
mod worker;

#[cfg(target_os = "windows")]
fn main() {
    windows::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "autopiercam-tray is currently Windows-only; use the headless `autopiercam` capture agent on this platform."
    );
}
