//! Windows Media Foundation H.264 encoding. No camera, shell, or external codec executable.
//! Shared by elapsed-time security recording and fixed-rate daily timelapses.
use anyhow::anyhow;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
mod media_api;
#[cfg(windows)]
mod windows_encoder;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sample {
    pub file: String,
    /// 100-nanosecond units; adjacent rational boundaries avoid FPS rounding drift.
    pub duration: i64,
}

pub(crate) fn write_manifest(directory: &Path, samples: &[Sample]) -> Result<()> {
    validate(samples)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, samples)?;
    file.as_file().sync_all()?;
    file.persist(directory.join("samples.json"))?;
    Ok(())
}

fn validate(samples: &[Sample]) -> Result<()> {
    ensure!(
        !samples.is_empty() && samples.len() <= 1560,
        "native video needs 1..=1560 samples"
    );
    let mut total = 0_i64;
    for sample in samples {
        ensure!(
            sample.file.ends_with(".jpg")
                && sample.file.len() <= 32
                && sample
                    .file
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                && !sample.file.contains(".."),
            "invalid native video sample name"
        );
        ensure!(
            (1..=6_000_000_000).contains(&sample.duration),
            "invalid native video sample duration"
        );
        total = total
            .checked_add(sample.duration)
            .context("native video duration overflow")?;
    }
    ensure!(
        total <= 6_100_000_000,
        "native video exceeds segment duration bound"
    );
    Ok(())
}

pub(crate) fn executable() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let mut path = current.with_file_name("autopiercam.exe");
    // Cargo examples/tests are one directory below the built application.
    // Never search PATH, the installed copy, or arbitrary parent directories.
    if !path.is_file()
        && current
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "examples" || name == "deps")
    {
        path = current
            .parent()
            .and_then(Path::parent)
            .context("missing development binary directory")?
            .join("autopiercam.exe");
    }
    ensure!(
        path.is_file(),
        "native encoder executable missing: {}",
        path.display()
    );
    Ok(path)
}

/// Hardware-free internal command: native codec execution is isolated here so
/// the parent can kill and reap a hung encoder without hanging camera shutdown.
pub fn encode(directory: &Path, output: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::io::Read;
        let mut json = Vec::new();
        fs::File::open(directory.join("samples.json"))?
            .take(128 * 1024 + 1)
            .read_to_end(&mut json)?;
        ensure!(json.len() <= 128 * 1024, "oversized native video manifest");
        let samples: Vec<Sample> = serde_json::from_slice(&json)?;
        validate(&samples)?;
        windows_encoder::encode(directory, &samples, output)
            .context("Windows native H.264 encoding failed; Windows Media Foundation and its H.264 encoder are required")
    }
    #[cfg(not(windows))]
    {
        let _ = (directory, output);
        anyhow::bail!("native video requires Windows Media Foundation")
    }
}

/// Decode every video sample through the Windows decoder, for hardware-free QA.
pub fn verify(path: &Path) -> Result<usize> {
    #[cfg(windows)]
    {
        Ok(windows_encoder::decode(path)?.len())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        anyhow::bail!("native video verification requires Windows")
    }
}

/// Encode privately and publish atomically without overwriting any prior movie.
/// The internal executable is AutoPierCam itself, never PATH-resolved.
#[doc(hidden)]
pub fn encode_atomic(
    executable: &Path,
    input: &Path,
    output: &Path,
    timeout: std::time::Duration,
) -> Result<()> {
    encode_atomic_checked(executable, input, output, timeout, || Ok(()))
}

