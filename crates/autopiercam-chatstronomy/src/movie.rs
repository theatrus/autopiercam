//! Private disk snapshots. Only bounded chunks cross the async HTTP boundary.
use anyhow::{Result, ensure};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex},
};

pub(crate) const CHUNK_BYTES: usize = 64 * 1024;

struct SnapshotFile {
    // Drop the private file before removing its directory. Outstanding
    // blocking reads retain this entire owner, including during cancellation.
    file: Mutex<File>,
    _path: tempfile::TempPath,
    _directory: tempfile::TempDir,
}

pub(crate) struct Movie {
    inner: Arc<SnapshotFile>,
    pub len: u64,
}

impl Movie {
    pub fn snapshot(path: &Path, still_allowed: impl Fn() -> bool) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Timelapse must be a regular file"
        );
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Timelapse links are not permitted"
            );
            // Lock the source against writes for the copy, but allow retention to
            // unlink it. Open the reparse point itself, never follow a swapped link.
            options.share_mode(1 | 4).custom_flags(0x0020_0000);
        }
        let mut source = options.open(path)?;
        let before = source.metadata()?;
        ensure!(before.is_file(), "Timelapse must be a regular file");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                before.file_attributes() & 0x400 == 0,
                "Timelapse links are not permitted"
            );
        }
        ensure!(
            before.len() > 0 && before.len() <= crate::protocol::TIMELAPSE_SAFETY_CEILING,
            "Timelapse exceeds the 512 MiB client safety ceiling; local movie unchanged"
        );
        // Same volume as the original, under a retention-accounted private prefix.
        let directory = tempfile::Builder::new()
            .prefix(".autopiercam-video-sharing-")
            .tempdir_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
        let mut copy = tempfile::NamedTempFile::new_in(directory.path())?;
        let mut buffer = [0; CHUNK_BYTES];
        let mut length = 0_u64;
        loop {
            ensure!(
                still_allowed(),
                "Timelapse snapshot cancelled; local movie unchanged"
            );
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            length += count as u64;
            ensure!(
                length <= before.len(),
                "Timelapse changed while being copied"
            );
            copy.write_all(&buffer[..count])?;
        }
        let after = source.metadata()?;
        ensure!(
            length == before.len()
                && after.len() == before.len()
                && after.modified()? == before.modified()?,
            "Timelapse changed while being copied"
        );
        copy.as_file().sync_all()?;
        // Close the only writer. Keep only a read-only handle for all attempts;
        // on Windows deny any other writer for the lifetime of this snapshot.
        let path = copy.into_temp_path();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1 | 4);
        }
        let file = options.open(&path)?;
        Ok(Self {
            inner: Arc::new(SnapshotFile {
                file: Mutex::new(file),
                _path: path,
                _directory: directory,
            }),
            len: length,
        })
    }

    /// Each attempt starts at offset zero; independent offsets also make a
    /// cancelled, already-running blocking read harmless to a subsequent retry.
    pub fn stream(
        &self,
    ) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
        // HTTP libraries may retain a cancelled body until their connection task
        // exits. That body must not retain our disk snapshot: only an individual
        // already-running 64 KiB read may briefly hold the owner after cancellation.
        let inner = Arc::downgrade(&self.inner);
        let length = self.len;
        futures_util::stream::try_unfold((inner, 0_u64), move |(inner, offset)| async move {
            if offset == length {
                return Ok(None);
            }
            let reader = inner
                .upgrade()
                .ok_or_else(|| std::io::Error::other("snapshot cancelled"))?;
            let count = (length - offset).min(CHUNK_BYTES as u64) as usize;
            let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                let mut file = reader
                    .file
                    .lock()
                    .map_err(|_| std::io::Error::other("snapshot lock poisoned"))?;
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = vec![0; count];
                file.read_exact(&mut bytes)?;
                Ok(bytes)
            })
            .await
            .map_err(std::io::Error::other)??;
            Ok(Some((bytes, (inner, offset + count as u64))))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn large_snapshot_stream_is_chunk_bounded_immutable_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("daily.mp4");
        let file = File::create(&original).unwrap();
        let size = 21 * 1024 * 1024 + 13;
        file.set_len(size).unwrap();
        drop(file);
        let movie = Movie::snapshot(&original, || true).unwrap();
        let private = movie.inner._directory.path().to_owned();
        assert_eq!(movie.len, size);
        std::fs::write(&original, b"new original contents").unwrap();
        #[cfg(windows)]
        assert!(
            OpenOptions::new()
                .write(true)
                .open(&movie.inner._path)
                .is_err()
        );
        for _ in 0..2 {
            let stream = movie.stream();
            futures_util::pin_mut!(stream);
            let mut received = 0_u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                assert!(chunk.len() <= CHUNK_BYTES);
                assert!(chunk.iter().all(|byte| *byte == 0));
                received += chunk.len() as u64;
            }
            assert_eq!(received, size);
        }
        // Even an HTTP-retained unconsumed body cannot retain a cancelled file.
        let stream = movie.stream();
        drop(movie);
        assert!(!private.exists());
        futures_util::pin_mut!(stream);
        assert!(stream.next().await.unwrap().is_err());
        assert_eq!(std::fs::read(original).unwrap(), b"new original contents");
    }

    #[test]
    fn snapshot_copy_cancellation_and_safety_ceiling_leave_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("daily.mp4");
        File::create(&original)
            .unwrap()
            .set_len((CHUNK_BYTES * 4) as u64)
            .unwrap();
        let calls = std::cell::Cell::new(0);
        assert!(
            Movie::snapshot(&original, || {
                calls.set(calls.get() + 1);
                calls.get() < 3
            })
            .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        OpenOptions::new()
            .write(true)
            .open(&original)
            .unwrap()
            .set_len(crate::protocol::TIMELAPSE_SAFETY_CEILING + 1)
            .unwrap();
        assert!(Movie::snapshot(&original, || true).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
