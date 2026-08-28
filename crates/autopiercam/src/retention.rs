use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autopiercam_core::config::CaptureConfig;
#[cfg(windows)]
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::warn;

const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1_000;
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const RETENTION_WAKE_CAPACITY: usize = 1;
const MAX_RETENTION_ERROR_MESSAGES: usize = 8;
const MAX_RETENTION_ERROR_MESSAGE_BYTES: usize = 1_024;

pub(crate) type GeneratedFrameNameParser = Arc<dyn Fn(&str) -> Option<u64> + Send + Sync + 'static>;
pub(crate) type RetentionObserver = Arc<dyn Fn(RetentionTelemetry) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionPolicy {
    max_age_ms: Option<u64>,
    max_managed_bytes: Option<u64>,
    min_free_bytes: Option<u64>,
    keep_latest: bool,
}

impl RetentionPolicy {
    pub(crate) fn from_capture_config(config: &CaptureConfig) -> Self {
        Self {
            max_age_ms: (config.retention_days != 0)
                .then(|| u64::from(config.retention_days).saturating_mul(MILLIS_PER_DAY)),
            max_managed_bytes: config.retention_max_bytes,
            min_free_bytes: config.retention_min_free_bytes,
            keep_latest: config.keep_latest,
        }
    }

    pub(crate) fn is_enabled(self) -> bool {
        self.max_age_ms.is_some() || self.has_byte_quota()
    }

    pub(crate) fn needs_free_space(self) -> bool {
        self.min_free_bytes.is_some()
    }

    fn has_byte_quota(self) -> bool {
        self.max_managed_bytes.is_some() || self.min_free_bytes.is_some()
    }
}

/// One regular, direct-child JPEG whose name exactly matches the capture
/// writer's generated-frame grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedArtifact {
    pub(crate) path: PathBuf,
    pub(crate) captured_at_unix_ms: u64,
    pub(crate) file_size_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetentionClassification {
    Reclaimable,
    Protected,
}

/// The ledger's classification and, for a completed row, its durable content
/// identity. Unknown, crash-gap, pending, retrying, active, and permanently
/// failed artifacts must be returned as [`RetentionClassification::Protected`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionAuthorization {
    pub(crate) classification: RetentionClassification,
    pub(crate) expected_file_size_bytes: Option<u64>,
    pub(crate) expected_sha256: Option<[u8; 32]>,
}

impl RetentionAuthorization {
    pub(crate) const fn protected() -> Self {
        Self {
            classification: RetentionClassification::Protected,
            expected_file_size_bytes: None,
            expected_sha256: None,
        }
    }

    pub(crate) const fn reclaimable(
        expected_file_size_bytes: Option<u64>,
        expected_sha256: Option<[u8; 32]>,
    ) -> Self {
        Self {
            classification: RetentionClassification::Reclaimable,
            expected_file_size_bytes,
            expected_sha256,
        }
    }

