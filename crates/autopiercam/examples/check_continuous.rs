//! Explicit operator-run hardware check; never run by automated tests or CI.
//! Close other camera owners first. No files, messages, configuration or USB resets.
use anyhow::{Context, Result, ensure};
use autopiercam_camera::{ControlType, Driver, ImageType, Roi};
use autopiercam_core::config::CameraDriver;
use clap::Parser;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Options {
    /// Required acknowledgement that this opens the explicitly selected camera.
    #[arg(long, required = true)]
    hardware: bool,
    #[arg(long, value_parser = ["ZWO ASI662MC", "ZWO ASI676MC"])]
    model: String,
    #[arg(long, value_parser = ["sdk", "direct"])]
    backend: String,
    #[arg(long)]
    sdk: Option<PathBuf>,
}

fn main() -> Result<()> {
    let options = Options::parse();
    let driver = Arc::new(Driver::new(
        options.sdk.as_deref(),
        if options.backend == "sdk" {
            CameraDriver::ZwoSdk
        } else {
            CameraDriver::ZwoDirect
        },
        None,
    )?);
    let mut matches: Vec<_> = driver
        .cameras()?
        .into_iter()
        .filter(|c| c.name == options.model)
        .collect();
    ensure!(
        matches.len() == 1,
        "selected model must match exactly one camera"
    );
    let info = matches.remove(0);
    let mut camera = driver.open(info.clone())?;
    camera.set_roi(Roi {
        width: info.max_width,
        height: info.max_height,
        bin: 1,
        image_type: ImageType::Raw16,
    })?;
    camera.set_max_fps(0.5)?;
    camera.start_capture()?;
    let mut pixels = Vec::new();
    let mut previous_exposure = 6_400_000;
    for (index, exposure) in [6_400_000, 25_000_000, 25_000_000, 60_000_000, 234_000]
        .into_iter()
        .enumerate()
    {
        camera.set_control(ControlType::EXPOSURE, exposure, false)?;
        camera.set_control(
            ControlType::GAIN,
            if index % 2 == 0 { 300 } else { 270 },
            false,
        )?;
        let began = Instant::now();
        for frame in 0..2 {
            let waiting = Instant::now();
            loop {
                match camera.poll_frame(&mut pixels, 100) {
                    Ok(meta) => {
                        ensure!(
                            meta.width == info.max_width && meta.height == info.max_height,
                            "wrong geometry"
                        );
                        ensure!(
                            pixels.len() == info.max_width as usize * info.max_height as usize * 2,
                            "wrong frame length"
                        );
                        println!(
                            "model={} backend={} exposure_us={exposure} frame={frame} elapsed_seconds={:.3} pacing_seconds={:.3}",
                            options.model,
                            options.backend,
                            began.elapsed().as_secs_f64(),
                            camera.pacing_allowance().as_secs_f64()
                        );
                        break;
                    }
                    Err(error) if error.is_timeout() => {
                        // Same base as the production frame watchdog; refresh its
                        // intentional transition/pacing allowance after each poll.
                        let budget = Duration::from_micros(exposure.max(previous_exposure) as u64)
                            .saturating_mul(2)
                            + Duration::from_secs(5)
                            + camera.pacing_allowance();
                        ensure!(
                            waiting.elapsed() < budget,
                            "production-equivalent frame deadline exceeded"
                        );
                    }
                    Err(error) => return Err(error).context("continuous hardware capture"),
                }
            }
        }
        previous_exposure = exposure;
    }
    camera.stop_capture()?;
    Ok(())
}
