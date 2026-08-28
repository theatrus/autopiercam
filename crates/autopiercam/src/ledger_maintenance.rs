use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use autopiercam_core::config::{Config, ConfigError};
use fs4::{FileExt, TryLockError};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, backup, params, types::ValueRef,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

use crate::upload::upload_store::{
    APPLICATION_ID, CREATE_DUE_INDEX, CREATE_IMMUTABLE_IDENTITY_TRIGGER,
    CREATE_IMMUTABLE_METADATA_TRIGGER, CREATE_INSERT_AGGREGATE_TRIGGER,
    CREATE_JOB_REVISION_GUARD_TRIGGER, CREATE_METADATA, CREATE_NO_DELETE_TRIGGER,
    CREATE_PREACTIVATION_ARTIFACTS, CREATE_UPDATE_AGGREGATE_TRIGGER, CREATE_UPLOAD_JOBS,
    DELIVERY_BINDING_DOMAIN, SCHEMA_SIGNATURE, SCHEMA_VERSION, UploadLedgerId, UploadStoreError,
    acquire_exclusive_ownership, is_generated_frame_filename, normalize_sql, read_ledger_id,
    schema_version, verify_schema,
};

const V3_SCHEMA_VERSION: i64 = 3;
const V3_SCHEMA_SIGNATURE: &str = "autopiercam-upload-ledger-v3-destination-authorization-preactivation-inventory-aggregates-sha256-20260828";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const V3_CREATE_METADATA: &str = r#"
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

const V3_CREATE_PREACTIVATION_ARTIFACTS: &str = r#"
CREATE TABLE upload_preactivation_artifacts (
    artifact_path TEXT PRIMARY KEY CHECK (length(artifact_path) > 0)
) STRICT, WITHOUT ROWID
"#;

const V3_CREATE_UPLOAD_JOBS: &str = r#"
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

const V3_CREATE_DUE_INDEX: &str = r#"
CREATE INDEX upload_jobs_due
    ON upload_jobs (COALESCE(next_attempt_at_ms, created_at_ms), id)
    WHERE state IN ('pending', 'retrying')
"#;

const V3_CREATE_INSERT_AGGREGATE_TRIGGER: &str = r#"
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

const V3_CREATE_UPDATE_AGGREGATE_TRIGGER: &str = r#"
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

const V3_CREATE_IMMUTABLE_IDENTITY_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_identity_immutable
BEFORE UPDATE OF artifact_path, filename, idempotency_key, file_size, sha256, created_at_ms
ON upload_jobs
BEGIN
    SELECT RAISE(ABORT, 'upload artifact identity is immutable');
END
"#;

const V3_CREATE_NO_DELETE_TRIGGER: &str = r#"
CREATE TRIGGER upload_jobs_no_delete
BEFORE DELETE ON upload_jobs
BEGIN
    SELECT RAISE(ABORT, 'upload jobs are append-only');
END
"#;

const V3_VERIFY_SCHEMA_PROJECTION: &str = r#"
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
pub(crate) enum LedgerLeaseError {
    #[error("could not open upload-ledger maintenance marker {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("upload-ledger offline maintenance is already active for {database:?}")]
    MaintenanceActive { database: PathBuf },
    #[error("the upload ledger is active; stop autopiercam before offline maintenance")]
    AgentActive,
    #[error("could not lock upload-ledger maintenance marker {path:?}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug)]
pub(crate) struct LedgerLease {
    _file: File,
}

impl LedgerLease {
    pub(crate) fn acquire_live(database: &Path) -> Result<Self, LedgerLeaseError> {
        Self::acquire(database, false)
    }

    fn acquire_maintenance(database: &Path) -> Result<Self, LedgerLeaseError> {
        Self::acquire(database, true)
    }

    fn acquire(database: &Path, exclusive: bool) -> Result<Self, LedgerLeaseError> {
        let path = maintenance_marker_path(database);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| LedgerLeaseError::Open {
                path: path.clone(),
                source,
            })?;
        let result = if exclusive {
            FileExt::try_lock(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        match result {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) if exclusive => Err(LedgerLeaseError::AgentActive),
            Err(TryLockError::WouldBlock) => Err(LedgerLeaseError::MaintenanceActive {
                database: database.to_path_buf(),
            }),
            Err(TryLockError::Error(source)) => Err(LedgerLeaseError::Lock { path, source }),
        }
    }
}

#[derive(Debug, Error)]
pub enum LedgerMaintenanceError {
    #[error("could not resolve {kind} path {path:?}")]
    ResolvePath {
        kind: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("upload-ledger maintenance lease failed: {0}")]
    Lease(String),
    #[error("SQLite upload-ledger maintenance failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("upload-ledger verification failed: {0}")]
    Store(String),
    #[error("upload ledger does not exist at {0:?}")]
    MissingLedger(PathBuf),
    #[error(
        "unsupported upload-ledger schema version {found}; maintenance supports only v3 -> v4 and exact v4"
    )]
    UnsupportedSchema { found: i64 },
    #[error("the v3 upload ledger does not match the exact released schema")]
    InvalidV3Schema,
    #[error("the upload ledger failed SQLite integrity verification: {0}")]
    Integrity(String),
    #[error("the configured capture directory does not match the ledger")]
    CaptureRootMismatch,
    #[error("expected ledger ID must be exactly 32 lowercase hexadecimal characters")]
    InvalidExpectedLedgerId,
    #[error("expected ledger ID {expected} does not match active ledger {actual}")]
    LedgerIdMismatch { expected: String, actual: String },
    #[error("archive refused: {count} nonterminal upload job(s) remain")]
    LiveWork { count: i64 },
    #[error("archive refused: {count} permanently failed upload job(s) require operator action")]
    PermanentFailures { count: i64 },
    #[error("archive refused: generated capture {path:?} has no durable upload job")]
    CrashGap { path: PathBuf },
    #[error("archive target already exists: {0:?}")]
    ArchiveExists(PathBuf),
    #[error("retired-ledger target already exists: {0:?}")]
    RetiredExists(PathBuf),
    #[error("refusing unsafe {kind} path (symlink, reparse point, or non-regular file): {path:?}")]
    UnsafePath { kind: &'static str, path: PathBuf },
    #[error("incomplete prior archive has inconsistent artifacts: {0}")]
    PartialArchive(String),
    #[error(
        "a verified prior archive exists, but the active ledger changed after it was published"
    )]
    ActiveChangedAfterArchive,
    #[error("could not {operation} {path:?}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("SQLite could not fully checkpoint the upload ledger before archive")]
    CheckpointBusy,
    #[error("SQLite did not leave DELETE journal mode before archive (reported {0:?})")]
    JournalMode(String),
    #[error("unexpected SQLite sidecar remains after checkpoint: {0:?}")]
    SidecarPresent(PathBuf),
}