    fn is_reclaimable(self) -> bool {
        self.classification == RetentionClassification::Reclaimable
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetentionCandidate {
    pub(crate) path: PathBuf,
    pub(crate) captured_at_unix_ms: u64,
    pub(crate) file_size_bytes: u64,
    pub(crate) authorization: RetentionAuthorization,
}

impl RetentionCandidate {
    fn is_reclaimable(&self) -> bool {
        self.authorization.is_reclaimable()
    }

    fn protect(&mut self) {
        self.authorization = RetentionAuthorization::protected();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetentionPlan {
    pub(crate) deletions: Vec<RetentionCandidate>,
    pub(crate) managed_bytes: u64,
    pub(crate) protected_bytes: u64,
    pub(crate) reclaimable_bytes: u64,
    pub(crate) planned_reclaim_bytes: u64,
    pub(crate) projected_managed_bytes: u64,
    pub(crate) projected_free_bytes: Option<u64>,
    /// True when every eligible artifact could be removed and a byte quota
    /// would still be violated. Callers must surface/suspend instead of
    /// deleting protected recovery data.
    pub(crate) quota_blocked: bool,
}

pub(crate) fn plan_retention(
    policy: RetentionPolicy,
    now_unix_ms: u64,
    free_bytes: Option<u64>,
    mut candidates: Vec<RetentionCandidate>,
) -> RetentionPlan {
    candidates.sort_by(|left, right| {
        left.captured_at_unix_ms
            .cmp(&right.captured_at_unix_ms)
            .then_with(|| left.path.cmp(&right.path))
    });

    let managed_bytes = candidates.iter().fold(0_u64, |total, candidate| {
        total.saturating_add(candidate.file_size_bytes)
    });

    if policy.keep_latest
        && let Some(latest) = candidates.last_mut()
    {
        latest.protect();
    }

    let protected_bytes = candidates
        .iter()
        .filter(|candidate| !candidate.is_reclaimable())
        .fold(0_u64, |total, candidate| {
            total.saturating_add(candidate.file_size_bytes)
        });
    let reclaimable_bytes = managed_bytes.saturating_sub(protected_bytes);

    let mut selected = vec![false; candidates.len()];
    if let Some(max_age_ms) = policy.max_age_ms {
        let cutoff = now_unix_ms.saturating_sub(max_age_ms);
        for (index, candidate) in candidates.iter().enumerate() {
            selected[index] = candidate.is_reclaimable() && candidate.captured_at_unix_ms <= cutoff;
        }
    }

    let mut planned_reclaim_bytes = selected
        .iter()
        .zip(&candidates)
        .filter(|(selected, _)| **selected)
        .fold(0_u64, |total, (_, candidate)| {
            total.saturating_add(candidate.file_size_bytes)
        });

    for (index, candidate) in candidates.iter().enumerate() {
        if selected[index] || !candidate.is_reclaimable() {
            continue;
        }
        if known_quotas_satisfied(
            policy,
            managed_bytes.saturating_sub(planned_reclaim_bytes),
            free_bytes.map(|bytes| bytes.saturating_add(planned_reclaim_bytes)),
        ) {
            break;
        }
        selected[index] = true;
        planned_reclaim_bytes = planned_reclaim_bytes.saturating_add(candidate.file_size_bytes);
    }

    let projected_managed_bytes = managed_bytes.saturating_sub(planned_reclaim_bytes);
    let projected_free_bytes = free_bytes.map(|bytes| bytes.saturating_add(planned_reclaim_bytes));
    let quota_blocked = !quotas_satisfied(policy, projected_managed_bytes, projected_free_bytes);
    let deletions = selected
        .into_iter()
        .zip(candidates)
        .filter_map(|(selected, candidate)| selected.then_some(candidate))
        .collect();

    RetentionPlan {
        deletions,
        managed_bytes,
        protected_bytes,
        reclaimable_bytes,
        planned_reclaim_bytes,
        projected_managed_bytes,
        projected_free_bytes,
        quota_blocked,
    }
}

fn quotas_satisfied(
    policy: RetentionPolicy,
    projected_managed_bytes: u64,
    projected_free_bytes: Option<u64>,
) -> bool {
    policy
        .max_managed_bytes
        .is_none_or(|maximum| projected_managed_bytes <= maximum)
        && policy
            .min_free_bytes
            .is_none_or(|minimum| projected_free_bytes.is_some_and(|free| free >= minimum))
}

fn known_quotas_satisfied(
    policy: RetentionPolicy,
    projected_managed_bytes: u64,
    projected_free_bytes: Option<u64>,
) -> bool {
    policy
        .max_managed_bytes
        .is_none_or(|maximum| projected_managed_bytes <= maximum)
        && match (policy.min_free_bytes, projected_free_bytes) {
            (Some(minimum), Some(free)) => free >= minimum,
            // A failed free-space probe must be surfaced as blocked, but it
            // must not cause a blind sweep of every otherwise eligible file.
            (Some(_), None) | (None, _) => true,
        }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetentionPressure {
    Ok,
    CleanupNeeded,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetentionTelemetry {
    pub(crate) swept_at_unix_ms: u64,
    pub(crate) managed_bytes: u64,
    pub(crate) protected_bytes: u64,
    pub(crate) reclaimable_bytes: u64,
    pub(crate) free_bytes: Option<u64>,
    pub(crate) reclaimed_file_count: u64,
    pub(crate) reclaimed_bytes: u64,
    pub(crate) blocked_pressure: bool,
    pub(crate) pressure: RetentionPressure,
    pub(crate) error: Option<String>,
}

impl RetentionTelemetry {
    fn failed(
        swept_at_unix_ms: u64,
        policy: RetentionPolicy,
        free_bytes: Option<u64>,
        error: impl Into<String>,
    ) -> Self {
        let blocked_pressure = policy.has_byte_quota();
        Self {
            swept_at_unix_ms,
            managed_bytes: 0,
            protected_bytes: 0,
            reclaimable_bytes: 0,
            free_bytes,
            reclaimed_file_count: 0,
            reclaimed_bytes: 0,
            blocked_pressure,
            pressure: if blocked_pressure {
                RetentionPressure::Blocked
            } else {
                RetentionPressure::CleanupNeeded
            },
            error: Some(bounded_error_message(error.into())),
        }
    }
}

#[derive(Default)]
struct RetentionErrors {
    messages: Vec<String>,
    omitted: usize,
}

impl RetentionErrors {
    fn push(&mut self, message: impl Into<String>) {
        if self.messages.len() == MAX_RETENTION_ERROR_MESSAGES {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        self.messages.push(bounded_error_message(message.into()));
    }

    fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.omitted == 0
    }

    fn finish(mut self) -> Option<String> {
        if self.omitted != 0 {
            self.messages.push(format!(
                "{} additional retention error(s) omitted",
                self.omitted
            ));
        }
        (!self.messages.is_empty()).then(|| self.messages.join("; "))
    }
}

fn bounded_error_message(mut message: String) -> String {
    if message.len() <= MAX_RETENTION_ERROR_MESSAGE_BYTES {
        return message;
    }
    let suffix = "…";
    let mut end = MAX_RETENTION_ERROR_MESSAGE_BYTES.saturating_sub(suffix.len());
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    message.truncate(end);
    message.push_str(suffix);
    message
}

/// Result from the platform deleter. `AlreadyAbsent` is safe but is not
/// credited as work performed by the retention worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactDeleteOutcome {
    Deleted,
    AlreadyAbsent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetentionDeletionOutcome {
    Deleted,
    AlreadyAbsent,
    Protected,
}

#[derive(Debug, Error)]
pub(crate) enum RetentionDeleteError {
    #[cfg(not(windows))]
    #[error("safe artifact deletion is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("refusing to delete a non-regular or reparse-point artifact: {path}")]
    UnsafeArtifact { path: PathBuf },
    #[error("artifact identity changed at {path}: {reason}")]
    IdentityChanged { path: PathBuf, reason: String },
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Platform operations are intentionally passed into the final authorization
/// callback. A ledger-backed implementation must keep its shared-store guard
/// alive while calling `delete_exact`, closing the state-check/delete race.
pub(crate) trait RetentionPlatform: Send + Sync + 'static {
    fn available_bytes(&self, directory: &Path) -> io::Result<u64>;

    fn delete_exact(
        &self,
        candidate: &RetentionCandidate,
        expectation: RetentionAuthorization,
    ) -> Result<ExactDeleteOutcome, RetentionDeleteError>;
}

/// Supplies classifications from the already-complete filesystem inventory,
/// then owns the final state recheck and deletion boundary. Implementations
/// must return exactly one authorization for each input artifact, in order.
pub(crate) trait RetentionAuthority: Send + Sync + 'static {
    fn classify_inventory(
        &self,
        inventory: &[ManagedArtifact],
    ) -> Result<Vec<RetentionAuthorization>, String>;

    /// Recheck the candidate against current state. When it remains eligible,
    /// call `platform.delete_exact` before releasing the synchronization that
    /// made the recheck authoritative.
    fn delete_if_still_authorized(
        &self,
        candidate: &RetentionCandidate,
        platform: &dyn RetentionPlatform,
    ) -> Result<RetentionDeletionOutcome, String>;
}

/// Fail-closed authority for a capture directory that may still be referenced
/// by an upload ledger whose owner is unavailable. It inventories bytes for
/// pressure telemetry but never permits automatic deletion.
#[derive(Debug, Default)]
pub(crate) struct ProtectAllRetentionAuthority;

impl RetentionAuthority for ProtectAllRetentionAuthority {
    fn classify_inventory(
        &self,
        inventory: &[ManagedArtifact],
    ) -> Result<Vec<RetentionAuthorization>, String> {
        Ok(vec![RetentionAuthorization::protected(); inventory.len()])
    }

    fn delete_if_still_authorized(
        &self,
        _candidate: &RetentionCandidate,
        _platform: &dyn RetentionPlatform,
    ) -> Result<RetentionDeletionOutcome, String> {
        Ok(RetentionDeletionOutcome::Protected)
    }
}

/// Safe only when the caller has established that no upload ledger exists.
/// A disabled uploader with an old ledger must instead fail closed.
#[derive(Debug, Default)]
pub(crate) struct LocalOnlyRetentionAuthority;

impl RetentionAuthority for LocalOnlyRetentionAuthority {
    fn classify_inventory(
        &self,
        inventory: &[ManagedArtifact],
    ) -> Result<Vec<RetentionAuthorization>, String> {
        Ok(inventory
            .iter()
            .map(|artifact| {
                RetentionAuthorization::reclaimable(Some(artifact.file_size_bytes), None)
            })
            .collect())
    }

    fn delete_if_still_authorized(
        &self,
        candidate: &RetentionCandidate,
        platform: &dyn RetentionPlatform,
    ) -> Result<RetentionDeletionOutcome, String> {
        platform
            .delete_exact(candidate, candidate.authorization)
            .map(|outcome| match outcome {
                ExactDeleteOutcome::Deleted => RetentionDeletionOutcome::Deleted,
                ExactDeleteOutcome::AlreadyAbsent => RetentionDeletionOutcome::AlreadyAbsent,
            })
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Default)]
pub(crate) struct SystemRetentionPlatform;

impl RetentionPlatform for SystemRetentionPlatform {
    fn available_bytes(&self, directory: &Path) -> io::Result<u64> {
        system_available_bytes(directory)
    }

    fn delete_exact(
        &self,
        candidate: &RetentionCandidate,
        expectation: RetentionAuthorization,
    ) -> Result<ExactDeleteOutcome, RetentionDeleteError> {
        system_delete_exact(candidate, expectation)
    }
}

#[cfg(windows)]
fn system_available_bytes(directory: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide_path = directory
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available_to_caller = 0_u64;
    // SAFETY: `wide_path` is NUL-terminated and lives through the call. The
    // optional output pointers may be null according to GetDiskFreeSpaceExW.
    let succeeded = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut available_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available_to_caller)
    }
}

#[cfg(not(windows))]
fn system_available_bytes(_directory: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "free-space probing is not implemented on this platform",
    ))
}

#[cfg(windows)]
fn open_exact_delete_handle(path: &Path) -> io::Result<fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };

    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_READ | DELETE)
        // Deny both write and delete sharing so pathname replacement, rename,
        // and deletion cannot race the identity check or disposition.
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(windows)]
fn system_delete_exact(
    candidate: &RetentionCandidate,
    expectation: RetentionAuthorization,
) -> Result<ExactDeleteOutcome, RetentionDeleteError> {
    use std::io::Read;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FileDispositionInfo,
        SetFileInformationByHandle,
    };

    let mut file = match open_exact_delete_handle(&candidate.path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ExactDeleteOutcome::AlreadyAbsent);
        }
        Err(source) => {
            return Err(RetentionDeleteError::Io {
                operation: "open artifact without following reparse points",
                path: candidate.path.clone(),
                source,
            });
        }
    };

    let metadata = file.metadata().map_err(|source| RetentionDeleteError::Io {
        operation: "read opened artifact metadata",
        path: candidate.path.clone(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(RetentionDeleteError::UnsafeArtifact {
            path: candidate.path.clone(),
        });
    }

    let opened_size = metadata.len();
    if opened_size != candidate.file_size_bytes {
        return Err(RetentionDeleteError::IdentityChanged {
            path: candidate.path.clone(),
            reason: format!(
                "inventory size was {} bytes but the opened file is {opened_size} bytes",
                candidate.file_size_bytes
            ),
        });
    }
    if let Some(expected_size) = expectation.expected_file_size_bytes
        && opened_size != expected_size
    {
        return Err(RetentionDeleteError::IdentityChanged {
            path: candidate.path.clone(),
            reason: format!(
                "authorized size was {expected_size} bytes but the opened file is {opened_size} bytes"
            ),
        });
    }
    if let Some(expected_sha256) = expectation.expected_sha256 {
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|source| RetentionDeleteError::Io {
                    operation: "hash opened artifact",
                    path: candidate.path.clone(),
                    source,
                })?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let actual_sha256: [u8; 32] = hasher.finalize().into();
        if actual_sha256 != expected_sha256 {
            return Err(RetentionDeleteError::IdentityChanged {
                path: candidate.path.clone(),
                reason: "SHA-256 digest no longer matches the authorized artifact".to_owned(),
            });
        }
    }

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let buffer_size = u32::try_from(std::mem::size_of_val(&disposition))
        .expect("FILE_DISPOSITION_INFO size fits in u32");
    // SAFETY: `file` owns a valid handle, the information class matches the
    // pointed-to structure, and that structure lives through the call. The
    // disposition attaches to this opened file object, so a later pathname
    // replacement cannot redirect the deletion.
    let succeeded = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            buffer_size,
        )
    };
    if succeeded == 0 {
        return Err(RetentionDeleteError::Io {
            operation: "mark opened artifact for deletion",
            path: candidate.path.clone(),
            source: io::Error::last_os_error(),
        });
    }
    drop(file);
    Ok(ExactDeleteOutcome::Deleted)
}

