//! Camera-only entry point for the pinned upstream Regain drivers.
//! Camera and device-scoped USB recovery use the pinned upstream implementation.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let vendor = args.next().unwrap_or_default();
    if vendor == "--version" {
        println!(
            "autopiercam-regain-worker {} (Regain f0c91226523f)",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if vendor == "usb" {
        // Automatic recovery must never open a UAC prompt on an unattended rig.
        // Run the host elevated explicitly; do not install a privileged service.
        #[cfg(windows)]
        anyhow::ensure!(
            unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() } != 0,
            "USB recovery requires an administrator-started AutoPierCam agent; no reset was attempted"
        );
        anyhow::ensure!(
            cfg!(windows),
            "AutoPierCam USB port recovery currently requires Windows"
        );
        return regain_transport::usb::run(args.collect());
    }
    let device = args.next().unwrap_or_default();
    match (vendor.as_str(), device.as_str()) {
        ("zwo", "camera-sdk") => regain_zwo::asi::sdk::run(args.collect()),
        ("zwo", "camera-direct") => regain_zwo::asi::direct::run(args.collect()),
        _ => anyhow::bail!("Usage: regain-device zwo camera-sdk|camera-direct [arguments]"),
    }
}
