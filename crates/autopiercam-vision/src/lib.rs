//! Offline image analysis. No camera, network, or application-control dependency.
//! Model inference is opt-in; no trained classifier is bundled.

pub mod dataset;
pub mod model;
pub mod preprocess;
#[cfg(feature = "onnx")]
pub mod runtime;

use anyhow::{Context, Result, ensure};
use std::{fs::File, io::Read, path::Path};

pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("Open {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= limit,
        "File exceeds {limit} bytes"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "File grew beyond {limit} bytes"
    );
    Ok(bytes)
}

pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