#[cfg(not(windows))]
fn system_delete_exact(
    _candidate: &RetentionCandidate,
    _expectation: RetentionAuthorization,
) -> Result<ExactDeleteOutcome, RetentionDeleteError> {
    Err(RetentionDeleteError::UnsupportedPlatform)
}

pub(crate) fn inventory_capture_directory(
    directory: &Path,
    parse_generated_name: &dyn Fn(&str) -> Option<u64>,
) -> Result<Vec<ManagedArtifact>, RetentionSweepError> {
    let entries = fs::read_dir(directory).map_err(|source| RetentionSweepError::Io {
        operation: "read capture directory",
        path: directory.to_path_buf(),
        source,
    })?;
    let mut inventory = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RetentionSweepError::Io {
            operation: "read capture directory entry",
            path: directory.to_path_buf(),
            source,
        })?;
        let Some(filename) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(captured_at_unix_ms) = parse_generated_name(&filename) else {
            continue;
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(RetentionSweepError::Io {
                    operation: "read capture artifact type",
                    path: entry.path(),
                    source,
                });
            }
        };
        // DirEntry::file_type does not follow symbolic links. Reparse points
        // and directories are never admitted to the managed inventory.
        if !file_type.is_file() {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(RetentionSweepError::Io {
                    operation: "read capture artifact metadata",
                    path: entry.path(),
                    source,
                });
            }
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
        }
        inventory.push(ManagedArtifact {
            path: entry.path(),
            captured_at_unix_ms,
            file_size_bytes: metadata.len(),
        });
    }
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(inventory)
}

#[derive(Debug, Error)]
pub(crate) enum RetentionSweepError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "retention authority returned {actual} classifications for an inventory of {expected} artifacts"
    )]
    ClassificationCount { expected: usize, actual: usize },
    #[error("retention authority failed: {0}")]
    Authority(String),
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system clock is outside the supported millisecond range")]
    ClockOutOfRange,
}

trait RetentionClock: Send + Sync + 'static {
    fn now_unix_ms(&self) -> Result<u64, RetentionSweepError>;
}

#[derive(Debug, Default)]
struct SystemRetentionClock;