impl From<LedgerLeaseError> for LedgerMaintenanceError {
    fn from(error: LedgerLeaseError) -> Self {
        Self::Lease(error.to_string())
    }
}

impl From<UploadStoreError> for LedgerMaintenanceError {
    fn from(error: UploadStoreError) -> Self {
        Self::Store(error.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerMigrationReport {
    pub database_path: PathBuf,
    pub ledger_id: String,
    pub migrated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerArchiveReport {
    pub ledger_id: String,
    pub archive_path: PathBuf,
    pub retired_path: PathBuf,
    pub sha256: String,
}

struct MaintenanceContext {
    database_path: PathBuf,
    capture_root: PathBuf,
}

struct LedgerIdentity {
    capture_root: String,
    destination: String,
    authorization: [u8; 32],
}

pub fn migrate_upload_ledger(
    config_path: &Path,
) -> Result<LedgerMigrationReport, LedgerMaintenanceError> {
    let context = maintenance_context(config_path)?;
    let _lease = LedgerLease::acquire_maintenance(&context.database_path)?;
    let mut connection = open_existing_exclusive(&context.database_path)?;
    let version = schema_version(&connection)?;
    let migrated = match version {
        V3_SCHEMA_VERSION => {
            let identity = verify_v3(&connection, &context.capture_root)?;
            migrate_v3_to_v4(&mut connection, &identity)?;
            true
        }
        SCHEMA_VERSION => false,
        found => return Err(LedgerMaintenanceError::UnsupportedSchema { found }),
    };
    let ledger_id = verify_v4(&connection, &context.capture_root)?;
    Ok(LedgerMigrationReport {
        database_path: context.database_path,
        ledger_id: ledger_id.as_hex(),
        migrated,
    })
}

pub fn archive_upload_ledger(
    config_path: &Path,
    expected_ledger_id: &str,
) -> Result<LedgerArchiveReport, LedgerMaintenanceError> {
    let expected = parse_expected_ledger_id(expected_ledger_id)?;
    let context = maintenance_context(config_path)?;
    let _lease = LedgerLease::acquire_maintenance(&context.database_path)?;
    let ledger_hex = expected.as_hex();
    let archive_path =
        config_path_for_output(config_path, &format!("upload.{ledger_hex}.archive.sqlite3"))?;
    let retired_path =
        config_path_for_output(config_path, &format!("upload.{ledger_hex}.retired.sqlite3"))?;
    let active_exists =
        safe_regular_file(&context.database_path, "active upload ledger")?.is_some();
    let archive_exists = safe_regular_file(&archive_path, "upload-ledger archive")?.is_some();
    let retired_exists = safe_regular_file(&retired_path, "retired upload ledger")?.is_some();

    if archive_exists && retired_exists {
        if active_exists {
            return Err(LedgerMaintenanceError::PartialArchive(
                "archive and retired ledger are complete, but an active database also exists; inspect its ledger ID before removing or starting another archive".to_owned(),
            ));
        }
        return completed_archive_report(
            expected,
            &archive_path,
            &retired_path,
            &context.capture_root,
        );
    }
    if retired_exists {
        return Err(LedgerMaintenanceError::PartialArchive(
            "a retired ledger exists without its verified archive; restore or inspect the retired file before retrying".to_owned(),
        ));
    }
    if !active_exists {
        let detail = if archive_exists {
            "a verified-archive candidate exists, but both active and retired ledgers are absent"
        } else {
            "the active upload ledger is absent"
        };
        return Err(LedgerMaintenanceError::PartialArchive(detail.to_owned()));
    }

    let source = open_existing_exclusive(&context.database_path)?;
    if schema_version(&source)? != SCHEMA_VERSION {
        return Err(LedgerMaintenanceError::UnsupportedSchema {
            found: schema_version(&source)?,
        });
    }
    let ledger_id = verify_v4(&source, &context.capture_root)?;
    ensure_expected_id(expected, ledger_id)?;
    ensure_archive_ready(&source, &context.capture_root)?;

    checkpoint_for_archive(&source, &context.database_path)?;
    ensure_archive_ready(&source, &context.capture_root)?;
    let archive_sha256 = if archive_exists {
        let archived_id = verify_archive(&archive_path, &context.capture_root)?;
        ensure_expected_id(ledger_id, archived_id)?;
        if ledger_content_digest(&source)? != ledger_content_digest_path(&archive_path)? {
            return Err(LedgerMaintenanceError::ActiveChangedAfterArchive);
        }
        sha256_path(&archive_path)?
    } else {
        let temporary_path = unique_partial_path(&archive_path)?;
        let mut temporary = TemporaryPath::new(temporary_path.clone());
        backup_database(&source, &temporary_path)?;
        let archived_id = verify_archive(&temporary_path, &context.capture_root)?;
        ensure_expected_id(ledger_id, archived_id)?;
        if ledger_content_digest(&source)? != ledger_content_digest_path(&temporary_path)? {
            return Err(LedgerMaintenanceError::PartialArchive(
                "SQLite backup contents did not match the active ledger".to_owned(),
            ));
        }
        let hash = sha256_path(&temporary_path)?;
        sync_file(&temporary_path)?;
        publish_no_replace(&temporary_path, &archive_path, "publish verified archive")?;
        temporary.disarm();
        sync_file(&archive_path)?;
        hash
    };

    drop(source);
    ensure_no_sqlite_sidecars(&context.database_path)?;
    sync_file(&context.database_path)?;
    publish_no_replace(
        &context.database_path,
        &retired_path,
        "retain retired source ledger",
    )?;

    Ok(LedgerArchiveReport {
        ledger_id: ledger_id.as_hex(),
        archive_path,
        retired_path,
        sha256: hex_encode(&archive_sha256),
    })
}

fn completed_archive_report(
    expected: UploadLedgerId,
    archive_path: &Path,
    retired_path: &Path,
    capture_root: &Path,
) -> Result<LedgerArchiveReport, LedgerMaintenanceError> {
    let archive_id = verify_archive(archive_path, capture_root)?;
    let retired_id = verify_archive(retired_path, capture_root)?;
    ensure_expected_id(expected, archive_id)?;
    ensure_expected_id(expected, retired_id)?;
    if ledger_content_digest_path(archive_path)? != ledger_content_digest_path(retired_path)? {
        return Err(LedgerMaintenanceError::PartialArchive(
            "archive and retired ledger have different logical contents".to_owned(),
        ));
    }
    Ok(LedgerArchiveReport {
        ledger_id: expected.as_hex(),
        archive_path: archive_path.to_path_buf(),
        retired_path: retired_path.to_path_buf(),
        sha256: hex_encode(&sha256_path(archive_path)?),
    })
}

fn maintenance_context(config_path: &Path) -> Result<MaintenanceContext, LedgerMaintenanceError> {
    let config_path =
        std::path::absolute(config_path).map_err(|source| LedgerMaintenanceError::ResolvePath {
            kind: "configuration",
            path: config_path.to_path_buf(),
            source,
        })?;
    let config = Config::load(&config_path)?;
    let configured_capture_root = if config.capture.directory.is_absolute() {
        config.capture.directory
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(config.capture.directory)
    };
    let capture_root = fs::canonicalize(&configured_capture_root).map_err(|source| {
        LedgerMaintenanceError::ResolvePath {
            kind: "capture-directory",
            path: configured_capture_root,
            source,
        }
    })?;
    Ok(MaintenanceContext {
        database_path: config_path.with_extension("upload.sqlite3"),
        capture_root,
    })
}

fn config_path_for_output(
    config_path: &Path,
    extension: &str,
) -> Result<PathBuf, LedgerMaintenanceError> {
    let absolute =
        std::path::absolute(config_path).map_err(|source| LedgerMaintenanceError::ResolvePath {
            kind: "configuration",
            path: config_path.to_path_buf(),
            source,
        })?;
    Ok(absolute.with_extension(extension))
}

fn maintenance_marker_path(database: &Path) -> PathBuf {
    let mut value = database.as_os_str().to_os_string();
    value.push(".maintenance.lock");
    PathBuf::from(value)
}

fn open_existing_exclusive(path: &Path) -> Result<Connection, LedgerMaintenanceError> {
    if safe_regular_file(path, "active upload ledger")?.is_none() {
        return Err(LedgerMaintenanceError::MissingLedger(path.to_path_buf()));
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    acquire_exclusive_ownership(&connection)?;
    Ok(connection)
}

fn safe_regular_file(
    path: &Path,
    kind: &'static str,
) -> Result<Option<fs::Metadata>, LedgerMaintenanceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LedgerMaintenanceError::Io {
                operation: "inspect ledger artifact",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || metadata_is_reparse(&metadata) {
        return Err(LedgerMaintenanceError::UnsafePath {
            kind,
            path: path.to_path_buf(),
        });
    }
    Ok(Some(metadata))
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn verify_v3(
    connection: &Connection,
    configured_capture_root: &Path,
) -> Result<LedgerIdentity, LedgerMaintenanceError> {
    verify_integrity(connection)?;
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(LedgerMaintenanceError::InvalidV3Schema);
    }
    let object_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if object_count != 8 {
        return Err(LedgerMaintenanceError::InvalidV3Schema);
    }
    for (kind, name, expected) in [
        ("table", "upload_metadata", V3_CREATE_METADATA),
        (
            "table",
            "upload_preactivation_artifacts",
            V3_CREATE_PREACTIVATION_ARTIFACTS,
        ),
        ("table", "upload_jobs", V3_CREATE_UPLOAD_JOBS),
        ("index", "upload_jobs_due", V3_CREATE_DUE_INDEX),
        (
            "trigger",
            "upload_jobs_after_insert",
            V3_CREATE_INSERT_AGGREGATE_TRIGGER,
        ),
        (
            "trigger",
            "upload_jobs_after_status_update",
            V3_CREATE_UPDATE_AGGREGATE_TRIGGER,
        ),
        (
            "trigger",
            "upload_jobs_identity_immutable",
            V3_CREATE_IMMUTABLE_IDENTITY_TRIGGER,
        ),
        (
            "trigger",
            "upload_jobs_no_delete",
            V3_CREATE_NO_DELETE_TRIGGER,
        ),
    ] {
        let actual: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = ?1 AND name = ?2",
                params![kind, name],
                |row| row.get(0),
            )
            .optional()?;
        if actual.as_deref().map(normalize_sql) != Some(normalize_sql(expected)) {
            return Err(LedgerMaintenanceError::InvalidV3Schema);
        }
    }
    connection.prepare(V3_VERIFY_SCHEMA_PROJECTION)?;
    let (signature, capture_root, destination, authorization): (String, String, String, Vec<u8>) =
        connection.query_row(
            r#"
            SELECT schema_signature, capture_root, destination, authorization_sha256
            FROM upload_metadata WHERE singleton = 1
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let authorization: [u8; 32] = authorization
        .try_into()
        .map_err(|_| LedgerMaintenanceError::InvalidV3Schema)?;
    if signature != V3_SCHEMA_SIGNATURE {
        return Err(LedgerMaintenanceError::InvalidV3Schema);
    }
    if Path::new(&capture_root) != configured_capture_root {
        return Err(LedgerMaintenanceError::CaptureRootMismatch);
    }
    verify_aggregate_counts(connection, true)?;
    Ok(LedgerIdentity {
        capture_root,
        destination,
        authorization,
    })
}

fn migrate_v3_to_v4(
    connection: &mut Connection,
    identity: &LedgerIdentity,
) -> Result<(), LedgerMaintenanceError> {
    let ledger_id = UploadLedgerId::random()?;
    let delivery_binding = delivery_binding(&identity.destination, &identity.authorization);
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        r#"
        DROP TRIGGER upload_jobs_after_insert;
        DROP TRIGGER upload_jobs_after_status_update;
        DROP TRIGGER upload_jobs_identity_immutable;
        DROP TRIGGER upload_jobs_no_delete;
        DROP INDEX upload_jobs_due;
        ALTER TABLE upload_metadata RENAME TO upload_metadata_v3;
        ALTER TABLE upload_preactivation_artifacts RENAME TO upload_preactivation_artifacts_v3;
        ALTER TABLE upload_jobs RENAME TO upload_jobs_v3;
        "#,
    )?;
    transaction.execute_batch(CREATE_METADATA)?;
    transaction.execute_batch(CREATE_PREACTIVATION_ARTIFACTS)?;
    transaction.execute_batch(CREATE_UPLOAD_JOBS)?;
    transaction.execute(
        r#"
        INSERT INTO upload_metadata (
            singleton, schema_signature, ledger_id, ledger_revision,
            capture_root, destination, authorization_sha256,
            pending_count, in_progress_count, retrying_count,
            completed_count, permanently_failed_count,
            last_success_at_ms, last_failure_at_ms, last_error
        )
        SELECT singleton, ?1, ?2, (SELECT COUNT(*) FROM upload_jobs_v3),
               capture_root, destination, authorization_sha256,
               pending_count, in_progress_count, retrying_count,
               completed_count, permanently_failed_count,
               last_success_at_ms, last_failure_at_ms, last_error
        FROM upload_metadata_v3 WHERE singleton = 1
        "#,
        params![SCHEMA_SIGNATURE, ledger_id.as_slice()],
    )?;
    transaction.execute(
        r#"
        INSERT INTO upload_preactivation_artifacts (artifact_path)
        SELECT artifact_path FROM upload_preactivation_artifacts_v3
        "#,
        [],
    )?;
    transaction.execute(
        r#"
        INSERT INTO upload_jobs (
            id, artifact_path, filename, idempotency_key, file_size, sha256,
            delivery_binding_sha256, state, attempt_count, next_attempt_at_ms,
            last_http_status, last_error, created_at_ms, updated_at_ms,
            completed_at_ms, last_failure_at_ms, job_revision, requeue_count,
            last_requeued_at_ms
        )
        SELECT id, artifact_path, filename, idempotency_key, file_size, sha256,
               ?1, state, attempt_count, next_attempt_at_ms,
               last_http_status, last_error, created_at_ms, updated_at_ms,
               completed_at_ms, last_failure_at_ms, 1, 0, NULL
        FROM upload_jobs_v3 ORDER BY id
        "#,
        [delivery_binding.as_slice()],
    )?;
    transaction.execute_batch(CREATE_DUE_INDEX)?;
    transaction.execute_batch(CREATE_INSERT_AGGREGATE_TRIGGER)?;
    transaction.execute_batch(CREATE_UPDATE_AGGREGATE_TRIGGER)?;
    transaction.execute_batch(CREATE_JOB_REVISION_GUARD_TRIGGER)?;
    transaction.execute_batch(CREATE_IMMUTABLE_IDENTITY_TRIGGER)?;
    transaction.execute_batch(CREATE_IMMUTABLE_METADATA_TRIGGER)?;
    transaction.execute_batch(CREATE_NO_DELETE_TRIGGER)?;
    transaction.execute_batch(
        r#"
        DROP TABLE upload_jobs_v3;
        DROP TABLE upload_preactivation_artifacts_v3;
        DROP TABLE upload_metadata_v3;
        "#,
    )?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    // Verify the exact target schema and all derived migration fields while
    // the transformation is still rollback-able.
    verify_schema(&transaction, &identity.capture_root, &identity.destination)?;
    verify_aggregate_counts(&transaction, false)?;
    let stored_ledger_id: Vec<u8> = transaction.query_row(
        "SELECT ledger_id FROM upload_metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let invalid_binding_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM upload_jobs WHERE delivery_binding_sha256 <> ?1",
        [delivery_binding.as_slice()],
        |row| row.get(0),
    )?;
    if stored_ledger_id.as_slice() != ledger_id.as_slice() || invalid_binding_count != 0 {
        return Err(LedgerMaintenanceError::Store(
            UploadStoreError::InvalidSchema.to_string(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

fn verify_v4(
    connection: &Connection,
    configured_capture_root: &Path,
) -> Result<UploadLedgerId, LedgerMaintenanceError> {
    verify_integrity(connection)?;
    let (capture_root, destination): (String, String) = connection.query_row(
        "SELECT capture_root, destination FROM upload_metadata WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if Path::new(&capture_root) != configured_capture_root {
        return Err(LedgerMaintenanceError::CaptureRootMismatch);
    }
    verify_schema(connection, &capture_root, &destination)?;
    verify_aggregate_counts(connection, false)?;
    Ok(read_ledger_id(connection)?)
}

fn verify_integrity(connection: &Connection) -> Result<(), LedgerMaintenanceError> {
    let result: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(LedgerMaintenanceError::Integrity(result))
    }
}

fn verify_aggregate_counts(
    connection: &Connection,
    v3: bool,
) -> Result<(), LedgerMaintenanceError> {
    let stored: (i64, i64, i64, i64, i64) = connection.query_row(
        r#"
        SELECT pending_count, in_progress_count, retrying_count,
               completed_count, permanently_failed_count
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
            ))
        },
    )?;
    let actual: (i64, i64, i64, i64, i64) = connection.query_row(
        r#"
        SELECT SUM(state = 'pending'), SUM(state = 'in_progress'),
               SUM(state = 'retrying'), SUM(state = 'completed'),
               SUM(state = 'permanently_failed')
        FROM upload_jobs
        "#,
        [],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        },
    )?;
    if stored == actual {
        Ok(())
    } else if v3 {
        Err(LedgerMaintenanceError::InvalidV3Schema)
    } else {
        Err(LedgerMaintenanceError::Store(
            UploadStoreError::InvalidSchema.to_string(),
        ))
    }
}

fn delivery_binding(destination: &str, authorization: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DELIVERY_BINDING_DOMAIN);
    hasher.update((destination.len() as u64).to_be_bytes());
    hasher.update(destination.as_bytes());
    hasher.update(authorization);
    hasher.finalize().into()
}

fn parse_expected_ledger_id(value: &str) -> Result<UploadLedgerId, LedgerMaintenanceError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(LedgerMaintenanceError::InvalidExpectedLedgerId);
    }
    UploadLedgerId::parse_hex(value).map_err(|_| LedgerMaintenanceError::InvalidExpectedLedgerId)
}

fn ensure_expected_id(
    expected: UploadLedgerId,
    actual: UploadLedgerId,
) -> Result<(), LedgerMaintenanceError> {
    if expected == actual {
        Ok(())
    } else {
        Err(LedgerMaintenanceError::LedgerIdMismatch {
            expected: expected.as_hex(),
            actual: actual.as_hex(),
        })
    }
}

fn ensure_archive_ready(
    connection: &Connection,
    capture_root: &Path,
) -> Result<(), LedgerMaintenanceError> {
    let live: i64 = connection.query_row(
        "SELECT COUNT(*) FROM upload_jobs WHERE state IN ('pending', 'in_progress', 'retrying')",
        [],
        |row| row.get(0),
    )?;
    if live != 0 {
        return Err(LedgerMaintenanceError::LiveWork { count: live });
    }
    let permanent: i64 = connection.query_row(
        "SELECT COUNT(*) FROM upload_jobs WHERE state = 'permanently_failed'",
        [],
        |row| row.get(0),
    )?;
    if permanent != 0 {
        return Err(LedgerMaintenanceError::PermanentFailures { count: permanent });
    }
    ensure_no_crash_gap(connection, capture_root)
}

fn ensure_no_crash_gap(
    connection: &Connection,
    capture_root: &Path,
) -> Result<(), LedgerMaintenanceError> {
    let mut statement = connection.prepare(
        r#"
        SELECT artifact_path FROM upload_jobs
        UNION
        SELECT artifact_path FROM upload_preactivation_artifacts
        "#,
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut recorded = HashSet::new();
    for row in rows {
        recorded.insert(row?);
    }
    let entries = fs::read_dir(capture_root).map_err(|source| LedgerMaintenanceError::Io {
        operation: "inventory capture directory for archive",
        path: capture_root.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| LedgerMaintenanceError::Io {
            operation: "read capture-directory entry for archive",
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
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| LedgerMaintenanceError::Io {
                operation: "inspect generated capture for archive",
                path: path.clone(),
                source,
            })?;
        if !metadata.file_type().is_file() || metadata_is_reparse(&metadata) {
            return Err(LedgerMaintenanceError::UnsafePath {
                kind: "generated capture",
                path,
            });
        }
        let Some(path_text) = path.to_str() else {
            return Err(LedgerMaintenanceError::CrashGap { path });
        };
        if !recorded.contains(path_text) {
            return Err(LedgerMaintenanceError::CrashGap { path });
        }
    }
    Ok(())
}

fn checkpoint_for_archive(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), LedgerMaintenanceError> {
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 {
        return Err(LedgerMaintenanceError::CheckpointBusy);
    }
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Err(LedgerMaintenanceError::JournalMode(journal_mode));
    }
    for sidecar in sqlite_sidecars(database_path) {
        if sidecar.exists() {
            return Err(LedgerMaintenanceError::SidecarPresent(sidecar));
        }
    }
    Ok(())
}

fn backup_database(
    source: &Connection,
    destination_path: &Path,
) -> Result<(), LedgerMaintenanceError> {
    let mut destination = Connection::open(destination_path)?;
    let backup = backup::Backup::new(source, &mut destination)?;
    backup.run_to_completion(100, Duration::ZERO, None)?;
    drop(backup);
    destination.close().map_err(|(_, error)| error)?;
    Ok(())
}

fn verify_archive(
    path: &Path,
    capture_root: &Path,
) -> Result<UploadLedgerId, LedgerMaintenanceError> {
    if safe_regular_file(path, "upload-ledger archive")?.is_none() {
        return Err(LedgerMaintenanceError::PartialArchive(format!(
            "ledger artifact is missing: {}",
            path.display()
        )));
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let id = verify_v4(&connection, capture_root)?;
    ensure_archive_ready(&connection, capture_root)?;
    Ok(id)
}

fn ledger_content_digest_path(path: &Path) -> Result<[u8; 32], LedgerMaintenanceError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    ledger_content_digest(&connection)
}

fn ledger_content_digest(connection: &Connection) -> Result<[u8; 32], LedgerMaintenanceError> {
    let mut hasher = Sha256::new();
    hash_query(
        connection,
        r#"
        SELECT singleton, schema_signature, ledger_id, ledger_revision,
               capture_root, destination, authorization_sha256,
               pending_count, in_progress_count, retrying_count,
               completed_count, permanently_failed_count,
               last_success_at_ms, last_failure_at_ms, last_error
        FROM upload_metadata ORDER BY singleton
        "#,
        15,
        &mut hasher,
    )?;
    hash_query(
        connection,
        r#"
        SELECT id, artifact_path, filename, idempotency_key, file_size, sha256,
               delivery_binding_sha256, state, attempt_count, next_attempt_at_ms,
               last_http_status, last_error, created_at_ms, updated_at_ms,
               completed_at_ms, last_failure_at_ms, job_revision, requeue_count,
               last_requeued_at_ms
        FROM upload_jobs ORDER BY id
        "#,
        19,
        &mut hasher,
    )?;
    hash_query(
        connection,
        "SELECT artifact_path FROM upload_preactivation_artifacts ORDER BY artifact_path",
        1,
        &mut hasher,
    )?;
    Ok(hasher.finalize().into())
}

fn hash_query(
    connection: &Connection,
    sql: &str,
    column_count: usize,
    hasher: &mut Sha256,
) -> Result<(), LedgerMaintenanceError> {
    hasher.update((sql.len() as u64).to_be_bytes());
    hasher.update(sql.as_bytes());
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        hasher.update([0xff]);
        for column in 0..column_count {
            match row.get_ref(column)? {
                ValueRef::Null => hasher.update([0]),
                ValueRef::Integer(value) => {
                    hasher.update([1]);
                    hasher.update(value.to_be_bytes());
                }
                ValueRef::Real(value) => {
                    hasher.update([2]);
                    hasher.update(value.to_bits().to_be_bytes());
                }
                ValueRef::Text(value) => {
                    hasher.update([3]);
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value);
                }
                ValueRef::Blob(value) => {
                    hasher.update([4]);
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value);
                }
            }
        }
    }
    Ok(())
}

