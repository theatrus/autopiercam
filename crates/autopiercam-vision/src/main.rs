use anyhow::Result;
use autopiercam_vision::dataset::{self, ImportOptions};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Local pier-camera datasets and CPU classification. Never opens cameras or sends images."
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Copy complete images, deduplicate and leave them unlabeled. Safe to rerun.
    Import {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        site: String,
        #[arg(long)]
        camera: String,
        /// Observing night/session ID. Keep related images together during training splits.
        #[arg(long)]
        group: String,
        #[arg(long, default_value_t = 300)]
        interval_seconds: u64,
        #[arg(long, default_value_t = 30)]
        min_age_seconds: u64,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Build an offline HTML review page in the dataset directory.
    Review {
        #[arg(long)]
        dataset: PathBuf,
    },
    /// Merge review edits. Rejects conflicting newer labels or invalid batches.
    Label {
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long)]
        file: PathBuf,
    },
    /// Verify image checksums and summarize labeling/group coverage.
    Check {
        #[arg(long)]
        dataset: PathBuf,
    },
    #[cfg(feature = "onnx")]
    /// Run a trusted, checksum-pinned ONNX classifier on one image.
    Infer {
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        image: PathBuf,
        /// Optional JSON normalized ROI {left, top, right, bottom}.
        #[arg(long)]
        roi: Option<PathBuf>,
    },
    #[cfg(feature = "onnx")]
    /// Display-only sky worker: bounded JPEG requests on stdin, JSON lines on stdout.
    ExperimentalStream {
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        spec: PathBuf,
    },
}
fn main() -> Result<()> {
    match Args::parse().command {
        Command::Import {
            source,
            output,
            site,
            camera,
            group,
            interval_seconds,
            min_age_seconds,
            limit,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&dataset::import(&ImportOptions {
                    source,
                    output,
                    site,
                    camera,
                    group,
                    interval_seconds,
                    min_age_seconds,
                    limit
                })?)?
            );
        }
        Command::Review { dataset } => println!("{}", dataset::review(&dataset)?.display()),
        Command::Label { dataset, file } => println!(
            "Applied {} annotations",
            dataset::apply_labels(&dataset, &file)?
        ),
        Command::Check { dataset } => println!(
            "{}",
            serde_json::to_string_pretty(&dataset::check(&dataset)?)?
        ),
        #[cfg(feature = "onnx")]
        Command::ExperimentalStream { model, spec } => {
            use autopiercam_vision::{
                model::{ModelSpec, Task},
                read_bounded,
                runtime::Classifier,
                stream,
            };
            let spec: ModelSpec = serde_json::from_slice(&read_bounded(&spec, 64 * 1024)?)?;
            anyhow::ensure!(
                spec.task == Task::Sky,
                "Experimental Viewer requires a sky model"
            );
            let model_id = spec.model_id.clone();
            let classifier = Classifier::load(&model, spec)?;
            let mut input = std::io::stdin().lock();
            let mut output = std::io::stdout().lock();
            stream::write_response(
                &mut output,
                &serde_json::json!({
                    "ready": true, "protocol": 1, "task": "sky", "model_id": model_id,
                }),
            )?;
            while let Some(bytes) = stream::read_frame(&mut input)? {
                match classifier.predict(&bytes, Default::default()) {
                    Ok(prediction) => stream::write_response(&mut output, &prediction)?,
                    Err(error) => stream::write_response(
                        &mut output,
                        &serde_json::json!({"error": error.to_string()}),
                    )?,
                }
            }
        }
        #[cfg(feature = "onnx")]
        Command::Infer {
            model,
            spec,
            image,
            roi,
        } => {
            use autopiercam_vision::{MAX_IMAGE_BYTES, read_bounded, runtime::Classifier};
            let spec = serde_json::from_slice(&read_bounded(&spec, 64 * 1024)?)?;
            let roi = roi
                .map(|path| -> Result<_> {
                    Ok(serde_json::from_slice(&read_bounded(&path, 4096)?)?)
                })
                .transpose()?
                .unwrap_or_default();
            let classifier = Classifier::load(&model, spec)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &classifier.predict(&read_bounded(&image, MAX_IMAGE_BYTES)?, roi)?
                )?
            );
        }
    }
    Ok(())
}
