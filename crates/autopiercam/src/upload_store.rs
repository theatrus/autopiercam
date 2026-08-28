use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 2;
const APPLICATION_ID: i64 = 0x4150_4355;
const SCHEMA_SIGNATURE: &str =
    "autopiercam-upload-ledger-v2-destination-authorization-activation-aggregates-sha256-20260828";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const IDEMPOTENCY_PREFIX: &str = "autopiercam-sha256-";
const AUTHORIZATION_FINGERPRINT_DOMAIN: &[u8] = b"autopiercam-upload-authorization-identity-v1\0";

const CREATE_METADATA: &str = r#"
CREATE TABLE upload_metadata (
    singleton                INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_signature         TEXT NOT NULL,
    capture_root             TEXT NOT NULL,
    destination              TEXT NOT NULL CHECK (
                                 length(destination) > 0 AND
                                 trim(destination) = destination
                             ),
    authorization_sha256     BLOB NOT NULL CHECK (
                                 length(authorization_sha256) = 32
                             ),
    activation_at_ms         INTEGER NOT NULL CHECK (
                                 typeof(activation_at_ms) = 'integer' AND
                                 activation_at_ms >= 0
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

const CREATE_UPLOAD_JOBS: &str = r#"
CREATE TABLE upload_jobs (
    id                     INTEGER PRIMARY KEY,
    artifact_path          TEXT NOT NULL UNIQUE,
    filename               TEXT NOT NULL,
    idempotency_key        TEXT NOT NULL UNIQUE,
    file_size              INTEGER NOT NULL CHECK (file_size >= 0),
    sha256                 BLOB NOT NULL CHECK (length(sha256) = 32),
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
    CHECK (
        (state = 'retrying' AND next_attempt_at_ms IS NOT NULL) OR
        (state <> 'retrying' AND next_attempt_at_ms IS NULL)
    ),
    CHECK (
        (state = 'completed' AND completed_at_ms IS NOT NULL) OR
        (state <> 'completed' AND completed_at_ms IS NULL)
    )
) STRICT
"#;

const CREATE_DUE_INDEX: &str = r#"
CREATE INDEX upload_jobs_due
    ON upload_jobs (COALESCE(next_attempt_at_ms, created_at_ms), id)
    WHERE state IN ('pending', 'retrying')
"#;

const CREATE_INSERT_AGGREGATE_TRIGGER: &str = r#"
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

const CREATE_UPDATE_AGGREGATE_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_after_status_update
AFTER UPDATE OF state, completed_at_ms, last_failure_at_ms, last_error ON upload_jobs
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

const CREATE_IMMUTABLE_IDENTITY_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_identity_immutable
BEFORE UPDATE OF artifact_path, filename, idempotency_key, file_size, sha256, created_at_ms
ON upload_jobs
BEGIN
    SELECT RAISE(ABORT, 'upload artifact identity is immutable');
END
"#;

const CREATE_NO_DELETE_TRIGGER: &str = r#"
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
    state,
    attempt_count,
    next_attempt_at_ms,
    last_http_status,
    last_error,
    created_at_ms,
    updated_at_ms,
    completed_at_ms,
    last_failure_at_ms
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

    #[cfg(test)]
    #[error("upload ledger contains an unknown upload state: {0:?}")]
    CorruptState(String),

    #[error("upload job {0} has inconsistent identity metadata")]
    CorruptArtifactIdentity(UploadJobId),

    #[error("upload job {0} is not in progress or its attempt is stale")]
    InvalidTransition(UploadJobId),

    #[error("upload job {0} has exhausted SQLite's attempt counter")]
    AttemptCountOverflow(UploadJobId),

    #[error("upload reconciliation counter overflowed")]
    ReconcileCountOverflow,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct UploadJobId(i64);

impl std::fmt::Display for UploadJobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
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

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UploadState {
    Pending,
    InProgress,
    Retrying,
    Completed,
    PermanentlyFailed,
}

#[cfg(test)]
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

pub(crate) enum OpenedClaimedArtifact {
    Verified(File),
    Rejected(ArtifactVerification),
}

pub(crate) struct UploadStore {
    connection: Connection,
    capture_root: PathBuf,
}

impl UploadStore {
    pub(crate) fn open(
        database_path: impl AsRef<Path>,
        capture_directory: impl AsRef<Path>,
        destination: &str,
        authorization_fingerprint: UploadAuthorizationFingerprint,
    ) -> Result<Self, UploadStoreError> {
        let database_path = database_path.as_ref();
        let capture_root = canonical_capture_root(capture_directory.as_ref())?;
        let capture_root_text = capture_root
            .to_str()
            .ok_or_else(|| UploadStoreError::CaptureDirectoryNotUtf8(capture_root.clone()))?;
        let destination = normalize_destination(destination)?;
        let mut connection = Connection::open(database_path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;

        let version = schema_version(&connection)?;
        match version {
            0 if database_is_completely_empty(&connection)? => {
                configure_durability(&connection)?;
                create_schema(
                    &mut connection,
                    capture_root_text,
                    &destination,
                    authorization_fingerprint,
                    current_unix_time_ms()?,
                )?;
            }
            0 => return Err(UploadStoreError::MissingSchema),
            SCHEMA_VERSION => {
                verify_schema(
                    &mut connection,
                    capture_root_text,
                    &destination,
                    authorization_fingerprint,
                )?;
                configure_durability(&connection)?;
            }
            found => {
                return Err(UploadStoreError::UnsupportedSchema {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
        }
        acquire_exclusive_ownership(&connection)?;
        recover_interrupted_jobs(&mut connection)?;
        Ok(Self {
            connection,
            capture_root,
        })
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
                state, attempt_count, next_attempt_at_ms, last_http_status,
                last_error, created_at_ms, updated_at_ms, completed_at_ms,
                last_failure_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, NULL, NULL, NULL,
                      ?6, ?6, NULL, NULL)
            ON CONFLICT(artifact_path) DO NOTHING
            "#,
            params![
                values.artifact_path,
                values.filename,
                values.idempotency_key,
                file_size,
                values.sha256.as_slice(),
                recorded_at
            ],
        )?;
        let raw_id: i64 = self.connection.query_row(
            "SELECT id FROM upload_jobs WHERE artifact_path = ?1",
            [values.artifact_path],
            |row| row.get(0),
        )?;
        Ok(RecordArtifactResult {
            job_id: job_id_from_database(raw_id)?,
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
                       file_size, sha256, attempt_count
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
                        attempt_count: row.get(6)?,
                    })
                },
            )
            .optional()?;
        let Some(candidate) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        let job_id = job_id_from_database(candidate.id)?;
        let claim = candidate.into_claim(job_id, &self.capture_root)?;
        let next_attempt_count = i64::try_from(claim.attempt_count)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(UploadStoreError::AttemptCountOverflow(job_id))?;
        let changed = transaction.execute(
            r#"
            UPDATE upload_jobs
            SET state = 'in_progress', attempt_count = ?2,
                next_attempt_at_ms = NULL, updated_at_ms = ?1
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
                updated_at_ms = ?1
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
                updated_at_ms = ?4, last_failure_at_ms = ?4
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
                completed_at_ms = ?2
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
                updated_at_ms = ?3, last_failure_at_ms = ?3
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
                       file_size, sha256, state, attempt_count,
                       next_attempt_at_ms, last_http_status, last_error,
                       created_at_ms, updated_at_ms, completed_at_ms,
                       last_failure_at_ms
                FROM upload_jobs WHERE id = ?1
                "#,
                [job_id.0],
                RawUploadJob::from_row,
            )
            .optional()?;
        raw.map(|raw| raw.into_record(&self.capture_root))
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

    pub(crate) fn reconcile_capture_directory(
        &mut self,
        recorded_at_unix_ms: u64,
    ) -> Result<ReconcileResult, UploadStoreError> {
        unix_ms_to_database(recorded_at_unix_ms)?;
        let activation_at: i64 = self.connection.query_row(
            "SELECT activation_at_ms FROM upload_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let activation_at = database_u64("activation_at_ms", activation_at)?;
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
            let Some(captured_at_unix_ms) = parse_generated_frame_filename(&filename) else {
                continue;
            };
            if captured_at_unix_ms < activation_at {
                continue;
            }
            let artifact_path = path
                .to_str()
                .ok_or_else(|| UploadStoreError::ArtifactPathNotUtf8(path.clone()))?;
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
}

#[cfg(test)]
pub(crate) fn verify_claimed_artifact(
    claim: &ClaimedUpload,
) -> Result<ArtifactVerification, UploadStoreError> {
    match open_claimed_artifact(claim)? {
        OpenedClaimedArtifact::Verified(_) => Ok(ArtifactVerification::Verified),
        OpenedClaimedArtifact::Rejected(reason) => Ok(reason),
    }
}

/// Opens and verifies the exact file handle that will be streamed. On Windows
/// the handle denies later write/delete opens while the HTTP request owns it,
/// preventing a path replacement from changing bytes under an idempotency key.
pub(crate) fn open_claimed_artifact(
    claim: &ClaimedUpload,
) -> Result<OpenedClaimedArtifact, UploadStoreError> {
    let metadata = match fs::symlink_metadata(&claim.artifact_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OpenedClaimedArtifact::Rejected(
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
        return Ok(OpenedClaimedArtifact::Rejected(
            ArtifactVerification::Symlink,
        ));
    }
    if !metadata.is_file() {
        return Ok(OpenedClaimedArtifact::Rejected(
            ArtifactVerification::NotRegularFile,
        ));
    }

    let mut file = match open_artifact_for_read(&claim.artifact_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OpenedClaimedArtifact::Rejected(
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
        return Ok(OpenedClaimedArtifact::Rejected(
            ArtifactVerification::NotRegularFile,
        ));
    }
    if opened_metadata.len() != claim.file_size {
        return Ok(OpenedClaimedArtifact::Rejected(
            ArtifactVerification::SizeMismatch {
                expected: claim.file_size,
                actual: opened_metadata.len(),
            },
        ));
    }
    let actual = sha256_open_file(&mut file, &claim.artifact_path)?;
    if actual != claim.sha256 {
        return Ok(OpenedClaimedArtifact::Rejected(
            ArtifactVerification::Sha256Mismatch {
                expected: claim.sha256,
                actual,
            },
        ));
    }
    Ok(OpenedClaimedArtifact::Verified(file))
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
    attempt_count: i64,
}

impl RawClaim {
    fn into_claim(
        self,
        job_id: UploadJobId,
        capture_root: &Path,
    ) -> Result<ClaimedUpload, UploadStoreError> {
        let sha256 = sha256_from_database(job_id, self.sha256)?;
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

#[cfg(test)]
struct RawUploadJob {
    id: i64,
    artifact_path: String,
    filename: String,
    idempotency_key: String,
    file_size: i64,
    sha256: Vec<u8>,
    state: String,
    attempt_count: i64,
    next_attempt_at_ms: Option<i64>,
    last_http_status: Option<i64>,
    last_error: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    completed_at_ms: Option<i64>,
    last_failure_at_ms: Option<i64>,
}

#[cfg(test)]
impl RawUploadJob {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            artifact_path: row.get(1)?,
            filename: row.get(2)?,
            idempotency_key: row.get(3)?,
            file_size: row.get(4)?,
            sha256: row.get(5)?,
            state: row.get(6)?,
            attempt_count: row.get(7)?,
            next_attempt_at_ms: row.get(8)?,
            last_http_status: row.get(9)?,
            last_error: row.get(10)?,
            created_at_ms: row.get(11)?,
            updated_at_ms: row.get(12)?,
            completed_at_ms: row.get(13)?,
            last_failure_at_ms: row.get(14)?,
        })
    }

    fn into_record(self, capture_root: &Path) -> Result<UploadJobRecord, UploadStoreError> {
        let job_id = job_id_from_database(self.id)?;
        let sha256 = sha256_from_database(job_id, self.sha256)?;
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
        })
    }
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

fn current_unix_time_ms() -> Result<u64, UploadStoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let value = u64::try_from(elapsed.as_millis())
        .map_err(|_| UploadStoreError::TimestampOutOfRange(u64::MAX))?;
    unix_ms_to_database(value)?;
    Ok(value)
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

fn acquire_exclusive_ownership(connection: &Connection) -> Result<(), UploadStoreError> {
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

fn schema_version(connection: &Connection) -> Result<i64, UploadStoreError> {
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

fn create_schema(
    connection: &mut Connection,
    capture_root: &str,
    destination: &str,
    authorization_fingerprint: UploadAuthorizationFingerprint,
    activation_at_unix_ms: u64,
) -> Result<(), UploadStoreError> {
    let activation_at = unix_ms_to_database(activation_at_unix_ms)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(CREATE_METADATA)?;
    transaction.execute_batch(CREATE_UPLOAD_JOBS)?;
    transaction.execute_batch(CREATE_DUE_INDEX)?;
    transaction.execute_batch(CREATE_INSERT_AGGREGATE_TRIGGER)?;
    transaction.execute_batch(CREATE_UPDATE_AGGREGATE_TRIGGER)?;
    transaction.execute_batch(CREATE_IMMUTABLE_IDENTITY_TRIGGER)?;
    transaction.execute_batch(CREATE_NO_DELETE_TRIGGER)?;
    transaction.execute(
        r#"
        INSERT INTO upload_metadata (
            singleton, schema_signature, capture_root, destination,
            authorization_sha256, activation_at_ms
        ) VALUES (1, ?1, ?2, ?3, ?4, ?5)
        "#,
        params![
            SCHEMA_SIGNATURE,
            capture_root,
            destination,
            authorization_fingerprint.as_slice(),
            activation_at
        ],
    )?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn verify_schema(
    connection: &mut Connection,
    capture_root: &str,
    destination: &str,
    authorization_fingerprint: UploadAuthorizationFingerprint,
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
    if user_schema_object_count != 7 {
        return Err(UploadStoreError::InvalidSchema);
    }
    for (kind, name, expected) in [
        ("table", "upload_metadata", CREATE_METADATA),
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
            "upload_jobs_identity_immutable",
            CREATE_IMMUTABLE_IDENTITY_TRIGGER,
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
    let (signature, stored_root, stored_destination): (String, String, String) = connection
        .query_row(
            r#"
        SELECT schema_signature, capture_root, destination
        FROM upload_metadata WHERE singleton = 1
        "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    if signature != SCHEMA_SIGNATURE {
        return Err(UploadStoreError::InvalidSchema);
    }
    if stored_root != capture_root {
        return Err(UploadStoreError::CaptureRootMismatch);
    }
    if stored_destination != destination {
        return Err(UploadStoreError::DestinationMismatch);
    }
    rotate_authorization_if_drained(connection, authorization_fingerprint)?;
    Ok(())
}

/// Rebinds a drained ledger without discarding terminal history. The immediate
/// transaction makes the live-work test and fingerprint update indivisible;
/// interrupted `in_progress` work is deliberately checked before recovery.
fn rotate_authorization_if_drained(
    connection: &mut Connection,
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
        if has_live_work {
            return Err(UploadStoreError::AuthorizationIdentityMismatch);
        }
        let changed = transaction.execute(
            "UPDATE upload_metadata SET authorization_sha256 = ?1 WHERE singleton = 1",
            [authorization_fingerprint.as_slice()],
        )?;
        if changed != 1 {
            return Err(UploadStoreError::InvalidSchema);
        }
    }
    transaction.commit()?;
    Ok(())
}

fn recover_interrupted_jobs(connection: &mut Connection) -> Result<(), UploadStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "UPDATE upload_jobs SET state = 'pending', next_attempt_at_ms = NULL WHERE state = 'in_progress'",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

fn normalize_sql(sql: &str) -> String {
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

fn parse_generated_frame_filename(filename: &str) -> Option<u64> {
    let stem = filename
        .strip_suffix(".jpg")
        .or_else(|| filename.strip_suffix(".jpeg"));
    let stem = stem?;
    let mut parts = stem.split('-');
    let (Some("frame"), Some(seconds), Some(millis), Some(sequence), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || millis.len() != 3
        || !millis.bytes().all(|byte| byte.is_ascii_digit())
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

fn is_generated_frame_filename(filename: &str) -> bool {
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

#[cfg(test)]
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

    fn activation_at(store: &UploadStore) -> u64 {
        let raw: i64 = store
            .connection
            .query_row(
                "SELECT activation_at_ms FROM upload_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        u64::try_from(raw).unwrap()
    }

    impl Drop for TestEnvironment {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
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
    fn reconciliation_uses_exact_generated_grammar_and_is_idempotent() {
        let environment = TestEnvironment::new();
        let future_seconds = current_unix_time_ms().unwrap() / 1_000 + 60;
        fs::write(
            environment
                .capture
                .join(format!("frame-{future_seconds}-001-000001.jpg")),
            b"one",
        )
        .unwrap();
        fs::write(
            environment
                .capture
                .join(format!("frame-{future_seconds}-999-1000000.jpeg")),
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

        let mut store = environment.open();
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
    fn activation_skips_history_but_reconciles_later_crash_gap_files() {
        let environment = TestEnvironment::new();
        let historical_at = current_unix_time_ms().unwrap().saturating_sub(60_000);
        environment.artifact_at(historical_at, 20, b"historical");

        let mut store = environment.open();
        let activation = activation_at(&store);
        let first = store.reconcile_capture_directory(activation).unwrap();
        assert_eq!(first.eligible, 0);
        assert_eq!(store.snapshot().unwrap().counts.total(), 0);

        let at_boundary = environment.artifact_at(activation, 21, b"at activation");
        let crash_gap = environment.artifact_at(activation + 1, 22, b"post activation");
        drop(store);
        let mut reopened = environment.open();
        assert_eq!(activation_at(&reopened), activation);
        let reconciled = reopened
            .reconcile_capture_directory(activation + 2)
            .unwrap();
        assert_eq!(reconciled.eligible, 2);
        assert_eq!(reconciled.inserted, 2);
        let first_claim = reopened.claim_due(activation + 2).unwrap().unwrap();
        assert_eq!(
            reopened
                .job(first_claim.job_id)
                .unwrap()
                .unwrap()
                .artifact_path,
            fs::canonicalize(at_boundary).unwrap()
        );
        assert!(crash_gap.exists());
    }

    #[test]
    fn reconciliation_skips_a_known_replaced_path_before_file_inspection() {
        let environment = TestEnvironment::new();
        let mut store = environment.open();
        let activation = activation_at(&store);
        let artifact = environment.artifact_at(activation + 1, 23, b"recorded bytes");
        store.record_artifact(&artifact, activation + 1).unwrap();

        fs::remove_file(&artifact).unwrap();
        fs::create_dir(&artifact).unwrap();
        let result = store.reconcile_capture_directory(activation + 2).unwrap();
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

        let unbound_environment = TestEnvironment::new();
        let unbound = Connection::open(&unbound_environment.database).unwrap();
        unbound.pragma_update(None, "user_version", 1).unwrap();
        drop(unbound);
        assert!(matches!(
            UploadStore::open(
                &unbound_environment.database,
                &unbound_environment.capture,
                TEST_DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            ),
            Err(UploadStoreError::UnsupportedSchema {
                found: 1,
                supported: SCHEMA_VERSION,
            })
        ));

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
            .execute(
                "UPDATE upload_metadata SET schema_signature = 'tampered' WHERE singleton = 1",
                [],
            )
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