impl RetentionClock for SystemRetentionClock {
    fn now_unix_ms(&self) -> Result<u64, RetentionSweepError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RetentionSweepError::ClockBeforeUnixEpoch)?;
        u64::try_from(elapsed.as_millis()).map_err(|_| RetentionSweepError::ClockOutOfRange)
    }
}

fn sweep_retention(
    policy: RetentionPolicy,
    directory: &Path,
    parse_generated_name: &dyn Fn(&str) -> Option<u64>,
    authority: &dyn RetentionAuthority,
    platform: &dyn RetentionPlatform,
    clock: &dyn RetentionClock,
) -> RetentionTelemetry {
    let swept_at_unix_ms = match clock.now_unix_ms() {
        Ok(now) => now,
        Err(error) => return RetentionTelemetry::failed(0, policy, None, error.to_string()),
    };
    let (initial_free_bytes, free_space_error) = match platform.available_bytes(directory) {
        Ok(bytes) => (Some(bytes), None),
        Err(error) if policy.needs_free_space() => (
            None,
            Some(format!(
                "querying free space for {} failed: {error}",
                directory.display()
            )),
        ),
        Err(_) => (None, None),
    };
    let inventory = match inventory_capture_directory(directory, parse_generated_name) {
        Ok(inventory) => inventory,
        Err(error) => {
            return RetentionTelemetry::failed(
                swept_at_unix_ms,
                policy,
                initial_free_bytes,
                error.to_string(),
            );
        }
    };
    let authorizations = match authority.classify_inventory(&inventory) {
        Ok(authorizations) => authorizations,
        Err(error) => {
            return RetentionTelemetry::failed(
                swept_at_unix_ms,
                policy,
                initial_free_bytes,
                RetentionSweepError::Authority(error).to_string(),
            );
        }
    };
    if authorizations.len() != inventory.len() {
        return RetentionTelemetry::failed(
            swept_at_unix_ms,
            policy,
            initial_free_bytes,
            RetentionSweepError::ClassificationCount {
                expected: inventory.len(),
                actual: authorizations.len(),
            }
            .to_string(),
        );
    }

    let mut errors = RetentionErrors::default();
    if let Some(error) = free_space_error {
        errors.push(error);
    }
    let candidates = inventory
        .into_iter()
        .zip(authorizations)
        .map(|(artifact, mut authorization)| {
            if authorization.is_reclaimable()
                && authorization
                    .expected_file_size_bytes
                    .is_some_and(|expected| expected != artifact.file_size_bytes)
            {
                errors.push(format!(
                    "protecting {} because its inventory size no longer matches the authorized size",
                    artifact.path.display()
                ));
                authorization = RetentionAuthorization::protected();
            }
            RetentionCandidate {
                path: artifact.path,
                captured_at_unix_ms: artifact.captured_at_unix_ms,
                file_size_bytes: artifact.file_size_bytes,
                authorization,
            }
        })
        .collect::<Vec<_>>();
    let plan = plan_retention(policy, swept_at_unix_ms, initial_free_bytes, candidates);

    let mut managed_bytes = plan.managed_bytes;
    let mut protected_bytes = plan.protected_bytes;
    let mut reclaimable_bytes = plan.reclaimable_bytes;
    let mut reclaimed_file_count = 0_u64;
    let mut reclaimed_bytes = 0_u64;
    let deletion_count = plan.deletions.len();
    let mut unresolved_deletions = 0_usize;
    for candidate in &plan.deletions {
        match authority.delete_if_still_authorized(candidate, platform) {
            Ok(RetentionDeletionOutcome::Deleted) => {
                managed_bytes = managed_bytes.saturating_sub(candidate.file_size_bytes);
                reclaimable_bytes = reclaimable_bytes.saturating_sub(candidate.file_size_bytes);
                reclaimed_file_count = reclaimed_file_count.saturating_add(1);
                reclaimed_bytes = reclaimed_bytes.saturating_add(candidate.file_size_bytes);
            }
            Ok(RetentionDeletionOutcome::AlreadyAbsent) => {
                managed_bytes = managed_bytes.saturating_sub(candidate.file_size_bytes);
                reclaimable_bytes = reclaimable_bytes.saturating_sub(candidate.file_size_bytes);
            }
            Ok(RetentionDeletionOutcome::Protected) => {
                protected_bytes = protected_bytes.saturating_add(candidate.file_size_bytes);
                reclaimable_bytes = reclaimable_bytes.saturating_sub(candidate.file_size_bytes);
                unresolved_deletions += 1;
            }
            Err(error) => {
                unresolved_deletions += 1;
                errors.push(format!(
                    "retaining {} after final authorization/deletion failed: {error}",
                    candidate.path.display()
                ));
            }
        }
    }

    let free_bytes = match platform.available_bytes(directory) {
        Ok(bytes) => Some(bytes),
        Err(error) if policy.needs_free_space() => {
            errors.push(format!(
                "verifying free space for {} after retention failed: {error}",
                directory.display()
            ));
            None
        }
        Err(_) => initial_free_bytes.map(|bytes| bytes.saturating_add(reclaimed_bytes)),
    };
    let blocked_pressure = !quotas_satisfied(policy, managed_bytes, free_bytes);
    let has_errors = !errors.is_empty();
    let pressure = if blocked_pressure {
        RetentionPressure::Blocked
    } else if unresolved_deletions != 0 || (deletion_count == 0 && has_errors) {
        RetentionPressure::CleanupNeeded
    } else {
        RetentionPressure::Ok
    };

    RetentionTelemetry {
        swept_at_unix_ms,
        managed_bytes,
        protected_bytes,
        reclaimable_bytes,
        free_bytes,
        reclaimed_file_count,
        reclaimed_bytes,
        blocked_pressure,
        pressure,
        error: errors.finish(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetentionWakeResult {
    Queued,
    Coalesced,
    Stopped,
}

#[derive(Clone)]
pub(crate) struct RetentionSink {
    wake_sender: SyncSender<()>,
    stop: Arc<AtomicBool>,
    capture_suspended: Arc<AtomicBool>,
}

impl RetentionSink {
    pub(crate) fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// True when the latest completed sweep found unsatisfied byte pressure,
    /// or when the worker is no longer available to enforce retention.
    pub(crate) fn capture_suspended(&self) -> bool {
        self.capture_suspended.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire)
    }

    /// Nonblocking and bounded: a full one-slot queue means an equivalent
    /// sweep is already pending, so additional capture notifications coalesce.
    pub(crate) fn try_wake(&self) -> RetentionWakeResult {
        if self.stop.load(Ordering::Acquire) {
            return RetentionWakeResult::Stopped;
        }
        match self.wake_sender.try_send(()) {
            Ok(()) => RetentionWakeResult::Queued,
            Err(TrySendError::Full(())) => RetentionWakeResult::Coalesced,
            Err(TrySendError::Disconnected(())) => {
                self.stop.store(true, Ordering::Release);
                RetentionWakeResult::Stopped
            }
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum RetentionWorkerError {
    #[error("retention sweep interval must be greater than zero")]
    InvalidSweepInterval,
    #[error("starting retention worker thread failed: {0}")]
    ThreadStart(#[source] io::Error),
    #[error("retention worker thread panicked")]
    WorkerPanicked,
}

pub(crate) struct RetentionWorker {
    stop: Arc<AtomicBool>,
    capture_suspended: Arc<AtomicBool>,
    wake_sender: SyncSender<()>,
    thread: Option<JoinHandle<()>>,
}

impl RetentionWorker {
    pub(crate) fn start(
        policy: RetentionPolicy,
        directory: PathBuf,
        parse_generated_name: GeneratedFrameNameParser,
        authority: Arc<dyn RetentionAuthority>,
        observer: RetentionObserver,
    ) -> Result<(Self, RetentionSink), RetentionWorkerError> {
        Self::start_with_runtime(
            policy,
            directory,
            parse_generated_name,
            authority,
            observer,
            Arc::new(SystemRetentionPlatform),
            Arc::new(SystemRetentionClock),
            DEFAULT_SWEEP_INTERVAL,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_runtime(
        policy: RetentionPolicy,
        directory: PathBuf,
        parse_generated_name: GeneratedFrameNameParser,
        authority: Arc<dyn RetentionAuthority>,
        observer: RetentionObserver,
        platform: Arc<dyn RetentionPlatform>,
        clock: Arc<dyn RetentionClock>,
        sweep_interval: Duration,
    ) -> Result<(Self, RetentionSink), RetentionWorkerError> {
        if sweep_interval.is_zero() {
            return Err(RetentionWorkerError::InvalidSweepInterval);
        }

        let (wake_sender, wake_receiver) = mpsc::sync_channel(RETENTION_WAKE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        // Fail closed until the synchronous initial sweep publishes a result.
        let capture_suspended = Arc::new(AtomicBool::new(true));

        // Deliberately synchronous: callers can start retention before the
        // camera/writer and observe storage pressure before new persistence.
        publish_retention_telemetry(
            capture_suspended.as_ref(),
            observer.as_ref(),
            sweep_retention(
                policy,
                &directory,
                parse_generated_name.as_ref(),
                authority.as_ref(),
                platform.as_ref(),
                clock.as_ref(),
            ),
        );

        let worker_capture_suspended = Arc::clone(&capture_suspended);
        let worker_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("autopiercam-retention".to_owned())
            .spawn(move || {
                let _stop_on_exit = RetentionStopOnDrop {
                    stop: Arc::clone(&worker_stop),
                    capture_suspended: Arc::clone(&worker_capture_suspended),
                };
                retention_loop(
                    wake_receiver,
                    &worker_stop,
                    &worker_capture_suspended,
                    policy,
                    &directory,
                    parse_generated_name.as_ref(),
                    authority.as_ref(),
                    platform.as_ref(),
                    clock.as_ref(),
                    observer.as_ref(),
                    sweep_interval,
                );
            })
            .map_err(RetentionWorkerError::ThreadStart)?;

        Ok((
            Self {
                stop: Arc::clone(&stop),
                capture_suspended: Arc::clone(&capture_suspended),
                wake_sender: wake_sender.clone(),
                thread: Some(thread),
            },
            RetentionSink {
                wake_sender,
                stop,
                capture_suspended,
            },
        ))
    }

    pub(crate) fn stop_and_join(mut self) -> Result<(), RetentionWorkerError> {
        self.stop_and_join_inner()
    }

    fn stop_and_join_inner(&mut self) -> Result<(), RetentionWorkerError> {
        self.capture_suspended.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        let _ = self.wake_sender.try_send(());
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| RetentionWorkerError::WorkerPanicked)
    }
}

impl Drop for RetentionWorker {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_join_inner() {
            warn!(%error, "retention worker did not shut down cleanly");
        }
    }
}

struct RetentionStopOnDrop {
    stop: Arc<AtomicBool>,
    capture_suspended: Arc<AtomicBool>,
}

impl Drop for RetentionStopOnDrop {
    fn drop(&mut self) {
        self.capture_suspended.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
    }
}

fn publish_retention_telemetry(
    capture_suspended: &AtomicBool,
    observer: &dyn Fn(RetentionTelemetry),
    telemetry: RetentionTelemetry,
) {
    capture_suspended.store(telemetry.blocked_pressure, Ordering::Release);
    observer(telemetry);
}

#[allow(clippy::too_many_arguments)]
fn retention_loop(
    wake_receiver: Receiver<()>,
    stop: &AtomicBool,
    capture_suspended: &AtomicBool,
    policy: RetentionPolicy,
    directory: &Path,
    parse_generated_name: &dyn Fn(&str) -> Option<u64>,
    authority: &dyn RetentionAuthority,
    platform: &dyn RetentionPlatform,
    clock: &dyn RetentionClock,
    observer: &dyn Fn(RetentionTelemetry),
    sweep_interval: Duration,
) {
    loop {
        match wake_receiver.recv_timeout(sweep_interval) {
            Ok(()) => loop {
                match wake_receiver.try_recv() {
                    Ok(()) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let stopping_before_sweep = stop.load(Ordering::Acquire);
        publish_retention_telemetry(
            capture_suspended,
            observer,
            sweep_retention(
                policy,
                directory,
                parse_generated_name,
                authority,
                platform,
                clock,
            ),
        );
        if stop.load(Ordering::Acquire) {
            // A stop arriving during a sweep can follow the writer's final
            // publication. Start one final inventory after that stop request.
            if !stopping_before_sweep {
                publish_retention_telemetry(
                    capture_suspended,
                    observer,
                    sweep_retention(
                        policy,
                        directory,
                        parse_generated_name,
                        authority,
                        platform,
                        clock,
                    ),
                );
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, atomic::AtomicU64};

    fn candidate(name: &str, captured_at_unix_ms: u64, bytes: u64) -> RetentionCandidate {
        RetentionCandidate {
            path: PathBuf::from(name),
            captured_at_unix_ms,
            file_size_bytes: bytes,
            authorization: RetentionAuthorization::reclaimable(Some(bytes), None),
        }
    }

    fn policy() -> RetentionPolicy {
        RetentionPolicy {
            max_age_ms: None,
            max_managed_bytes: None,
            min_free_bytes: None,
            keep_latest: false,
        }
    }

    fn parse_test_frame_name(filename: &str) -> Option<u64> {
        let stem = filename.strip_suffix(".jpg")?;
        let millis = stem.strip_prefix("frame-")?;
        (!millis.is_empty() && millis.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| millis.parse().ok())
            .flatten()
    }

    #[test]
    fn age_rule_removes_only_reclaimable_expired_artifacts() {
        let mut protected = candidate("protected.jpg", 0, 7);
        protected.protect();
        let plan = plan_retention(
            RetentionPolicy {
                max_age_ms: Some(1_000),
                ..policy()
            },
            2_000,
            None,
            vec![
                candidate("old.jpg", 1_000, 5),
                candidate("young.jpg", 1_001, 6),
                protected,
            ],
        );

        assert_eq!(
            plan.deletions
                .iter()
                .map(|candidate| candidate.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("old.jpg")]
        );
        assert_eq!(plan.managed_bytes, 18);
        assert_eq!(plan.protected_bytes, 7);
        assert_eq!(plan.reclaimable_bytes, 11);
        assert!(!plan.quota_blocked);
    }

    #[test]
    fn byte_limits_reclaim_oldest_until_both_are_satisfied() {
        let plan = plan_retention(
            RetentionPolicy {
                max_managed_bytes: Some(20),
                min_free_bytes: Some(105),
                ..policy()
            },
            10_000,
            Some(100),
            vec![
                candidate("oldest.jpg", 1, 4),
                candidate("middle.jpg", 2, 6),
                candidate("newest.jpg", 3, 15),
            ],
        );

        assert_eq!(
            plan.deletions
                .iter()
                .map(|candidate| candidate.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("oldest.jpg"), Path::new("middle.jpg")]
        );
        assert_eq!(plan.planned_reclaim_bytes, 10);
        assert_eq!(plan.projected_managed_bytes, 15);
        assert_eq!(plan.projected_free_bytes, Some(110));
        assert!(!plan.quota_blocked);
    }

    #[test]
    fn keep_latest_overrides_age_and_quota_rules() {
        let plan = plan_retention(
            RetentionPolicy {
                max_age_ms: Some(1),
                max_managed_bytes: Some(0),
                keep_latest: true,
                ..policy()
            },
            100,
            None,
            vec![candidate("old.jpg", 1, 3), candidate("latest.jpg", 2, 5)],
        );

        assert_eq!(plan.deletions, vec![candidate("old.jpg", 1, 3)]);
        assert_eq!(plan.protected_bytes, 5);
        assert!(plan.quota_blocked);
    }

    #[test]
    fn protected_bytes_block_quota_without_being_selected() {
        let mut protected = candidate("failed.jpg", 1, 10);
        protected.protect();
        let plan = plan_retention(
            RetentionPolicy {
                max_managed_bytes: Some(5),
                ..policy()
            },
            100,
            None,
            vec![protected],
        );

        assert!(plan.deletions.is_empty());
        assert_eq!(plan.protected_bytes, 10);
        assert!(plan.quota_blocked);
    }

    #[test]
    fn missing_free_space_measurement_fails_minimum_free_quota_closed() {
        let plan = plan_retention(
            RetentionPolicy {
                min_free_bytes: Some(100),
                ..policy()
            },
            100,
            None,
            vec![candidate("frame.jpg", 1, 10)],
        );

        assert!(plan.deletions.is_empty());
        assert!(plan.quota_blocked);
    }

    #[test]
    fn zero_retention_days_disable_age_in_policy_conversion() {
        let config = CaptureConfig {
            retention_days: 0,
            ..CaptureConfig::default()
        };
        let policy = RetentionPolicy::from_capture_config(&config);

        assert_eq!(policy.max_age_ms, None);
        assert!(!policy.needs_free_space());
        assert!(!policy.is_enabled());
    }

    #[test]
    fn inventory_admits_only_regular_files_accepted_by_exact_parser() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("frame-100.jpg"), b"one").unwrap();
        fs::write(directory.path().join("frame-200.jpeg"), b"two").unwrap();
        fs::write(directory.path().join("other.jpg"), b"three").unwrap();
        fs::create_dir(directory.path().join("frame-300.jpg")).unwrap();

        let inventory =
            inventory_capture_directory(directory.path(), &parse_test_frame_name).unwrap();

        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].path, directory.path().join("frame-100.jpg"));
        assert_eq!(inventory[0].captured_at_unix_ms, 100);
        assert_eq!(inventory[0].file_size_bytes, 3);
    }

    #[derive(Debug)]
    struct FixedClock(u64);

    impl RetentionClock for FixedClock {
        fn now_unix_ms(&self) -> Result<u64, RetentionSweepError> {
            Ok(self.0)
        }
    }

    #[derive(Debug)]
    struct RecordingPlatform {
        free_bytes: u64,
        deleted: Mutex<Vec<PathBuf>>,
    }

    impl RetentionPlatform for RecordingPlatform {
        fn available_bytes(&self, _directory: &Path) -> io::Result<u64> {
            Ok(self.free_bytes)
        }

        fn delete_exact(
            &self,
            candidate: &RetentionCandidate,
            _expectation: RetentionAuthorization,
        ) -> Result<ExactDeleteOutcome, RetentionDeleteError> {
            self.deleted.lock().unwrap().push(candidate.path.clone());
            Ok(ExactDeleteOutcome::Deleted)
        }
    }

    #[derive(Debug)]
    struct FailingVerificationPlatform {
        probes: AtomicU64,
        deleted: Mutex<Vec<PathBuf>>,
    }

    impl RetentionPlatform for FailingVerificationPlatform {
        fn available_bytes(&self, _directory: &Path) -> io::Result<u64> {
            if self.probes.fetch_add(1, Ordering::AcqRel) == 0 {
                Ok(8)
            } else {
                Err(io::Error::other("injected post-delete probe failure"))
            }
        }

        fn delete_exact(
            &self,
            candidate: &RetentionCandidate,
            _expectation: RetentionAuthorization,
        ) -> Result<ExactDeleteOutcome, RetentionDeleteError> {
            self.deleted.lock().unwrap().push(candidate.path.clone());
            Ok(ExactDeleteOutcome::Deleted)
        }
    }

    #[test]
    fn sweep_reports_reclaimed_and_protected_bytes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("frame-100.jpg"), b"old").unwrap();
        fs::write(directory.path().join("frame-200.jpg"), b"keep!").unwrap();
        let platform = RecordingPlatform {
            free_bytes: 1_000,
            deleted: Mutex::new(Vec::new()),
        };
        let telemetry = sweep_retention(
            RetentionPolicy {
                max_age_ms: Some(50),
                ..policy()
            },
            directory.path(),
            &parse_test_frame_name,
            &LocalOnlyRetentionAuthority,
            &platform,
            &FixedClock(175),
        );

        assert_eq!(telemetry.managed_bytes, 5);
        assert_eq!(telemetry.protected_bytes, 0);
        assert_eq!(telemetry.reclaimable_bytes, 5);
        assert_eq!(telemetry.reclaimed_file_count, 1);
        assert_eq!(telemetry.reclaimed_bytes, 3);
        assert!(!telemetry.blocked_pressure);
        assert_eq!(telemetry.pressure, RetentionPressure::Ok);
        assert_eq!(
            *platform.deleted.lock().unwrap(),
            vec![directory.path().join("frame-100.jpg")]
        );
    }

    #[test]
    fn failed_post_delete_free_space_probe_stays_blocked() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("frame-100.jpg"), b"old").unwrap();
        let platform = FailingVerificationPlatform {
            probes: AtomicU64::new(0),
            deleted: Mutex::new(Vec::new()),
        };
        let telemetry = sweep_retention(
            RetentionPolicy {
                min_free_bytes: Some(10),
                ..policy()
            },
            directory.path(),
            &parse_test_frame_name,
            &LocalOnlyRetentionAuthority,
            &platform,
            &FixedClock(175),
        );

        assert_eq!(telemetry.reclaimed_file_count, 1);
        assert_eq!(telemetry.free_bytes, None);
        assert!(telemetry.blocked_pressure);
        assert_eq!(telemetry.pressure, RetentionPressure::Blocked);
        assert!(
            telemetry
                .error
                .as_deref()
                .is_some_and(|error| error.contains("post-delete probe failure"))
        );
    }

    #[test]
    fn retention_errors_are_bounded_and_report_omissions() {
        let mut errors = RetentionErrors::default();
        for index in 0..20 {
            errors.push(format!("error {index}: {}", "x".repeat(4_096)));
        }
        let error = errors.finish().unwrap();

        assert!(error.len() < 10_000);
        assert!(error.contains("12 additional retention error(s) omitted"));
        assert!(error.contains('…'));
    }

    struct ProtectNewestAuthority;

    impl RetentionAuthority for ProtectNewestAuthority {
        fn classify_inventory(
            &self,
            inventory: &[ManagedArtifact],
        ) -> Result<Vec<RetentionAuthorization>, String> {
            Ok(inventory
                .iter()
                .map(|artifact| {
                    if artifact.captured_at_unix_ms == 200 {
                        RetentionAuthorization::protected()
                    } else {
                        RetentionAuthorization::reclaimable(Some(artifact.file_size_bytes), None)
                    }
                })
                .collect())
        }

        fn delete_if_still_authorized(
            &self,
            candidate: &RetentionCandidate,
            platform: &dyn RetentionPlatform,
        ) -> Result<RetentionDeletionOutcome, String> {
            LocalOnlyRetentionAuthority.delete_if_still_authorized(candidate, platform)
        }
    }

    #[test]
    fn authority_protection_can_make_byte_pressure_blocked() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("frame-100.jpg"), b"old").unwrap();
        fs::write(directory.path().join("frame-200.jpg"), b"protected").unwrap();
        let platform = RecordingPlatform {
            free_bytes: 1_000,
            deleted: Mutex::new(Vec::new()),
        };
        let telemetry = sweep_retention(
            RetentionPolicy {
                max_managed_bytes: Some(5),
                ..policy()
            },
            directory.path(),
            &parse_test_frame_name,
            &ProtectNewestAuthority,
            &platform,
            &FixedClock(300),
        );

        assert_eq!(telemetry.managed_bytes, 9);
        assert_eq!(telemetry.protected_bytes, 9);
        assert_eq!(telemetry.reclaimable_bytes, 0);
        assert_eq!(telemetry.reclaimed_file_count, 1);
        assert!(telemetry.blocked_pressure);
        assert_eq!(telemetry.pressure, RetentionPressure::Blocked);
    }

    #[test]
    fn protect_all_authority_never_deletes_prior_ledger_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("frame-100.jpg"), b"ledgered").unwrap();
        let platform = RecordingPlatform {
            free_bytes: 1_000,
            deleted: Mutex::new(Vec::new()),
        };
        let telemetry = sweep_retention(
            RetentionPolicy {
                max_managed_bytes: Some(0),
                ..policy()
            },
            directory.path(),
            &parse_test_frame_name,
            &ProtectAllRetentionAuthority,
            &platform,
            &FixedClock(300),
        );

        assert_eq!(telemetry.managed_bytes, 8);
        assert_eq!(telemetry.protected_bytes, 8);
        assert_eq!(telemetry.reclaimable_bytes, 0);
        assert_eq!(telemetry.reclaimed_file_count, 0);
        assert!(telemetry.blocked_pressure);
        assert!(platform.deleted.lock().unwrap().is_empty());
    }

    struct RevokeAtDeletionAuthority;

    impl RetentionAuthority for RevokeAtDeletionAuthority {
        fn classify_inventory(
            &self,
            inventory: &[ManagedArtifact],
        ) -> Result<Vec<RetentionAuthorization>, String> {
            Ok(inventory
                .iter()
                .map(|artifact| {
                    RetentionAuthorization::reclaimable(Some(artifact.file_size_bytes), None)
                })
                .collect())
        }

        fn delete_if_still_authorized(
            &self,
            _candidate: &RetentionCandidate,
            _platform: &dyn RetentionPlatform,
        ) -> Result<RetentionDeletionOutcome, String> {
            Ok(RetentionDeletionOutcome::Protected)
        }
    }

    #[test]
    fn final_recheck_can_protect_a_planned_deletion() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("frame-100.jpg"), b"keep").unwrap();
        let platform = RecordingPlatform {
            free_bytes: 1_000,
            deleted: Mutex::new(Vec::new()),
        };
        let telemetry = sweep_retention(
            RetentionPolicy {
                max_managed_bytes: Some(0),
                ..policy()
            },
            directory.path(),
            &parse_test_frame_name,
            &RevokeAtDeletionAuthority,
            &platform,
            &FixedClock(300),
        );

        assert_eq!(telemetry.managed_bytes, 4);
        assert_eq!(telemetry.protected_bytes, 4);
        assert_eq!(telemetry.reclaimable_bytes, 0);
        assert_eq!(telemetry.reclaimed_file_count, 0);
        assert!(telemetry.blocked_pressure);
        assert!(platform.deleted.lock().unwrap().is_empty());
    }

    #[test]
    fn wake_queue_is_bounded_and_coalesces() {
        let (wake_sender, wake_receiver) = mpsc::sync_channel(RETENTION_WAKE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let sink = RetentionSink {
            wake_sender,
            stop: Arc::clone(&stop),
            capture_suspended: Arc::new(AtomicBool::new(false)),
        };

        assert_eq!(sink.try_wake(), RetentionWakeResult::Queued);
        assert_eq!(sink.try_wake(), RetentionWakeResult::Coalesced);
        assert_eq!(wake_receiver.try_recv(), Ok(()));
        drop(wake_receiver);
        assert_eq!(sink.try_wake(), RetentionWakeResult::Stopped);
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn worker_publishes_initial_sweep_and_stops_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let (telemetry_sender, telemetry_receiver) = mpsc::sync_channel(1);
        let observer: RetentionObserver = Arc::new(move |telemetry| {
            let _ = telemetry_sender.try_send(telemetry);
        });
        let parser: GeneratedFrameNameParser = Arc::new(parse_test_frame_name);

        let (worker, sink) = RetentionWorker::start(
            policy(),
            directory.path().to_path_buf(),
            parser,
            Arc::new(LocalOnlyRetentionAuthority),
            observer,
        )
        .unwrap();

        let initial = telemetry_receiver.recv().unwrap();
        assert_eq!(initial.managed_bytes, 0);
        assert_eq!(initial.pressure, RetentionPressure::Ok);
        assert!(!sink.is_stopped());
        assert!(!sink.capture_suspended());
        assert!(matches!(
            sink.try_wake(),
            RetentionWakeResult::Queued | RetentionWakeResult::Coalesced
        ));
        worker.stop_and_join().unwrap();
        assert!(sink.is_stopped());
        assert!(sink.capture_suspended());
        assert_eq!(sink.try_wake(), RetentionWakeResult::Stopped);
    }

    #[test]
    fn shutdown_performs_the_final_queued_sweep() {
        let directory = tempfile::tempdir().unwrap();
        let platform = Arc::new(RecordingPlatform {
            free_bytes: 1_000,
            deleted: Mutex::new(Vec::new()),
        });
        let calls = Arc::new(AtomicU64::new(0));
        let observer_calls = Arc::clone(&calls);
        let (sweep_entered_sender, sweep_entered_receiver) = mpsc::channel();
        let (release_sweep_sender, release_sweep_receiver) = mpsc::channel();
        let release_sweep_receiver = Mutex::new(release_sweep_receiver);
        let observer: RetentionObserver = Arc::new(move |_| {
            if observer_calls.fetch_add(1, Ordering::AcqRel) == 1 {
                sweep_entered_sender.send(()).unwrap();
                release_sweep_receiver.lock().unwrap().recv().unwrap();
            }
        });
        let (worker, sink) = RetentionWorker::start_with_runtime(
            RetentionPolicy {
                max_managed_bytes: Some(1),
                ..policy()
            },
            directory.path().to_path_buf(),
            Arc::new(parse_test_frame_name),
            Arc::new(LocalOnlyRetentionAuthority),
            observer,
            platform.clone(),
            Arc::new(FixedClock(300)),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(sink.try_wake(), RetentionWakeResult::Queued);
        sweep_entered_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        // This artifact appears after the in-flight sweep's inventory. A stop
        // requested now must force a new, post-stop inventory.
        let artifact = directory.path().join("frame-100.jpg");
        fs::write(&artifact, b"artifact").unwrap();
        let (stopped_sender, stopped_receiver) = mpsc::channel();
        let stop_thread = thread::spawn(move || {
            stopped_sender.send(worker.stop_and_join()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sink.is_stopped() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        release_sweep_sender.send(()).unwrap();

        stopped_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        stop_thread.join().unwrap();

        assert!(sink.is_stopped());
        assert_eq!(*platform.deleted.lock().unwrap(), [artifact]);
        assert_eq!(calls.load(Ordering::Acquire), 3);
    }

    #[test]
    fn observer_panic_stops_and_suspends_retention_health() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let observer_calls = Arc::clone(&calls);
        let observer: RetentionObserver = Arc::new(move |_| {
            if observer_calls.fetch_add(1, Ordering::AcqRel) != 0 {
                panic!("injected retention observer panic");
            }
        });
        let (worker, sink) = RetentionWorker::start(
            policy(),
            directory.path().to_path_buf(),
            Arc::new(parse_test_frame_name),
            Arc::new(LocalOnlyRetentionAuthority),
            observer,
        )
        .unwrap();
        assert_eq!(sink.try_wake(), RetentionWakeResult::Queued);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sink.is_stopped() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(sink.capture_suspended());
        assert!(matches!(
            worker.stop_and_join(),
            Err(RetentionWorkerError::WorkerPanicked)
        ));
    }

    #[test]
    fn initial_blocked_sweep_suspends_capture_before_worker_returns() {
        let directory = tempfile::tempdir().unwrap();
        let platform: Arc<dyn RetentionPlatform> = Arc::new(RecordingPlatform {
            free_bytes: 10,
            deleted: Mutex::new(Vec::new()),
        });
        let (worker, sink) = RetentionWorker::start_with_runtime(
            RetentionPolicy {
                min_free_bytes: Some(11),
                ..policy()
            },
            directory.path().to_path_buf(),
            Arc::new(parse_test_frame_name),
            Arc::new(LocalOnlyRetentionAuthority),
            Arc::new(|_| {}),
            platform,
            Arc::new(FixedClock(300)),
            Duration::from_secs(60),
        )
        .unwrap();

        assert!(sink.capture_suspended());
        worker.stop_and_join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn exact_delete_handle_fences_path_rename_until_close() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("frame-100.jpg");
        let renamed = directory.path().join("frame-101.jpg");
        fs::write(&path, b"artifact").unwrap();
        let file = open_exact_delete_handle(&path).unwrap();

        assert!(fs::rename(&path, &renamed).is_err());
        assert!(path.exists());
        drop(file);
        fs::rename(&path, &renamed).unwrap();
        assert!(renamed.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_exact_deletion_verifies_digest_before_marking_open_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("frame-100.jpg");
        fs::write(&path, b"artifact").unwrap();
        let digest: [u8; 32] = Sha256::digest(b"artifact").into();
        let candidate = RetentionCandidate {
            path: path.clone(),
            captured_at_unix_ms: 100,
            file_size_bytes: 8,
            authorization: RetentionAuthorization::reclaimable(Some(8), Some(digest)),
        };

        assert_eq!(
            SystemRetentionPlatform
                .delete_exact(&candidate, candidate.authorization)
                .unwrap(),
            ExactDeleteOutcome::Deleted
        );
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_exact_deletion_retains_digest_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("frame-100.jpg");
        fs::write(&path, b"artifact").unwrap();
        let candidate = RetentionCandidate {
            path: path.clone(),
            captured_at_unix_ms: 100,
            file_size_bytes: 8,
            authorization: RetentionAuthorization::reclaimable(Some(8), Some([0x55; 32])),
        };

        assert!(matches!(
            SystemRetentionPlatform.delete_exact(&candidate, candidate.authorization),
            Err(RetentionDeleteError::IdentityChanged { .. })
        ));
        assert_eq!(fs::read(path).unwrap(), b"artifact");
    }

    #[cfg(windows)]
    #[test]
    fn windows_free_space_probe_reports_available_capacity() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            SystemRetentionPlatform
                .available_bytes(directory.path())
                .unwrap()
                > 0
        );
    }
}
