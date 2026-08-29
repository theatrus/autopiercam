#[path = "../windows_resources.rs"]
mod windows_resources;

fn main() {
    windows_resources::compile(
        "AutoPierCam.Capture",
        "AutoPierCam capture engine and diagnostic CLI",
        "autopiercam.exe",
        false,
    );
}