fn unique_partial_path(archive: &Path) -> Result<PathBuf, LedgerMaintenanceError> {
    for _ in 0..32 {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|source| LedgerMaintenanceError::Io {
            operation: "generate archive temporary name",
            path: archive.to_path_buf(),
            source: io::Error::other(source.to_string()),
        })?;
        let mut value = archive.as_os_str().to_os_string();
        value.push(format!(".partial-{}", hex_encode(&random)));
        let path = PathBuf::from(value);
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(LedgerMaintenanceError::Io {
                    operation: "create archive temporary file",
                    path,
                    source,
                });
            }
        }
    }
    Err(LedgerMaintenanceError::Io {
        operation: "create unique archive temporary file",
        path: archive.to_path_buf(),
        source: io::Error::new(io::ErrorKind::AlreadyExists, "temporary-name collision"),
    })
}

fn publish_no_replace(
    source_path: &Path,
    destination: &Path,
    operation: &'static str,
) -> Result<(), LedgerMaintenanceError> {
    publish_no_replace_platform(source_path, destination).map_err(|source| {
        LedgerMaintenanceError::Io {
            operation,
            path: destination.to_path_buf(),
            source,
        }
    })
}

#[cfg(windows)]
fn publish_no_replace_platform(source: &Path, destination: &Path) -> io::Result<()> {
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // No REPLACE_EXISTING flag: publication fails closed if the destination
    // appeared after preflight. WRITE_THROUGH makes the name change durable.
    let moved = unsafe {
        // SAFETY: Both UTF-16 buffers are NUL-terminated and remain alive for
        // the duration of the synchronous Win32 call.
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn publish_no_replace_platform(source_path: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(source_path, destination)?;
    sync_parent_directory(destination)?;
    if let Err(source) = fs::remove_file(source_path) {
        let _ = fs::remove_file(destination);
        let _ = sync_parent_directory(destination);
        return Err(source);
    }
    sync_parent_directory(source_path)?;
    if source_path.parent() != destination.parent() {
        sync_parent_directory(destination)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

fn sync_file(path: &Path) -> Result<(), LedgerMaintenanceError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| LedgerMaintenanceError::Io {
            operation: "open durable ledger artifact",
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| LedgerMaintenanceError::Io {
            operation: "synchronize durable ledger artifact",
            path: path.to_path_buf(),
            source,
        })
}

fn ensure_no_sqlite_sidecars(database: &Path) -> Result<(), LedgerMaintenanceError> {
    for sidecar in sqlite_sidecars(database) {
        if sidecar.exists() {
            return Err(LedgerMaintenanceError::SidecarPresent(sidecar));
        }
    }
    Ok(())
}

fn sqlite_sidecars(database: &Path) -> [PathBuf; 2] {
    let mut wal = database.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = database.as_os_str().to_os_string();
    shm.push("-shm");
    [PathBuf::from(wal), PathBuf::from(shm)]
}

fn sha256_path(path: &Path) -> Result<[u8; 32], LedgerMaintenanceError> {
    let mut file = File::open(path).map_err(|source| LedgerMaintenanceError::Io {
        operation: "open archive for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| LedgerMaintenanceError::Io {
                operation: "read archive for hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn hex_encode(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

struct TemporaryPath {
    path: Option<PathBuf>,
}

impl TemporaryPath {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::upload_store::{UploadAuthorizationFingerprint, UploadStore};

    const DESTINATION: &str = "https://example.invalid/upload";

    struct TestEnvironment {
        root: tempfile::TempDir,
        config: PathBuf,
        capture: PathBuf,
        database: PathBuf,
    }

    impl TestEnvironment {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("autopiercam.toml");
            let capture = root.path().join("captures");
            fs::create_dir(&capture).unwrap();
            fs::write(
                &config,
                "[capture]\ndirectory = \"captures\"\n\n[upload]\nenabled = false\n",
            )
            .unwrap();
            let database = config.with_extension("upload.sqlite3");
            Self {
                root,
                config,
                capture: fs::canonicalize(capture).unwrap(),
                database,
            }
        }

        fn open_v4(&self) -> UploadStore {
            UploadStore::open(
                &self.database,
                &self.capture,
                DESTINATION,
                UploadAuthorizationFingerprint::anonymous(),
            )
            .unwrap()
        }

        fn artifact(&self, sequence: u64, contents: &[u8]) -> PathBuf {
            let path = self
                .capture
                .join(format!("frame-1700000000-123-{sequence:06}.jpg"));
            fs::write(&path, contents).unwrap();
            path
        }
    }

    fn create_v3(environment: &TestEnvironment) -> Connection {
        let connection = Connection::open(&environment.database).unwrap();
        connection.execute_batch(V3_CREATE_METADATA).unwrap();
        connection
            .execute_batch(V3_CREATE_PREACTIVATION_ARTIFACTS)
            .unwrap();
        connection.execute_batch(V3_CREATE_UPLOAD_JOBS).unwrap();
        connection.execute_batch(V3_CREATE_DUE_INDEX).unwrap();
        connection
            .execute_batch(V3_CREATE_INSERT_AGGREGATE_TRIGGER)
            .unwrap();
        connection
            .execute_batch(V3_CREATE_UPDATE_AGGREGATE_TRIGGER)
            .unwrap();
        connection
            .execute_batch(V3_CREATE_IMMUTABLE_IDENTITY_TRIGGER)
            .unwrap();
        connection
            .execute_batch(V3_CREATE_NO_DELETE_TRIGGER)
            .unwrap();
        connection
            .execute(
                r#"
                INSERT INTO upload_metadata (
                    singleton, schema_signature, capture_root, destination,
                    authorization_sha256
                ) VALUES (1, ?1, ?2, ?3, ?4)
                "#,
                params![
                    V3_SCHEMA_SIGNATURE,
                    environment.capture.to_str().unwrap(),
                    DESTINATION,
                    [7_u8; 32].as_slice()
                ],
            )
            .unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection
            .pragma_update(None, "user_version", V3_SCHEMA_VERSION)
            .unwrap();
        connection
    }

    fn permanently_fail(store: &mut UploadStore, artifact: &Path) {
        store.record_artifact(artifact, 10).unwrap();
        let claim = store.claim_due(10).unwrap().unwrap();
        store
            .mark_permanently_failed(&claim, 11, Some(422), Some("operator action"))
            .unwrap();
    }

    fn completed_ledger(environment: &TestEnvironment, sequence: u64) -> String {
        let mut store = environment.open_v4();
        let artifact = environment.artifact(sequence, b"completed artifact");
        store.record_artifact(&artifact, 10).unwrap();
        let claim = store.claim_due(10).unwrap().unwrap();
        store.mark_completed(&claim, 11, 204).unwrap();
        let ledger_id = store.ledger_id().as_hex();
        drop(store);
        ledger_id
    }

    fn test_archive_path(environment: &TestEnvironment, ledger_id: &str) -> PathBuf {
        config_path_for_output(
            &environment.config,
            &format!("upload.{ledger_id}.archive.sqlite3"),
        )
        .unwrap()
    }

    fn test_retired_path(environment: &TestEnvironment, ledger_id: &str) -> PathBuf {
        config_path_for_output(
            &environment.config,
            &format!("upload.{ledger_id}.retired.sqlite3"),
        )
        .unwrap()
    }

    fn make_backup_at(environment: &TestEnvironment, path: &Path) {
        let source = open_existing_exclusive(&environment.database).unwrap();
        checkpoint_for_archive(&source, &environment.database).unwrap();
        backup_database(&source, path).unwrap();
    }

    #[cfg(windows)]
    fn symlink_file(source: &Path, destination: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_file(source, destination)
    }

    #[cfg(unix)]
    fn symlink_file(source: &Path, destination: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(source, destination)
    }

    #[cfg(not(any(unix, windows)))]
    fn symlink_file(_source: &Path, _destination: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlink test unsupported on this platform",
        ))
    }

    #[test]
    fn lease_excludes_live_and_offline_owners_without_deleting_marker() {
        let environment = TestEnvironment::new();
        let live = LedgerLease::acquire_live(&environment.database).unwrap();
        assert!(matches!(
            LedgerLease::acquire_maintenance(&environment.database),
            Err(LedgerLeaseError::AgentActive)
        ));
        drop(live);

        let maintenance = LedgerLease::acquire_maintenance(&environment.database).unwrap();
        assert!(matches!(
            LedgerLease::acquire_live(&environment.database),
            Err(LedgerLeaseError::MaintenanceActive { .. })
        ));
        let marker = maintenance_marker_path(&environment.database);
        assert!(marker.is_file());
        drop(maintenance);
        assert!(LedgerLease::acquire_live(&environment.database).is_ok());
        assert!(marker.is_file());
    }

    #[test]
    fn exact_v3_migration_preserves_rows_and_v4_is_a_verified_noop() {
        let environment = TestEnvironment::new();
        let connection = create_v3(&environment);
        let artifact = environment.artifact(1, b"migrated artifact");
        connection
            .execute(
                r#"
                INSERT INTO upload_jobs (
                    artifact_path, filename, idempotency_key, file_size, sha256,
                    state, attempt_count, next_attempt_at_ms, last_http_status,
                    last_error, created_at_ms, updated_at_ms, completed_at_ms,
                    last_failure_at_ms
                ) VALUES (?1, ?2, 'migration-key', 17, ?3, 'completed', 1,
                          NULL, 204, NULL, 10, 11, 11, NULL)
                "#,
                params![
                    artifact.to_str().unwrap(),
                    artifact.file_name().unwrap().to_str().unwrap(),
                    [9_u8; 32].as_slice()
                ],
            )
            .unwrap();
        drop(connection);

        let report = migrate_upload_ledger(&environment.config).unwrap();
        assert!(report.migrated);
        assert_eq!(report.ledger_id.len(), 32);
        let migrated = Connection::open(&environment.database).unwrap();
        assert_eq!(schema_version(&migrated).unwrap(), SCHEMA_VERSION);
        let row: (i64, Vec<u8>, i64, i64, Option<i64>) = migrated
            .query_row(
                r#"
                SELECT id, delivery_binding_sha256, job_revision,
                       requeue_count, last_requeued_at_ms
                FROM upload_jobs
                "#,
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, 1);
        assert_eq!(row.1, delivery_binding(DESTINATION, &[7_u8; 32]));
        assert_eq!((row.2, row.3, row.4), (1, 0, None));
        drop(migrated);

        let second = migrate_upload_ledger(&environment.config).unwrap();
        assert!(!second.migrated);
        assert_eq!(second.ledger_id, report.ledger_id);
    }

    #[test]
    fn migration_fails_closed_for_other_versions_and_tampered_v3() {
        let unsupported = TestEnvironment::new();
        let connection = Connection::open(&unsupported.database).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);
        assert!(matches!(
            migrate_upload_ledger(&unsupported.config),
            Err(LedgerMaintenanceError::UnsupportedSchema { found: 2 })
        ));

        let tampered = TestEnvironment::new();
        let connection = create_v3(&tampered);
        connection
            .execute("DROP TRIGGER upload_jobs_no_delete", [])
            .unwrap();
        drop(connection);
        assert!(matches!(
            migrate_upload_ledger(&tampered.config),
            Err(LedgerMaintenanceError::InvalidV3Schema)
        ));
    }

    #[test]
    fn archive_refuses_live_permanent_and_crash_gap_work() {
        let live = TestEnvironment::new();
        let mut live_store = live.open_v4();
        live_store
            .record_artifact(&live.artifact(2, b"pending"), 10)
            .unwrap();
        let live_id = live_store.ledger_id().as_hex();
        drop(live_store);
        assert!(matches!(
            archive_upload_ledger(&live.config, &live_id),
            Err(LedgerMaintenanceError::LiveWork { count: 1 })
        ));
        assert!(live.database.is_file());

        let permanent = TestEnvironment::new();
        let mut permanent_store = permanent.open_v4();
        permanently_fail(&mut permanent_store, &permanent.artifact(3, b"permanent"));
        let permanent_id = permanent_store.ledger_id().as_hex();
        drop(permanent_store);
        assert!(matches!(
            archive_upload_ledger(&permanent.config, &permanent_id),
            Err(LedgerMaintenanceError::PermanentFailures { count: 1 })
        ));

        let crash_gap = TestEnvironment::new();
        let crash_store = crash_gap.open_v4();
        let crash_id = crash_store.ledger_id().as_hex();
        drop(crash_store);
        let gap = crash_gap.artifact(4, b"publish-record crash gap");
        assert!(matches!(
            archive_upload_ledger(&crash_gap.config, &crash_id),
            Err(LedgerMaintenanceError::CrashGap { path }) if path == gap
        ));
    }

    #[test]
    fn archive_accepts_retained_preactivation_but_rejects_unsafe_generated_entries() {
        let preactivation = TestEnvironment::new();
        let retained = preactivation.artifact(6, b"known preactivation capture");
        let store = preactivation.open_v4();
        let ledger_id = store.ledger_id().as_hex();
        drop(store);
        let report = archive_upload_ledger(&preactivation.config, &ledger_id).unwrap();
        assert!(retained.is_file());
        assert!(report.archive_path.is_file());

        let unsafe_entry = TestEnvironment::new();
        let store = unsafe_entry.open_v4();
        let ledger_id = store.ledger_id().as_hex();
        drop(store);
        let generated_directory = unsafe_entry.capture.join("frame-1700000000-123-000007.jpg");
        fs::create_dir(&generated_directory).unwrap();
        assert!(matches!(
            archive_upload_ledger(&unsafe_entry.config, &ledger_id),
            Err(LedgerMaintenanceError::UnsafePath { path, .. })
                if path == generated_directory
        ));

        let reparse = TestEnvironment::new();
        let store = reparse.open_v4();
        let ledger_id = store.ledger_id().as_hex();
        drop(store);
        let target = reparse.capture.join("unmanaged-target.jpg");
        fs::write(&target, b"target").unwrap();
        let generated_link = reparse.capture.join("frame-1700000000-123-000008.jpg");
        if symlink_file(&target, &generated_link).is_ok() {
            assert!(matches!(
                archive_upload_ledger(&reparse.config, &ledger_id),
                Err(LedgerMaintenanceError::UnsafePath { path, .. })
                    if path == generated_link
            ));
        }
    }

    #[test]
    fn maintenance_rejects_a_symlink_or_reparse_point_active_database() {
        let environment = TestEnvironment::new();
        drop(environment.open_v4());
        let real_database = environment.root.path().join("real-ledger.sqlite3");
        fs::rename(&environment.database, &real_database).unwrap();
        if symlink_file(&real_database, &environment.database).is_ok() {
            assert!(matches!(
                migrate_upload_ledger(&environment.config),
                Err(LedgerMaintenanceError::UnsafePath { path, .. })
                    if path == environment.database
            ));
        }
    }

    #[test]
    fn archive_resumes_exact_copy_and_fails_closed_on_other_partial_states() {
        let resumable = TestEnvironment::new();
        let resumable_id = completed_ledger(&resumable, 20);
        let resumable_archive = test_archive_path(&resumable, &resumable_id);
        make_backup_at(&resumable, &resumable_archive);
        let resumed = archive_upload_ledger(&resumable.config, &resumable_id).unwrap();
        assert!(!resumable.database.exists());
        assert!(resumed.retired_path.is_file());

        let changed = TestEnvironment::new();
        let changed_id = completed_ledger(&changed, 21);
        let changed_archive = test_archive_path(&changed, &changed_id);
        make_backup_at(&changed, &changed_archive);
        drop(
            UploadStore::open(
                &changed.database,
                &changed.capture,
                DESTINATION,
                UploadAuthorizationFingerprint::for_bearer_token("rotated-after-archive"),
            )
            .unwrap(),
        );
        let changed_result = archive_upload_ledger(&changed.config, &changed_id);
        assert!(
            matches!(
                changed_result,
                Err(LedgerMaintenanceError::ActiveChangedAfterArchive)
            ),
            "{changed_result:?}"
        );
        assert!(changed.database.is_file());

        let archive_only = TestEnvironment::new();
        let archive_only_id = completed_ledger(&archive_only, 23);
        let archive_only_path = test_archive_path(&archive_only, &archive_only_id);
        make_backup_at(&archive_only, &archive_only_path);
        fs::remove_file(&archive_only.database).unwrap();
        assert!(matches!(
            archive_upload_ledger(&archive_only.config, &archive_only_id),
            Err(LedgerMaintenanceError::PartialArchive(_))
        ));

        let retired_only = TestEnvironment::new();
        let retired_only_id = completed_ledger(&retired_only, 24);
        let retired_only_path = test_retired_path(&retired_only, &retired_only_id);
        fs::rename(&retired_only.database, retired_only_path).unwrap();
        assert!(matches!(
            archive_upload_ledger(&retired_only.config, &retired_only_id),
            Err(LedgerMaintenanceError::PartialArchive(_))
        ));

        let all_three = TestEnvironment::new();
        let all_three_id = completed_ledger(&all_three, 25);
        let all_three_archive = test_archive_path(&all_three, &all_three_id);
        let all_three_retired = test_retired_path(&all_three, &all_three_id);
        make_backup_at(&all_three, &all_three_archive);
        fs::copy(&all_three_archive, &all_three_retired).unwrap();
        assert!(matches!(
            archive_upload_ledger(&all_three.config, &all_three_id),
            Err(LedgerMaintenanceError::PartialArchive(_))
        ));
    }

    #[test]
    fn archive_is_verified_published_without_replace_and_active_is_retired_last() {
        let environment = TestEnvironment::new();
        let ledger_id = completed_ledger(&environment, 5);

        assert!(matches!(
            archive_upload_ledger(&environment.config, "00000000000000000000000000000000"),
            Err(LedgerMaintenanceError::LedgerIdMismatch { .. })
        ));
        assert!(environment.database.is_file());

        let report = archive_upload_ledger(&environment.config, &ledger_id).unwrap();
        assert_eq!(report.ledger_id, ledger_id);
        assert!(!environment.database.exists());
        assert!(report.archive_path.is_file());
        assert!(report.retired_path.is_file());
        assert_eq!(
            report.sha256,
            hex_encode(&sha256_path(&report.archive_path).unwrap())
        );
        let archived =
            Connection::open_with_flags(&report.archive_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        assert_eq!(read_ledger_id(&archived).unwrap().as_hex(), ledger_id);
        verify_integrity(&archived).unwrap();
        drop(archived);

        let repeated = archive_upload_ledger(&environment.config, &ledger_id).unwrap();
        assert_eq!(repeated, report);

        let _keep_tempdir_alive = &environment.root;
    }
}
