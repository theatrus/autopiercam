#[path = "../windows_resources.rs"]
mod windows_resources;

fn main() {
    windows_resources::compile(
        "AutoPierCam.Tray",
        "AutoPierCam Windows notification-area host",
        "autopiercam-tray.exe",
        true,
    );
}
