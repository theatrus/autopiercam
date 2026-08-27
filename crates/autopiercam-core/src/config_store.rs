use crate::config::{Config, ConfigError};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
static STORE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
pub struct ConfigSnapshot {
    pub revision: u64,
    pub config: Config,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("configuration revision conflict: expected {expected}, current {current}")]
pub struct RevisionConflict {
    pub expected: u64,
    pub current: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigStoreError {
    #[error("configuration target path has no file name: {path}")]
    InvalidTarget { path: PathBuf },
    #[error("could not {action} at {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("could not serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error(transparent)]
    RevisionConflict(#[from] RevisionConflict),
}

impl ConfigStore {
    /// Opens a managed configuration file, creating its parent directory and a
    /// validated default configuration when the target is absent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ConfigStoreError> {
        let path = resolve_target(path.as_ref())?;
        let lock = lock_for_path(&path);
        let store = Self { path, lock };

        {
            let _guard = lock_unpoisoned(&store.lock);
            let exists = store
                .path
                .try_exists()
                .map_err(|source| io_error("inspect configuration target", &store.path, source))?;
            if !exists {
                let config = Config::default();
                config.validate()?;
                let canonical = canonical_toml(&config)?;
                atomic_write(&store.path, &canonical)?;
            }

            // Refuse to open an existing malformed or invalid configuration.
            load_snapshot(&store.path)?;
        }

        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reloads and validates the file on every call.
    pub fn snapshot(&self) -> Result<ConfigSnapshot, ConfigStoreError> {
        let _guard = lock_unpoisoned(&self.lock);
        load_snapshot(&self.path)
    }

    /// Replaces the file only when its current content-derived revision matches
    /// expected_revision.
    pub fn replace(
        &self,
        expected_revision: u64,
        config: Config,
    ) -> Result<ConfigSnapshot, ConfigStoreError> {
        config.validate()?;
        let canonical = canonical_toml(&config)?;

        let _guard = lock_unpoisoned(&self.lock);
        let current = load_snapshot(&self.path)?;
        if current.revision != expected_revision {
            return Err(RevisionConflict {
                expected: expected_revision,
                current: current.revision,
            }
            .into());
        }

        atomic_write(&self.path, &canonical)?;
        Ok(ConfigSnapshot {
            revision: fnv1a(&canonical),
            config,
        })
    }
}

fn resolve_target(path: &Path) -> Result<PathBuf, ConfigStoreError> {
    let Some(file_name) = path.file_name().filter(|name| !name.is_empty()) else {
        return Err(ConfigStoreError::InvalidTarget {
            path: path.to_path_buf(),
        });
    };

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| io_error("resolve current directory", path, source))?
            .join(path)
    };
    let Some(parent) = absolute.parent() else {
        return Err(ConfigStoreError::InvalidTarget {
            path: path.to_path_buf(),
        });
    };

    fs::create_dir_all(parent)
        .map_err(|source| io_error("create configuration parent directory", parent, source))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|source| io_error("resolve configuration parent directory", parent, source))?;
    Ok(canonical_parent.join(file_name))
}

fn load_snapshot(path: &Path) -> Result<ConfigSnapshot, ConfigStoreError> {
    let config = Config::load(path)?;
    let canonical = canonical_toml(&config)?;
    Ok(ConfigSnapshot {
        revision: fnv1a(&canonical),
        config,
    })
}

fn canonical_toml(config: &Config) -> Result<Vec<u8>, ConfigStoreError> {
    Ok(toml::to_string_pretty(config)?.into_bytes())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV1A_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV1A_PRIME)
    })
}

fn lock_for_path(path: &Path) -> Arc<Mutex<()>> {
    let registry = STORE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = lock_unpoisoned(registry);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return lock;
    }

    registry.retain(|_, lock| lock.strong_count() != 0);
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn atomic_write(target: &Path, contents: &[u8]) -> Result<(), ConfigStoreError> {
    let (temp_path, mut temp_file) = create_unique_temp(target)?;
    let mut cleanup = TempCleanup::new(temp_path.clone());

    temp_file
        .write_all(contents)
        .map_err(|source| io_error("write temporary configuration", &temp_path, source))?;
    temp_file
        .flush()
        .map_err(|source| io_error("flush temporary configuration", &temp_path, source))?;
    temp_file
        .sync_all()
        .map_err(|source| io_error("sync temporary configuration", &temp_path, source))?;
    drop(temp_file);

    replace_file(&temp_path, target)
        .map_err(|source| io_error("atomically replace configuration", target, source))?;
    cleanup.disarm();
    Ok(())
}

