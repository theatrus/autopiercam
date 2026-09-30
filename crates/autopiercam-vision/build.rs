#[path = "../windows_resources.rs"]
mod windows_resources;

fn main() {
    windows_resources::compile(
        "AutoPierCam.Vision",
        "AutoPierCam experimental local image analysis",
        "autopiercam-vision.exe",
        false,
    );
}
