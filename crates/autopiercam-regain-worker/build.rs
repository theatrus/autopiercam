#[path = "../windows_resources.rs"]
mod windows_resources;
fn main() {
    windows_resources::compile(
        "AutoPierCam.Regain",
        "AutoPierCam Regain camera worker",
        "regain-device.exe",
        false,
    );
}
