//! Camera-only entry point for the pinned upstream Regain drivers.
//! No local SDK bindings, protocol implementation, or USB recovery commands.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let vendor = args.next().unwrap_or_default();
    if vendor == "--version" {
        println!(
            "autopiercam-regain-worker {} (Regain 98302af3c1c8)",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let device = args.next().unwrap_or_default();
    match (vendor.as_str(), device.as_str()) {
        ("zwo", "camera-sdk") => regain_zwo::asi::sdk::run(args.collect()),
        ("zwo", "camera-direct") => regain_zwo::asi::direct::run(args.collect()),
        _ => anyhow::bail!("Usage: regain-device zwo camera-sdk|camera-direct [arguments]"),
    }
}
