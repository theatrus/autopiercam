use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ledger_maintenance::{LedgerLease, LedgerLeaseError};

pub(crate) const SCHEMA_VERSION: i64 = 4;
pub(crate) const APPLICATION_ID: i64 = 0x4150_4355;
pub(crate) const SCHEMA_SIGNATURE: &str =
    "autopiercam-upload-ledger-v4-operator-pagination-requeue-delivery-binding-revisions-20260828";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const IDEMPOTENCY_PREFIX: &str = "autopiercam-sha256-";
const CAPTURE_SESSION_NONCE_HEX_LENGTH: usize = 32;
const AUTHORIZATION_FINGERPRINT_DOMAIN: &[u8] = b"autopiercam-upload-authorization-identity-v1\0";
pub(crate) const DELIVERY_BINDING_DOMAIN: &[u8] = b"autopiercam-upload-delivery-binding-v1\0";
const LIST_CURSOR_DOMAIN: &[u8] = b"autopiercam-upload-list-cursor-v1\0";
const LIST_CURSOR_LEDGER_DOMAIN: &[u8] = b"autopiercam-upload-list-ledger-v1\0";
const LIST_CURSOR_PREFIX: &str = "apcu1_";
const LIST_CURSOR_PAYLOAD_LENGTH: usize = 35;
const LIST_CURSOR_MAC_LENGTH: usize = 16;
const LIST_CURSOR_ENCODED_LENGTH: usize =
    LIST_CURSOR_PREFIX.len() + (LIST_CURSOR_PAYLOAD_LENGTH + LIST_CURSOR_MAC_LENGTH) * 2;
const ALL_UPLOAD_STATE_FILTER_MASK: u8 = 0b1_1111;
pub(crate) const MAX_UPLOAD_LIST_PAGE_SIZE: u16 = 100;

pub(crate) const CREATE_METADATA: &str = r#"
CREATE TABLE upload_metadata (
    singleton                INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_signature         TEXT NOT NULL,
    ledger_id                BLOB NOT NULL UNIQUE CHECK (length(ledger_id) = 16),
    ledger_revision          INTEGER NOT NULL DEFAULT 0 CHECK (
                                 typeof(ledger_revision) = 'integer' AND
                                 ledger_revision >= 0
                             ),
    capture_root             TEXT NOT NULL,
    destination              TEXT NOT NULL CHECK (
                                 length(destination) > 0 AND
                                 trim(destination) = destination
                             ),
    authorization_sha256     BLOB NOT NULL CHECK (
                                 length(authorization_sha256) = 32
                             ),
    pending_count            INTEGER NOT NULL DEFAULT 0 CHECK (
                                 typeof(pending_count) = 'integer' AND
                                 pending_count >= 0
                             ),
    in_progress_count        INTEGER NOT NULL DEFAULT 0 CHECK (
                                 typeof(in_progress_count) = 'integer' AND
                                 in_progress_count >= 0
                             ),
    retrying_count           INTEGER NOT NULL DEFAULT 0 CHECK (
                                 typeof(retrying_count) = 'integer' AND
                                 retrying_count >= 0
                             ),
    completed_count          INTEGER NOT NULL DEFAULT 0 CHECK (
                                 typeof(completed_count) = 'integer' AND
                                 completed_count >= 0
                             ),
    permanently_failed_count INTEGER NOT NULL DEFAULT 0 CHECK (
                                 typeof(permanently_failed_count) = 'integer' AND
                                 permanently_failed_count >= 0
                             ),
    last_success_at_ms       INTEGER CHECK (
                                 last_success_at_ms IS NULL OR
                                 last_success_at_ms >= 0
                             ),
    last_failure_at_ms       INTEGER CHECK (
                                 last_failure_at_ms IS NULL OR
                                 last_failure_at_ms >= 0
                             ),
    last_error               TEXT
) STRICT
"#;

pub(crate) const CREATE_PREACTIVATION_ARTIFACTS: &str = r#"
CREATE TABLE upload_preactivation_artifacts (
    artifact_path TEXT PRIMARY KEY CHECK (length(artifact_path) > 0)
) STRICT, WITHOUT ROWID
"#;

pub(crate) const CREATE_UPLOAD_JOBS: &str = r#"
CREATE TABLE upload_jobs (
    id                     INTEGER PRIMARY KEY,
    artifact_path          TEXT NOT NULL UNIQUE,
    filename               TEXT NOT NULL,
    idempotency_key        TEXT NOT NULL UNIQUE,
    file_size              INTEGER NOT NULL CHECK (file_size >= 0),
    sha256                 BLOB NOT NULL CHECK (length(sha256) = 32),
    delivery_binding_sha256 BLOB NOT NULL CHECK (
                                length(delivery_binding_sha256) = 32
                            ),
    state                  TEXT NOT NULL CHECK (
                               state IN (
                                   'pending',
                                   'in_progress',
                                   'retrying',
                                   'completed',
                                   'permanently_failed'
                               )
                           ),
    attempt_count          INTEGER NOT NULL DEFAULT 0
                           CHECK (typeof(attempt_count) = 'integer' AND attempt_count >= 0),
    next_attempt_at_ms     INTEGER
                           CHECK (next_attempt_at_ms IS NULL OR next_attempt_at_ms >= 0),
    last_http_status       INTEGER
                           CHECK (
                               last_http_status IS NULL OR
                               last_http_status BETWEEN 100 AND 599
                           ),
    last_error             TEXT,
    created_at_ms          INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms          INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    completed_at_ms        INTEGER CHECK (completed_at_ms IS NULL OR completed_at_ms >= 0),
    last_failure_at_ms     INTEGER CHECK (
                               last_failure_at_ms IS NULL OR last_failure_at_ms >= 0
                           ),
    job_revision           INTEGER NOT NULL DEFAULT 1 CHECK (
                               typeof(job_revision) = 'integer' AND job_revision >= 1
                           ),
    requeue_count          INTEGER NOT NULL DEFAULT 0 CHECK (
                               typeof(requeue_count) = 'integer' AND requeue_count >= 0
                           ),
    last_requeued_at_ms    INTEGER CHECK (
                               last_requeued_at_ms IS NULL OR last_requeued_at_ms >= 0
                           ),
    CHECK (
        (state = 'retrying' AND next_attempt_at_ms IS NOT NULL) OR
        (state <> 'retrying' AND next_attempt_at_ms IS NULL)
    ),
    CHECK (
        (state = 'completed' AND completed_at_ms IS NOT NULL) OR
        (state <> 'completed' AND completed_at_ms IS NULL)
    ),
    CHECK (
        (requeue_count = 0 AND last_requeued_at_ms IS NULL) OR
        (requeue_count > 0 AND last_requeued_at_ms IS NOT NULL)
    )
) STRICT
"#;

pub(crate) const CREATE_DUE_INDEX: &str = r#"
CREATE INDEX upload_jobs_due
    ON upload_jobs (COALESCE(next_attempt_at_ms, created_at_ms), id)
    WHERE state IN ('pending', 'retrying')
"#;

pub(crate) const CREATE_INSERT_AGGREGATE_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_after_insert
AFTER INSERT ON upload_jobs
BEGIN
    UPDATE upload_metadata
    SET pending_count = pending_count + (NEW.state = 'pending'),
        in_progress_count = in_progress_count + (NEW.state = 'in_progress'),
        retrying_count = retrying_count + (NEW.state = 'retrying'),
        completed_count = completed_count + (NEW.state = 'completed'),
        permanently_failed_count =
            permanently_failed_count + (NEW.state = 'permanently_failed'),
        ledger_revision = ledger_revision + 1,
        last_success_at_ms = CASE
            WHEN NEW.completed_at_ms IS NOT NULL AND
                 (last_success_at_ms IS NULL OR
                  NEW.completed_at_ms >= last_success_at_ms)
            THEN NEW.completed_at_ms
            ELSE last_success_at_ms
        END,
        last_failure_at_ms = CASE
            WHEN NEW.last_failure_at_ms IS NOT NULL AND
                 (last_failure_at_ms IS NULL OR
                  NEW.last_failure_at_ms >= last_failure_at_ms)
            THEN NEW.last_failure_at_ms
            ELSE last_failure_at_ms
        END,
        last_error = CASE
            WHEN NEW.last_failure_at_ms IS NOT NULL AND
                 (last_failure_at_ms IS NULL OR
                  NEW.last_failure_at_ms >= last_failure_at_ms)
            THEN NEW.last_error
            ELSE last_error
        END
    WHERE singleton = 1;
END
"#;

pub(crate) const CREATE_UPDATE_AGGREGATE_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_after_status_update
AFTER UPDATE ON upload_jobs
BEGIN
    UPDATE upload_metadata
    SET pending_count = pending_count
            - (OLD.state = 'pending') + (NEW.state = 'pending'),
        in_progress_count = in_progress_count
            - (OLD.state = 'in_progress') + (NEW.state = 'in_progress'),
        retrying_count = retrying_count
            - (OLD.state = 'retrying') + (NEW.state = 'retrying'),
        completed_count = completed_count
            - (OLD.state = 'completed') + (NEW.state = 'completed'),
        permanently_failed_count = permanently_failed_count
            - (OLD.state = 'permanently_failed')
            + (NEW.state = 'permanently_failed'),
        ledger_revision = ledger_revision + 1,
        last_success_at_ms = CASE
            WHEN NEW.completed_at_ms IS NOT OLD.completed_at_ms AND
                 NEW.completed_at_ms IS NOT NULL AND
                 (last_success_at_ms IS NULL OR
                  NEW.completed_at_ms >= last_success_at_ms)
            THEN NEW.completed_at_ms
            ELSE last_success_at_ms
        END,
        last_failure_at_ms = CASE
            WHEN NEW.last_failure_at_ms IS NOT OLD.last_failure_at_ms AND
                 NEW.last_failure_at_ms IS NOT NULL AND
                 (last_failure_at_ms IS NULL OR
                  NEW.last_failure_at_ms >= last_failure_at_ms)
            THEN NEW.last_failure_at_ms
            ELSE last_failure_at_ms
        END,
        last_error = CASE
            WHEN NEW.last_failure_at_ms IS NOT OLD.last_failure_at_ms AND
                 NEW.last_failure_at_ms IS NOT NULL AND
                 (last_failure_at_ms IS NULL OR
                  NEW.last_failure_at_ms >= last_failure_at_ms)
            THEN NEW.last_error
            ELSE last_error
        END
    WHERE singleton = 1;
END
"#;

pub(crate) const CREATE_JOB_REVISION_GUARD_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_revision_guard
BEFORE UPDATE ON upload_jobs
WHEN NEW.job_revision <> OLD.job_revision + 1
BEGIN
    SELECT RAISE(ABORT, 'upload job revision must increment exactly once');
END
"#;

pub(crate) const CREATE_IMMUTABLE_IDENTITY_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_identity_immutable
BEFORE UPDATE OF artifact_path, filename, idempotency_key, file_size, sha256,
                 delivery_binding_sha256, created_at_ms
ON upload_jobs
BEGIN
    SELECT RAISE(ABORT, 'upload artifact identity is immutable');
END
"#;

pub(crate) const CREATE_IMMUTABLE_METADATA_TRIGGER: &str = r#"
CREATE TRIGGER upload_metadata_identity_immutable
BEFORE UPDATE OF schema_signature, ledger_id, capture_root, destination
ON upload_metadata
BEGIN
    SELECT RAISE(ABORT, 'upload ledger identity is immutable');
END
"#;

pub(crate) const CREATE_NO_DELETE_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_no_delete
BEFORE DELETE ON upload_jobs
BEGIN
    SELECT RAISE(ABORT, 'upload jobs are append-only');
END
"#;

const VERIFY_SCHEMA_PROJECTION: &str = r#"
SELECT
    id,
    artifact_path,
    filename,
    idempotency_key,
    file_size,
    sha256,
    delivery_binding_sha256,
    state,
    attempt_count,
    next_attempt_at_ms,
    last_http_status,
    last_error,
    created_at_ms,
    updated_at_ms,
    completed_at_ms,
    last_failure_at_ms,
    job_revision,
    requeue_count,
    last_requeued_at_ms
FROM upload_jobs
LIMIT 0
"#;