pub(crate) fn encode_atomic_checked(
    executable: &Path,
    input: &Path,
    output: &Path,
    timeout: Duration,
    check_space: impl FnMut() -> Result<()>,
) -> Result<()> {
    let temporary = crate::temporary_artifact_path(output)?;
    let result = (|| {
        run_encoder_checked(
            executable,
            input,
            &temporary,
            &std::sync::atomic::AtomicBool::new(false),
            timeout,
            check_space,
        )?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temporary)?;
        ensure!(
            file.metadata()?.len() > 0,
            "native encoder produced an empty movie"
        );
        file.sync_all()?;
        drop(file);
        crate::publish_temporary_artifact(&temporary, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn run_encoder_checked(
    executable: &Path,
    input: &Path,
    output: &Path,
    stop: &AtomicBool,
    timeout: Duration,
    mut check_space: impl FnMut() -> Result<()>,
) -> Result<()> {
    check_space()?;
    let mut command = Command::new(executable);
    command
        .arg("encode-video")
        .arg("--input-dir")
        .arg(input)
        .arg("--output")
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000 | 0x0000_4000); // hidden, below-normal priority
    }
    let mut child = command
        .spawn()
        .context("starting Windows native video encoder")?;
    let mut stderr = child.stderr.take().context("encoder stderr missing")?;
    let reader = thread::spawn(move || {
        // Drain everything to avoid a full pipe deadlocking the encoder, retain
        // only the bounded tail for actionable HRESULT/codec errors.
        let mut tail = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(count) = stderr.read(&mut buffer) {
            if count == 0 {
                break;
            }
            tail.extend_from_slice(&buffer[..count]);
            if tail.len() > 16384 {
                tail.drain(..tail.len() - 16384);
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let started = Instant::now();
    let mut checked = Instant::now();
    let result = loop {
        if stop.load(Ordering::Acquire) || started.elapsed() >= timeout {
            break Err(anyhow!(
                "native video encoder cancelled or exceeded deadline"
            ));
        }
        if checked.elapsed() >= Duration::from_secs(1) {
            if let Err(error) = check_space() {
                break Err(error);
            }
            checked = Instant::now();
        }
        match child.try_wait() {
            Ok(Some(status)) => break check_space().map(|()| status),
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => break Err(error.into()),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let errors = reader.join().unwrap_or_default();
    ensure!(
        result?.success(),
        "Windows native video encoding failed: {}",
        errors.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(windows)]
    fn storage_pressure_prevents_spawn_and_cleans_output_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("clip.mp4");
        let error = encode_atomic_checked(
            Path::new("must-not-launch.exe"),
            root.path(),
            &output,
            Duration::from_secs(20),
            || Err(crate::retention::VideoStoragePressure.into()),
        )
        .unwrap_err();
        assert!(error.is::<crate::retention::VideoStoragePressure>());
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
        image::RgbImage::from_pixel(64, 48, image::Rgb([20, 40, 60]))
            .save(root.path().join("sample.jpg"))
            .unwrap();
        write_manifest(
            root.path(),
            &[Sample {
                file: "sample.jpg".into(),
                duration: 1_000_000,
            }],
        )
        .unwrap();
        let mut calls = 0;
        let error = encode_atomic_checked(
            &executable().unwrap(),
            root.path(),
            &output,
            Duration::from_secs(20),
            || {
                calls += 1;
                if calls == 1 {
                    Ok(())
                } else {
                    Err(crate::retention::VideoStoragePressure.into())
                }
            },
        )
        .unwrap_err();
        assert!(error.is::<crate::retention::VideoStoragePressure>());
        assert!(calls >= 2);
        assert!(!output.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
    #[test]
    fn manifest_rejects_paths_empty_jobs_and_unbounded_durations() {
        assert!(validate(&[]).is_err());
        for name in [
            "../x.jpg",
            "C:/x.jpg",
            "x\\y.jpg",
            "/x.jpg",
            "x.jpg:stream",
            "x.png",
        ] {
            assert!(
                validate(&[Sample {
                    file: name.into(),
                    duration: 1
                }])
                .is_err()
            );
        }
        for duration in [0, -1, i64::MAX] {
            assert!(
                validate(&[Sample {
                    file: "x.jpg".into(),
                    duration
                }])
                .is_err()
            );
        }
        assert!(
            validate(&[Sample {
                file: "sample-000001.jpg".into(),
                duration: 416667
            }])
            .is_ok()
        );
        let mut long_day = (0..1560)
            .map(|i| Sample {
                file: format!("{i:04}.jpg"),
                duration: 416667,
            })
            .collect::<Vec<_>>();
        assert!(validate(&long_day).is_ok());
        long_day.push(Sample {
            file: "1560.jpg".into(),
            duration: 416667,
        });
        assert!(validate(&long_day).is_err());
    }
}
