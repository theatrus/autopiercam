//! Offline training bridge: use the runtime's exact decoder/resize/normalization.
//! Input is a trusted local job, not a network-facing API. Never opens a camera.
use anyhow::{Result, ensure};
use autopiercam_vision::{
    MAX_IMAGE_BYTES,
    model::ModelSpec,
    preprocess::{self, Roi},
    read_bounded, sha256,
};
use serde::Deserialize;
use std::{fs, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    spec: ModelSpec,
    roi: Roi,
    images: Vec<TrainingImage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrainingImage {
    id: String,
    path: PathBuf,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2,
        "Usage: training_export job.json NEW_OUTPUT_DIRECTORY"
    );
    let job: Job =
        serde_json::from_slice(&read_bounded(&PathBuf::from(&args[0]), 32 * 1024 * 1024)?)?;
    job.spec.validate()?;
    job.roi.validate()?;
    let output = PathBuf::from(&args[1]);
    // Do not replace a previous run's cache. The caller must choose a fresh path.
    fs::create_dir(&output)?;
    fs::write(output.join(".gitignore"), "*\n")?;
    let mut seen = std::collections::BTreeSet::new();
    for image in job.images {
        let bytes = read_bounded(&image.path, MAX_IMAGE_BYTES)?;
        ensure!(
            sha256(&bytes) == image.id,
            "Image checksum mismatch: {}",
            image.path.display()
        );
        ensure!(seen.insert(image.id.clone()), "Duplicate image ID");
        let tensor = preprocess::prepare(&bytes, &job.spec, job.roi)?;
        let encoded: Vec<u8> = tensor.iter().flat_map(|v| v.to_le_bytes()).collect();
        fs::write(output.join(format!("{}.f32", image.id)), encoded)?;
    }
    println!("Exported {} tensors (little-endian f32 NCHW)", seen.len());
    Ok(())
}
