use anyhow::Result;
use autopiercam::{
    archive_upload_ledger, list_cameras, migrate_upload_ledger, probe_camera, run_agent, snapshot,
};
use autopiercam_asi::Sdk;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "windows")]
mod control_client;

#[derive(Debug, Parser)]
#[command(version, about = "Unattended capture for ZWO ASI planetary cameras")]
struct Cli {
    /// Explicit path to ASICamera2.dll (or the platform equivalent).
    #[arg(long, global = true, env = "AUTOPIERCAM_ASI_SDK_PATH")]
    sdk: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enumerate connected cameras without opening them.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Open a camera, clear persisted SDK dark subtraction, and print controls.
    Probe {
        #[arg(long)]
        camera_id: Option<i32>,
    },
    /// Capture and debayer one full-resolution frame.
    Snapshot {
        #[arg(long)]
        camera_id: Option<i32>,
        #[arg(long, default_value = "capture.jpg")]
        output: PathBuf,
        #[arg(long, default_value_t = 6)]
        settle_frames: u32,
        #[arg(long, default_value_t = 5_000_000)]
        max_exposure_us: i64,
        #[arg(long, default_value_t = 300)]
        max_gain: i64,
        #[arg(long, default_value_t = 100)]
        target_brightness: i64,
        #[arg(long, default_value_t = 88)]
        jpeg_quality: u8,
    },
    /// Run the continuous headless capture worker described by a TOML file.
    Run {
        #[arg(long, default_value = "autopiercam.toml")]
        config: PathBuf,
        /// Stop after queueing this many stills; useful for installation tests.
        #[arg(long)]
        max_frames: Option<u64>,
    },
    /// Perform offline maintenance on the durable upload ledger.
    UploadLedger {
        #[command(subcommand)]
        command: UploadLedgerCommand,
    },
    /// Ask the current user's tray agent to shut down cleanly and wait for it.
    ShutdownAgent {
        /// Treat an agent that is not running as a successful no-op.
        #[arg(long)]
        if_running: bool,
        /// Maximum time for the request and orderly process shutdown.
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=300))]
        timeout_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum UploadLedgerCommand {
    /// Migrate an exact v3 ledger to v4, or verify an exact v4 ledger.
    Migrate {
        #[arg(long)]
        config: PathBuf,
    },
    /// Archive a drained v4 ledger and retire the active database.
    Archive {
        #[arg(long)]
        config: PathBuf,
        /// Immutable 32-character ledger ID shown by the outbox or migrate command.
        #[arg(long)]
        expected_ledger_id: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let command = match cli.command {
        Command::UploadLedger {
            command: UploadLedgerCommand::Migrate { config },
        } => {
            let report = migrate_upload_ledger(&config)?;
            if report.migrated {
                println!(
                    "migrated upload ledger v3 -> v4: {}\nledger_id={}",
                    report.database_path.display(),
                    report.ledger_id
                );
            } else {
                println!(
                    "verified upload ledger v4 (no changes): {}\nledger_id={}",
                    report.database_path.display(),
                    report.ledger_id
                );
            }
            return Ok(());
        }
        Command::UploadLedger {
            command:
                UploadLedgerCommand::Archive {
                    config,
                    expected_ledger_id,
                },
        } => {
            let report = archive_upload_ledger(&config, &expected_ledger_id)?;
            println!(
                "archived upload ledger {}\narchive={}\nretired={}\nsha256={}",
                report.ledger_id,
                report.archive_path.display(),
                report.retired_path.display(),
                report.sha256
            );
            return Ok(());
        }
        Command::ShutdownAgent {
            if_running,
            timeout_seconds,
        } => {
            #[cfg(target_os = "windows")]
            {
                let stopped = control_client::shutdown_agent(
                    std::time::Duration::from_secs(timeout_seconds),
                    if_running,
                )?;
                if stopped {
                    println!("AutoPierCam stopped cleanly.");
                } else {
                    println!("AutoPierCam was not running.");
                }
                return Ok(());
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (if_running, timeout_seconds);
                anyhow::bail!("shutdown-agent is currently supported only on Windows");
            }
        }
        command => command,
    };
    let sdk = Arc::new(match cli.sdk {
        Some(path) => Sdk::load(path)?,
        None => Sdk::load_default()?,
    });
    info!(version = %sdk.version(), path = %sdk.path().display(), "loaded ZWO ASI SDK");

    match command {
        Command::List { json } => list_cameras(&sdk, json),
        Command::Probe { camera_id } => probe_camera(&sdk, camera_id),
        Command::Snapshot {
            camera_id,
            output,
            settle_frames,
            max_exposure_us,
            max_gain,
            target_brightness,
            jpeg_quality,
        } => snapshot(
            &sdk,
            camera_id,
            &output,
            settle_frames,
            max_exposure_us,
            max_gain,
            target_brightness,
            jpeg_quality,
        ),
        Command::Run { config, max_frames } => run_agent(&sdk, &config, max_frames),
        Command::UploadLedger { .. } | Command::ShutdownAgent { .. } => {
            unreachable!("non-camera command returned before SDK load")
        }
    }
}