fn create_unique_temp(target: &Path) -> Result<(PathBuf, File), ConfigStoreError> {
    let parent = target
        .parent()
        .expect("resolved configuration targets always have a parent");
    let file_name = target
        .file_name()
        .expect("resolved configuration targets always have a file name");

    loop {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".autopiercam-{}-{id}.tmp", std::process::id()));
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(io_error(
                    "create temporary configuration",
                    &temp_path,
                    source,
                ));
            }
        }
    }
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> ConfigStoreError {
    ConfigStoreError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

struct TempCleanup {
    path: Option<PathBuf>,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();

    // SAFETY: both arguments are valid, NUL-terminated UTF-16 path buffers that
    // remain alive for the duration of the call.
    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(not(any(windows, unix)))]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            loop {
                let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "autopiercam-config-store-{label}-{}-{id}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory {path:?}: {error}"),
                }
            }
        }

        fn config_path(&self) -> PathBuf {
            self.0.join("nested").join("autopiercam.toml")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_parent_and_default_configuration() {
        let directory = TestDir::new("default");
        let path = directory.config_path();
        let store = ConfigStore::open(&path).unwrap();
        let snapshot = store.snapshot().unwrap();

        assert!(path.is_file());
        assert_eq!(
            store.path(),
            fs::canonicalize(path.parent().unwrap())
                .unwrap()
                .join("autopiercam.toml")
        );
        assert_eq!(snapshot.config.camera.bin, 1);
        assert_eq!(
            snapshot.revision,
            fnv1a(&canonical_toml(&Config::default()).unwrap())
        );
    }

    #[test]
    fn replaces_and_reloads_across_a_new_store() {
        let directory = TestDir::new("replace");
        let path = directory.config_path();
        let store = ConfigStore::open(&path).unwrap();
        let initial = store.snapshot().unwrap();
        let mut replacement = initial.config;
        replacement.capture.interval_ms = 2_500;

        let replaced = store.replace(initial.revision, replacement).unwrap();
        assert_ne!(replaced.revision, initial.revision);
        assert_eq!(replaced.config.capture.interval_ms, 2_500);

        let reopened = ConfigStore::open(&path).unwrap().snapshot().unwrap();
        assert_eq!(reopened.revision, replaced.revision);
        assert_eq!(reopened.config.capture.interval_ms, 2_500);
    }

    #[test]
    fn stale_conflict_preserves_the_current_file() {
        let directory = TestDir::new("conflict");
        let path = directory.config_path();
        let store = ConfigStore::open(&path).unwrap();
        let initial = store.snapshot().unwrap();

        let mut first = initial.config.clone();
        first.capture.interval_ms = 2_000;
        let current = store.replace(initial.revision, first).unwrap();
        let before = fs::read(&path).unwrap();

        let mut stale = initial.config;
        stale.capture.interval_ms = 3_000;
        let error = store.replace(initial.revision, stale).unwrap_err();
        assert_eq!(
            match error {
                ConfigStoreError::RevisionConflict(conflict) => conflict,
                other => panic!("expected revision conflict, got {other:?}"),
            },
            RevisionConflict {
                expected: initial.revision,
                current: current.revision,
            }
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(store.snapshot().unwrap().revision, current.revision);
    }

    #[test]
    fn invalid_replacement_preserves_the_file() {
        let directory = TestDir::new("invalid");
        let path = directory.config_path();
        let store = ConfigStore::open(&path).unwrap();
        let initial = store.snapshot().unwrap();
        let before = fs::read(&path).unwrap();
        let mut invalid = initial.config;
        invalid.capture.interval_ms = 0;

        assert!(matches!(
            store.replace(initial.revision, invalid),
            Err(ConfigStoreError::Config(ConfigError::Validation(_)))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(store.snapshot().unwrap().revision, initial.revision);
    }

    #[test]
    fn external_semantic_edit_changes_snapshot_revision() {
        let directory = TestDir::new("external");
        let path = directory.config_path();
        let store = ConfigStore::open(&path).unwrap();
        let initial = store.snapshot().unwrap();
        let mut external = initial.config;
        external.api.listen = "127.0.0.1:9999".to_owned();
        fs::write(&path, canonical_toml(&external).unwrap()).unwrap();

        let edited = store.snapshot().unwrap();
        assert_ne!(edited.revision, initial.revision);
        assert_eq!(edited.config.api.listen, "127.0.0.1:9999");
    }

    #[test]
    fn fnv1a_revision_uses_the_standard_64_bit_constants() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn target_without_a_file_name_is_rejected() {
        let root = Path::new(std::path::MAIN_SEPARATOR_STR);
        assert!(matches!(
            ConfigStore::open(root),
            Err(ConfigStoreError::InvalidTarget { .. })
        ));
    }
}