#[derive(Debug, Error)]
pub(crate) enum UploadStoreError {
    #[error("SQLite upload-ledger operation failed")]
    Sqlite(#[from] rusqlite::Error),

    #[error("could not {operation} {path:?}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    LedgerLease(#[from] LedgerLeaseError),

    #[error("the existing upload ledger has no recognized schema")]
    MissingSchema,

    #[error(
        "unsupported upload-ledger schema version {found}; this build supports version {supported}"
    )]
    UnsupportedSchema { found: i64, supported: i64 },

    #[error("the existing upload ledger schema does not match this build")]
    InvalidSchema,

    #[error("the upload ledger belongs to a different capture directory")]
    CaptureRootMismatch,

    #[error("upload destination must be a non-empty normalized string")]
    InvalidDestination,

    #[error("the upload ledger belongs to a different upload destination")]
    DestinationMismatch,

    #[error("the upload ledger belongs to a different upload authorization identity")]
    AuthorizationIdentityMismatch,

    #[error("could not obtain secure randomness for a new upload ledger identity")]
    Randomness,

    #[error("SQLite did not enable the requested {name} setting (reported {actual:?})")]
    Configuration { name: &'static str, actual: String },

    #[error("artifact path must be absolute: {0:?}")]
    ArtifactPathNotAbsolute(PathBuf),

    #[error("artifact path must be valid UTF-8: {0:?}")]
    ArtifactPathNotUtf8(PathBuf),

    #[error("artifact path must be a direct child of the configured capture directory: {0:?}")]
    ArtifactOutsideCaptureRoot(PathBuf),

    #[error("artifact path must not be a symbolic link: {0:?}")]
    ArtifactIsSymlink(PathBuf),

    #[error("artifact path must name a regular file: {0:?}")]
    ArtifactNotRegularFile(PathBuf),

    #[error("artifact filename does not match AutoPierCam's generated JPEG grammar: {0:?}")]
    InvalidArtifactFilename(PathBuf),

    #[error("artifact path conflicts with a different identity already in the upload ledger")]
    ArtifactIdentityConflict,

    #[error("capture directory must be absolute: {0:?}")]
    CaptureDirectoryNotAbsolute(PathBuf),

    #[error("capture directory must be valid UTF-8: {0:?}")]
    CaptureDirectoryNotUtf8(PathBuf),

    #[error("Unix timestamp {0}ms cannot be represented by SQLite")]
    TimestampOutOfRange(u64),

    #[error("artifact size {0} cannot be represented by SQLite")]
    ArtifactSizeOutOfRange(u64),

    #[error("upload ledger contains an invalid {column} integer: {value}")]
    CorruptInteger { column: &'static str, value: i64 },

    #[error("upload ledger contains an unknown upload state: {0:?}")]
    CorruptState(String),

    #[error("upload job {0} has inconsistent identity metadata")]
    CorruptArtifactIdentity(UploadJobId),

    #[error("upload job {0} has a delivery binding inconsistent with the active ledger identity")]
    UploadJobDeliveryBindingMismatch(UploadJobId),

    #[error("upload job ID {0} is outside the supported range")]
    InvalidJobId(u64),

    #[error("upload job {0} is not in progress or its attempt is stale")]
    InvalidTransition(UploadJobId),

    #[error("upload job {0} has exhausted SQLite's attempt counter")]
    AttemptCountOverflow(UploadJobId),

    #[error("upload reconciliation counter overflowed")]
    ReconcileCountOverflow,

    #[error("upload list page size must be between 1 and {maximum}")]
    InvalidListPageSize { maximum: u16 },

    #[error("upload list cursor is invalid")]
    InvalidListCursor,

    #[error("upload list cursor is stale")]
    StaleListCursor,

    #[error("upload list cursor does not match the requested filter or page size")]
    ListCursorParametersMismatch,

    #[error("upload list state filter contains a duplicate state")]
    DuplicateListStateFilter,

    #[error("the upload ledger identity does not match the requeue request")]
    RequeueLedgerMismatch,

    #[error("the upload job does not exist")]
    RequeueJobNotFound,

    #[error("the upload job is not permanently failed")]
    RequeueWrongState,

    #[error("the upload job changed after it was listed")]
    RequeueStaleJobRevision,

    #[error("the upload job belongs to a different delivery identity")]
    RequeueDeliveryBindingMismatch,

    #[error("the upload artifact is missing or is not a safe regular file")]
    RequeueArtifactUnavailable,

    #[error("the upload artifact no longer matches its recorded size and digest")]
    RequeueArtifactChanged,

    #[error("upload job {0} has exhausted SQLite's revision counter")]
    JobRevisionOverflow(UploadJobId),

    #[error("upload job {0} has exhausted SQLite's requeue counter")]
    RequeueCountOverflow(UploadJobId),

    #[error("the upload preactivation inventory contains an invalid artifact path")]
    CorruptPreactivationArtifact,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct UploadJobId(i64);

impl std::fmt::Display for UploadJobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl UploadJobId {
    pub(crate) fn get(self) -> u64 {
        self.0 as u64
    }

    pub(crate) fn from_u64(value: u64) -> Result<Self, UploadStoreError> {
        let value = i64::try_from(value).map_err(|_| UploadStoreError::InvalidJobId(value))?;
        if value <= 0 {
            return Err(UploadStoreError::InvalidJobId(value as u64));
        }
        job_id_from_database(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct UploadLedgerId([u8; 16]);

impl UploadLedgerId {
    pub(crate) fn random() -> Result<Self, UploadStoreError> {
        let mut value = [0_u8; 16];
        getrandom::fill(&mut value).map_err(|_| UploadStoreError::Randomness)?;
        Ok(Self(value))
    }

    fn from_database(value: Vec<u8>) -> Result<Self, UploadStoreError> {
        value
            .try_into()
            .map(Self)
            .map_err(|_| UploadStoreError::InvalidSchema)
    }

    pub(crate) fn as_hex(self) -> String {
        hex_encode(&self.0)
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn parse_hex(value: &str) -> Result<Self, UploadStoreError> {
        let bytes = hex_decode_exact::<16>(value).ok_or(UploadStoreError::RequeueLedgerMismatch)?;
        Ok(Self(bytes))
    }
}

/// One-way identity bound to a durable upload ledger.
///
/// Bearer identity is derived from the actual token, never its environment
/// variable name. Consequently, renaming the variable is safe while rotation
/// is permitted only after all queued work reaches a terminal state. A token
/// change with pending, in-progress, or retrying work fails ledger reopen.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct UploadAuthorizationFingerprint([u8; 32]);

impl UploadAuthorizationFingerprint {
    pub(crate) fn anonymous() -> Self {
        Self::derive(b"anonymous", &[])
    }

    pub(crate) fn for_bearer_token(token: &str) -> Self {
        Self::derive(b"bearer", token.as_bytes())
    }

    fn derive(kind: &[u8], identity: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(AUTHORIZATION_FINGERPRINT_DOMAIN);
        hasher.update(kind);
        hasher.update([0]);
        hasher.update(identity);
        Self(hasher.finalize().into())
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UploadDeliveryBinding([u8; 32]);

impl UploadDeliveryBinding {
    fn derive(destination: &str, authorization: UploadAuthorizationFingerprint) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DELIVERY_BINDING_DOMAIN);
        hasher.update((destination.len() as u64).to_be_bytes());
        hasher.update(destination.as_bytes());
        hasher.update(authorization.as_slice());
        Self(hasher.finalize().into())
    }

    fn from_database(job_id: UploadJobId, value: Vec<u8>) -> Result<Self, UploadStoreError> {
        value
            .try_into()
            .map(Self)
            .map_err(|_| UploadStoreError::CorruptArtifactIdentity(job_id))
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UploadState {
    Pending,
    InProgress,
    Retrying,
    Completed,
    PermanentlyFailed,
}

impl UploadState {
    fn from_database(value: String) -> Result<Self, UploadStoreError> {
        match value.as_str() {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "retrying" => Ok(Self::Retrying),
            "completed" => Ok(Self::Completed),
            "permanently_failed" => Ok(Self::PermanentlyFailed),
            _ => Err(UploadStoreError::CorruptState(value)),
        }
    }

    fn cursor_code(self) -> u8 {
        match self {
            Self::Pending => 1,
            Self::InProgress => 2,
            Self::Retrying => 3,
            Self::Completed => 4,
            Self::PermanentlyFailed => 5,
        }
    }

    fn filter_bit(self) -> u8 {
        1 << (self.cursor_code() - 1)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UploadListCursor(String);

impl UploadListCursor {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UploadListItem {
    pub(crate) job_id: UploadJobId,
    pub(crate) artifact_path: PathBuf,
    pub(crate) filename: String,
    pub(crate) file_size: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) state: UploadState,
    pub(crate) attempt_count: u64,
    pub(crate) next_attempt_at_unix_ms: Option<u64>,
    pub(crate) last_http_status: Option<u16>,
    pub(crate) last_error: Option<String>,
    pub(crate) created_at_unix_ms: u64,
    pub(crate) updated_at_unix_ms: u64,
    pub(crate) completed_at_unix_ms: Option<u64>,
    pub(crate) last_failure_at_unix_ms: Option<u64>,
    pub(crate) job_revision: u64,
    pub(crate) requeue_count: u64,
    pub(crate) last_requeued_at_unix_ms: Option<u64>,
    pub(crate) delivery_binding_is_current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UploadListPage {
    pub(crate) ledger_id: UploadLedgerId,
    pub(crate) ledger_revision: u64,
    pub(crate) jobs: Vec<UploadListItem>,
    pub(crate) next_cursor: Option<UploadListCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequeueUploadRequest {
    pub(crate) ledger_id: UploadLedgerId,
    pub(crate) job_id: UploadJobId,
    pub(crate) expected_job_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RequeueUploadResult {
    pub(crate) ledger_revision: u64,
    pub(crate) job: UploadListItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordDisposition {
    Inserted,
    AlreadyRecorded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordArtifactResult {
    pub(crate) job_id: UploadJobId,
    pub(crate) disposition: RecordDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClaimedUpload {
    pub(crate) job_id: UploadJobId,
    pub(crate) artifact_path: PathBuf,
    pub(crate) filename: String,
    pub(crate) idempotency_key: String,
    pub(crate) file_size: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) attempt_count: u64,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UploadJobRecord {
    pub(crate) job_id: UploadJobId,
    pub(crate) artifact_path: PathBuf,
    pub(crate) filename: String,
    pub(crate) idempotency_key: String,
    pub(crate) file_size: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) state: UploadState,
    pub(crate) attempt_count: u64,
    pub(crate) next_attempt_at_unix_ms: Option<u64>,
    pub(crate) last_http_status: Option<u16>,
    pub(crate) last_error: Option<String>,
    pub(crate) created_at_unix_ms: u64,
    pub(crate) updated_at_unix_ms: u64,
    pub(crate) completed_at_unix_ms: Option<u64>,
    pub(crate) last_failure_at_unix_ms: Option<u64>,
    pub(crate) job_revision: u64,
    pub(crate) requeue_count: u64,
    pub(crate) last_requeued_at_unix_ms: Option<u64>,
    pub(crate) delivery_binding_is_current: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct UploadStateCounts {
    pub(crate) pending: u64,
    pub(crate) in_progress: u64,
    pub(crate) retrying: u64,
    pub(crate) completed: u64,
    pub(crate) permanently_failed: u64,
}

impl UploadStateCounts {
    #[cfg(test)]
    pub(crate) fn total(self) -> u64 {
        self.pending
            .saturating_add(self.in_progress)
            .saturating_add(self.retrying)
            .saturating_add(self.completed)
            .saturating_add(self.permanently_failed)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UploadStoreSnapshot {
    pub(crate) counts: UploadStateCounts,
    pub(crate) last_success_at_unix_ms: Option<u64>,
    pub(crate) last_failure_at_unix_ms: Option<u64>,
    pub(crate) last_error: Option<String>,
    pub(crate) next_due_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UploadRetentionBinding {
    /// Acknowledged bytes may be reclaimed only while this exact immutable job
    /// remains completed at the same revision.
    Completed {
        job_id: UploadJobId,
        job_revision: u64,
        file_size: u64,
        sha256: [u8; 32],
    },
    /// Captures present before this ledger was activated were never intended
    /// for its destination and may follow local retention policy.
    Preactivation,
    /// Nonterminal, permanently failed, or otherwise unacknowledged work.
    Protected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UploadRetentionEntry {
    pub(crate) artifact_path: PathBuf,
    pub(crate) binding: UploadRetentionBinding,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UploadRetentionSnapshot {
    pub(crate) entries: Vec<UploadRetentionEntry>,
}

type AggregateSnapshotRow = (
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReconcileResult {
    pub(crate) examined: u64,
    pub(crate) eligible: u64,
    pub(crate) inserted: u64,
    pub(crate) already_recorded: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactVerification {
    #[cfg(test)]
    Verified,
    Missing,
    Symlink,
    NotRegularFile,
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    Sha256Mismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
}

pub(crate) enum SnapshottedClaimedArtifact {
    Verified(File),
    Rejected(ArtifactVerification),
}

pub(crate) struct UploadStore {
    _ledger_lease: LedgerLease,
    connection: Connection,
    capture_root: PathBuf,
    ledger_id: UploadLedgerId,
    delivery_binding: UploadDeliveryBinding,
}

impl UploadStore {
    pub(crate) fn open(
        database_path: impl AsRef<Path>,
        capture_directory: impl AsRef<Path>,
        destination: &str,
        authorization_fingerprint: UploadAuthorizationFingerprint,
    ) -> Result<Self, UploadStoreError> {
        let database_path = database_path.as_ref();
        let ledger_lease = LedgerLease::acquire_live(database_path)?;
        let capture_root = canonical_capture_root(capture_directory.as_ref())?;
        let capture_root_text = capture_root
            .to_str()
            .ok_or_else(|| UploadStoreError::CaptureDirectoryNotUtf8(capture_root.clone()))?;
        let destination = normalize_destination(destination)?;
        let delivery_binding =
            UploadDeliveryBinding::derive(&destination, authorization_fingerprint);
        let mut connection = Connection::open(database_path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;

        let version = schema_version(&connection)?;
        match version {
            0 if database_is_completely_empty(&connection)? => {
                configure_durability(&connection)?;
                let preactivation_artifacts = inventory_generated_artifacts(&capture_root)?;
                create_schema(
                    &mut connection,
                    capture_root_text,
                    &destination,
                    authorization_fingerprint,
                    UploadLedgerId::random()?,
                    &preactivation_artifacts,
                )?;
            }
            0 => return Err(UploadStoreError::MissingSchema),
            SCHEMA_VERSION => {
                configure_durability(&connection)?;
            }
            found => {
                return Err(UploadStoreError::UnsupportedSchema {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
        }
        // The authorization fingerprint is the one mutable part of ledger
        // identity. Acquire SQLite's process-lifetime exclusive ownership
        // before validating or rebinding it so a losing concurrent start can
        // never alter the winner's route identity after validation.
        acquire_exclusive_ownership(&connection)?;
        verify_schema(&connection, capture_root_text, &destination)?;
        rotate_authorization_if_drained(&mut connection, &capture_root, authorization_fingerprint)?;
        recover_interrupted_jobs(&mut connection)?;
        let ledger_id = read_ledger_id(&connection)?;
        Ok(Self {
            _ledger_lease: ledger_lease,
            connection,
            capture_root,
            ledger_id,
            delivery_binding,
        })
    }

    #[cfg(test)]
    pub(crate) fn ledger_id(&self) -> UploadLedgerId {
        self.ledger_id
    }

    pub(crate) fn record_artifact(
        &mut self,
        artifact_path: &Path,
        recorded_at_unix_ms: u64,
    ) -> Result<RecordArtifactResult, UploadStoreError> {
        let values = ArtifactValues::read(artifact_path, &self.capture_root)?;
        let recorded_at = unix_ms_to_database(recorded_at_unix_ms)?;
        let file_size = i64::try_from(values.file_size)
            .map_err(|_| UploadStoreError::ArtifactSizeOutOfRange(values.file_size))?;
        let inserted = self.connection.execute(
            r#"
            INSERT INTO upload_jobs (
                artifact_path, filename, idempotency_key, file_size, sha256,
                delivery_binding_sha256,
                state, attempt_count, next_attempt_at_ms, last_http_status,
                last_error, created_at_ms, updated_at_ms, completed_at_ms,
                last_failure_at_ms, job_revision, requeue_count,
                last_requeued_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, NULL, NULL, NULL,
                      ?7, ?7, NULL, NULL, 1, 0, NULL)
            ON CONFLICT(artifact_path) DO NOTHING
            "#,
            params![
                &values.artifact_path,
                &values.filename,
                &values.idempotency_key,
                file_size,
                values.sha256.as_slice(),
                self.delivery_binding.as_slice(),
                recorded_at
            ],
        )?;
        let existing: (i64, String, String, i64, Vec<u8>) = self.connection.query_row(
            r#"
            SELECT id, filename, idempotency_key, file_size, sha256
            FROM upload_jobs WHERE artifact_path = ?1
            "#,
            [&values.artifact_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        if inserted == 0
            && (existing.1 != values.filename
                || existing.2 != values.idempotency_key
                || existing.3 != file_size
                || existing.4.as_slice() != values.sha256.as_slice())
        {
            return Err(UploadStoreError::ArtifactIdentityConflict);
        }
        Ok(RecordArtifactResult {
            job_id: job_id_from_database(existing.0)?,
            disposition: if inserted == 1 {
                RecordDisposition::Inserted
            } else {
                RecordDisposition::AlreadyRecorded
            },
        })
    }

    pub(crate) fn claim_due(
        &mut self,
        now_unix_ms: u64,
    ) -> Result<Option<ClaimedUpload>, UploadStoreError> {
        let now = unix_ms_to_database(now_unix_ms)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate = transaction
            .query_row(
                r#"
                SELECT id, artifact_path, filename, idempotency_key,
                       file_size, sha256, delivery_binding_sha256,
                       attempt_count
                FROM upload_jobs
                WHERE state IN ('pending', 'retrying')
                  AND COALESCE(next_attempt_at_ms, created_at_ms) <= ?1
                ORDER BY COALESCE(next_attempt_at_ms, created_at_ms), id
                LIMIT 1
                "#,
                [now],
                |row| {
                    Ok(RawClaim {
                        id: row.get(0)?,
                        artifact_path: row.get(1)?,
                        filename: row.get(2)?,
                        idempotency_key: row.get(3)?,
                        file_size: row.get(4)?,
                        sha256: row.get(5)?,
                        delivery_binding: row.get(6)?,
                        attempt_count: row.get(7)?,
                    })
                },
            )
            .optional()?;
        let Some(candidate) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        let job_id = job_id_from_database(candidate.id)?;
        let claim = candidate.into_claim(job_id, &self.capture_root, self.delivery_binding)?;
        let next_attempt_count = i64::try_from(claim.attempt_count)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(UploadStoreError::AttemptCountOverflow(job_id))?;
        let changed = transaction.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'in_progress', attempt_count = ?2,
                next_attempt_at_ms = NULL, updated_at_ms = ?1,
                job_revision = job_revision + 1
            WHERE id = ?3
              AND state IN ('pending', 'retrying')
              AND COALESCE(next_attempt_at_ms, created_at_ms) <= ?1
            "#,
            params![now, next_attempt_count, job_id.0],
        )?;
        if changed != 1 {
            return Err(UploadStoreError::InvalidTransition(job_id));
        }
        transaction.commit()?;
        Ok(Some(ClaimedUpload {
            attempt_count: database_u64("attempt_count", next_attempt_count)?,
            ..claim
        }))
    }

    /// Releases a claim that was fenced but never submitted to the HTTP
    /// transport. The attempt number deliberately remains consumed so a stale
    /// claimant can never acquire the same fencing token as a later claim.
    pub(crate) fn release_claim(
        &mut self,
        claim: &ClaimedUpload,
        now_unix_ms: u64,
    ) -> Result<(), UploadStoreError> {
        let now = unix_ms_to_database(now_unix_ms)?;
        let attempt_count = attempt_count_to_database(claim)?;
        let changed = self.connection.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'pending', next_attempt_at_ms = NULL,
                updated_at_ms = ?1, job_revision = job_revision + 1
            WHERE id = ?2 AND state = 'in_progress' AND attempt_count = ?3
            "#,
            params![now, claim.job_id.0, attempt_count],
        )?;
        ensure_transition(changed, claim.job_id)
    }

    pub(crate) fn mark_retrying(
        &mut self,
        claim: &ClaimedUpload,
        failed_at_unix_ms: u64,
        next_attempt_at_unix_ms: u64,
        http_status: Option<u16>,
        error: Option<&str>,
    ) -> Result<(), UploadStoreError> {
        let failed_at = unix_ms_to_database(failed_at_unix_ms)?;
        let next_attempt_at = unix_ms_to_database(next_attempt_at_unix_ms)?;
        let attempt_count = attempt_count_to_database(claim)?;
        let changed = self.connection.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'retrying', next_attempt_at_ms = ?1,
                last_http_status = ?2, last_error = ?3,
                updated_at_ms = ?4, last_failure_at_ms = ?4,
                job_revision = job_revision + 1
            WHERE id = ?5 AND state = 'in_progress' AND attempt_count = ?6
            "#,
            params![
                next_attempt_at,
                http_status.map(i64::from),
                error,
                failed_at,
                claim.job_id.0,
                attempt_count
            ],
        )?;
        ensure_transition(changed, claim.job_id)
    }

    pub(crate) fn mark_completed(
        &mut self,
        claim: &ClaimedUpload,
        completed_at_unix_ms: u64,
        http_status: u16,
    ) -> Result<(), UploadStoreError> {
        let completed_at = unix_ms_to_database(completed_at_unix_ms)?;
        let attempt_count = attempt_count_to_database(claim)?;
        let changed = self.connection.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'completed', next_attempt_at_ms = NULL,
                last_http_status = ?1, updated_at_ms = ?2,
                completed_at_ms = ?2, job_revision = job_revision + 1
            WHERE id = ?3 AND state = 'in_progress' AND attempt_count = ?4
            "#,
            params![
                i64::from(http_status),
                completed_at,
                claim.job_id.0,
                attempt_count
            ],
        )?;
        ensure_transition(changed, claim.job_id)
    }

    pub(crate) fn mark_permanently_failed(
        &mut self,
        claim: &ClaimedUpload,
        failed_at_unix_ms: u64,
        http_status: Option<u16>,
        error: Option<&str>,
    ) -> Result<(), UploadStoreError> {
        let failed_at = unix_ms_to_database(failed_at_unix_ms)?;
        let attempt_count = attempt_count_to_database(claim)?;
        let changed = self.connection.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'permanently_failed', next_attempt_at_ms = NULL,
                last_http_status = ?1, last_error = ?2,
                updated_at_ms = ?3, last_failure_at_ms = ?3,
                job_revision = job_revision + 1
            WHERE id = ?4 AND state = 'in_progress' AND attempt_count = ?5
            "#,
            params![
                http_status.map(i64::from),
                error,
                failed_at,
                claim.job_id.0,
                attempt_count
            ],
        )?;
        ensure_transition(changed, claim.job_id)
    }

    #[cfg(test)]
    pub(crate) fn job(
        &self,
        job_id: UploadJobId,
    ) -> Result<Option<UploadJobRecord>, UploadStoreError> {
        let raw = self
            .connection
            .query_row(
                r#"
                SELECT id, artifact_path, filename, idempotency_key,
                       file_size, sha256, delivery_binding_sha256,
                       state, attempt_count,
                       next_attempt_at_ms, last_http_status, last_error,
                       created_at_ms, updated_at_ms, completed_at_ms,
                       last_failure_at_ms, job_revision, requeue_count,
                       last_requeued_at_ms
                FROM upload_jobs WHERE id = ?1
                "#,
                [job_id.0],
                RawUploadJob::from_row,
            )
            .optional()?;
        raw.map(|raw| raw.into_record(&self.capture_root, self.delivery_binding))
            .transpose()
    }

    pub(crate) fn snapshot(&self) -> Result<UploadStoreSnapshot, UploadStoreError> {
        let aggregate: AggregateSnapshotRow = self.connection.query_row(
            r#"
            SELECT pending_count, in_progress_count, retrying_count,
                   completed_count, permanently_failed_count,
                   last_success_at_ms, last_failure_at_ms, last_error
            FROM upload_metadata WHERE singleton = 1
            "#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let next_due: Option<i64> = self.connection.query_row(
            r#"
            SELECT MIN(COALESCE(next_attempt_at_ms, created_at_ms))
            FROM upload_jobs WHERE state IN ('pending', 'retrying')
            "#,
            [],
            |row| row.get(0),
        )?;
        Ok(UploadStoreSnapshot {
            counts: UploadStateCounts {
                pending: database_u64("pending count", aggregate.0)?,
                in_progress: database_u64("in-progress count", aggregate.1)?,
                retrying: database_u64("retrying count", aggregate.2)?,
                completed: database_u64("completed count", aggregate.3)?,
                permanently_failed: database_u64("permanently-failed count", aggregate.4)?,
            },
            last_success_at_unix_ms: optional_database_u64("last_success_at_ms", aggregate.5)?,
            last_failure_at_unix_ms: optional_database_u64("last_failure_at_ms", aggregate.6)?,
            last_error: aggregate.7,
            next_due_at_unix_ms: optional_database_u64("next due time", next_due)?,
        })
    }

    /// Returns a filesystem-free classification of every path known to the
    /// owned ledger. Directory enumeration and hashing happen after the store
    /// mutex is released; this method holds SQLite only long enough to copy and
    /// validate immutable row identities.
    pub(crate) fn retention_snapshot(&self) -> Result<UploadRetentionSnapshot, UploadStoreError> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT id, artifact_path, filename, idempotency_key,
                   file_size, sha256, delivery_binding_sha256,
                   state, attempt_count, next_attempt_at_ms,
                   last_http_status, last_error, created_at_ms,
                   updated_at_ms, completed_at_ms, last_failure_at_ms,
                   job_revision, requeue_count, last_requeued_at_ms
            FROM upload_jobs
            ORDER BY id
            "#,
        )?;
        let jobs = collect_list_items(
            statement.query_map([], RawUploadJob::from_row)?,
            &self.capture_root,
            self.delivery_binding,
        )?;
        let mut entries = Vec::with_capacity(jobs.len());
        let mut recorded_paths = HashSet::with_capacity(jobs.len());
        for job in jobs {
            recorded_paths.insert(job.artifact_path.clone());
            let binding = if job.state == UploadState::Completed {
                UploadRetentionBinding::Completed {
                    job_id: job.job_id,
                    job_revision: job.job_revision,
                    file_size: job.file_size,
                    sha256: job.sha256,
                }
            } else {
                UploadRetentionBinding::Protected
            };
            entries.push(UploadRetentionEntry {
                artifact_path: job.artifact_path,
                binding,
            });
        }
        drop(statement);

        let mut statement = self.connection.prepare(
            "SELECT artifact_path FROM upload_preactivation_artifacts ORDER BY artifact_path",
        )?;
        let paths = statement.query_map([], |row| row.get::<_, String>(0))?;
        for path in paths {
            let path = validate_preactivation_artifact_path(path?, &self.capture_root)?;
            if recorded_paths.insert(path.clone()) {
                entries.push(UploadRetentionEntry {
                    artifact_path: path,
                    binding: UploadRetentionBinding::Preactivation,
                });
            }
        }
        entries.sort_by(|left, right| left.artifact_path.cmp(&right.artifact_path));
        Ok(UploadRetentionSnapshot { entries })
    }

    /// Rechecks a previously classified artifact immediately before an
    /// exact-handle deleter runs while the caller still holds the shared store
    /// mutex. False is a normal race/fence result and must retain the file.
    pub(crate) fn retention_still_authorized(
        &self,
        entry: &UploadRetentionEntry,
    ) -> Result<bool, UploadStoreError> {
        let Some(path) = entry.artifact_path.to_str() else {
            return Ok(false);
        };
        match &entry.binding {
            UploadRetentionBinding::Completed {
                job_id,
                job_revision,
                file_size,
                sha256,
            } => {
                let expected_revision =
                    i64::try_from(*job_revision).map_err(|_| UploadStoreError::CorruptInteger {
                        column: "job_revision",
                        value: i64::MIN,
                    })?;
                let expected_size =
                    i64::try_from(*file_size).map_err(|_| UploadStoreError::CorruptInteger {
                        column: "file_size",
                        value: i64::MIN,
                    })?;
                let matches: bool = self.connection.query_row(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM upload_jobs
                        WHERE id = ?1 AND artifact_path = ?2
                          AND state = 'completed' AND job_revision = ?3
                          AND file_size = ?4 AND sha256 = ?5
                    )
                    "#,
                    params![
                        job_id.0,
                        path,
                        expected_revision,
                        expected_size,
                        sha256.as_slice()
                    ],
                    |row| row.get(0),
                )?;
                Ok(matches)
            }
            UploadRetentionBinding::Preactivation => self
                .connection
                .query_row(
                    r#"
                    SELECT
                        EXISTS (
                            SELECT 1 FROM upload_preactivation_artifacts
                            WHERE artifact_path = ?1
                        )
                        AND NOT EXISTS (
                            SELECT 1 FROM upload_jobs WHERE artifact_path = ?1
                        )
                    "#,
                    [path],
                    |row| row.get(0),
                )
                .map_err(UploadStoreError::from),
            UploadRetentionBinding::Protected => Ok(false),
        }
    }

    /// Lists a revision-stable, newest-first page. The cursor is intentionally
    /// opaque: it is authenticated against this ledger's random identity and
    /// binds the global revision, high-water job ID, filter, and page size.
    /// Consequently every durable mutation invalidates every outstanding
    /// cursor instead of producing a mixed-time operator view.
    pub(crate) fn list_jobs(
        &self,
        state_filters: &[UploadState],
        page_size: u16,
        cursor: Option<&str>,
    ) -> Result<UploadListPage, UploadStoreError> {
        if page_size == 0 || page_size > MAX_UPLOAD_LIST_PAGE_SIZE {
            return Err(UploadStoreError::InvalidListPageSize {
                maximum: MAX_UPLOAD_LIST_PAGE_SIZE,
            });
        }
        let (raw_ledger_id, raw_revision, raw_high_water): (Vec<u8>, i64, i64) =
            self.connection.query_row(
                r#"
                SELECT ledger_id, ledger_revision,
                       COALESCE((SELECT MAX(id) FROM upload_jobs), 0)
                FROM upload_metadata WHERE singleton = 1
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let ledger_id = UploadLedgerId::from_database(raw_ledger_id)?;
        if ledger_id != self.ledger_id {
            return Err(UploadStoreError::InvalidSchema);
        }
        let ledger_revision = database_u64("ledger_revision", raw_revision)?;
        let current_high_water = database_u64("high-water id", raw_high_water)?;
        let filter_mask = canonical_state_filter_mask(state_filters)?;
        let (high_water, before_id) = match cursor {
            Some(cursor) => {
                let decoded = decode_list_cursor(cursor, ledger_id)?;
                if decoded.ledger_tag != list_cursor_ledger_tag(ledger_id)
                    || decoded.ledger_revision != ledger_revision
                {
                    return Err(UploadStoreError::StaleListCursor);
                }
                if decoded.filter_mask != filter_mask || decoded.page_size != page_size {
                    return Err(UploadStoreError::ListCursorParametersMismatch);
                }
                if decoded.high_water_id == 0
                    || decoded.before_id == 0
                    || decoded.before_id > decoded.high_water_id
                    || decoded.high_water_id > current_high_water
                {
                    return Err(UploadStoreError::InvalidListCursor);
                }
                (decoded.high_water_id, decoded.before_id)
            }
            None => (current_high_water, u64::MAX),
        };

        let high_water_database =
            i64::try_from(high_water).map_err(|_| UploadStoreError::InvalidListCursor)?;
        let before_database = if cursor.is_some() {
            Some(i64::try_from(before_id).map_err(|_| UploadStoreError::InvalidListCursor)?)
        } else {
            None
        };
        let limit = i64::from(page_size) + 1;
        let mut statement = self.connection.prepare(
            r#"
            SELECT id, artifact_path, filename, idempotency_key,
                   file_size, sha256, delivery_binding_sha256,
                   state, attempt_count, next_attempt_at_ms,
                   last_http_status, last_error, created_at_ms,
                   updated_at_ms, completed_at_ms, last_failure_at_ms,
                   job_revision, requeue_count, last_requeued_at_ms
            FROM upload_jobs
            WHERE id <= ?1 AND (?2 IS NULL OR id < ?2)
              AND (?3 & CASE state
                    WHEN 'pending' THEN 1
                    WHEN 'in_progress' THEN 2
                    WHEN 'retrying' THEN 4
                    WHEN 'completed' THEN 8
                    WHEN 'permanently_failed' THEN 16
                    ELSE 0
                  END) <> 0
            ORDER BY id DESC
            LIMIT ?4
            "#,
        )?;
        let mut jobs = collect_list_items(
            statement.query_map(
                params![high_water_database, before_database, filter_mask, limit],
                RawUploadJob::from_row,
            )?,
            &self.capture_root,
            self.delivery_binding,
        )?;

        let has_more = jobs.len() > usize::from(page_size);
        if has_more {
            jobs.truncate(usize::from(page_size));
        }
        let next_cursor = if has_more {
            let last_id = jobs
                .last()
                .expect("a nonzero full page has a last upload job")
                .job_id
                .get();
            Some(encode_list_cursor(
                ledger_id,
                ledger_revision,
                high_water,
                last_id,
                filter_mask,
                page_size,
            ))
        } else {
            None
        };
        Ok(UploadListPage {
            ledger_id,
            ledger_revision,
            jobs,
            next_cursor,
        })
    }

    /// Requeues exactly one terminal failure after verifying the same local
    /// bytes still exist. All operator fences are checked within one immediate
    /// transaction, and the verified file handle remains open through commit.
    pub(crate) fn requeue_permanently_failed(
        &mut self,
        request: RequeueUploadRequest,
        requeued_at_unix_ms: u64,
    ) -> Result<RequeueUploadResult, UploadStoreError> {
        if request.ledger_id != self.ledger_id {
            return Err(UploadStoreError::RequeueLedgerMismatch);
        }
        let requeued_at = unix_ms_to_database(requeued_at_unix_ms)?;
        let expected_job_revision = i64::try_from(request.expected_job_revision)
            .map_err(|_| UploadStoreError::RequeueStaleJobRevision)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw = transaction
            .query_row(
                r#"
                SELECT artifact_path, filename, idempotency_key, file_size,
                       sha256, delivery_binding_sha256, state, job_revision,
                       requeue_count
                FROM upload_jobs WHERE id = ?1
                "#,
                [request.job_id.0],
                |row| {
                    Ok(RawRequeueJob {
                        artifact_path: row.get(0)?,
                        filename: row.get(1)?,
                        idempotency_key: row.get(2)?,
                        file_size: row.get(3)?,
                        sha256: row.get(4)?,
                        delivery_binding: row.get(5)?,
                        state: row.get(6)?,
                        job_revision: row.get(7)?,
                        requeue_count: row.get(8)?,
                    })
                },
            )
            .optional()?
            .ok_or(UploadStoreError::RequeueJobNotFound)?;
        if raw.state != "permanently_failed" {
            return Err(UploadStoreError::RequeueWrongState);
        }
        if raw.job_revision != expected_job_revision {
            return Err(UploadStoreError::RequeueStaleJobRevision);
        }
        let stored_binding =
            UploadDeliveryBinding::from_database(request.job_id, raw.delivery_binding)?;
        if stored_binding != self.delivery_binding {
            return Err(UploadStoreError::RequeueDeliveryBindingMismatch);
        }
        let sha256 = sha256_from_database(request.job_id, raw.sha256)?;
        let artifact_path = validate_stored_identity(
            request.job_id,
            raw.artifact_path,
            &raw.filename,
            &raw.idempotency_key,
            &sha256,
            &self.capture_root,
        )?;
        let file_size = database_u64("file_size", raw.file_size)?;
        let verified_file = open_verified_requeue_artifact(&artifact_path, file_size, &sha256)?;
        let next_job_revision = raw
            .job_revision
            .checked_add(1)
            .ok_or(UploadStoreError::JobRevisionOverflow(request.job_id))?;
        let next_requeue_count = raw
            .requeue_count
            .checked_add(1)
            .ok_or(UploadStoreError::RequeueCountOverflow(request.job_id))?;
        let changed = transaction.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'pending', next_attempt_at_ms = NULL,
                updated_at_ms = ?1, job_revision = ?2,
                requeue_count = ?3, last_requeued_at_ms = ?1
            WHERE id = ?4 AND state = 'permanently_failed'
              AND job_revision = ?5 AND delivery_binding_sha256 = ?6
              AND EXISTS (
                  SELECT 1 FROM upload_metadata
                  WHERE singleton = 1 AND ledger_id = ?7
              )
            "#,
            params![
                requeued_at,
                next_job_revision,
                next_requeue_count,
                request.job_id.0,
                expected_job_revision,
                self.delivery_binding.as_slice(),
                self.ledger_id.0.as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(UploadStoreError::RequeueStaleJobRevision);
        }
        let ledger_revision: i64 = transaction.query_row(
            "SELECT ledger_revision FROM upload_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let updated = transaction.query_row(
            r#"
            SELECT id, artifact_path, filename, idempotency_key,
                   file_size, sha256, delivery_binding_sha256,
                   state, attempt_count, next_attempt_at_ms,
                   last_http_status, last_error, created_at_ms,
                   updated_at_ms, completed_at_ms, last_failure_at_ms,
                   job_revision, requeue_count, last_requeued_at_ms
            FROM upload_jobs WHERE id = ?1
            "#,
            [request.job_id.0],
            RawUploadJob::from_row,
        )?;
        let job = updated.into_list_item(&self.capture_root, self.delivery_binding)?;
        transaction.commit()?;
        drop(verified_file);
        Ok(RequeueUploadResult {
            ledger_revision: database_u64("ledger_revision", ledger_revision)?,
            job,
        })
    }

    pub(crate) fn reconcile_capture_directory(
        &mut self,
        recorded_at_unix_ms: u64,
    ) -> Result<ReconcileResult, UploadStoreError> {
        unix_ms_to_database(recorded_at_unix_ms)?;
        let entries = fs::read_dir(&self.capture_root).map_err(|source| UploadStoreError::Io {
            operation: "read capture directory",
            path: self.capture_root.clone(),
            source,
        })?;
        let mut result = ReconcileResult::default();
        let mut eligible_paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| UploadStoreError::Io {
                operation: "read capture-directory entry",
                path: self.capture_root.clone(),
                source,
            })?;
            result.examined = checked_reconcile_increment(result.examined)?;
            let path = entry.path();
            let Some(filename) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_generated_frame_filename(&filename) {
                continue;
            }
            let artifact_path = path
                .to_str()
                .ok_or_else(|| UploadStoreError::ArtifactPathNotUtf8(path.clone()))?;
            if self.is_preactivation_artifact(artifact_path)? {
                continue;
            }
            // Avoid re-opening and hashing every historical job on each
            // restart. Newly produced names contain a random session nonce,
            // and direct recording verifies the complete identity on a path
            // collision. Legacy-path replacement is still caught when the
            // persisted claim is verified immediately before upload.
            if self.recorded_job_id(artifact_path)?.is_some() {
                result.eligible = checked_reconcile_increment(result.eligible)?;
                result.already_recorded = checked_reconcile_increment(result.already_recorded)?;
                continue;
            }
            let file_type = entry.file_type().map_err(|source| UploadStoreError::Io {
                operation: "inspect capture artifact",
                path: path.clone(),
                source,
            })?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            result.eligible = checked_reconcile_increment(result.eligible)?;
            eligible_paths.push(path);
        }
        eligible_paths.sort_unstable();
        for path in eligible_paths {
            match self
                .record_artifact(&path, recorded_at_unix_ms)?
                .disposition
            {
                RecordDisposition::Inserted => {
                    result.inserted = checked_reconcile_increment(result.inserted)?;
                }
                RecordDisposition::AlreadyRecorded => {
                    result.already_recorded = checked_reconcile_increment(result.already_recorded)?;
                }
            }
        }
        Ok(result)
    }

    fn recorded_job_id(
        &self,
        artifact_path: &str,
    ) -> Result<Option<UploadJobId>, UploadStoreError> {
        let raw_id = self
            .connection
            .query_row(
                "SELECT id FROM upload_jobs WHERE artifact_path = ?1",
                [artifact_path],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        raw_id.map(job_id_from_database).transpose()
    }

    fn is_preactivation_artifact(&self, artifact_path: &str) -> Result<bool, UploadStoreError> {
        Ok(self.connection.query_row(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM upload_preactivation_artifacts
                WHERE artifact_path = ?1
            )
            "#,
            [artifact_path],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
pub(crate) fn verify_claimed_artifact(
    claim: &ClaimedUpload,
) -> Result<ArtifactVerification, UploadStoreError> {
    match snapshot_claimed_artifact(claim)? {
        SnapshottedClaimedArtifact::Verified(_) => Ok(ArtifactVerification::Verified),
        SnapshottedClaimedArtifact::Rejected(reason) => Ok(reason),
    }
}

/// Copies the claimed artifact into a private anonymous file, then verifies the
/// snapshot's size and digest before returning it for streaming. The source is
/// closed before HTTP starts, so later mutation or replacement cannot change
/// the bytes sent under the claim's idempotency key.
pub(crate) fn snapshot_claimed_artifact(
    claim: &ClaimedUpload,
) -> Result<SnapshottedClaimedArtifact, UploadStoreError> {
    let metadata = match fs::symlink_metadata(&claim.artifact_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SnapshottedClaimedArtifact::Rejected(
                ArtifactVerification::Missing,
            ));
        }
        Err(source) => {
            return Err(UploadStoreError::Io {
                operation: "inspect claimed upload artifact",
                path: claim.artifact_path.clone(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(SnapshottedClaimedArtifact::Rejected(
            ArtifactVerification::Symlink,
        ));
    }
    if !metadata.is_file() {
        return Ok(SnapshottedClaimedArtifact::Rejected(
            ArtifactVerification::NotRegularFile,
        ));
    }

    let mut file = match open_artifact_for_read(&claim.artifact_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SnapshottedClaimedArtifact::Rejected(
                ArtifactVerification::Missing,
            ));
        }
        Err(source) => {
            return Err(UploadStoreError::Io {
                operation: "open claimed upload artifact",
                path: claim.artifact_path.clone(),
                source,
            });
        }
    };
    let opened_metadata = file.metadata().map_err(|source| UploadStoreError::Io {
        operation: "inspect opened upload artifact",
        path: claim.artifact_path.clone(),
        source,
    })?;
    if !opened_metadata.is_file() {
        return Ok(SnapshottedClaimedArtifact::Rejected(
            ArtifactVerification::NotRegularFile,
        ));
    }
    if opened_metadata.len() != claim.file_size {
        return Ok(SnapshottedClaimedArtifact::Rejected(
            ArtifactVerification::SizeMismatch {
                expected: claim.file_size,
                actual: opened_metadata.len(),
            },
        ));
    }

    let mut snapshot = tempfile::tempfile().map_err(|source| UploadStoreError::Io {
        operation: "create anonymous upload snapshot",
        path: claim.artifact_path.clone(),
        source,
    })?;
    let (actual_size, actual) = copy_and_hash_snapshot(
        &mut file,
        &mut snapshot,
        claim.file_size,
        &claim.artifact_path,
    )?;
    if actual_size != claim.file_size {
        return Ok(SnapshottedClaimedArtifact::Rejected(
            ArtifactVerification::SizeMismatch {
                expected: claim.file_size,
                actual: actual_size,
            },
        ));
    }
    if actual != claim.sha256 {
        return Ok(SnapshottedClaimedArtifact::Rejected(
            ArtifactVerification::Sha256Mismatch {
                expected: claim.sha256,
                actual,
            },
        ));
    }
    snapshot.rewind().map_err(|source| UploadStoreError::Io {
        operation: "rewind verified upload snapshot",
        path: claim.artifact_path.clone(),
        source,
    })?;
    Ok(SnapshottedClaimedArtifact::Verified(snapshot))
}

fn open_verified_requeue_artifact(
    path: &Path,
    expected_size: u64,
    expected_sha256: &[u8; 32],
) -> Result<File, UploadStoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(UploadStoreError::RequeueArtifactUnavailable);
        }
        Err(source) => {
            return Err(UploadStoreError::Io {
                operation: "inspect artifact for requeue",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UploadStoreError::RequeueArtifactUnavailable);
    }
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(UploadStoreError::RequeueArtifactUnavailable);
        }
        Err(source) => {
            return Err(UploadStoreError::Io {
                operation: "resolve artifact for requeue",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if canonical != path {
        return Err(UploadStoreError::RequeueArtifactUnavailable);
    }
    let mut file = match open_artifact_for_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(UploadStoreError::RequeueArtifactUnavailable);
        }
        Err(source) => {
            return Err(UploadStoreError::Io {
                operation: "open artifact for requeue",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let opened_metadata = file.metadata().map_err(|source| UploadStoreError::Io {
        operation: "inspect opened artifact for requeue",
        path: path.to_path_buf(),
        source,
    })?;
    if !opened_metadata.is_file() {
        return Err(UploadStoreError::RequeueArtifactUnavailable);
    }
    if opened_metadata.len() != expected_size {
        return Err(UploadStoreError::RequeueArtifactChanged);
    }
    let actual_sha256 = sha256_open_file(&mut file, path)?;
    let final_size = file
        .metadata()
        .map_err(|source| UploadStoreError::Io {
            operation: "reinspect opened artifact for requeue",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if final_size != expected_size || actual_sha256 != *expected_sha256 {
        return Err(UploadStoreError::RequeueArtifactChanged);
    }
    Ok(file)
}

fn copy_and_hash_snapshot(
    source: &mut File,
    snapshot: &mut File,
    expected_size: u64,
    source_path: &Path,
) -> Result<(u64, [u8; 32]), UploadStoreError> {
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let remaining_with_probe = expected_size.saturating_sub(copied).saturating_add(1);
        let read_limit = usize::try_from(remaining_with_probe)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count =
            source
                .read(&mut buffer[..read_limit])
                .map_err(|source| UploadStoreError::Io {
                    operation: "read artifact into upload snapshot",
                    path: source_path.to_path_buf(),
                    source,
                })?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(count as u64);
        if copied > expected_size {
            return Ok((copied, hasher.finalize().into()));
        }
        snapshot
            .write_all(&buffer[..count])
            .map_err(|source| UploadStoreError::Io {
                operation: "write anonymous upload snapshot",
                path: source_path.to_path_buf(),
                source,
            })?;
        hasher.update(&buffer[..count]);
    }
    Ok((copied, hasher.finalize().into()))
}

struct ArtifactValues {
    artifact_path: String,
    filename: String,
    idempotency_key: String,
    file_size: u64,
    sha256: [u8; 32],
}

impl ArtifactValues {
    fn read(path: &Path, capture_root: &Path) -> Result<Self, UploadStoreError> {
        if !path.is_absolute() {
            return Err(UploadStoreError::ArtifactPathNotAbsolute(
                path.to_path_buf(),
            ));
        }
        if path.to_str().is_none() {
            return Err(UploadStoreError::ArtifactPathNotUtf8(path.to_path_buf()));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| UploadStoreError::Io {
            operation: "inspect finalized artifact",
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(UploadStoreError::ArtifactIsSymlink(path.to_path_buf()));
        }
        if !metadata.is_file() {
            return Err(UploadStoreError::ArtifactNotRegularFile(path.to_path_buf()));
        }
        let canonical = fs::canonicalize(path).map_err(|source| UploadStoreError::Io {
            operation: "resolve finalized artifact",
            path: path.to_path_buf(),
            source,
        })?;
        if canonical.parent() != Some(capture_root) {
            return Err(UploadStoreError::ArtifactOutsideCaptureRoot(canonical));
        }
        let artifact_path = canonical
            .to_str()
            .ok_or_else(|| UploadStoreError::ArtifactPathNotUtf8(canonical.clone()))?
            .to_owned();
        let filename = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| is_generated_frame_filename(name))
            .ok_or_else(|| UploadStoreError::InvalidArtifactFilename(canonical.clone()))?
            .to_owned();
        let mut file =
            open_artifact_for_read(&canonical).map_err(|source| UploadStoreError::Io {
                operation: "open finalized artifact",
                path: canonical.clone(),
                source,
            })?;
        let opened_metadata = file.metadata().map_err(|source| UploadStoreError::Io {
            operation: "inspect opened finalized artifact",
            path: canonical.clone(),
            source,
        })?;
        if !opened_metadata.is_file() {
            return Err(UploadStoreError::ArtifactNotRegularFile(canonical));
        }
        let sha256 = sha256_open_file(&mut file, &canonical)?;
        let idempotency_key = idempotency_key(&filename, &sha256);
        Ok(Self {
            artifact_path,
            idempotency_key,
            filename,
            file_size: opened_metadata.len(),
            sha256,
        })
    }
}

struct RawClaim {
    id: i64,
    artifact_path: String,
    filename: String,
    idempotency_key: String,
    file_size: i64,
    sha256: Vec<u8>,
    delivery_binding: Vec<u8>,
    attempt_count: i64,
}

impl RawClaim {
    fn into_claim(
        self,
        job_id: UploadJobId,
        capture_root: &Path,
        current_delivery_binding: UploadDeliveryBinding,
    ) -> Result<ClaimedUpload, UploadStoreError> {
        let sha256 = sha256_from_database(job_id, self.sha256)?;
        let delivery_binding = UploadDeliveryBinding::from_database(job_id, self.delivery_binding)?;
        if delivery_binding != current_delivery_binding {
            return Err(UploadStoreError::UploadJobDeliveryBindingMismatch(job_id));
        }
        let artifact_path = validate_stored_identity(
            job_id,
            self.artifact_path,
            &self.filename,
            &self.idempotency_key,
            &sha256,
            capture_root,
        )?;
        Ok(ClaimedUpload {
            job_id,
            artifact_path,
            filename: self.filename,
            idempotency_key: self.idempotency_key,
            file_size: database_u64("file_size", self.file_size)?,
            sha256,
            attempt_count: database_u64("attempt_count", self.attempt_count)?,
        })
    }
}

struct RawUploadJob {
    id: i64,
    artifact_path: String,
    filename: String,
    idempotency_key: String,
    file_size: i64,
    sha256: Vec<u8>,
    delivery_binding: Vec<u8>,
    state: String,
    attempt_count: i64,
    next_attempt_at_ms: Option<i64>,
    last_http_status: Option<i64>,
    last_error: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    completed_at_ms: Option<i64>,
    last_failure_at_ms: Option<i64>,
    job_revision: i64,
    requeue_count: i64,
    last_requeued_at_ms: Option<i64>,
}

impl RawUploadJob {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            artifact_path: row.get(1)?,
            filename: row.get(2)?,
            idempotency_key: row.get(3)?,
            file_size: row.get(4)?,
            sha256: row.get(5)?,
            delivery_binding: row.get(6)?,
            state: row.get(7)?,
            attempt_count: row.get(8)?,
            next_attempt_at_ms: row.get(9)?,
            last_http_status: row.get(10)?,
            last_error: row.get(11)?,
            created_at_ms: row.get(12)?,
            updated_at_ms: row.get(13)?,
            completed_at_ms: row.get(14)?,
            last_failure_at_ms: row.get(15)?,
            job_revision: row.get(16)?,
            requeue_count: row.get(17)?,
            last_requeued_at_ms: row.get(18)?,
        })
    }

    #[cfg(test)]
    fn into_record(
        self,
        capture_root: &Path,
        current_delivery_binding: UploadDeliveryBinding,
    ) -> Result<UploadJobRecord, UploadStoreError> {
        let job_id = job_id_from_database(self.id)?;
        let sha256 = sha256_from_database(job_id, self.sha256)?;
        let delivery_binding = UploadDeliveryBinding::from_database(job_id, self.delivery_binding)?;
        let artifact_path = validate_stored_identity(
            job_id,
            self.artifact_path,
            &self.filename,
            &self.idempotency_key,
            &sha256,
            capture_root,
        )?;
        Ok(UploadJobRecord {
            job_id,
            artifact_path,
            filename: self.filename,
            idempotency_key: self.idempotency_key,
            file_size: database_u64("file_size", self.file_size)?,
            sha256,
            state: UploadState::from_database(self.state)?,
            attempt_count: database_u64("attempt_count", self.attempt_count)?,
            next_attempt_at_unix_ms: optional_database_u64(
                "next_attempt_at_ms",
                self.next_attempt_at_ms,
            )?,
            last_http_status: optional_database_u16("last_http_status", self.last_http_status)?,
            last_error: self.last_error,
            created_at_unix_ms: database_u64("created_at_ms", self.created_at_ms)?,
            updated_at_unix_ms: database_u64("updated_at_ms", self.updated_at_ms)?,
            completed_at_unix_ms: optional_database_u64("completed_at_ms", self.completed_at_ms)?,
            last_failure_at_unix_ms: optional_database_u64(
                "last_failure_at_ms",
                self.last_failure_at_ms,
            )?,
            job_revision: database_u64("job_revision", self.job_revision)?,
            requeue_count: database_u64("requeue_count", self.requeue_count)?,
            last_requeued_at_unix_ms: optional_database_u64(
                "last_requeued_at_ms",
                self.last_requeued_at_ms,
            )?,
            delivery_binding_is_current: delivery_binding == current_delivery_binding,
        })
    }

    fn into_list_item(
        self,
        capture_root: &Path,
        current_delivery_binding: UploadDeliveryBinding,
    ) -> Result<UploadListItem, UploadStoreError> {
        let job_id = job_id_from_database(self.id)?;
        let sha256 = sha256_from_database(job_id, self.sha256)?;
        let delivery_binding = UploadDeliveryBinding::from_database(job_id, self.delivery_binding)?;
        let artifact_path = validate_stored_identity(
            job_id,
            self.artifact_path,
            &self.filename,
            &self.idempotency_key,
            &sha256,
            capture_root,
        )?;
        Ok(UploadListItem {
            job_id,
            artifact_path,
            filename: self.filename,
            file_size: database_u64("file_size", self.file_size)?,
            sha256,
            state: UploadState::from_database(self.state)?,
            attempt_count: database_u64("attempt_count", self.attempt_count)?,
            next_attempt_at_unix_ms: optional_database_u64(
                "next_attempt_at_ms",
                self.next_attempt_at_ms,
            )?,
            last_http_status: optional_database_u16("last_http_status", self.last_http_status)?,
            last_error: self.last_error,
            created_at_unix_ms: database_u64("created_at_ms", self.created_at_ms)?,
            updated_at_unix_ms: database_u64("updated_at_ms", self.updated_at_ms)?,
            completed_at_unix_ms: optional_database_u64("completed_at_ms", self.completed_at_ms)?,
            last_failure_at_unix_ms: optional_database_u64(
                "last_failure_at_ms",
                self.last_failure_at_ms,
            )?,
            job_revision: database_u64("job_revision", self.job_revision)?,
            requeue_count: database_u64("requeue_count", self.requeue_count)?,
            last_requeued_at_unix_ms: optional_database_u64(
                "last_requeued_at_ms",
                self.last_requeued_at_ms,
            )?,
            delivery_binding_is_current: delivery_binding == current_delivery_binding,
        })
    }
}

struct RawRequeueJob {
    artifact_path: String,
    filename: String,
    idempotency_key: String,
    file_size: i64,
    sha256: Vec<u8>,
    delivery_binding: Vec<u8>,
    state: String,
    job_revision: i64,
    requeue_count: i64,
}

fn collect_list_items(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<RawUploadJob>,
    >,
    capture_root: &Path,
    current_delivery_binding: UploadDeliveryBinding,
) -> Result<Vec<UploadListItem>, UploadStoreError> {
    rows.map(|row| {
        row.map_err(UploadStoreError::from)?
            .into_list_item(capture_root, current_delivery_binding)
    })
    .collect()
}

fn canonical_capture_root(path: &Path) -> Result<PathBuf, UploadStoreError> {
    if !path.is_absolute() {
        return Err(UploadStoreError::CaptureDirectoryNotAbsolute(
            path.to_path_buf(),
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|source| UploadStoreError::Io {
        operation: "resolve capture directory",
        path: path.to_path_buf(),
        source,
    })?;
    if canonical.to_str().is_none() {
        return Err(UploadStoreError::CaptureDirectoryNotUtf8(canonical));
    }
    Ok(canonical)
}

fn normalize_destination(destination: &str) -> Result<String, UploadStoreError> {
    let normalized = destination.trim();
    if normalized.is_empty() || normalized.chars().any(char::is_control) {
        return Err(UploadStoreError::InvalidDestination);
    }
    Ok(normalized.to_owned())
}

pub(crate) fn read_ledger_id(connection: &Connection) -> Result<UploadLedgerId, UploadStoreError> {
    let value: Vec<u8> = connection.query_row(
        "SELECT ledger_id FROM upload_metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    UploadLedgerId::from_database(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedListCursor {
    ledger_tag: [u8; 8],
    ledger_revision: u64,
    high_water_id: u64,
    before_id: u64,
    filter_mask: u8,
    page_size: u16,
}

fn encode_list_cursor(
    ledger_id: UploadLedgerId,
    ledger_revision: u64,
    high_water_id: u64,
    before_id: u64,
    filter_mask: u8,
    page_size: u16,
) -> UploadListCursor {
    let mut payload = [0_u8; LIST_CURSOR_PAYLOAD_LENGTH];
    payload[..8].copy_from_slice(&list_cursor_ledger_tag(ledger_id));
    payload[8..16].copy_from_slice(&ledger_revision.to_be_bytes());
    payload[16..24].copy_from_slice(&high_water_id.to_be_bytes());
    payload[24..32].copy_from_slice(&before_id.to_be_bytes());
    payload[32] = filter_mask;
    payload[33..35].copy_from_slice(&page_size.to_be_bytes());
    let mac = list_cursor_mac(ledger_id, &payload);
    let mut encoded = String::with_capacity(LIST_CURSOR_ENCODED_LENGTH);
    encoded.push_str(LIST_CURSOR_PREFIX);
    encoded.push_str(&hex_encode(&payload));
    encoded.push_str(&hex_encode(&mac));
    UploadListCursor(encoded)
}

fn decode_list_cursor(
    cursor: &str,
    ledger_id: UploadLedgerId,
) -> Result<DecodedListCursor, UploadStoreError> {
    if cursor.len() != LIST_CURSOR_ENCODED_LENGTH || !cursor.starts_with(LIST_CURSOR_PREFIX) {
        return Err(UploadStoreError::InvalidListCursor);
    }
    let encoded = &cursor[LIST_CURSOR_PREFIX.len()..];
    let bytes =
        hex_decode_exact::<{ LIST_CURSOR_PAYLOAD_LENGTH + LIST_CURSOR_MAC_LENGTH }>(encoded)
            .ok_or(UploadStoreError::InvalidListCursor)?;
    let (payload, supplied_mac) = bytes.split_at(LIST_CURSOR_PAYLOAD_LENGTH);
    let mut ledger_tag = [0_u8; 8];
    ledger_tag.copy_from_slice(&payload[..8]);
    if ledger_tag != list_cursor_ledger_tag(ledger_id) {
        return Err(UploadStoreError::StaleListCursor);
    }
    let expected_mac = list_cursor_mac(ledger_id, payload);
    if !constant_time_equal(supplied_mac, &expected_mac) {
        return Err(UploadStoreError::InvalidListCursor);
    }
    let mut revision = [0_u8; 8];
    revision.copy_from_slice(&payload[8..16]);
    let mut high_water = [0_u8; 8];
    high_water.copy_from_slice(&payload[16..24]);
    let mut before = [0_u8; 8];
    before.copy_from_slice(&payload[24..32]);
    let filter_mask = payload[32];
    if filter_mask == 0 || filter_mask & !ALL_UPLOAD_STATE_FILTER_MASK != 0 {
        return Err(UploadStoreError::InvalidListCursor);
    }
    let mut page_size = [0_u8; 2];
    page_size.copy_from_slice(&payload[33..35]);
    Ok(DecodedListCursor {
        ledger_tag,
        ledger_revision: u64::from_be_bytes(revision),
        high_water_id: u64::from_be_bytes(high_water),
        before_id: u64::from_be_bytes(before),
        filter_mask,
        page_size: u16::from_be_bytes(page_size),
    })
}

fn canonical_state_filter_mask(states: &[UploadState]) -> Result<u8, UploadStoreError> {
    if states.is_empty() {
        return Ok(ALL_UPLOAD_STATE_FILTER_MASK);
    }
    let mut mask = 0_u8;
    for state in states {
        let bit = state.filter_bit();
        if mask & bit != 0 {
            return Err(UploadStoreError::DuplicateListStateFilter);
        }
        mask |= bit;
    }
    Ok(mask)
}

fn list_cursor_ledger_tag(ledger_id: UploadLedgerId) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(LIST_CURSOR_LEDGER_DOMAIN);
    hasher.update(ledger_id.0);
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..8]
        .try_into()
        .expect("an eight-byte SHA-256 prefix has a fixed length")
}

fn list_cursor_mac(ledger_id: UploadLedgerId, payload: &[u8]) -> [u8; LIST_CURSOR_MAC_LENGTH] {
    let mut hasher = Sha256::new();
    hasher.update(LIST_CURSOR_DOMAIN);
    hasher.update(ledger_id.0);
    hasher.update(payload);
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..LIST_CURSOR_MAC_LENGTH]
        .try_into()
        .expect("the cursor MAC prefix has a fixed length")
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

fn hex_decode_exact<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 || !value.is_ascii() {
        return None;
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(decoded)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn configure_durability(connection: &Connection) -> Result<(), UploadStoreError> {
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(configuration_error("journal_mode=WAL", journal_mode));
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if synchronous != 2 {
        return Err(configuration_error(
            "synchronous=FULL",
            synchronous.to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn acquire_exclusive_ownership(connection: &Connection) -> Result<(), UploadStoreError> {
    let locking_mode: String =
        connection.pragma_update_and_check(None, "locking_mode", "EXCLUSIVE", |row| row.get(0))?;
    if !locking_mode.eq_ignore_ascii_case("exclusive") {
        return Err(configuration_error("locking_mode=EXCLUSIVE", locking_mode));
    }
    connection.execute_batch("BEGIN EXCLUSIVE; COMMIT;")?;
    Ok(())
}

fn configuration_error(name: &'static str, actual: String) -> UploadStoreError {
    UploadStoreError::Configuration { name, actual }
}

pub(crate) fn schema_version(connection: &Connection) -> Result<i64, UploadStoreError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn database_is_completely_empty(connection: &Connection) -> Result<bool, UploadStoreError> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let object_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(application_id == 0 && object_count == 0)
}

fn inventory_generated_artifacts(capture_root: &Path) -> Result<Vec<String>, UploadStoreError> {
    let entries = fs::read_dir(capture_root).map_err(|source| UploadStoreError::Io {
        operation: "inventory capture directory before upload activation",
        path: capture_root.to_path_buf(),
        source,
    })?;
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| UploadStoreError::Io {
            operation: "read capture-directory entry before upload activation",
            path: capture_root.to_path_buf(),
            source,
        })?;
        let Some(filename) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_generated_frame_filename(&filename) {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| UploadStoreError::Io {
            operation: "inspect capture artifact before upload activation",
            path: path.clone(),
            source,
        })?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        artifacts.push(
            path.to_str()
                .ok_or_else(|| UploadStoreError::ArtifactPathNotUtf8(path.clone()))?
                .to_owned(),
        );
    }
    artifacts.sort_unstable();
    artifacts.dedup();
    Ok(artifacts)
}

fn create_schema(
    connection: &mut Connection,
    capture_root: &str,
    destination: &str,
    authorization_fingerprint: UploadAuthorizationFingerprint,
    ledger_id: UploadLedgerId,
    preactivation_artifacts: &[String],
) -> Result<(), UploadStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(CREATE_METADATA)?;
    transaction.execute_batch(CREATE_PREACTIVATION_ARTIFACTS)?;
    transaction.execute_batch(CREATE_UPLOAD_JOBS)?;
    transaction.execute_batch(CREATE_DUE_INDEX)?;
    transaction.execute_batch(CREATE_INSERT_AGGREGATE_TRIGGER)?;
    transaction.execute_batch(CREATE_UPDATE_AGGREGATE_TRIGGER)?;
    transaction.execute_batch(CREATE_JOB_REVISION_GUARD_TRIGGER)?;
    transaction.execute_batch(CREATE_IMMUTABLE_IDENTITY_TRIGGER)?;
    transaction.execute_batch(CREATE_IMMUTABLE_METADATA_TRIGGER)?;
    transaction.execute_batch(CREATE_NO_DELETE_TRIGGER)?;
    transaction.execute(
        r#"
        INSERT INTO upload_metadata (
            singleton, schema_signature, ledger_id, ledger_revision,
            capture_root, destination, authorization_sha256
        ) VALUES (1, ?1, ?2, 0, ?3, ?4, ?5)
        "#,
        params![
            SCHEMA_SIGNATURE,
            ledger_id.0.as_slice(),
            capture_root,
            destination,
            authorization_fingerprint.as_slice()
        ],
    )?;
    {
        let mut insert = transaction
            .prepare("INSERT INTO upload_preactivation_artifacts (artifact_path) VALUES (?1)")?;
        for artifact_path in preactivation_artifacts {
            insert.execute([artifact_path])?;
        }
    }
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn verify_schema(
    connection: &Connection,
    capture_root: &str,
    destination: &str,
) -> Result<(), UploadStoreError> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(UploadStoreError::InvalidSchema);
    }
    let user_schema_object_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if user_schema_object_count != 10 {
        return Err(UploadStoreError::InvalidSchema);
    }
    for (kind, name, expected) in [
        ("table", "upload_metadata", CREATE_METADATA),
        (
            "table",
            "upload_preactivation_artifacts",
            CREATE_PREACTIVATION_ARTIFACTS,
        ),
        ("table", "upload_jobs", CREATE_UPLOAD_JOBS),
        ("index", "upload_jobs_due", CREATE_DUE_INDEX),
        (
            "trigger",
            "upload_jobs_after_insert",
            CREATE_INSERT_AGGREGATE_TRIGGER,
        ),
        (
            "trigger",
            "upload_jobs_after_status_update",
            CREATE_UPDATE_AGGREGATE_TRIGGER,
        ),
        (
            "trigger",
            "upload_jobs_revision_guard",
            CREATE_JOB_REVISION_GUARD_TRIGGER,
        ),
        (
            "trigger",
            "upload_jobs_identity_immutable",
            CREATE_IMMUTABLE_IDENTITY_TRIGGER,
        ),
        (
            "trigger",
            "upload_metadata_identity_immutable",
            CREATE_IMMUTABLE_METADATA_TRIGGER,
        ),
        ("trigger", "upload_jobs_no_delete", CREATE_NO_DELETE_TRIGGER),
    ] {
        let actual: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = ?1 AND name = ?2",
                params![kind, name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(actual) = actual else {
            return Err(UploadStoreError::MissingSchema);
        };
        if normalize_sql(&actual) != normalize_sql(expected) {
            return Err(UploadStoreError::InvalidSchema);
        }
    }
    connection.prepare(VERIFY_SCHEMA_PROJECTION)?;
    connection.prepare("SELECT artifact_path FROM upload_preactivation_artifacts LIMIT 0")?;
    let (signature, ledger_id, ledger_revision, stored_root, stored_destination, authorization): (
        String,
        Vec<u8>,
        i64,
        String,
        String,
        Vec<u8>,
    ) = connection.query_row(
        r#"
        SELECT schema_signature, ledger_id, ledger_revision, capture_root,
               destination, authorization_sha256
        FROM upload_metadata WHERE singleton = 1
        "#,
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if signature != SCHEMA_SIGNATURE
        || ledger_id.len() != 16
        || ledger_revision < 0
        || authorization.len() != 32
    {
        return Err(UploadStoreError::InvalidSchema);
    }
    if stored_root != capture_root {
        return Err(UploadStoreError::CaptureRootMismatch);
    }
    if stored_destination != destination {
        return Err(UploadStoreError::DestinationMismatch);
    }
    Ok(())
}

/// Verifies the identity fields shared by legacy and current upload rows.
///
/// This intentionally validates only persisted metadata. Completed artifacts
/// may already have been removed by retention, so maintenance must not require
/// the recorded file to still exist merely to trust or migrate its ledger row.
pub(crate) fn verify_persisted_artifact_identities(
    connection: &Connection,
    capture_root: &Path,
) -> Result<(), UploadStoreError> {
    let mut statement = connection.prepare(
        r#"
        SELECT id, artifact_path, filename, idempotency_key, sha256
        FROM upload_jobs ORDER BY id
        "#,
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    for row in rows {
        let (raw_id, artifact_path, filename, stored_idempotency_key, raw_sha256) = row?;
        let job_id = job_id_from_database(raw_id)?;
        let sha256 = sha256_from_database(job_id, raw_sha256)?;
        validate_stored_identity(
            job_id,
            artifact_path,
            &filename,
            &stored_idempotency_key,
            &sha256,
            capture_root,
        )?;
    }
    verify_persisted_preactivation_paths(connection, capture_root)
}

/// Applies the complete runtime row decoder to every current-schema job.
/// Unlike an upload claim this is file-independent, but it rejects malformed
/// IDs, hashes, paths, states, counters, timestamps, and delivery bindings.
pub(crate) fn verify_persisted_v4_rows(
    connection: &Connection,
    capture_root: &Path,
) -> Result<(), UploadStoreError> {
    let (destination, raw_authorization): (String, Vec<u8>) = connection.query_row(
        r#"
        SELECT destination, authorization_sha256
        FROM upload_metadata WHERE singleton = 1
        "#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let authorization: [u8; 32] = raw_authorization
        .try_into()
        .map_err(|_| UploadStoreError::InvalidSchema)?;
    let current_delivery_binding =
        UploadDeliveryBinding::derive(&destination, UploadAuthorizationFingerprint(authorization));
    let mut statement = connection.prepare(
        r#"
        SELECT id, artifact_path, filename, idempotency_key,
               file_size, sha256, delivery_binding_sha256,
               state, attempt_count, next_attempt_at_ms,
               last_http_status, last_error, created_at_ms,
               updated_at_ms, completed_at_ms, last_failure_at_ms,
               job_revision, requeue_count, last_requeued_at_ms
        FROM upload_jobs ORDER BY id
        "#,
    )?;
    let rows = statement.query_map([], RawUploadJob::from_row)?;
    for row in rows {
        row?.into_list_item(capture_root, current_delivery_binding)?;
    }
    verify_persisted_preactivation_paths(connection, capture_root)
}

fn verify_persisted_preactivation_paths(
    connection: &Connection,
    capture_root: &Path,
) -> Result<(), UploadStoreError> {
    let mut statement = connection.prepare(
        "SELECT artifact_path FROM upload_preactivation_artifacts ORDER BY artifact_path",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        validate_preactivation_artifact_path(row?, capture_root)?;
    }
    Ok(())
}

/// Rebinds a drained ledger without discarding terminal history. The immediate
/// transaction makes the live-work test and fingerprint update indivisible;
/// interrupted `in_progress` work is deliberately checked before recovery.
fn rotate_authorization_if_drained(
    connection: &mut Connection,
    capture_root: &Path,
    authorization_fingerprint: UploadAuthorizationFingerprint,
) -> Result<(), UploadStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let stored: Vec<u8> = transaction.query_row(
        "SELECT authorization_sha256 FROM upload_metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if stored.as_slice() != authorization_fingerprint.as_slice() {
        let has_live_work: bool = transaction.query_row(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM upload_jobs
                WHERE state IN ('pending', 'in_progress', 'retrying')
            )
            "#,
            [],
            |row| row.get(0),
        )?;
        if has_live_work || has_unrecorded_reconcilable_artifact(&transaction, capture_root)? {
            return Err(UploadStoreError::AuthorizationIdentityMismatch);
        }
        let changed = transaction.execute(
            r#"
            UPDATE upload_metadata
            SET authorization_sha256 = ?1,
                ledger_revision = ledger_revision + 1
            WHERE singleton = 1
            "#,
            [authorization_fingerprint.as_slice()],
        )?;
        if changed != 1 {
            return Err(UploadStoreError::InvalidSchema);
        }
    }
    transaction.commit()?;
    Ok(())
}

/// Finds publish/record crash-gap files without hashing them. A different
/// authorization identity may not claim those files merely because SQLite has
/// no live row yet; the prior identity must reconcile and drain them first.
fn has_unrecorded_reconcilable_artifact(
    transaction: &rusqlite::Transaction<'_>,
    capture_root: &Path,
) -> Result<bool, UploadStoreError> {
    let entries = fs::read_dir(capture_root).map_err(|source| UploadStoreError::Io {
        operation: "inspect capture directory before authorization rotation",
        path: capture_root.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| UploadStoreError::Io {
            operation: "read capture-directory entry before authorization rotation",
            path: capture_root.to_path_buf(),
            source,
        })?;
        let Some(filename) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_generated_frame_filename(&filename) {
            continue;
        }
        let path = entry.path();
        let artifact_path = path
            .to_str()
            .ok_or_else(|| UploadStoreError::ArtifactPathNotUtf8(path.clone()))?;
        let (preactivation, recorded): (bool, bool) = transaction.query_row(
            r#"
            SELECT
                EXISTS (
                    SELECT 1 FROM upload_preactivation_artifacts
                    WHERE artifact_path = ?1
                ),
                EXISTS (
                    SELECT 1 FROM upload_jobs WHERE artifact_path = ?1
                )
            "#,
            [artifact_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if preactivation || recorded {
            continue;
        }
        let file_type = entry.file_type().map_err(|source| UploadStoreError::Io {
            operation: "inspect capture artifact before authorization rotation",
            path: path.clone(),
            source,
        })?;
        if file_type.is_file() && !file_type.is_symlink() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn recover_interrupted_jobs(connection: &mut Connection) -> Result<(), UploadStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        r#"
        UPDATE upload_jobs
        SET state = 'pending', next_attempt_at_ms = NULL,
            job_revision = job_revision + 1
        WHERE state = 'in_progress'
        "#,
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .to_owned()
}

fn validate_stored_identity(
    job_id: UploadJobId,
    artifact_path: String,
    filename: &str,
    stored_idempotency_key: &str,
    sha256: &[u8; 32],
    capture_root: &Path,
) -> Result<PathBuf, UploadStoreError> {
    let path = PathBuf::from(artifact_path);
    let valid = path.is_absolute()
        && path.parent() == Some(capture_root)
        && path.file_name().and_then(|name| name.to_str()) == Some(filename)
        && is_generated_frame_filename(filename)
        && stored_idempotency_key == idempotency_key(filename, sha256);
    if valid {
        Ok(path)
    } else {
        Err(UploadStoreError::CorruptArtifactIdentity(job_id))
    }
}

fn validate_preactivation_artifact_path(
    artifact_path: String,
    capture_root: &Path,
) -> Result<PathBuf, UploadStoreError> {
    let path = PathBuf::from(artifact_path);
    let valid = path.is_absolute()
        && path.parent() == Some(capture_root)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_generated_frame_filename);
    if valid {
        Ok(path)
    } else {
        Err(UploadStoreError::CorruptPreactivationArtifact)
    }
}

fn idempotency_key(filename: &str, sha256: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut value = String::with_capacity(IDEMPOTENCY_PREFIX.len() + 64 + 1 + filename.len());
    value.push_str(IDEMPOTENCY_PREFIX);
    for byte in sha256 {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value.push('-');
    value.push_str(filename);
    value
}

#[cfg(test)]
fn sha256_file(path: &Path) -> Result<[u8; 32], UploadStoreError> {
    let mut file = open_artifact_for_read(path).map_err(|source| UploadStoreError::Io {
        operation: "open artifact for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    sha256_open_file(&mut file, path)
}

fn sha256_open_file(file: &mut File, path: &Path) -> Result<[u8; 32], UploadStoreError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| UploadStoreError::Io {
                operation: "hash artifact",
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    file.rewind().map_err(|source| UploadStoreError::Io {
        operation: "rewind verified artifact",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(hasher.finalize().into())
}

fn open_artifact_for_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path)
}

fn sha256_from_database(job_id: UploadJobId, value: Vec<u8>) -> Result<[u8; 32], UploadStoreError> {
    value
        .try_into()
        .map_err(|_| UploadStoreError::CorruptArtifactIdentity(job_id))
}

pub(crate) fn parse_generated_frame_filename(filename: &str) -> Option<u64> {
    let stem = filename
        .strip_suffix(".jpg")
        .or_else(|| filename.strip_suffix(".jpeg"));
    let stem = stem?;
    let mut parts = stem.split('-');
    let (Some("frame"), Some(seconds), Some(millis), Some(fourth)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let fifth = parts.next();
    let (session_nonce, sequence) = match (fifth, parts.next()) {
        (None, None) => (None, fourth),
        (Some(sequence), None) => (Some(fourth), sequence),
        _ => return None,
    };
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || millis.len() != 3
        || !millis.bytes().all(|byte| byte.is_ascii_digit())
        || session_nonce.is_some_and(|nonce| {
            nonce.len() != CAPTURE_SESSION_NONCE_HEX_LENGTH
                || !nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || sequence.len() < 6
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let seconds_text = seconds;
    let sequence_text = sequence;
    let seconds = seconds_text.parse::<u64>().ok()?;
    let millis = millis.parse::<u64>().ok()?;
    let sequence = sequence_text.parse::<u64>().ok()?;
    if seconds_text != seconds.to_string() || sequence_text != format!("{sequence:06}") {
        return None;
    }
    seconds.checked_mul(1_000)?.checked_add(millis)
}

pub(crate) fn is_generated_frame_filename(filename: &str) -> bool {
    parse_generated_frame_filename(filename).is_some()
}

fn ensure_transition(changed: usize, job_id: UploadJobId) -> Result<(), UploadStoreError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(UploadStoreError::InvalidTransition(job_id))
    }
}

fn unix_ms_to_database(value: u64) -> Result<i64, UploadStoreError> {
    i64::try_from(value).map_err(|_| UploadStoreError::TimestampOutOfRange(value))
}

fn attempt_count_to_database(claim: &ClaimedUpload) -> Result<i64, UploadStoreError> {
    i64::try_from(claim.attempt_count)
        .map_err(|_| UploadStoreError::AttemptCountOverflow(claim.job_id))
}

fn job_id_from_database(value: i64) -> Result<UploadJobId, UploadStoreError> {
    if value > 0 {
        Ok(UploadJobId(value))
    } else {
        Err(UploadStoreError::CorruptInteger {
            column: "id",
            value,
        })
    }
}

fn database_u64(column: &'static str, value: i64) -> Result<u64, UploadStoreError> {
    u64::try_from(value).map_err(|_| UploadStoreError::CorruptInteger { column, value })
}

fn optional_database_u64(
    column: &'static str,
    value: Option<i64>,
) -> Result<Option<u64>, UploadStoreError> {
    value.map(|value| database_u64(column, value)).transpose()
}

fn optional_database_u16(
    column: &'static str,
    value: Option<i64>,
) -> Result<Option<u16>, UploadStoreError> {
    value
        .map(|value| {
            u16::try_from(value).map_err(|_| UploadStoreError::CorruptInteger { column, value })
        })
        .transpose()
}

fn checked_reconcile_increment(value: u64) -> Result<u64, UploadStoreError> {
    value
        .checked_add(1)
        .ok_or(UploadStoreError::ReconcileCountOverflow)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const TEST_DESTINATION: &str = "https://example.invalid/autopiercam/upload";

    struct TestEnvironment {
        root: PathBuf,
        capture: PathBuf,
        other_capture: PathBuf,
        database: PathBuf,
    }

    impl TestEnvironment {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "autopiercam-upload-ledger-test-{}-{sequence}",
                std::process::id()
            ));
            let capture = root.join("captures");
            let other_capture = root.join("other-captures");
            fs::create_dir_all(&capture).unwrap();
            fs::create_dir_all(&other_capture).unwrap();
            let database = root.join("uploads.sqlite3");
            Self {
                root,
                capture,
                other_capture,
                database,
            }
        }

        fn artifact(&self, sequence: u64, contents: &[u8]) -> PathBuf {
            let path = self
                .capture
                .join(format!("frame-1700000000-123-{sequence:06}.jpg"));
            fs::write(&path, contents).unwrap();
            path
        }

        fn artifact_at(&self, captured_at_ms: u64, sequence: u64, contents: &[u8]) -> PathBuf {
            let path = self.capture.join(format!(
                "frame-{}-{:03}-{sequence:06}.jpg",
                captured_at_ms / 1_000,
                captured_at_ms % 1_000
            ));
            fs::write(&path, contents).unwrap();
            path
        }

        fn open(&self) -> UploadStore {
            UploadStore::open(
                &self.database,
                &self.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .unwrap()
        }
    }

    impl Drop for TestEnvironment {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn ledger_revision(store: &UploadStore) -> u64 {
        let revision: i64 = store
            .connection
            .query_row(
                "SELECT ledger_revision FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        u64::try_from(revision).unwrap()
    }

    fn permanently_fail(store: &mut UploadStore, artifact: &Path, at: u64) -> UploadJobId {
        let job_id = store.record_artifact(artifact, at).unwrap().job_id;
        let claim = store.claim_due(at).unwrap().unwrap();
        assert_eq!(claim.job_id, job_id);
        store
            .mark_permanently_failed(&claim, at + 1, Some(422), Some("operator test failure"))
            .unwrap();
        job_id
    }

    #[test]
    fn operator_listing_is_newest_first_filtered_bounded_and_revision_stable() {
        let environment = TestEnvironment::new();
        let mut store = environment.open();
        let first = environment.artifact(101, b"first permanent");
        permanently_fail(&mut store, &first, 10);

        let second = environment.artifact(102, b"completed");
        store.record_artifact(&second, 20).unwrap();
        let claim = store.claim_due(20).unwrap().unwrap();
        store.mark_completed(&claim, 21, 204).unwrap();

        let third = environment.artifact(103, b"third permanent");
        permanently_fail(&mut store, &third, 30);
        store
            .record_artifact(&environment.artifact(104, b"pending"), 40)
            .unwrap();
        let fifth = environment.artifact(105, b"fifth permanent");
        // The older pending job is due first, so terminalize it temporarily
        // and then create the final permanent job in ID order.
        let pending_claim = store.claim_due(40).unwrap().unwrap();
        store
            .mark_permanently_failed(&pending_claim, 41, Some(400), Some("fourth permanent"))
            .unwrap();
        permanently_fail(&mut store, &fifth, 50);

        assert!(matches!(
            store.list_jobs(&[], 0, None),
            Err(UploadStoreError::InvalidListPageSize { .. })
        ));
        assert!(matches!(
            store.list_jobs(&[], MAX_UPLOAD_LIST_PAGE_SIZE + 1, None),
            Err(UploadStoreError::InvalidListPageSize { .. })
        ));

        let first_page = store.list_jobs(&[], 2, None).unwrap();
        assert_eq!(
            first_page
                .jobs
                .iter()
                .map(|job| job.job_id.get())
                .collect::<Vec<_>>(),
            vec![5, 4]
        );
        assert_eq!(first_page.ledger_id, store.ledger_id);
        assert_eq!(first_page.ledger_revision, ledger_revision(&store));
        assert_eq!(
            UploadLedgerId::parse_hex(&first_page.ledger_id.as_hex()).unwrap(),
            first_page.ledger_id
        );
        let cursor = first_page.next_cursor.unwrap();
        assert_eq!(cursor.as_str().len(), LIST_CURSOR_ENCODED_LENGTH);

        assert!(matches!(
            store.list_jobs(&[UploadState::PermanentlyFailed], 2, Some(cursor.as_str())),
            Err(UploadStoreError::ListCursorParametersMismatch)
        ));
        assert!(matches!(
            store.list_jobs(&[], 3, Some(cursor.as_str())),
            Err(UploadStoreError::ListCursorParametersMismatch)
        ));
        let mut tampered = cursor.as_str().to_owned();
        let replacement = if tampered.ends_with('0') { '1' } else { '0' };
        tampered.pop();
        tampered.push(replacement);
        assert!(matches!(
            store.list_jobs(&[], 2, Some(&tampered)),
            Err(UploadStoreError::InvalidListCursor)
        ));

        let second_page = store.list_jobs(&[], 2, Some(cursor.as_str())).unwrap();
        assert_eq!(
            second_page
                .jobs
                .iter()
                .map(|job| job.job_id.get())
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
        let third_page = store
            .list_jobs(&[], 2, Some(second_page.next_cursor.unwrap().as_str()))
            .unwrap();
        assert_eq!(third_page.jobs[0].job_id.get(), 1);
        assert!(third_page.next_cursor.is_none());

        let filtered = store
            .list_jobs(&[UploadState::PermanentlyFailed], 2, None)
            .unwrap();
        assert_eq!(
            filtered
                .jobs
                .iter()
                .map(|job| job.job_id.get())
                .collect::<Vec<_>>(),
            vec![5, 4]
        );
        let filtered_next = store
            .list_jobs(
                &[UploadState::PermanentlyFailed],
                2,
                Some(filtered.next_cursor.unwrap().as_str()),
            )
            .unwrap();
        assert_eq!(
            filtered_next
                .jobs
                .iter()
                .map(|job| job.job_id.get())
                .collect::<Vec<_>>(),
            vec![3, 1]
        );
        assert!(filtered_next.next_cursor.is_none());

        assert!(matches!(
            store.list_jobs(&[UploadState::Completed, UploadState::Completed], 2, None,),
            Err(UploadStoreError::DuplicateListStateFilter)
        ));
        let multi_state = store
            .list_jobs(
                &[UploadState::Completed, UploadState::PermanentlyFailed],
                2,
                None,
            )
            .unwrap();
        let multi_next = store
            .list_jobs(
                &[UploadState::PermanentlyFailed, UploadState::Completed],
                2,
                Some(multi_state.next_cursor.unwrap().as_str()),
            )
            .unwrap();
        assert_eq!(
            multi_next
                .jobs
                .iter()
                .map(|job| job.job_id.get())
                .collect::<Vec<_>>(),
            vec![3, 2]
        );

        let every_state = [
            UploadState::Pending,
            UploadState::InProgress,
            UploadState::Retrying,
            UploadState::Completed,
            UploadState::PermanentlyFailed,
        ];
        let explicit_all = store.list_jobs(&every_state, 2, None).unwrap();
        assert!(
            store
                .list_jobs(&[], 2, Some(explicit_all.next_cursor.unwrap().as_str()))
                .is_ok()
        );
    }

    #[test]
    fn every_mutation_and_other_ledger_make_list_cursors_stale() {
        let environment = TestEnvironment::new();
        let mut store = environment.open();
        for sequence in 110..=112 {
            store
                .record_artifact(&environment.artifact(sequence, &[sequence as u8]), sequence)
                .unwrap();
        }
        let page = store.list_jobs(&[], 1, None).unwrap();
        let cursor = page.next_cursor.unwrap();
        let revision_before = page.ledger_revision;
        let claim = store.claim_due(112).unwrap().unwrap();
        assert_eq!(ledger_revision(&store), revision_before + 1);
        assert!(matches!(
            store.list_jobs(&[], 1, Some(cursor.as_str())),
            Err(UploadStoreError::StaleListCursor)
        ));
        store.release_claim(&claim, 113).unwrap();

        let other_environment = TestEnvironment::new();
        let mut other = other_environment.open();
        for sequence in 120..=121 {
            other
                .record_artifact(
                    &other_environment.artifact(sequence, &[sequence as u8]),
                    sequence,
                )
                .unwrap();
        }
        let other_cursor = other.list_jobs(&[], 1, None).unwrap().next_cursor.unwrap();
        assert!(matches!(
            store.list_jobs(&[], 1, Some(other_cursor.as_str())),
            Err(UploadStoreError::StaleListCursor)
        ));
    }

    #[test]
    fn exact_requeue_preserves_failure_audit_and_persists_revisions_and_aggregates() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(130, b"verified requeue bytes");
        let mut store = environment.open();
        let job_id = permanently_fail(&mut store, &artifact, 100);
        let before = store.job(job_id).unwrap().unwrap();
        let page = store
            .list_jobs(&[UploadState::PermanentlyFailed], 10, None)
            .unwrap();
        let listed = &page.jobs[0];
        assert_eq!(listed.job_id, job_id);
        assert!(listed.delivery_binding_is_current);
        let result = store
            .requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: page.ledger_id,
                    job_id,
                    expected_job_revision: listed.job_revision,
                },
                200,
            )
            .unwrap();
        assert_eq!(result.ledger_revision, page.ledger_revision + 1);
        assert_eq!(result.job.job_revision, listed.job_revision + 1);
        assert_eq!(result.job.requeue_count, 1);

        let after = store.job(job_id).unwrap().unwrap();
        assert_eq!(after.state, UploadState::Pending);
        assert_eq!(after.attempt_count, before.attempt_count);
        assert_eq!(after.last_http_status, before.last_http_status);
        assert_eq!(after.last_error, before.last_error);
        assert_eq!(
            after.last_failure_at_unix_ms,
            before.last_failure_at_unix_ms
        );
        assert_eq!(after.completed_at_unix_ms, None);
        assert_eq!(after.updated_at_unix_ms, 200);
        assert_eq!(after.last_requeued_at_unix_ms, Some(200));
        assert_eq!(after.job_revision, result.job.job_revision);
        assert_eq!(after.requeue_count, 1);
        assert_eq!(
            store.snapshot().unwrap().counts,
            UploadStateCounts {
                pending: 1,
                ..UploadStateCounts::default()
            }
        );
        drop(store);

        let reopened = environment.open();
        let persisted = reopened.job(job_id).unwrap().unwrap();
        assert_eq!(persisted.state, UploadState::Pending);
        assert_eq!(persisted.job_revision, result.job.job_revision);
        assert_eq!(persisted.requeue_count, 1);
        assert_eq!(persisted.last_requeued_at_unix_ms, Some(200));
        assert_eq!(reopened.snapshot().unwrap().counts.pending, 1);
        assert_eq!(ledger_revision(&reopened), result.ledger_revision);
    }

    #[test]
    fn job_and_global_revisions_increment_exactly_once_per_durable_mutation() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(135, b"revision sequence");
        let mut store = environment.open();
        assert_eq!(ledger_revision(&store), 0);

        let job_id = store.record_artifact(&artifact, 1).unwrap().job_id;
        assert_eq!(ledger_revision(&store), 1);
        assert_eq!(store.job(job_id).unwrap().unwrap().job_revision, 1);

        let first = store.claim_due(1).unwrap().unwrap();
        assert_eq!(ledger_revision(&store), 2);
        assert_eq!(store.job(job_id).unwrap().unwrap().job_revision, 2);
        store.release_claim(&first, 2).unwrap();
        assert_eq!(ledger_revision(&store), 3);
        assert_eq!(store.job(job_id).unwrap().unwrap().job_revision, 3);

        let second = store.claim_due(2).unwrap().unwrap();
        assert_eq!(ledger_revision(&store), 4);
        store
            .mark_retrying(&second, 3, 4, Some(503), Some("retry"))
            .unwrap();
        assert_eq!(ledger_revision(&store), 5);
        let third = store.claim_due(4).unwrap().unwrap();
        assert_eq!(ledger_revision(&store), 6);
        store
            .mark_permanently_failed(&third, 5, Some(422), Some("terminal"))
            .unwrap();
        assert_eq!(ledger_revision(&store), 7);

        let listed = store.list_jobs(&[], 10, None).unwrap();
        assert_eq!(listed.jobs[0].job_revision, 7);
        let requeued = store
            .requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: listed.ledger_id,
                    job_id,
                    expected_job_revision: 7,
                },
                6,
            )
            .unwrap();
        assert_eq!(requeued.ledger_revision, 8);
        assert_eq!(requeued.job.job_revision, 8);
        let fourth = store.claim_due(6).unwrap().unwrap();
        assert_eq!(ledger_revision(&store), 9);
        store.mark_completed(&fourth, 7, 204).unwrap();
        assert_eq!(ledger_revision(&store), 10);
        assert_eq!(store.job(job_id).unwrap().unwrap().job_revision, 10);

        let duplicate = store.record_artifact(&artifact, 100).unwrap();
        assert_eq!(duplicate.disposition, RecordDisposition::AlreadyRecorded);
        assert_eq!(ledger_revision(&store), 10);
    }

    #[test]
    fn requeue_rejects_wrong_ledger_state_revision_and_delivery_binding_without_mutation() {
        let environment = TestEnvironment::new();
        let original = UploadAuthorizationFingerprint::for_bearer_token("original-requeue-token");
        let mut store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            original,
        )
        .unwrap();
        let permanent_artifact = environment.artifact(140, b"terminal binding");
        let job_id = permanently_fail(&mut store, &permanent_artifact, 100);
        let permanent = store.list_jobs(&[], 10, None).unwrap();
        let job_revision = permanent.jobs[0].job_revision;
        let baseline_revision = permanent.ledger_revision;

        let other_environment = TestEnvironment::new();
        let other_ledger = other_environment.open().ledger_id;
        assert!(matches!(
            store.requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: other_ledger,
                    job_id,
                    expected_job_revision: job_revision,
                },
                200,
            ),
            Err(UploadStoreError::RequeueLedgerMismatch)
        ));
        assert!(matches!(
            store.requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: store.ledger_id,
                    job_id,
                    expected_job_revision: job_revision - 1,
                },
                200,
            ),
            Err(UploadStoreError::RequeueStaleJobRevision)
        ));
        assert!(matches!(
            store.requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: store.ledger_id,
                    job_id: UploadJobId::from_u64(job_id.get() + 10_000).unwrap(),
                    expected_job_revision: job_revision,
                },
                200,
            ),
            Err(UploadStoreError::RequeueJobNotFound)
        ));

        let pending = store
            .record_artifact(&environment.artifact(141, b"still pending"), 110)
            .unwrap()
            .job_id;
        let pending_revision = store.job(pending).unwrap().unwrap().job_revision;
        assert!(matches!(
            store.requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: store.ledger_id,
                    job_id: pending,
                    expected_job_revision: pending_revision,
                },
                200,
            ),
            Err(UploadStoreError::RequeueWrongState)
        ));
        assert_eq!(ledger_revision(&store), baseline_revision + 1);

        // Drain the remaining pending job, then rotate authorization. Terminal
        // history remains visible but cannot cross the immutable delivery
        // binding under which it was created.
        let pending_claim = store.claim_due(110).unwrap().unwrap();
        store.mark_completed(&pending_claim, 111, 204).unwrap();
        let completed_revision = store.job(pending).unwrap().unwrap().job_revision;
        let before_completed_rejection = ledger_revision(&store);
        assert!(matches!(
            store.requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: store.ledger_id,
                    job_id: pending,
                    expected_job_revision: completed_revision,
                },
                200,
            ),
            Err(UploadStoreError::RequeueWrongState)
        ));
        assert_eq!(ledger_revision(&store), before_completed_rejection);
        drop(store);
        let rotated = UploadAuthorizationFingerprint::for_bearer_token("rotated-requeue-token");
        let mut rotated_store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            rotated,
        )
        .unwrap();
        let rotated_page = rotated_store
            .list_jobs(&[UploadState::PermanentlyFailed], 10, None)
            .unwrap();
        assert_eq!(rotated_page.ledger_id, permanent.ledger_id);
        assert!(!rotated_page.jobs[0].delivery_binding_is_current);
        let rotated_revision = rotated_page.ledger_revision;
        assert!(matches!(
            rotated_store.requeue_permanently_failed(
                RequeueUploadRequest {
                    ledger_id: rotated_page.ledger_id,
                    job_id,
                    expected_job_revision: rotated_page.jobs[0].job_revision,
                },
                300,
            ),
            Err(UploadStoreError::RequeueDeliveryBindingMismatch)
        ));
        assert_eq!(ledger_revision(&rotated_store), rotated_revision);
        assert_eq!(
            rotated_store.job(job_id).unwrap().unwrap().state,
            UploadState::PermanentlyFailed
        );
    }

    #[test]
    fn requeue_rejects_missing_and_changed_artifacts_without_database_mutation() {
        for (sequence, replacement, expected) in [
            (150, None, UploadStoreError::RequeueArtifactUnavailable),
            (
                151,
                Some(b"mutated!bytes!value!".as_slice()),
                UploadStoreError::RequeueArtifactChanged,
            ),
        ] {
            let environment = TestEnvironment::new();
            let original = b"original bytes value";
            assert_eq!(
                replacement.map_or(original.len(), <[u8]>::len),
                original.len()
            );
            let artifact = environment.artifact(sequence, original);
            let mut store = environment.open();
            let job_id = permanently_fail(&mut store, &artifact, 100);
            let listed = store.list_jobs(&[], 10, None).unwrap();
            let baseline_revision = listed.ledger_revision;
            let baseline_counts = store.snapshot().unwrap().counts;
            if let Some(replacement) = replacement {
                fs::write(&artifact, replacement).unwrap();
            } else {
                fs::remove_file(&artifact).unwrap();
            }
            let error = store
                .requeue_permanently_failed(
                    RequeueUploadRequest {
                        ledger_id: listed.ledger_id,
                        job_id,
                        expected_job_revision: listed.jobs[0].job_revision,
                    },
                    200,
                )
                .unwrap_err();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected)
            );
            assert_eq!(ledger_revision(&store), baseline_revision);
            assert_eq!(store.snapshot().unwrap().counts, baseline_counts);
            let unchanged = store.job(job_id).unwrap().unwrap();
            assert_eq!(unchanged.state, UploadState::PermanentlyFailed);
            assert_eq!(unchanged.requeue_count, 0);
            assert_eq!(unchanged.last_requeued_at_unix_ms, None);
        }
    }

    #[test]
    fn durable_lifecycle_persists_identity_retry_and_terminal_telemetry() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(1, b"durable jpeg bytes");
        let mut store = environment.open();
        let journal: String = store
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let synchronous: i64 = store
            .connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        let busy: i64 = store
            .connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        let locking: String = store
            .connection
            .query_row("PRAGMA locking_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(synchronous, 2);
        assert_eq!(busy, 5_000);
        assert_eq!(locking, "exclusive");

        let recorded = store.record_artifact(&artifact, 100).unwrap();
        let first = store.claim_due(100).unwrap().unwrap();
        assert_eq!(first.job_id, recorded.job_id);
        assert_eq!(first.file_size, 18);
        assert_eq!(first.sha256, sha256_file(&artifact).unwrap());
        assert_eq!(first.attempt_count, 1);
        assert_eq!(
            verify_claimed_artifact(&first).unwrap(),
            ArtifactVerification::Verified
        );
        store
            .mark_retrying(&first, 110, 500, Some(503), Some("temporary"))
            .unwrap();
        let persisted = store.job(recorded.job_id).unwrap().unwrap();
        assert_eq!(persisted.state, UploadState::Retrying);
        assert_eq!(persisted.next_attempt_at_unix_ms, Some(500));
        assert_eq!(persisted.last_http_status, Some(503));
        assert_eq!(persisted.last_error.as_deref(), Some("temporary"));
        assert!(store.claim_due(499).unwrap().is_none());

        let second = store.claim_due(500).unwrap().unwrap();
        assert_eq!(second.attempt_count, 2);
        assert!(matches!(
            store.mark_completed(&first, 510, 204),
            Err(UploadStoreError::InvalidTransition(id)) if id == recorded.job_id
        ));
        store.mark_completed(&second, 510, 204).unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.counts.completed, 1);
        assert_eq!(snapshot.counts.total(), 1);
        assert_eq!(snapshot.last_success_at_unix_ms, Some(510));
        assert_eq!(snapshot.last_failure_at_unix_ms, Some(110));
        assert_eq!(snapshot.last_error.as_deref(), Some("temporary"));
        assert_eq!(snapshot.next_due_at_unix_ms, None);

        let duplicate = store.record_artifact(&artifact, 999).unwrap();
        assert_eq!(duplicate.job_id, recorded.job_id);
        assert_eq!(duplicate.disposition, RecordDisposition::AlreadyRecorded);
        assert_eq!(store.snapshot().unwrap().counts.total(), 1);
    }

    #[test]
    fn destination_is_normalized_bound_and_never_exposed_by_mismatch() {
        let environment = TestEnvironment::new();
        let destination = "https://example.invalid/upload?site=pier";
        let store = UploadStore::open(
            &environment.database,
            &environment.capture,
            &format!("  {destination}  "),
            UploadAuthorizationFingerprint::anonymous(),
        )
        .unwrap();
        let stored: String = store
            .connection
            .query_row(
                "SELECT destination FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, destination);
        drop(store);

        assert!(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                destination,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .is_ok()
        );
        assert!(matches!(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                "https://different.invalid/upload",
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::DestinationMismatch)
        ));
        assert!(matches!(
            UploadStore::open(
                environment.root.join("blank.sqlite3"),
                &environment.capture,
                "  ",
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::InvalidDestination)
        ));
    }

    #[test]
    fn anonymous_and_bearer_authorization_identities_cannot_share_live_work() {
        let environment = TestEnvironment::new();
        let anonymous = UploadAuthorizationFingerprint::anonymous();
        let mut store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            anonymous,
        )
        .unwrap();
        let stored: Vec<u8> = store
            .connection
            .query_row(
                "SELECT authorization_sha256 FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.len(), 32);
        assert!(stored.as_slice() == anonymous.as_slice());
        store
            .record_artifact(&environment.artifact(80, b"pending anonymous work"), 80)
            .unwrap();
        drop(store);

        let bearer = UploadAuthorizationFingerprint::for_bearer_token("account-a-secret");
        assert!(matches!(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                bearer,
            ),
            Err(UploadStoreError::AuthorizationIdentityMismatch)
        ));
    }

    #[test]
    fn bearer_token_identity_allows_same_token_and_rejects_rotation_with_live_work() {
        let environment = TestEnvironment::new();
        let original_token = "same-actual-token-from-either-environment-variable";
        let original = UploadAuthorizationFingerprint::for_bearer_token(original_token);
        drop(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                original,
            )
            .unwrap(),
        );

        // The source environment-variable name is deliberately absent from
        // the identity API, so the same actual token remains equivalent.
        let mut store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            UploadAuthorizationFingerprint::for_bearer_token(original_token),
        )
        .unwrap();
        store
            .record_artifact(&environment.artifact(81, b"pending bearer work"), 81)
            .unwrap();
        drop(store);

        let error = match UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            UploadAuthorizationFingerprint::for_bearer_token("rotated-account-token"),
        ) {
            Ok(_) => panic!("a changed bearer token reopened the existing upload ledger"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            UploadStoreError::AuthorizationIdentityMismatch
        ));
        let error_text = error.to_string();
        assert!(!error_text.contains(original_token));
        assert!(!error_text.contains("rotated-account-token"));

        for database_file in [
            environment.database.clone(),
            environment.database.with_extension("sqlite3-wal"),
        ] {
            if let Ok(bytes) = fs::read(database_file) {
                assert!(
                    !bytes
                        .windows(original_token.len())
                        .any(|window| window == original_token.as_bytes())
                );
            }
        }
    }

    #[test]
    fn authorization_rotation_rebinds_a_drained_ledger_with_terminal_history() {
        let environment = TestEnvironment::new();
        let original = UploadAuthorizationFingerprint::for_bearer_token("original-token");
        let rotated = UploadAuthorizationFingerprint::for_bearer_token("rotated-token");
        let mut store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            original,
        )
        .unwrap();
        store
            .record_artifact(&environment.artifact(82, b"terminal history"), 82)
            .unwrap();
        let claim = store.claim_due(82).unwrap().unwrap();
        store.mark_completed(&claim, 83, 204).unwrap();
        store
            .record_artifact(&environment.artifact(84, b"permanent terminal history"), 84)
            .unwrap();
        let claim = store.claim_due(84).unwrap().unwrap();
        store
            .mark_permanently_failed(&claim, 85, Some(400), Some("rejected"))
            .unwrap();
        drop(store);

        let reopened = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            rotated,
        )
        .unwrap();
        assert_eq!(reopened.snapshot().unwrap().counts.completed, 1);
        assert_eq!(reopened.snapshot().unwrap().counts.permanently_failed, 1);
        let stored: Vec<u8> = reopened
            .connection
            .query_row(
                "SELECT authorization_sha256 FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored.as_slice() == rotated.as_slice());
    }

    #[test]
    fn authorization_rotation_rejects_an_unledgered_crash_gap_until_it_is_drained() {
        let environment = TestEnvironment::new();
        let original = UploadAuthorizationFingerprint::for_bearer_token("original-token");
        let rotated = UploadAuthorizationFingerprint::for_bearer_token("rotated-token");
        drop(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                original,
            )
            .unwrap(),
        );

        // Model a crash after atomic publication but before the writer could
        // insert the durable row. SQLite looks drained, but these bytes still
        // belong to the authorization identity active at publication time.
        let crash_gap = environment.artifact(86, b"published before crash");
        assert!(matches!(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                rotated,
            ),
            Err(UploadStoreError::AuthorizationIdentityMismatch)
        ));

        let mut original_store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            original,
        )
        .unwrap();
        let stored: Vec<u8> = original_store
            .connection
            .query_row(
                "SELECT authorization_sha256 FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_slice(), original.as_slice());
        let reconciled = original_store.reconcile_capture_directory(86).unwrap();
        assert_eq!(reconciled.inserted, 1);
        let claim = original_store.claim_due(86).unwrap().unwrap();
        assert_eq!(claim.artifact_path, fs::canonicalize(crash_gap).unwrap());
        original_store.mark_completed(&claim, 87, 204).unwrap();
        drop(original_store);

        let rotated_store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            rotated,
        )
        .unwrap();
        let stored: Vec<u8> = rotated_store
            .connection
            .query_row(
                "SELECT authorization_sha256 FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_slice(), rotated.as_slice());
    }

    #[test]
    fn exclusive_owner_fences_a_stale_concurrent_authorization_rebind() {
        let environment = TestEnvironment::new();
        let original = UploadAuthorizationFingerprint::for_bearer_token("original-token");
        drop(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                original,
            )
            .unwrap(),
        );

        let capture_root = canonical_capture_root(&environment.capture).unwrap();
        let capture_root_text = capture_root.to_str().unwrap();
        let winner = UploadAuthorizationFingerprint::for_bearer_token("winner-token");
        let loser = UploadAuthorizationFingerprint::for_bearer_token("loser-token");
        let mut winner_connection = Connection::open(&environment.database).unwrap();
        winner_connection
            .busy_timeout(Duration::from_millis(20))
            .unwrap();
        verify_schema(&winner_connection, capture_root_text, TEST_DESTINATION).unwrap();

        acquire_exclusive_ownership(&winner_connection).unwrap();
        rotate_authorization_if_drained(&mut winner_connection, &capture_root, winner).unwrap();
        let mut loser_connection = Connection::open(&environment.database).unwrap();
        loser_connection
            .busy_timeout(Duration::from_millis(20))
            .unwrap();
        let error = rotate_authorization_if_drained(&mut loser_connection, &capture_root, loser)
            .unwrap_err();
        match error {
            UploadStoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) => assert!(
                matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ),
                "unexpected SQLite contention code: {:?}",
                code.code
            ),
            other => panic!("unexpected stale-owner error: {other:?}"),
        }

        let stored: Vec<u8> = winner_connection
            .query_row(
                "SELECT authorization_sha256 FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_slice(), winner.as_slice());
    }

    #[test]
    fn authorization_rotation_rejects_every_nonterminal_state_before_recovery() {
        for state in ["pending", "in_progress", "retrying"] {
            let environment = TestEnvironment::new();
            let original = UploadAuthorizationFingerprint::for_bearer_token("original-token");
            let mut store = UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                original,
            )
            .unwrap();
            store
                .record_artifact(&environment.artifact(83, state.as_bytes()), 83)
                .unwrap();
            if state != "pending" {
                let claim = store.claim_due(83).unwrap().unwrap();
                if state == "retrying" {
                    store
                        .mark_retrying(&claim, 84, 1_000, Some(503), Some("outage"))
                        .unwrap();
                }
            }
            drop(store);

            assert!(matches!(
                UploadStore::open(
                    &environment.database,
                    &environment.capture,
                    TEST_DESTINATION,
                    UploadAuthorizationFingerprint::for_bearer_token("rotated-token"),
                ),
                Err(UploadStoreError::AuthorizationIdentityMismatch)
            ));
        }
    }

    #[test]
    fn reopen_recovers_in_progress_and_preserves_attempt_and_key() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(2, b"recover");
        let stale = {
            let mut store = environment.open();
            store.record_artifact(&artifact, 200).unwrap();
            store.claim_due(200).unwrap().unwrap()
        };
        let mut reopened = environment.open();
        let record = reopened.job(stale.job_id).unwrap().unwrap();
        assert_eq!(record.state, UploadState::Pending);
        assert_eq!(record.attempt_count, 1);
        let recovered = reopened.claim_due(200).unwrap().unwrap();
        assert_eq!(recovered.attempt_count, 2);
        assert_eq!(recovered.idempotency_key, stale.idempotency_key);
    }

    #[test]
    fn generated_filename_parser_accepts_nonce_and_legacy_grammars_exactly() {
        assert_eq!(
            parse_generated_frame_filename(
                "frame-1700000000-123-00112233445566778899aabbccddeeff-000042.jpg"
            ),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            parse_generated_frame_filename("frame-1700000000-123-000042.jpeg"),
            Some(1_700_000_000_123)
        );
        for invalid in [
            "frame-1700000000-123-00112233445566778899AABBCCDDEEFF-000042.jpg",
            "frame-1700000000-123-00112233445566778899aabbccddee-000042.jpg",
            "frame-1700000000-123-00112233445566778899aabbccddeeff-00042.jpg",
            "frame-01700000000-123-00112233445566778899aabbccddeeff-000042.jpg",
            "frame-1700000000-123-00112233445566778899aabbccddeeff-000042-extra.jpg",
        ] {
            assert_eq!(parse_generated_frame_filename(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn reconciliation_uses_exact_generated_grammar_and_is_idempotent() {
        let environment = TestEnvironment::new();
        let mut store = environment.open();
        let future_seconds = 9_999_999_999_u64;
        fs::write(
            environment
                .capture
                .join(format!("frame-{future_seconds}-001-000001.jpg")),
            b"one",
        )
        .unwrap();
        fs::write(
            environment.capture.join(format!(
                "frame-{future_seconds}-999-00112233445566778899aabbccddeeff-1000000.jpeg"
            )),
            b"two",
        )
        .unwrap();
        for invalid in [
            "frame-loose.jpg",
            "frame-9999999999-01-000002.jpg",
            "frame-9999999999-001-00002.jpg",
            "frame-9999999999-001-000003.JPG",
            "other-9999999999-001-000004.jpg",
            "frame-9999999999-001-000005.png",
            "frame-9999999999-001-00112233445566778899AABBCCDDEEFF-000005.jpg",
            "frame-9999999999-001-00112233445566778899aabbccddee-000005.jpg",
        ] {
            fs::write(environment.capture.join(invalid), b"ignored").unwrap();
        }
        let nested = environment.capture.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(
            nested.join(format!("frame-{future_seconds}-001-000006.jpg")),
            b"nested",
        )
        .unwrap();

        let first = store.reconcile_capture_directory(300).unwrap();
        assert_eq!(first.eligible, 2);
        assert_eq!(first.inserted, 2);
        assert_eq!(first.already_recorded, 0);
        let second = store.reconcile_capture_directory(400).unwrap();
        assert_eq!(second.eligible, 2);
        assert_eq!(second.inserted, 0);
        assert_eq!(second.already_recorded, 2);
        assert_eq!(store.snapshot().unwrap().counts.pending, 2);
    }

    #[test]
    fn preactivation_inventory_skips_future_history_and_recovers_backdated_crash_gap() {
        let environment = TestEnvironment::new();
        let historical = environment.artifact_at(9_999_999_999_999, 20, b"future history");
        let historical_canonical = fs::canonicalize(&historical).unwrap();
        // Model a crash after SQLite created the file but before schema
        // initialization. Reopen must create schema and inventory together.
        drop(Connection::open(&environment.database).unwrap());

        let mut store = environment.open();
        let inventoried: Vec<String> = store
            .connection
            .prepare(
                "SELECT artifact_path FROM upload_preactivation_artifacts ORDER BY artifact_path",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            inventoried,
            vec![historical_canonical.to_str().unwrap().to_owned()]
        );
        let first = store.reconcile_capture_directory(10).unwrap();
        assert_eq!(first.eligible, 0);
        assert_eq!(store.snapshot().unwrap().counts.total(), 0);

        // This is created after activation with a deliberately backdated
        // clock. Dropping the store models both a crash gap and captures made
        // while upload is disabled before the next enabled restart.
        let crash_gap = environment
            .capture
            .join("frame-1-000-00112233445566778899aabbccddeeff-000021.jpg");
        fs::write(&crash_gap, b"post activation, backdated").unwrap();
        drop(store);
        let mut reopened = environment.open();
        let reconciled = reopened.reconcile_capture_directory(20).unwrap();
        assert_eq!(reconciled.eligible, 1);
        assert_eq!(reconciled.inserted, 1);
        let first_claim = reopened.claim_due(20).unwrap().unwrap();
        assert_eq!(
            reopened
                .job(first_claim.job_id)
                .unwrap()
                .unwrap()
                .artifact_path,
            fs::canonicalize(&crash_gap).unwrap()
        );
        assert!(
            reopened
                .recorded_job_id(historical_canonical.to_str().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconciliation_skips_a_known_replaced_path_before_file_inspection() {
        let environment = TestEnvironment::new();
        let mut store = environment.open();
        let artifact = environment.artifact_at(1, 23, b"recorded bytes");
        store.record_artifact(&artifact, 10).unwrap();

        fs::remove_file(&artifact).unwrap();
        fs::create_dir(&artifact).unwrap();
        let result = store.reconcile_capture_directory(20).unwrap();
        assert_eq!(result.eligible, 1);
        assert_eq!(result.inserted, 0);
        assert_eq!(result.already_recorded, 1);
    }

    #[test]
    fn artifacts_are_root_bound_and_changed_bytes_fail_verification() {
        let environment = TestEnvironment::new();
        let outside = environment.root.join("frame-1700000000-001-000010.jpg");
        fs::write(&outside, b"outside").unwrap();
        let artifact = environment.artifact(11, b"same-size-a");
        let mut store = environment.open();
        assert!(matches!(
            store.record_artifact(&outside, 1),
            Err(UploadStoreError::ArtifactOutsideCaptureRoot(_))
        ));
        store.record_artifact(&artifact, 1).unwrap();
        let claim = store.claim_due(1).unwrap().unwrap();
        fs::write(&artifact, b"same-size-b").unwrap();
        assert!(matches!(
            verify_claimed_artifact(&claim).unwrap(),
            ArtifactVerification::Sha256Mismatch { .. }
        ));
        fs::write(&artifact, b"different-size").unwrap();
        assert!(matches!(
            verify_claimed_artifact(&claim).unwrap(),
            ArtifactVerification::SizeMismatch { .. }
        ));
    }

    #[test]
    fn verified_snapshot_keeps_recorded_bytes_after_source_mutation() {
        let environment = TestEnvironment::new();
        let recorded_bytes = b"original-jpeg-bytes";
        let mutated_bytes = b"mutated--jpeg-bytes";
        assert_eq!(recorded_bytes.len(), mutated_bytes.len());
        let artifact = environment.artifact(50, recorded_bytes);
        let mut store = environment.open();
        store.record_artifact(&artifact, 1).unwrap();
        let claim = store.claim_due(1).unwrap().unwrap();

        let mut snapshot = match snapshot_claimed_artifact(&claim).unwrap() {
            SnapshottedClaimedArtifact::Verified(snapshot) => snapshot,
            SnapshottedClaimedArtifact::Rejected(reason) => {
                panic!("recorded artifact was unexpectedly rejected: {reason:?}")
            }
        };
        fs::write(&artifact, mutated_bytes).unwrap();

        let mut uploaded_bytes = Vec::new();
        snapshot.read_to_end(&mut uploaded_bytes).unwrap();
        assert_eq!(uploaded_bytes, recorded_bytes);
        assert_eq!(fs::read(&artifact).unwrap(), mutated_bytes);
    }

    #[test]
    fn verified_snapshot_keeps_recorded_bytes_after_source_replacement() {
        let environment = TestEnvironment::new();
        let recorded_bytes = b"recorded-before-replacement";
        let replacement_bytes = b"replacement-on-disk";
        let artifact = environment.artifact(51, recorded_bytes);
        let mut store = environment.open();
        store.record_artifact(&artifact, 1).unwrap();
        let claim = store.claim_due(1).unwrap().unwrap();

        let mut snapshot = match snapshot_claimed_artifact(&claim).unwrap() {
            SnapshottedClaimedArtifact::Verified(snapshot) => snapshot,
            SnapshottedClaimedArtifact::Rejected(reason) => {
                panic!("recorded artifact was unexpectedly rejected: {reason:?}")
            }
        };
        fs::remove_file(&artifact).unwrap();
        fs::write(&artifact, replacement_bytes).unwrap();

        let mut uploaded_bytes = Vec::new();
        snapshot.read_to_end(&mut uploaded_bytes).unwrap();
        assert_eq!(uploaded_bytes, recorded_bytes);
        assert_eq!(fs::read(&artifact).unwrap(), replacement_bytes);
    }

    #[test]
    fn same_path_with_different_bytes_is_an_identity_conflict() {
        let environment = TestEnvironment::new();
        let mut store = environment.open();
        let artifact = environment.artifact(12, b"same-size-a");
        store.record_artifact(&artifact, 1).unwrap();

        fs::write(&artifact, b"same-size-b").unwrap();
        assert!(matches!(
            store.record_artifact(&artifact, 2),
            Err(UploadStoreError::ArtifactIdentityConflict)
        ));
        assert_eq!(store.snapshot().unwrap().counts.total(), 1);
    }

    #[test]
    fn aggregate_snapshot_tracks_every_transition_and_release_fencing() {
        let environment = TestEnvironment::new();
        let completed_artifact = environment.artifact(30, b"completed");
        let permanent_artifact = environment.artifact(31, b"permanent");
        let retrying_artifact = environment.artifact(32, b"retrying");
        let active_artifact = environment.artifact(33, b"active");
        let pending_artifact = environment.artifact(34, b"pending");
        let mut store = environment.open();

        store.record_artifact(&completed_artifact, 10).unwrap();
        let completed = store.claim_due(10).unwrap().unwrap();
        store.mark_completed(&completed, 11, 204).unwrap();

        store.record_artifact(&permanent_artifact, 20).unwrap();
        let permanent = store.claim_due(20).unwrap().unwrap();
        store
            .mark_permanently_failed(&permanent, 21, Some(400), Some("permanent"))
            .unwrap();

        store.record_artifact(&retrying_artifact, 30).unwrap();
        let retrying = store.claim_due(30).unwrap().unwrap();
        store
            .mark_retrying(&retrying, 31, 300, Some(503), Some("retry"))
            .unwrap();

        store.record_artifact(&active_artifact, 40).unwrap();
        let active = store.claim_due(40).unwrap().unwrap();
        store.record_artifact(&pending_artifact, 50).unwrap();

        let snapshot = store.snapshot().unwrap();
        assert_eq!(
            snapshot.counts,
            UploadStateCounts {
                pending: 1,
                in_progress: 1,
                retrying: 1,
                completed: 1,
                permanently_failed: 1,
            }
        );
        assert_eq!(snapshot.last_success_at_unix_ms, Some(11));
        assert_eq!(snapshot.last_failure_at_unix_ms, Some(31));
        assert_eq!(snapshot.last_error.as_deref(), Some("retry"));
        assert_eq!(snapshot.next_due_at_unix_ms, Some(50));

        store.release_claim(&active, 60).unwrap();
        let released = store.snapshot().unwrap();
        assert_eq!(released.counts.pending, 2);
        assert_eq!(released.counts.in_progress, 0);
        assert!(matches!(
            store.release_claim(&active, 61),
            Err(UploadStoreError::InvalidTransition(id)) if id == active.job_id
        ));
        let reclaimed = store.claim_due(60).unwrap().unwrap();
        assert_eq!(reclaimed.job_id, active.job_id);
        assert_eq!(reclaimed.attempt_count, active.attempt_count + 1);
        assert!(matches!(
            store.mark_completed(&active, 62, 204),
            Err(UploadStoreError::InvalidTransition(id)) if id == active.job_id
        ));
    }

    #[test]
    fn idempotency_identity_uses_digest_and_filename() {
        let first_environment = TestEnvironment::new();
        let first_artifact = first_environment.artifact(40, b"first bytes");
        let same_bytes_new_name = first_environment.artifact(41, b"first bytes");
        let mut first_store = first_environment.open();
        let first_id = first_store
            .record_artifact(&first_artifact, 1)
            .unwrap()
            .job_id;
        let second_name_id = first_store
            .record_artifact(&same_bytes_new_name, 1)
            .unwrap()
            .job_id;
        let first = first_store.job(first_id).unwrap().unwrap();
        let second_name = first_store.job(second_name_id).unwrap().unwrap();
        assert_ne!(first.idempotency_key, second_name.idempotency_key);
        assert_eq!(
            first.idempotency_key,
            idempotency_key(&first.filename, &first.sha256)
        );

        let second_environment = TestEnvironment::new();
        let same_name_new_bytes = second_environment.artifact(40, b"different bytes");
        let mut second_store = second_environment.open();
        let second_id = second_store
            .record_artifact(&same_name_new_bytes, 1)
            .unwrap()
            .job_id;
        let second = second_store.job(second_id).unwrap().unwrap();
        assert_eq!(first.filename, second.filename);
        assert_ne!(first.sha256, second.sha256);
        assert_ne!(first.idempotency_key, second.idempotency_key);
        assert!(first.idempotency_key.starts_with(IDEMPOTENCY_PREFIX));
        assert!(first.idempotency_key.ends_with(&first.filename));
    }

    #[test]
    fn empty_unsupported_partial_tampered_and_wrong_root_schemas_are_handled() {
        let empty_environment = TestEnvironment::new();
        drop(Connection::open(&empty_environment.database).unwrap());
        assert!(
            UploadStore::open(
                &empty_environment.database,
                &empty_environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .is_ok()
        );

        let partial_environment = TestEnvironment::new();
        let partial = Connection::open(&partial_environment.database).unwrap();
        partial
            .execute("CREATE TABLE unrelated (value TEXT)", [])
            .unwrap();
        drop(partial);
        assert!(matches!(
            UploadStore::open(
                &partial_environment.database,
                &partial_environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::MissingSchema)
        ));

        let future_environment = TestEnvironment::new();
        let future = Connection::open(&future_environment.database).unwrap();
        future.pragma_update(None, "user_version", 99).unwrap();
        drop(future);
        assert!(matches!(
            UploadStore::open(
                &future_environment.database,
                &future_environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::UnsupportedSchema { found: 99, .. })
        ));

        for old_version in [1, 2, 3] {
            let old_environment = TestEnvironment::new();
            let old = Connection::open(&old_environment.database).unwrap();
            old.pragma_update(None, "user_version", old_version)
                .unwrap();
            drop(old);
            assert!(matches!(
                UploadStore::open(
                    &old_environment.database,
                    &old_environment.capture,
                    TEST_DESTINATION,
                    UploadAuthorizationFingerprint::anonymous(),
                ),
                Err(UploadStoreError::UnsupportedSchema {
                    found,
                    supported: SCHEMA_VERSION,
                }) if found == old_version
            ));
        }

        let tampered_environment = TestEnvironment::new();
        drop(
            UploadStore::open(
                &tampered_environment.database,
                &tampered_environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .unwrap(),
        );
        let tampered = Connection::open(&tampered_environment.database).unwrap();
        tampered
            .execute("DROP TRIGGER upload_jobs_no_delete", [])
            .unwrap();
        drop(tampered);
        assert!(matches!(
            UploadStore::open(
                &tampered_environment.database,
                &tampered_environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::InvalidSchema)
        ));

        let root_environment = TestEnvironment::new();
        drop(root_environment.open());
        assert!(matches!(
            UploadStore::open(
                &root_environment.database,
                &root_environment.other_capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::CaptureRootMismatch)
        ));
    }

    #[test]
    fn a_live_store_excludes_a_second_owner() {
        let environment = TestEnvironment::new();
        let first = environment.open();
        assert!(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .is_err()
        );
        drop(first);
        assert!(
            UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .is_ok()
        );
    }

    #[test]
    fn retention_snapshot_protects_unacknowledged_jobs_and_fences_completed_deletes() {
        let environment = TestEnvironment::new();
        let historical = environment.artifact_at(1_000, 1, b"historical local frame");
        let historical = fs::canonicalize(historical).unwrap();
        let mut store = environment.open();

        let completed_path = environment.artifact_at(2_000, 2, b"acknowledged frame");
        store.record_artifact(&completed_path, 2_000).unwrap();
        let completed_path = fs::canonicalize(completed_path).unwrap();
        let completed_claim = store.claim_due(2_000).unwrap().unwrap();
        store.mark_completed(&completed_claim, 2_100, 204).unwrap();
        let pending_path = environment.artifact_at(3_000, 3, b"pending frame");
        store.record_artifact(&pending_path, 3_000).unwrap();
        let pending_path = fs::canonicalize(pending_path).unwrap();

        let snapshot = store.retention_snapshot().unwrap();
        let historical_entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.artifact_path == historical)
            .unwrap();
        assert_eq!(
            historical_entry.binding,
            UploadRetentionBinding::Preactivation
        );
        assert!(store.retention_still_authorized(historical_entry).unwrap());

        let completed_entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.artifact_path == completed_path)
            .unwrap();
        assert!(matches!(
            completed_entry.binding,
            UploadRetentionBinding::Completed { .. }
        ));
        assert!(store.retention_still_authorized(completed_entry).unwrap());
        let mut stale_completed = completed_entry.clone();
        if let UploadRetentionBinding::Completed { job_revision, .. } = &mut stale_completed.binding
        {
            *job_revision += 1;
        }
        assert!(!store.retention_still_authorized(&stale_completed).unwrap());

        let pending_entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.artifact_path == pending_path)
            .unwrap();
        assert_eq!(pending_entry.binding, UploadRetentionBinding::Protected);
        assert!(!store.retention_still_authorized(pending_entry).unwrap());
    }

    #[test]
    fn overflow_and_invalid_names_are_rejected() {
        let environment = TestEnvironment::new();
        let invalid = environment.capture.join("frame-not-generated.jpg");
        fs::write(&invalid, b"bad name").unwrap();
        let valid = environment.artifact(12, b"valid");
        let mut store = environment.open();
        assert!(matches!(
            store.record_artifact(&invalid, 1),
            Err(UploadStoreError::InvalidArtifactFilename(_))
        ));
        assert!(matches!(
            store.record_artifact(&valid, (i64::MAX as u64) + 1),
            Err(UploadStoreError::TimestampOutOfRange(_))
        ));
    }
}
