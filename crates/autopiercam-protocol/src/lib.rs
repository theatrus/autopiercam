//! Versioned control protocol shared by the AutoPierCam agent and local clients.
//!
//! Each message is UTF-8 JSON prefixed by its byte length as an unsigned
//! little-endian 32-bit integer. A clean EOF before any prefix byte is distinct
//! from a truncated prefix or payload.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::io::{self, Read, Write};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const PIPE_NAME: &str = "autopiercam-control-v1";
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const PREVIEW_PROTOCOL_VERSION: u16 = 1;
pub const PREVIEW_PIPE_NAME: &str = "autopiercam-preview-v1";
pub const MAX_PREVIEW_METADATA_SIZE: usize = 4 * 1024;
pub const MAX_PREVIEW_JPEG_SIZE: usize = 4 * 1024 * 1024;
pub const PREVIEW_MAX_DIMENSION: u32 = 1_280;
pub const PREVIEW_MAX_PIXELS: u64 = 1_638_400;

pub const METHOD_UPLOADS_LIST: &str = "uploads.list";
pub const METHOD_UPLOADS_REQUEUE: &str = "uploads.requeue";
pub const CAPABILITY_UPLOADS_LIST: &str = METHOD_UPLOADS_LIST;
pub const CAPABILITY_UPLOADS_REQUEUE: &str = METHOD_UPLOADS_REQUEUE;

pub const UPLOAD_LIST_DEFAULT_PAGE_SIZE: u16 = 50;
pub const UPLOAD_LIST_MAX_PAGE_SIZE: u16 = 100;
pub const UPLOAD_LIST_MAX_STATE_FILTERS: usize = 5;
pub const UPLOAD_CURSOR_MAX_BYTES: usize = 512;
pub const UPLOAD_LEDGER_ID_MAX_BYTES: usize = 128;
pub const UPLOAD_FILENAME_MAX_BYTES: usize = 255;
pub const UPLOAD_ERROR_MAX_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Method {
    #[serde(rename = "status.get")]
    StatusGet,
    #[serde(rename = "cameras.list")]
    CamerasList,
    #[serde(rename = "config.get")]
    ConfigGet,
    #[serde(rename = "config.replace")]
    ConfigReplace,
    #[serde(rename = "capture.pause")]
    CapturePause,
    #[serde(rename = "capture.resume")]
    CaptureResume,
    #[serde(rename = "capture.now")]
    CaptureNow,
    #[serde(rename = "artifacts.list")]
    ArtifactsList,
    #[serde(rename = "uploads.list")]
    UploadsList,
    #[serde(rename = "uploads.requeue")]
    UploadsRequeue,
    #[serde(rename = "agent.shutdown")]
    AgentShutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub request_id: String,
    pub method: Method,
    #[serde(default = "empty_object")]
    pub payload: Value,
}

impl Request {
    pub fn new(request_id: impl Into<String>, method: Method) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            method,
            payload: empty_object(),
        }
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_envelope(self.version, &self.request_id)
    }
}

impl ProtocolMessage for Request {
    fn validate_message(&self) -> Result<(), ValidationError> {
        self.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Response {
    pub version: u16,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn success(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(request_id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            result: None,
            error: Some(error),
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_envelope(self.version, &self.request_id)?;
        match (&self.result, &self.error) {
            (Some(_), None) => Ok(()),
            (None, Some(error)) => error.validate(),
            _ => Err(ValidationError::InvalidResponseInvariant),
        }
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ResponseWire::deserialize(deserializer)?;
        Ok(Self {
            version: wire.version,
            request_id: wire.request_id,
            result: wire.result.into_option(),
            error: wire.error.into_option(),
        })
    }
}

impl ProtocolMessage for Response {
    fn validate_message(&self) -> Result<(), ValidationError> {
        self.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.code.trim().is_empty() {
            return Err(ValidationError::EmptyErrorCode);
        }
        if self.message.trim().is_empty() {
            return Err(ValidationError::EmptyErrorMessage);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Starting,
    Idle,
    Capturing,
    Paused,
    Faulted,
    Stopping,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StatusCamera {
    pub id: i32,
    pub name: String,
}

/// Durable outbox telemetry for the optional HTTP upload worker.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusUpload {
    /// Finalized artifacts waiting for their first upload attempt.
    pub pending: u64,
    /// Upload requests currently in progress.
    pub active: u64,
    /// Upload intents waiting for their next retry attempt.
    pub retrying: u64,
    /// Successful uploads retained in the durable ledger.
    pub completed: u64,
    /// Upload intents retained after a permanent failure.
    pub permanently_failed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoragePressure {
    #[default]
    Ok,
    CleanupNeeded,
    Blocked,
}

/// Latest bounded retention sweep and capture-volume telemetry.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusStorage {
    /// Bytes held by exact AutoPierCam capture names, including protected data.
    pub managed_bytes: u64,
    /// Managed bytes that retention is currently forbidden to delete.
    pub protected_bytes: u64,
    /// Managed bytes currently eligible for automatic retention.
    pub reclaimable_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sweep_unix_ms: Option<u64>,
    pub last_reclaimed_files: u64,
    pub last_reclaimed_bytes: u64,
    pub pressure: StoragePressure,
    /// Scheduled still persistence pauses when protected data alone prevents a
    /// configured byte limit from being met. Capture-now remains available.
    pub capture_suspended: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<StatusCamera>,
    pub frames_captured: u64,
    pub frames_saved: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<StatusUpload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StatusStorage>,
    /// Optional operations supported by this agent build. Older v1 agents omit
    /// this field, and older clients can safely ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

impl AgentStatus {
    pub fn new(state: AgentState) -> Self {
        Self {
            state,
            camera: None,
            frames_captured: 0,
            frames_saved: 0,
            last_artifact: None,
            last_error: None,
            upload: None,
            storage: None,
            capabilities: Vec::new(),
        }
    }
}

/// Persisted state of a durable upload job.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadJobState {
    Pending,
    InProgress,
    Retrying,
    Completed,
    PermanentlyFailed,
}

/// Filters and pagination for `uploads.list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadListRequest {
    /// Empty means all states. Duplicate filters are invalid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<UploadJobState>,
    #[serde(default = "default_upload_list_page_size")]
    pub page_size: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl Default for UploadListRequest {
    fn default() -> Self {
        Self {
            states: Vec::new(),
            page_size: UPLOAD_LIST_DEFAULT_PAGE_SIZE,
            cursor: None,
        }
    }
}

impl UploadListRequest {
    pub fn validate(&self) -> Result<(), UploadAdminValidationError> {
        if !(1..=UPLOAD_LIST_MAX_PAGE_SIZE).contains(&self.page_size) {
            return Err(UploadAdminValidationError::InvalidPageSize {
                found: self.page_size,
                max: UPLOAD_LIST_MAX_PAGE_SIZE,
            });
        }
        if self.states.len() > UPLOAD_LIST_MAX_STATE_FILTERS {
            return Err(UploadAdminValidationError::TooManyStateFilters {
                found: self.states.len(),
                max: UPLOAD_LIST_MAX_STATE_FILTERS,
            });
        }
        for (index, state) in self.states.iter().enumerate() {
            if self.states[..index].contains(state) {
                return Err(UploadAdminValidationError::DuplicateStateFilter(*state));
            }
        }
        if let Some(cursor) = &self.cursor {
            validate_opaque_upload_token(
                cursor,
                "cursor",
                UPLOAD_CURSOR_MAX_BYTES,
                UploadAdminValidationError::InvalidCursor,
            )?;
        }
        Ok(())
    }
}

/// One bounded, path-free job projection returned to local operator clients.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadJobSummary {
    pub job_id: u64,
    pub job_revision: u64,
    pub filename: String,
    pub state: UploadJobState,
    pub file_size_bytes: u64,
    pub attempt_count: u64,
    pub requeue_count: u64,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_requeued_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub requeue_eligible: bool,
}

impl UploadJobSummary {
    pub fn validate(&self) -> Result<(), UploadAdminValidationError> {
        if self.job_id == 0 {
            return Err(UploadAdminValidationError::ZeroJobId);
        }
        if self.job_revision == 0 {
            return Err(UploadAdminValidationError::ZeroJobRevision);
        }
        validate_upload_filename(&self.filename)?;
        if self.updated_at_unix_ms < self.created_at_unix_ms {
            return Err(UploadAdminValidationError::TimestampBeforeCreation(
                "updated_at_unix_ms",
            ));
        }
        validate_upload_history_timestamp(
            "completed_at_unix_ms",
            self.completed_at_unix_ms,
            self.created_at_unix_ms,
            self.updated_at_unix_ms,
        )?;
        validate_upload_history_timestamp(
            "last_failure_at_unix_ms",
            self.last_failure_at_unix_ms,
            self.created_at_unix_ms,
            self.updated_at_unix_ms,
        )?;
        validate_upload_history_timestamp(
            "last_requeued_at_unix_ms",
            self.last_requeued_at_unix_ms,
            self.created_at_unix_ms,
            self.updated_at_unix_ms,
        )?;
        if self
            .next_attempt_at_unix_ms
            .is_some_and(|next| next < self.updated_at_unix_ms)
        {
            return Err(UploadAdminValidationError::TimestampBeforeUpdate(
                "next_attempt_at_unix_ms",
            ));
        }
        if (self.state == UploadJobState::Retrying) != self.next_attempt_at_unix_ms.is_some() {
            return Err(UploadAdminValidationError::InvalidNextAttemptForState);
        }
        if (self.state == UploadJobState::Completed) != self.completed_at_unix_ms.is_some() {
            return Err(UploadAdminValidationError::InvalidCompletionForState);
        }
        if (self.requeue_count > 0) != self.last_requeued_at_unix_ms.is_some() {
            return Err(UploadAdminValidationError::InvalidRequeueHistory);
        }
        if self.requeue_eligible && self.state != UploadJobState::PermanentlyFailed {
            return Err(UploadAdminValidationError::InvalidRequeueEligibility);
        }
        if let Some(status) = self.last_http_status
            && !(100..=599).contains(&status)
        {
            return Err(UploadAdminValidationError::InvalidHttpStatus(status));
        }
        if let Some(error) = &self.last_error {
            validate_bounded_upload_text(
                error,
                "last_error",
                UPLOAD_ERROR_MAX_BYTES,
                UploadAdminValidationError::InvalidLastError,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadListResponse {
    pub ledger_id: String,
    pub jobs: Vec<UploadJobSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl UploadListResponse {
    pub fn validate(&self) -> Result<(), UploadAdminValidationError> {
        validate_upload_ledger_id(&self.ledger_id)?;
        if self.jobs.len() > usize::from(UPLOAD_LIST_MAX_PAGE_SIZE) {
            return Err(UploadAdminValidationError::TooManyJobs {
                found: self.jobs.len(),
                max: usize::from(UPLOAD_LIST_MAX_PAGE_SIZE),
            });
        }
        for (index, job) in self.jobs.iter().enumerate() {
            job.validate()?;
            if self.jobs[..index]
                .iter()
                .any(|prior| prior.job_id == job.job_id)
            {
                return Err(UploadAdminValidationError::DuplicateJobId(job.job_id));
            }
        }
        if let Some(cursor) = &self.next_cursor {
            validate_opaque_upload_token(
                cursor,
                "next_cursor",
                UPLOAD_CURSOR_MAX_BYTES,
                UploadAdminValidationError::InvalidCursor,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRequeueRequest {
    pub ledger_id: String,
    pub job_id: u64,
    pub expected_job_revision: u64,
}

impl UploadRequeueRequest {
    pub fn validate(&self) -> Result<(), UploadAdminValidationError> {
        validate_upload_ledger_id(&self.ledger_id)?;
        if self.job_id == 0 {
            return Err(UploadAdminValidationError::ZeroJobId);
        }
        if self.expected_job_revision == 0 {
            return Err(UploadAdminValidationError::ZeroJobRevision);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRequeueResult {
    pub job: UploadJobSummary,
    pub worker_notified: bool,
}

impl UploadRequeueResult {
    pub fn validate(&self) -> Result<(), UploadAdminValidationError> {
        self.job.validate()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum UploadAdminValidationError {
    #[error("page_size {found} is outside 1..={max}")]
    InvalidPageSize { found: u16, max: u16 },
    #[error("state filter count {found} exceeds the limit of {max}")]
    TooManyStateFilters { found: usize, max: usize },
    #[error("state filter {0:?} appears more than once")]
    DuplicateStateFilter(UploadJobState),
    #[error("invalid upload cursor: {0}")]
    InvalidCursor(String),
    #[error("invalid upload ledger_id: {0}")]
    InvalidLedgerId(String),
    #[error("job_id must be greater than zero")]
    ZeroJobId,
    #[error("job revision must be greater than zero")]
    ZeroJobRevision,
    #[error("invalid upload filename: {0}")]
    InvalidFilename(String),
    #[error("{0} must not be earlier than created_at_unix_ms")]
    TimestampBeforeCreation(&'static str),
    #[error("{0} must not be earlier than updated_at_unix_ms")]
    TimestampBeforeUpdate(&'static str),
    #[error("{0} must not be later than updated_at_unix_ms")]
    TimestampAfterUpdate(&'static str),
    #[error("next_attempt_at_unix_ms must be present exactly for retrying jobs")]
    InvalidNextAttemptForState,
    #[error("completed_at_unix_ms must be present exactly for completed jobs")]
    InvalidCompletionForState,
    #[error("last_requeued_at_unix_ms must be present exactly when requeue_count is nonzero")]
    InvalidRequeueHistory,
    #[error("only permanently_failed jobs can be requeue eligible")]
    InvalidRequeueEligibility,
    #[error("last_http_status {0} is outside 100..=599")]
    InvalidHttpStatus(u16),
    #[error("invalid last_error: {0}")]
    InvalidLastError(String),
    #[error("upload response contains {found} jobs; maximum is {max}")]
    TooManyJobs { found: usize, max: usize },
    #[error("upload response repeats job_id {0}")]
    DuplicateJobId(u64),
}

pub fn validate_upload_ledger_id(ledger_id: &str) -> Result<(), UploadAdminValidationError> {
    validate_opaque_upload_token(
        ledger_id,
        "ledger_id",
        UPLOAD_LEDGER_ID_MAX_BYTES,
        UploadAdminValidationError::InvalidLedgerId,
    )
}

fn default_upload_list_page_size() -> u16 {
    UPLOAD_LIST_DEFAULT_PAGE_SIZE
}

fn validate_upload_filename(filename: &str) -> Result<(), UploadAdminValidationError> {
    validate_bounded_upload_text(
        filename,
        "filename",
        UPLOAD_FILENAME_MAX_BYTES,
        UploadAdminValidationError::InvalidFilename,
    )?;
    if filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains(':')
    {
        return Err(UploadAdminValidationError::InvalidFilename(
            "must be a base filename, not a path".to_owned(),
        ));
    }
    Ok(())
}

fn validate_opaque_upload_token<F>(
    token: &str,
    field: &str,
    max_bytes: usize,
    error: F,
) -> Result<(), UploadAdminValidationError>
where
    F: FnOnce(String) -> UploadAdminValidationError,
{
    validate_bounded_upload_text(token, field, max_bytes, error)
}

fn validate_bounded_upload_text<F>(
    value: &str,
    field: &str,
    max_bytes: usize,
    error: F,
) -> Result<(), UploadAdminValidationError>
where
    F: FnOnce(String) -> UploadAdminValidationError,
{
    let reason = if value.is_empty() {
        Some("must not be empty".to_owned())
    } else if value.len() > max_bytes {
        Some(format!("is {} bytes; maximum is {max_bytes}", value.len()))
    } else if value.chars().any(char::is_control) {
        Some("must not contain control characters".to_owned())
    } else {
        None
    };
    match reason {
        Some(reason) => Err(error(format!("{field} {reason}"))),
        None => Ok(()),
    }
}

fn validate_upload_history_timestamp(
    field: &'static str,
    value: Option<u64>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
) -> Result<(), UploadAdminValidationError> {
    if let Some(value) = value {
        if value < created_at_unix_ms {
            return Err(UploadAdminValidationError::TimestampBeforeCreation(field));
        }
        if value > updated_at_unix_ms {
            return Err(UploadAdminValidationError::TimestampAfterUpdate(field));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot<T> {
    pub revision: u64,
    pub config: T,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReplace<T> {
    pub expected_revision: u64,
    pub config: T,
}

/// A wire value whose serialized object shape must include every field emitted
/// by `T::default()`, including optional fields represented by JSON null.
/// This lets full-replacement APIs stay strict while their storage model may
/// still use serde defaults for hand-written TOML files.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Complete<T>(pub T);

impl<T> Complete<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<'de, T> Deserialize<'de> for Complete<T>
where
    T: Default + Serialize + DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let actual = Value::deserialize(deserializer)?;
        let template = serde_json::to_value(T::default()).map_err(serde::de::Error::custom)?;
        require_complete_shape(&actual, &template, "config").map_err(serde::de::Error::custom)?;
        serde_json::from_value(actual)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

fn require_complete_shape(actual: &Value, template: &Value, path: &str) -> Result<(), String> {
    let Some(expected_fields) = template.as_object() else {
        return Ok(());
    };
    let Some(actual_fields) = actual.as_object() else {
        return Err(format!("field {path} must be an object"));
    };

    for (name, expected_value) in expected_fields {
        let child_path = format!("{path}.{name}");
        let actual_value = actual_fields
            .get(name)
            .ok_or_else(|| format!("missing required field {child_path}"))?;
        require_complete_shape(actual_value, expected_value, &child_path)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigSaved {
    pub revision: u64,
    pub saved: bool,
    pub restart_scheduled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionConflictDetails {
    pub expected_revision: u64,
    pub current_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreviewContentType {
    #[serde(rename = "image/jpeg")]
    Jpeg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewMode {
    Unknown,
    Day,
    Night,
}

/// Metadata attached to one encoded preview image.
///
/// Exposure and gain are required wire fields whose values may be null when
/// the camera did not provide reliable telemetry for that frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreviewMetadata {
    pub version: u16,
    pub session_generation: u64,
    pub sequence: u64,
    pub captured_at_unix_ms: u64,
    pub width: u32,
    pub height: u32,
    pub exposure_us: Option<i64>,
    pub gain: Option<i64>,
    pub content_type: PreviewContentType,
    pub mode: PreviewMode,
    pub dropped_frames: u64,
}

impl PreviewMetadata {
    pub fn validate(&self) -> Result<(), PreviewValidationError> {
        if self.version != PREVIEW_PROTOCOL_VERSION {
            return Err(PreviewValidationError::UnsupportedVersion {
                found: self.version,
                expected: PREVIEW_PROTOCOL_VERSION,
            });
        }
        if self.width == 0 || self.height == 0 {
            return Err(PreviewValidationError::ZeroDimensions);
        }
        if self.width > PREVIEW_MAX_DIMENSION || self.height > PREVIEW_MAX_DIMENSION {
            return Err(PreviewValidationError::DimensionsTooLarge {
                width: self.width,
                height: self.height,
                max: PREVIEW_MAX_DIMENSION,
            });
        }
        let pixels = u64::from(self.width) * u64::from(self.height);
        if pixels > PREVIEW_MAX_PIXELS {
            return Err(PreviewValidationError::PixelCountTooLarge {
                pixels,
                max: PREVIEW_MAX_PIXELS,
            });
        }
        if self.exposure_us.is_some_and(|exposure| exposure <= 0) {
            return Err(PreviewValidationError::InvalidExposure);
        }
        if self.gain.is_some_and(|gain| gain < 0) {
            return Err(PreviewValidationError::InvalidGain);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for PreviewMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreviewMetadataWire::deserialize(deserializer)?;
        let exposure_us = match wire.exposure_us {
            Present::Value(value) => value,
            Present::Missing => return Err(serde::de::Error::missing_field("exposure_us")),
        };
        let gain = match wire.gain {
            Present::Value(value) => value,
            Present::Missing => return Err(serde::de::Error::missing_field("gain")),
        };
        Ok(Self {
            version: wire.version,
            session_generation: wire.session_generation,
            sequence: wire.sequence,
            captured_at_unix_ms: wire.captured_at_unix_ms,
            width: wire.width,
            height: wire.height,
            exposure_us,
            gain,
            content_type: wire.content_type,
            mode: wire.mode,
            dropped_frames: wire.dropped_frames,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewMetadataWire {
    version: u16,
    session_generation: u64,
    sequence: u64,
    captured_at_unix_ms: u64,
    width: u32,
    height: u32,
    #[serde(default)]
    exposure_us: Present<Option<i64>>,
    #[serde(default)]
    gain: Present<Option<i64>>,
    content_type: PreviewContentType,
    mode: PreviewMode,
    dropped_frames: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewFrame {
    pub metadata: PreviewMetadata,
    pub jpeg: Vec<u8>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PreviewValidationError {
    #[error("unsupported preview protocol version {found}; expected {expected}")]
    UnsupportedVersion { found: u16, expected: u16 },
    #[error("preview dimensions must both be greater than zero")]
    ZeroDimensions,
    #[error("preview dimensions {width}x{height} exceed the {max}-pixel edge limit")]
    DimensionsTooLarge { width: u32, height: u32, max: u32 },
    #[error("preview contains {pixels} pixels; maximum is {max}")]
    PixelCountTooLarge { pixels: u64, max: u64 },
    #[error("preview exposure must be positive when present")]
    InvalidExposure,
    #[error("preview gain must not be negative when present")]
    InvalidGain,
}

#[derive(Debug, Error)]
pub enum PreviewFrameError {
    #[error("I/O error while transferring a preview frame: {0}")]
    Io(#[source] io::Error),
    #[error("preview metadata length prefix ended after {received} of 4 bytes")]
    TruncatedMetadataLength { received: usize },
    #[error("preview JPEG length prefix ended after {received} of 4 bytes")]
    TruncatedJpegLength { received: usize },
    #[error("preview metadata length must not be zero")]
    ZeroMetadataLength,
    #[error("preview JPEG length must not be zero")]
    ZeroJpegLength,
    #[error("preview metadata size {size} exceeds the {max} byte limit")]
    MetadataTooLarge { size: usize, max: usize },
    #[error("preview JPEG size {size} exceeds the {max} byte limit")]
    JpegTooLarge { size: usize, max: usize },
    #[error("preview metadata ended after {received} of {expected} bytes")]
    TruncatedMetadata { expected: usize, received: usize },
    #[error("preview JPEG ended after {received} of {expected} bytes")]
    TruncatedJpeg { expected: usize, received: usize },
    #[error("preview metadata is not valid UTF-8 JSON: {0}")]
    MetadataJson(#[source] serde_json::Error),
    #[error("invalid preview metadata: {0}")]
    MetadataValidation(#[from] PreviewValidationError),
    #[error("preview payload does not have JPEG start and end markers")]
    InvalidJpegMarkers,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ValidationError {
    #[error("unsupported protocol version {found}; expected {expected}")]
    UnsupportedVersion { found: u16, expected: u16 },
    #[error("request_id must not be empty")]
    EmptyRequestId,
    #[error("a response must contain exactly one of result or error")]
    InvalidResponseInvariant,
    #[error("error code must not be empty")]
    EmptyErrorCode,
    #[error("error message must not be empty")]
    EmptyErrorMessage,
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("I/O error while transferring a protocol frame: {0}")]
    Io(#[source] io::Error),
    #[error("frame length prefix ended after {received} of 4 bytes")]
    TruncatedLength { received: usize },
    #[error("frame payload ended after {received} of {expected} bytes")]
    TruncatedPayload { expected: usize, received: usize },
    #[error("frame size {size} exceeds the {max} byte limit")]
    FrameTooLarge { size: usize, max: usize },
    #[error("frame payload is not valid UTF-8 JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("invalid protocol message: {0}")]
    Validation(#[from] ValidationError),
}

pub trait ProtocolMessage: Serialize + DeserializeOwned {
    fn validate_message(&self) -> Result<(), ValidationError>;
}

/// Writes one validated, length-prefixed JSON message.
pub fn write_frame<W, M>(writer: &mut W, message: &M) -> Result<(), FrameError>
where
    W: Write,
    M: ProtocolMessage,
{
    message.validate_message()?;
    let encoded = serde_json::to_vec(message).map_err(FrameError::Json)?;
    if encoded.len() > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: encoded.len(),
            max: MAX_FRAME_SIZE,
        });
    }
    let length = u32::try_from(encoded.len()).expect("1 MiB always fits in u32");
    writer
        .write_all(&length.to_le_bytes())
        .map_err(FrameError::Io)?;
    writer.write_all(&encoded).map_err(FrameError::Io)
}

/// Reads one validated message. `Ok(None)` means EOF before any prefix byte.
/// EOF after any prefix or payload byte is returned as a truncation error.
pub fn read_frame<R, M>(reader: &mut R) -> Result<Option<M>, FrameError>
where
    R: Read,
    M: ProtocolMessage,
{
    let Some(prefix) = read_length_prefix(reader)? else {
        return Ok(None);
    };
    let length = u32::from_le_bytes(prefix) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: length,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut payload = vec![0_u8; length];
    read_payload(reader, &mut payload)?;
    let message = serde_json::from_slice::<M>(&payload).map_err(FrameError::Json)?;
    message.validate_message()?;
    Ok(Some(message))
}

pub fn write_request<W: Write>(writer: &mut W, request: &Request) -> Result<(), FrameError> {
    write_frame(writer, request)
}

pub fn read_request<R: Read>(reader: &mut R) -> Result<Option<Request>, FrameError> {
    read_frame(reader)
}

pub fn write_response<W: Write>(writer: &mut W, response: &Response) -> Result<(), FrameError> {
    write_frame(writer, response)
}

pub fn read_response<R: Read>(reader: &mut R) -> Result<Option<Response>, FrameError> {
    read_frame(reader)
}

/// Writes one preview frame as metadata length, metadata JSON, JPEG length,
/// and JPEG bytes. Both lengths are unsigned little-endian 32-bit integers.
pub fn write_preview_frame<W: Write>(
    writer: &mut W,
    metadata: &PreviewMetadata,
    jpeg: &[u8],
) -> Result<(), PreviewFrameError> {
    metadata.validate()?;
    let encoded_metadata = serde_json::to_vec(metadata).map_err(PreviewFrameError::MetadataJson)?;
    if encoded_metadata.is_empty() {
        return Err(PreviewFrameError::ZeroMetadataLength);
    }
    if encoded_metadata.len() > MAX_PREVIEW_METADATA_SIZE {
        return Err(PreviewFrameError::MetadataTooLarge {
            size: encoded_metadata.len(),
            max: MAX_PREVIEW_METADATA_SIZE,
        });
    }
    validate_preview_jpeg(jpeg)?;

    let metadata_length =
        u32::try_from(encoded_metadata.len()).expect("the preview metadata limit fits in u32");
    let jpeg_length = u32::try_from(jpeg.len()).expect("the preview JPEG limit fits in u32");
    writer
        .write_all(&metadata_length.to_le_bytes())
        .map_err(PreviewFrameError::Io)?;
    writer
        .write_all(&encoded_metadata)
        .map_err(PreviewFrameError::Io)?;
    writer
        .write_all(&jpeg_length.to_le_bytes())
        .map_err(PreviewFrameError::Io)?;
    writer.write_all(jpeg).map_err(PreviewFrameError::Io)
}

/// Reads one preview frame. `Ok(None)` means EOF before any metadata-prefix
/// byte; EOF anywhere else is a truncation error.
pub fn read_preview_frame<R: Read>(
    reader: &mut R,
) -> Result<Option<PreviewFrame>, PreviewFrameError> {
    let Some(metadata_prefix) = read_preview_length_prefix(reader, PreviewPart::Metadata)? else {
        return Ok(None);
    };
    let metadata_length = u32::from_le_bytes(metadata_prefix) as usize;
    if metadata_length == 0 {
        return Err(PreviewFrameError::ZeroMetadataLength);
    }
    if metadata_length > MAX_PREVIEW_METADATA_SIZE {
        return Err(PreviewFrameError::MetadataTooLarge {
            size: metadata_length,
            max: MAX_PREVIEW_METADATA_SIZE,
        });
    }
    let mut encoded_metadata = vec![0_u8; metadata_length];
    read_preview_payload(reader, &mut encoded_metadata, PreviewPart::Metadata)?;
    let metadata = serde_json::from_slice::<PreviewMetadata>(&encoded_metadata)
        .map_err(PreviewFrameError::MetadataJson)?;
    metadata.validate()?;

    let jpeg_prefix = read_preview_length_prefix(reader, PreviewPart::Jpeg)?
        .expect("JPEG length EOF is always reported as truncation");
    let jpeg_length = u32::from_le_bytes(jpeg_prefix) as usize;
    if jpeg_length == 0 {
        return Err(PreviewFrameError::ZeroJpegLength);
    }
    if jpeg_length > MAX_PREVIEW_JPEG_SIZE {
        return Err(PreviewFrameError::JpegTooLarge {
            size: jpeg_length,
            max: MAX_PREVIEW_JPEG_SIZE,
        });
    }
    let mut jpeg = vec![0_u8; jpeg_length];
    read_preview_payload(reader, &mut jpeg, PreviewPart::Jpeg)?;
    validate_preview_jpeg(&jpeg)?;
    Ok(Some(PreviewFrame { metadata, jpeg }))
}

pub fn validate_preview_jpeg(jpeg: &[u8]) -> Result<(), PreviewFrameError> {
    if jpeg.is_empty() {
        return Err(PreviewFrameError::ZeroJpegLength);
    }
    if jpeg.len() > MAX_PREVIEW_JPEG_SIZE {
        return Err(PreviewFrameError::JpegTooLarge {
            size: jpeg.len(),
            max: MAX_PREVIEW_JPEG_SIZE,
        });
    }
    if !jpeg.starts_with(&[0xff, 0xd8]) || !jpeg.ends_with(&[0xff, 0xd9]) {
        return Err(PreviewFrameError::InvalidJpegMarkers);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PreviewPart {
    Metadata,
    Jpeg,
}

fn read_preview_length_prefix<R: Read>(
    reader: &mut R,
    part: PreviewPart,
) -> Result<Option<[u8; 4]>, PreviewFrameError> {
    let mut prefix = [0_u8; 4];
    let mut received = 0;
    while received < prefix.len() {
        match reader.read(&mut prefix[received..]) {
            Ok(0) if received == 0 && matches!(part, PreviewPart::Metadata) => return Ok(None),
            Ok(0) => {
                return Err(match part {
                    PreviewPart::Metadata => {
                        PreviewFrameError::TruncatedMetadataLength { received }
                    }
                    PreviewPart::Jpeg => PreviewFrameError::TruncatedJpegLength { received },
                });
            }
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(PreviewFrameError::Io(error)),
        }
    }
    Ok(Some(prefix))
}

fn read_preview_payload<R: Read>(
    reader: &mut R,
    payload: &mut [u8],
    part: PreviewPart,
) -> Result<(), PreviewFrameError> {
    let mut received = 0;
    while received < payload.len() {
        match reader.read(&mut payload[received..]) {
            Ok(0) => {
                return Err(match part {
                    PreviewPart::Metadata => PreviewFrameError::TruncatedMetadata {
                        expected: payload.len(),
                        received,
                    },
                    PreviewPart::Jpeg => PreviewFrameError::TruncatedJpeg {
                        expected: payload.len(),
                        received,
                    },
                });
            }
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(PreviewFrameError::Io(error)),
        }
    }
    Ok(())
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn validate_envelope(version: u16, request_id: &str) -> Result<(), ValidationError> {
    if version != PROTOCOL_VERSION {
        return Err(ValidationError::UnsupportedVersion {
            found: version,
            expected: PROTOCOL_VERSION,
        });
    }
    if request_id.trim().is_empty() {
        return Err(ValidationError::EmptyRequestId);
    }
    Ok(())
}

fn read_length_prefix<R: Read>(reader: &mut R) -> Result<Option<[u8; 4]>, FrameError> {
    let mut prefix = [0_u8; 4];
    let mut received = 0;
    while received < prefix.len() {
        match reader.read(&mut prefix[received..]) {
            Ok(0) if received == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::TruncatedLength { received }),
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(Some(prefix))
}

fn read_payload<R: Read>(reader: &mut R, payload: &mut [u8]) -> Result<(), FrameError> {
    let mut received = 0;
    while received < payload.len() {
        match reader.read(&mut payload[received..]) {
            Ok(0) => {
                return Err(FrameError::TruncatedPayload {
                    expected: payload.len(),
                    received,
                });
            }
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ResponseWire {
    version: u16,
    request_id: String,
    #[serde(default)]
    result: Present<Value>,
    #[serde(default)]
    error: Present<ErrorBody>,
}

#[derive(Default)]
enum Present<T> {
    #[default]
    Missing,
    Value(T),
}

impl<T> Present<T> {
    fn into_option(self) -> Option<T> {
        match self {
            Self::Missing => None,
            Self::Value(value) => Some(value),
        }
    }
}

impl<'de, T> Deserialize<'de> for Present<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Value)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::StatusGet => "status.get",
            Self::CamerasList => "cameras.list",
            Self::ConfigGet => "config.get",
            Self::ConfigReplace => "config.replace",
            Self::CapturePause => "capture.pause",
            Self::CaptureResume => "capture.resume",
            Self::CaptureNow => "capture.now",
            Self::ArtifactsList => "artifacts.list",
            Self::UploadsList => METHOD_UPLOADS_LIST,
            Self::UploadsRequeue => METHOD_UPLOADS_REQUEUE,
            Self::AgentShutdown => "agent.shutdown",
        };
        formatter.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn preview_metadata() -> PreviewMetadata {
        PreviewMetadata {
            version: PREVIEW_PROTOCOL_VERSION,
            session_generation: 2,
            sequence: 42,
            captured_at_unix_ms: 1_725_000_000_123,
            width: 1_280,
            height: 960,
            exposure_us: Some(12_500),
            gain: Some(120),
            content_type: PreviewContentType::Jpeg,
            mode: PreviewMode::Night,
            dropped_frames: 3,
        }
    }

    fn preview_wire(metadata: &[u8], jpeg: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        wire.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        wire.extend_from_slice(metadata);
        wire.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        wire.extend_from_slice(jpeg);
        wire
    }

    fn upload_job_summary() -> UploadJobSummary {
        UploadJobSummary {
            job_id: 42,
            job_revision: 7,
            filename: "autopiercam-20260828T010203.004Z-s1-n2.jpg".to_owned(),
            state: UploadJobState::PermanentlyFailed,
            file_size_bytes: 1_048_576,
            attempt_count: 3,
            requeue_count: 1,
            created_at_unix_ms: 100,
            updated_at_unix_ms: 500,
            next_attempt_at_unix_ms: None,
            completed_at_unix_ms: None,
            last_failure_at_unix_ms: Some(400),
            last_requeued_at_unix_ms: Some(300),
            last_http_status: Some(413),
            last_error: Some("payload rejected".to_owned()),
            requeue_eligible: true,
        }
    }

    #[test]
    fn request_golden_json_uses_v1_method_names_and_default_payload() {
        let request = Request::new("request-1", Method::StatusGet);
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"version":1,"request_id":"request-1","method":"status.get","payload":{}}"#
        );

        let without_payload = r#"{"version":1,"request_id":"request-2","method":"capture.now"}"#;
        let decoded: Request = serde_json::from_str(without_payload).unwrap();
        assert_eq!(decoded.payload, json!({}));
        assert_eq!(decoded.method, Method::CaptureNow);
    }

    #[test]
    fn upload_admin_methods_have_stable_v1_wire_and_capability_names() {
        assert_eq!(METHOD_UPLOADS_LIST, "uploads.list");
        assert_eq!(METHOD_UPLOADS_REQUEUE, "uploads.requeue");
        assert_eq!(CAPABILITY_UPLOADS_LIST, METHOD_UPLOADS_LIST);
        assert_eq!(CAPABILITY_UPLOADS_REQUEUE, METHOD_UPLOADS_REQUEUE);
        assert_eq!(Method::UploadsList.to_string(), METHOD_UPLOADS_LIST);
        assert_eq!(Method::UploadsRequeue.to_string(), METHOD_UPLOADS_REQUEUE);

        assert_eq!(
            serde_json::to_string(&Request::new("list-1", Method::UploadsList)).unwrap(),
            r#"{"version":1,"request_id":"list-1","method":"uploads.list","payload":{}}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::new("requeue-1", Method::UploadsRequeue)).unwrap(),
            r#"{"version":1,"request_id":"requeue-1","method":"uploads.requeue","payload":{}}"#
        );
    }

    #[test]
    fn upload_job_states_have_exact_snake_case_wire_names() {
        let states = [
            UploadJobState::Pending,
            UploadJobState::InProgress,
            UploadJobState::Retrying,
            UploadJobState::Completed,
            UploadJobState::PermanentlyFailed,
        ];
        assert_eq!(
            serde_json::to_string(&states).unwrap(),
            r#"["pending","in_progress","retrying","completed","permanently_failed"]"#
        );
        assert!(serde_json::from_str::<UploadJobState>(r#""permanent_failed""#).is_err());
        assert!(serde_json::from_str::<UploadJobState>(r#""in-progress""#).is_err());
    }

    #[test]
    fn upload_list_request_has_bounded_defaults_and_strict_golden_shape() {
        let default: UploadListRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(default, UploadListRequest::default());
        assert_eq!(default.page_size, UPLOAD_LIST_DEFAULT_PAGE_SIZE);
        default.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&default).unwrap(),
            r#"{"page_size":50}"#
        );

        let request = UploadListRequest {
            states: vec![UploadJobState::Retrying, UploadJobState::PermanentlyFailed],
            page_size: 25,
            cursor: Some("opaque-page-2".to_owned()),
        };
        request.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"states":["retrying","permanently_failed"],"page_size":25,"cursor":"opaque-page-2"}"#
        );

        assert!(
            serde_json::from_str::<UploadListRequest>(r#"{"page_size":50,"unexpected":true}"#)
                .is_err()
        );
    }

    #[test]
    fn upload_list_request_validation_covers_page_filter_and_cursor_boundaries() {
        for page_size in [1, UPLOAD_LIST_MAX_PAGE_SIZE] {
            UploadListRequest {
                page_size,
                ..UploadListRequest::default()
            }
            .validate()
            .unwrap();
        }
        for page_size in [0, UPLOAD_LIST_MAX_PAGE_SIZE + 1] {
            assert!(matches!(
                UploadListRequest {
                    page_size,
                    ..UploadListRequest::default()
                }
                .validate(),
                Err(UploadAdminValidationError::InvalidPageSize { .. })
            ));
        }

        let every_state = vec![
            UploadJobState::Pending,
            UploadJobState::InProgress,
            UploadJobState::Retrying,
            UploadJobState::Completed,
            UploadJobState::PermanentlyFailed,
        ];
        UploadListRequest {
            states: every_state.clone(),
            ..UploadListRequest::default()
        }
        .validate()
        .unwrap();
        let mut duplicated = every_state;
        duplicated.push(UploadJobState::Pending);
        assert!(matches!(
            UploadListRequest {
                states: duplicated,
                ..UploadListRequest::default()
            }
            .validate(),
            Err(UploadAdminValidationError::TooManyStateFilters { .. })
        ));
        assert!(matches!(
            UploadListRequest {
                states: vec![UploadJobState::Pending, UploadJobState::Pending],
                ..UploadListRequest::default()
            }
            .validate(),
            Err(UploadAdminValidationError::DuplicateStateFilter(
                UploadJobState::Pending
            ))
        ));

        UploadListRequest {
            cursor: Some("x".repeat(UPLOAD_CURSOR_MAX_BYTES)),
            ..UploadListRequest::default()
        }
        .validate()
        .unwrap();
        for cursor in [
            String::new(),
            "x".repeat(UPLOAD_CURSOR_MAX_BYTES + 1),
            "opaque\nvalue".to_owned(),
        ] {
            assert!(matches!(
                UploadListRequest {
                    cursor: Some(cursor),
                    ..UploadListRequest::default()
                }
                .validate(),
                Err(UploadAdminValidationError::InvalidCursor(_))
            ));
        }
    }

    #[test]
    fn upload_job_summary_has_a_path_free_bounded_golden_shape() {
        let job = upload_job_summary();
        job.validate().unwrap();
        let encoded = serde_json::to_string(&job).unwrap();
        assert_eq!(
            encoded,
            r#"{"job_id":42,"job_revision":7,"filename":"autopiercam-20260828T010203.004Z-s1-n2.jpg","state":"permanently_failed","file_size_bytes":1048576,"attempt_count":3,"requeue_count":1,"created_at_unix_ms":100,"updated_at_unix_ms":500,"last_failure_at_unix_ms":400,"last_requeued_at_unix_ms":300,"last_http_status":413,"last_error":"payload rejected","requeue_eligible":true}"#
        );
        for forbidden in [
            "artifact_path",
            "capture_root",
            "destination",
            "authorization",
            "idempotency",
        ] {
            assert!(!encoded.contains(forbidden));
        }
        assert!(
            serde_json::from_str::<UploadJobSummary>(&encoded.replace(
                "\"requeue_eligible\":true",
                "\"requeue_eligible\":true,\"artifact_path\":\"secret\""
            ))
            .is_err()
        );
    }

    #[test]
    fn upload_job_summary_validation_covers_identity_and_text_boundaries() {
        let mut job = upload_job_summary();
        job.filename = format!("{}.jpg", "a".repeat(UPLOAD_FILENAME_MAX_BYTES - 4));
        job.last_error = Some("e".repeat(UPLOAD_ERROR_MAX_BYTES));
        job.validate().unwrap();

        for filename in [
            String::new(),
            format!("{}.jpg", "a".repeat(UPLOAD_FILENAME_MAX_BYTES - 3)),
            "subdir/frame.jpg".to_owned(),
            "C:frame.jpg".to_owned(),
            "frame\n.jpg".to_owned(),
        ] {
            let mut invalid = upload_job_summary();
            invalid.filename = filename;
            assert!(matches!(
                invalid.validate(),
                Err(UploadAdminValidationError::InvalidFilename(_))
            ));
        }

        job = upload_job_summary();
        job.last_error = Some("e".repeat(UPLOAD_ERROR_MAX_BYTES + 1));
        assert!(matches!(
            job.validate(),
            Err(UploadAdminValidationError::InvalidLastError(_))
        ));
        for (job_id, revision) in [(0, 1), (1, 0)] {
            job = upload_job_summary();
            job.job_id = job_id;
            job.job_revision = revision;
            assert!(job.validate().is_err());
        }
        for status in [99, 600] {
            job = upload_job_summary();
            job.last_http_status = Some(status);
            assert!(matches!(
                job.validate(),
                Err(UploadAdminValidationError::InvalidHttpStatus(found)) if found == status
            ));
        }
        for status in [100, 599] {
            job = upload_job_summary();
            job.last_http_status = Some(status);
            job.validate().unwrap();
        }
    }

    #[test]
    fn upload_job_summary_validation_enforces_state_and_timestamp_invariants() {
        let mut job = upload_job_summary();
        job.updated_at_unix_ms = job.created_at_unix_ms - 1;
        assert!(matches!(
            job.validate(),
            Err(UploadAdminValidationError::TimestampBeforeCreation(
                "updated_at_unix_ms"
            ))
        ));

        job = upload_job_summary();
        job.last_failure_at_unix_ms = Some(job.created_at_unix_ms - 1);
        assert!(job.validate().is_err());
        job = upload_job_summary();
        job.last_requeued_at_unix_ms = Some(job.updated_at_unix_ms + 1);
        assert!(matches!(
            job.validate(),
            Err(UploadAdminValidationError::TimestampAfterUpdate(
                "last_requeued_at_unix_ms"
            ))
        ));

        job = upload_job_summary();
        job.state = UploadJobState::Retrying;
        assert_eq!(
            job.validate(),
            Err(UploadAdminValidationError::InvalidNextAttemptForState)
        );
        job.next_attempt_at_unix_ms = Some(job.updated_at_unix_ms + 1);
        job.requeue_eligible = false;
        job.validate().unwrap();

        job = upload_job_summary();
        job.state = UploadJobState::Completed;
        job.requeue_eligible = false;
        job.completed_at_unix_ms = Some(job.updated_at_unix_ms);
        job.validate().unwrap();
        job.completed_at_unix_ms = None;
        assert_eq!(
            job.validate(),
            Err(UploadAdminValidationError::InvalidCompletionForState)
        );

        job = upload_job_summary();
        job.requeue_count = 0;
        assert_eq!(
            job.validate(),
            Err(UploadAdminValidationError::InvalidRequeueHistory)
        );
        job = upload_job_summary();
        job.state = UploadJobState::Pending;
        assert_eq!(
            job.validate(),
            Err(UploadAdminValidationError::InvalidRequeueEligibility)
        );
    }

    #[test]
    fn upload_list_and_requeue_messages_have_strict_golden_contracts() {
        let job = upload_job_summary();
        let response = UploadListResponse {
            ledger_id: "ledger-0123456789abcdef".to_owned(),
            jobs: vec![job.clone()],
            next_cursor: Some("opaque-page-2".to_owned()),
        };
        response.validate().unwrap();
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["ledger_id"], "ledger-0123456789abcdef");
        assert_eq!(encoded["jobs"][0]["job_revision"], 7);
        assert_eq!(encoded["next_cursor"], "opaque-page-2");

        let request = UploadRequeueRequest {
            ledger_id: response.ledger_id,
            job_id: job.job_id,
            expected_job_revision: job.job_revision,
        };
        request.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"ledger_id":"ledger-0123456789abcdef","job_id":42,"expected_job_revision":7}"#
        );
        assert!(
            serde_json::from_str::<UploadRequeueRequest>(
                r#"{"ledger_id":"ledger","job_id":1,"expected_job_revision":1,"force":true}"#
            )
            .is_err()
        );

        let result = UploadRequeueResult {
            job,
            worker_notified: true,
        };
        result.validate().unwrap();
        assert!(serde_json::to_value(result).unwrap()["worker_notified"] == true);
    }

    #[test]
    fn upload_list_response_and_requeue_validation_are_bounded() {
        validate_upload_ledger_id(&"l".repeat(UPLOAD_LEDGER_ID_MAX_BYTES)).unwrap();
        for ledger_id in [
            String::new(),
            "l".repeat(UPLOAD_LEDGER_ID_MAX_BYTES + 1),
            "ledger\u{0}".to_owned(),
        ] {
            assert!(matches!(
                validate_upload_ledger_id(&ledger_id),
                Err(UploadAdminValidationError::InvalidLedgerId(_))
            ));
        }

        let mut jobs = Vec::new();
        for job_id in 1..=u64::from(UPLOAD_LIST_MAX_PAGE_SIZE) {
            let mut job = upload_job_summary();
            job.job_id = job_id;
            jobs.push(job);
        }
        UploadListResponse {
            ledger_id: "ledger".to_owned(),
            jobs: jobs.clone(),
            next_cursor: None,
        }
        .validate()
        .unwrap();
        let mut extra = upload_job_summary();
        extra.job_id = u64::from(UPLOAD_LIST_MAX_PAGE_SIZE) + 1;
        jobs.push(extra);
        assert!(matches!(
            UploadListResponse {
                ledger_id: "ledger".to_owned(),
                jobs,
                next_cursor: None,
            }
            .validate(),
            Err(UploadAdminValidationError::TooManyJobs { .. })
        ));

        let duplicate = upload_job_summary();
        assert!(matches!(
            UploadListResponse {
                ledger_id: "ledger".to_owned(),
                jobs: vec![duplicate.clone(), duplicate],
                next_cursor: None,
            }
            .validate(),
            Err(UploadAdminValidationError::DuplicateJobId(42))
        ));
        for (ledger_id, job_id, revision) in [("", 1, 1), ("ledger", 0, 1), ("ledger", 1, 0)] {
            assert!(
                UploadRequeueRequest {
                    ledger_id: ledger_id.to_owned(),
                    job_id,
                    expected_job_revision: revision,
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn framed_request_and_response_roundtrip_and_end_at_clean_eof() {
        let request = Request::new("roundtrip-1", Method::ConfigReplace)
            .with_payload(json!({"revision": 7, "camera": {"bin": 1}}));
        let mut request_bytes = Vec::new();
        write_request(&mut request_bytes, &request).unwrap();
        let mut request_cursor = Cursor::new(request_bytes);
        assert_eq!(read_request(&mut request_cursor).unwrap(), Some(request));
        assert_eq!(read_request(&mut request_cursor).unwrap(), None);

        let response = Response::success("roundtrip-1", json!({"revision": 8}));
        let mut response_bytes = Vec::new();
        write_response(&mut response_bytes, &response).unwrap();
        let mut response_cursor = Cursor::new(response_bytes);
        assert_eq!(read_response(&mut response_cursor).unwrap(), Some(response));
        assert_eq!(read_response(&mut response_cursor).unwrap(), None);
    }

    #[test]
    fn present_null_result_remains_a_valid_success_response() {
        let response: Response =
            serde_json::from_str(r#"{"version":1,"request_id":"null-result","result":null}"#)
                .unwrap();
        assert_eq!(response.result, Some(Value::Null));
        assert!(response.validate().is_ok());
    }

    #[test]
    fn oversized_frames_are_rejected_before_transfer_or_allocation() {
        let request = Request::new("oversized", Method::ConfigReplace)
            .with_payload(Value::String("x".repeat(MAX_FRAME_SIZE)));
        let error = write_request(&mut Vec::new(), &request).unwrap_err();
        assert!(matches!(error, FrameError::FrameTooLarge { .. }));

        let oversized_prefix = ((MAX_FRAME_SIZE + 1) as u32).to_le_bytes();
        let error = read_request(&mut Cursor::new(oversized_prefix)).unwrap_err();
        assert!(matches!(
            error,
            FrameError::FrameTooLarge {
                size,
                max: MAX_FRAME_SIZE
            } if size == MAX_FRAME_SIZE + 1
        ));
    }

    #[test]
    fn clean_eof_is_distinct_from_truncated_prefix_and_payload() {
        assert_eq!(
            read_request(&mut Cursor::new(Vec::<u8>::new())).unwrap(),
            None
        );

        let error = read_request(&mut Cursor::new(vec![1, 0])).unwrap_err();
        assert!(matches!(error, FrameError::TruncatedLength { received: 2 }));

        let mut truncated_payload = 3_u32.to_le_bytes().to_vec();
        truncated_payload.push(b'{');
        let error = read_request(&mut Cursor::new(truncated_payload)).unwrap_err();
        assert!(matches!(
            error,
            FrameError::TruncatedPayload {
                expected: 3,
                received: 1
            }
        ));
    }

    #[test]
    fn invalid_version_and_response_invariants_are_rejected() {
        let mut request = Request::new("bad-version", Method::StatusGet);
        request.version = 2;
        assert!(matches!(
            request.validate(),
            Err(ValidationError::UnsupportedVersion {
                found: 2,
                expected: PROTOCOL_VERSION
            })
        ));
        assert!(matches!(
            write_request(&mut Vec::new(), &request),
            Err(FrameError::Validation(
                ValidationError::UnsupportedVersion { .. }
            ))
        ));

        let missing_both = Response {
            version: PROTOCOL_VERSION,
            request_id: "missing".to_owned(),
            result: None,
            error: None,
        };
        assert_eq!(
            missing_both.validate(),
            Err(ValidationError::InvalidResponseInvariant)
        );

        let both = Response {
            version: PROTOCOL_VERSION,
            request_id: "both".to_owned(),
            result: Some(json!({})),
            error: Some(ErrorBody::new("conflict", "both fields were populated")),
        };
        assert_eq!(
            both.validate(),
            Err(ValidationError::InvalidResponseInvariant)
        );
    }

    #[test]
    fn typed_agent_status_serializes_as_a_response_result() {
        let mut status = AgentStatus::new(AgentState::Capturing);
        status.camera = Some(StatusCamera {
            id: 0,
            name: "ZWO ASI676MC".to_owned(),
        });
        status.frames_captured = 42;
        status.frames_saved = 3;
        status.last_artifact = Some("captures/frame-000003.jpg".to_owned());

        let response = Response::success("status-1", serde_json::to_value(&status).unwrap());
        response.validate().unwrap();
        let encoded = serde_json::to_value(response).unwrap();
        assert_eq!(encoded["result"]["state"], "capturing");
        assert_eq!(encoded["result"]["camera"]["id"], 0);
        assert_eq!(encoded["result"]["frames_captured"], 42);
        assert!(encoded["result"].get("last_error").is_none());
        assert!(encoded["result"].get("upload").is_none());
        assert!(encoded["result"].get("storage").is_none());
        assert!(encoded["result"].get("capabilities").is_none());

        let legacy: AgentStatus = serde_json::from_value(json!({
            "state": "capturing",
            "frames_captured": 42,
            "frames_saved": 3
        }))
        .unwrap();
        assert!(legacy.upload.is_none());
        assert!(legacy.storage.is_none());
        assert!(legacy.capabilities.is_empty());
    }

    #[test]
    fn typed_storage_status_is_strict_and_omits_unavailable_details() {
        let storage = StatusStorage {
            managed_bytes: 1_000,
            protected_bytes: 700,
            reclaimable_bytes: 300,
            free_bytes: Some(20_000),
            last_sweep_unix_ms: Some(1_725_000_000_123),
            last_reclaimed_files: 2,
            last_reclaimed_bytes: 250,
            pressure: StoragePressure::Blocked,
            capture_suspended: true,
            last_error: None,
        };
        assert_eq!(
            serde_json::to_value(&storage).unwrap(),
            json!({
                "managed_bytes": 1_000,
                "protected_bytes": 700,
                "reclaimable_bytes": 300,
                "free_bytes": 20_000,
                "last_sweep_unix_ms": 1_725_000_000_123_u64,
                "last_reclaimed_files": 2,
                "last_reclaimed_bytes": 250,
                "pressure": "blocked",
                "capture_suspended": true
            })
        );
        let encoded = serde_json::to_string(&storage).unwrap();
        assert!(
            serde_json::from_str::<StatusStorage>(&encoded.replace(
                "\"capture_suspended\":true",
                "\"capture_suspended\":true,\"unexpected\":1"
            ))
            .is_err()
        );
    }

    #[test]
    fn agent_status_capabilities_are_optional_and_have_a_stable_shape() {
        let mut status = AgentStatus::new(AgentState::Idle);
        status.capabilities = vec![
            CAPABILITY_UPLOADS_LIST.to_owned(),
            CAPABILITY_UPLOADS_REQUEUE.to_owned(),
        ];
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"state":"idle","frames_captured":0,"frames_saved":0,"capabilities":["uploads.list","uploads.requeue"]}"#
        );

        let old_v1: AgentStatus =
            serde_json::from_str(r#"{"state":"idle","frames_captured":0,"frames_saved":0}"#)
                .unwrap();
        assert!(old_v1.capabilities.is_empty());
        assert!(
            serde_json::to_value(old_v1)
                .unwrap()
                .get("capabilities")
                .is_none()
        );
    }

    #[test]
    fn typed_upload_status_is_strict_and_omits_unavailable_details() {
        let upload = StatusUpload {
            pending: 2,
            active: 1,
            retrying: 1,
            completed: 7,
            permanently_failed: 3,
            last_success_unix_ms: Some(1_725_000_000_123),
            last_failure_unix_ms: None,
            last_error: Some("HTTP 413".to_owned()),
        };
        let encoded = serde_json::to_value(&upload).unwrap();
        assert_eq!(
            encoded,
            json!({
                "pending": 2,
                "active": 1,
                "retrying": 1,
                "completed": 7,
                "permanently_failed": 3,
                "last_success_unix_ms": 1_725_000_000_123_u64,
                "last_error": "HTTP 413"
            })
        );
        assert_eq!(
            serde_json::from_value::<StatusUpload>(encoded).unwrap(),
            upload
        );

        let minimal = json!({
            "pending": 0,
            "active": 0,
            "retrying": 0,
            "completed": 0,
            "permanently_failed": 0
        });
        assert_eq!(
            serde_json::from_value::<StatusUpload>(minimal.clone()).unwrap(),
            StatusUpload::default()
        );

        let mut missing_required = minimal.clone();
        missing_required.as_object_mut().unwrap().remove("pending");
        assert!(serde_json::from_value::<StatusUpload>(missing_required).is_err());

        let mut unknown = minimal;
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<StatusUpload>(unknown).is_err());

        let mut status = AgentStatus::new(AgentState::Capturing);
        status.upload = Some(upload);
        let response = Response::success("status-upload", serde_json::to_value(status).unwrap());
        assert_eq!(response.result.unwrap()["upload"]["active"], 1);
    }

    #[test]
    fn typed_configuration_messages_preserve_revision_contract() {
        let replacement = ConfigReplace {
            expected_revision: 41,
            config: json!({"camera": {"bin": 1}}),
        };
        let payload = serde_json::to_value(&replacement).unwrap();
        assert_eq!(payload["expected_revision"], 41);
        assert_eq!(payload["config"]["camera"]["bin"], 1);

        let saved = ConfigSaved {
            revision: 42,
            saved: true,
            restart_scheduled: true,
        };
        assert_eq!(
            serde_json::to_value(saved).unwrap(),
            json!({"revision": 42, "saved": true, "restart_scheduled": true})
        );
        let conflict = RevisionConflictDetails {
            expected_revision: 40,
            current_revision: 41,
        };
        assert_eq!(conflict.current_revision, 41);

        let unknown = serde_json::from_value::<ConfigReplace<Value>>(json!({
            "expected_revision": 41,
            "config": {},
            "unexpected": true
        }));
        assert!(unknown.is_err());
    }

    #[test]
    fn complete_wire_values_require_nested_and_optional_fields() {
        #[derive(Debug, Default, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct WireConfig {
            required: u32,
            optional: Option<String>,
            nested: WireNested,
        }

        #[derive(Debug, Default, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct WireNested {
            leaf: bool,
        }

        let complete = json!({
            "expected_revision": 7,
            "config": {
                "required": 1,
                "optional": null,
                "nested": { "leaf": true }
            }
        });
        let replacement =
            serde_json::from_value::<ConfigReplace<Complete<WireConfig>>>(complete).unwrap();
        assert_eq!(replacement.config.into_inner().required, 1);

        let missing_optional = json!({
            "expected_revision": 7,
            "config": { "required": 1, "nested": { "leaf": true } }
        });
        assert!(
            serde_json::from_value::<ConfigReplace<Complete<WireConfig>>>(missing_optional)
                .unwrap_err()
                .to_string()
                .contains("config.optional")
        );

        let missing_leaf = json!({
            "expected_revision": 7,
            "config": { "required": 1, "optional": null, "nested": {} }
        });
        assert!(
            serde_json::from_value::<ConfigReplace<Complete<WireConfig>>>(missing_leaf)
                .unwrap_err()
                .to_string()
                .contains("config.nested.leaf")
        );
    }

    #[test]
    fn preview_metadata_has_a_strict_golden_v1_shape() {
        let metadata = preview_metadata();
        assert_eq!(
            serde_json::to_string(&metadata).unwrap(),
            "{\"version\":1,\"session_generation\":2,\"sequence\":42,\"captured_at_unix_ms\":1725000000123,\"width\":1280,\"height\":960,\"exposure_us\":12500,\"gain\":120,\"content_type\":\"image/jpeg\",\"mode\":\"night\",\"dropped_frames\":3}"
        );
        metadata.validate().unwrap();

        let nullable = serde_json::to_value(&metadata).unwrap();
        let mut nullable = nullable.as_object().unwrap().clone();
        nullable.insert("exposure_us".to_owned(), Value::Null);
        nullable.insert("gain".to_owned(), Value::Null);
        let decoded: PreviewMetadata =
            serde_json::from_value(Value::Object(nullable.clone())).unwrap();
        assert_eq!(decoded.exposure_us, None);
        assert_eq!(decoded.gain, None);

        nullable.remove("exposure_us");
        assert!(serde_json::from_value::<PreviewMetadata>(Value::Object(nullable)).is_err());

        let mut unknown = serde_json::to_value(metadata).unwrap();
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<PreviewMetadata>(unknown).is_err());
    }

    #[test]
    fn preview_frame_roundtrips_with_little_endian_lengths() {
        let metadata = preview_metadata();
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0xff, 0xd9];
        let encoded_metadata = serde_json::to_vec(&metadata).unwrap();
        let mut wire = Vec::new();
        write_preview_frame(&mut wire, &metadata, &jpeg).unwrap();

        assert_eq!(&wire[..4], &(encoded_metadata.len() as u32).to_le_bytes());
        let jpeg_prefix = 4 + encoded_metadata.len();
        assert_eq!(
            &wire[jpeg_prefix..jpeg_prefix + 4],
            &(jpeg.len() as u32).to_le_bytes()
        );

        let mut cursor = Cursor::new(wire);
        assert_eq!(
            read_preview_frame(&mut cursor).unwrap(),
            Some(PreviewFrame {
                metadata,
                jpeg: jpeg.to_vec(),
            })
        );
        assert_eq!(read_preview_frame(&mut cursor).unwrap(), None);
    }

    #[test]
    fn preview_metadata_validation_rejects_unsafe_values() {
        let mut metadata = preview_metadata();
        metadata.version += 1;
        assert!(matches!(
            metadata.validate(),
            Err(PreviewValidationError::UnsupportedVersion { .. })
        ));

        metadata = preview_metadata();
        metadata.width = 0;
        assert_eq!(
            metadata.validate(),
            Err(PreviewValidationError::ZeroDimensions)
        );

        metadata = preview_metadata();
        metadata.width = PREVIEW_MAX_DIMENSION + 1;
        assert!(matches!(
            metadata.validate(),
            Err(PreviewValidationError::DimensionsTooLarge { .. })
        ));

        metadata = preview_metadata();
        metadata.exposure_us = Some(0);
        assert_eq!(
            metadata.validate(),
            Err(PreviewValidationError::InvalidExposure)
        );

        metadata = preview_metadata();
        metadata.gain = Some(-1);
        assert_eq!(
            metadata.validate(),
            Err(PreviewValidationError::InvalidGain)
        );
    }

    #[test]
    fn preview_lengths_are_bounded_before_payload_allocation() {
        let error = read_preview_frame(&mut Cursor::new(0_u32.to_le_bytes())).unwrap_err();
        assert!(matches!(error, PreviewFrameError::ZeroMetadataLength));

        let oversized_metadata = ((MAX_PREVIEW_METADATA_SIZE + 1) as u32).to_le_bytes();
        let error = read_preview_frame(&mut Cursor::new(oversized_metadata)).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::MetadataTooLarge {
                size,
                max: MAX_PREVIEW_METADATA_SIZE
            } if size == MAX_PREVIEW_METADATA_SIZE + 1
        ));

        let encoded_metadata = serde_json::to_vec(&preview_metadata()).unwrap();
        let mut zero_jpeg = Vec::new();
        zero_jpeg.extend_from_slice(&(encoded_metadata.len() as u32).to_le_bytes());
        zero_jpeg.extend_from_slice(&encoded_metadata);
        zero_jpeg.extend_from_slice(&0_u32.to_le_bytes());
        let error = read_preview_frame(&mut Cursor::new(zero_jpeg)).unwrap_err();
        assert!(matches!(error, PreviewFrameError::ZeroJpegLength));

        let mut oversized_jpeg = Vec::new();
        oversized_jpeg.extend_from_slice(&(encoded_metadata.len() as u32).to_le_bytes());
        oversized_jpeg.extend_from_slice(&encoded_metadata);
        oversized_jpeg.extend_from_slice(&((MAX_PREVIEW_JPEG_SIZE + 1) as u32).to_le_bytes());
        let error = read_preview_frame(&mut Cursor::new(oversized_jpeg)).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::JpegTooLarge {
                size,
                max: MAX_PREVIEW_JPEG_SIZE
            } if size == MAX_PREVIEW_JPEG_SIZE + 1
        ));

        let mut destination = Vec::new();
        let error = write_preview_frame(&mut destination, &preview_metadata(), &[]).unwrap_err();
        assert!(matches!(error, PreviewFrameError::ZeroJpegLength));
        assert!(destination.is_empty());

        let oversized_jpeg = vec![0_u8; MAX_PREVIEW_JPEG_SIZE + 1];
        let error = write_preview_frame(&mut destination, &preview_metadata(), &oversized_jpeg)
            .unwrap_err();
        assert!(matches!(error, PreviewFrameError::JpegTooLarge { .. }));
        assert!(destination.is_empty());
    }

    #[test]
    fn preview_reader_distinguishes_truncation_and_invalid_payloads() {
        assert_eq!(
            read_preview_frame(&mut Cursor::new(Vec::<u8>::new())).unwrap(),
            None
        );

        let error = read_preview_frame(&mut Cursor::new(vec![1, 0])).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::TruncatedMetadataLength { received: 2 }
        ));

        let mut truncated_metadata = 3_u32.to_le_bytes().to_vec();
        truncated_metadata.push(b'{');
        let error = read_preview_frame(&mut Cursor::new(truncated_metadata)).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::TruncatedMetadata {
                expected: 3,
                received: 1
            }
        ));

        let malformed_json = preview_wire(b"{", &[0xff, 0xd8, 0xff, 0xd9]);
        let error = read_preview_frame(&mut Cursor::new(malformed_json)).unwrap_err();
        assert!(matches!(error, PreviewFrameError::MetadataJson(_)));

        let encoded_metadata = serde_json::to_vec(&preview_metadata()).unwrap();
        let mut invalid_metadata = serde_json::to_value(preview_metadata()).unwrap();
        invalid_metadata["version"] = json!(PREVIEW_PROTOCOL_VERSION + 1);
        let invalid_metadata = serde_json::to_vec(&invalid_metadata).unwrap();
        let invalid_metadata = preview_wire(&invalid_metadata, &[0xff, 0xd8, 0xff, 0xd9]);
        let error = read_preview_frame(&mut Cursor::new(invalid_metadata)).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::MetadataValidation(
                PreviewValidationError::UnsupportedVersion { .. }
            )
        ));

        let mut truncated_jpeg_prefix = Vec::new();
        truncated_jpeg_prefix.extend_from_slice(&(encoded_metadata.len() as u32).to_le_bytes());
        truncated_jpeg_prefix.extend_from_slice(&encoded_metadata);
        truncated_jpeg_prefix.extend_from_slice(&[4, 0]);
        let error = read_preview_frame(&mut Cursor::new(truncated_jpeg_prefix)).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::TruncatedJpegLength { received: 2 }
        ));

        let truncated_jpeg = preview_wire(&encoded_metadata, &[0xff, 0xd8]);
        let mut truncated_jpeg = truncated_jpeg[..truncated_jpeg.len() - 1].to_vec();
        // Restore the declared JPEG size after slicing a payload byte.
        let jpeg_prefix = 4 + encoded_metadata.len();
        truncated_jpeg[jpeg_prefix..jpeg_prefix + 4].copy_from_slice(&2_u32.to_le_bytes());
        let error = read_preview_frame(&mut Cursor::new(truncated_jpeg)).unwrap_err();
        assert!(matches!(
            error,
            PreviewFrameError::TruncatedJpeg {
                expected: 2,
                received: 1
            }
        ));

        let invalid_jpeg = preview_wire(&encoded_metadata, b"not-a-jpeg");
        let error = read_preview_frame(&mut Cursor::new(invalid_jpeg)).unwrap_err();
        assert!(matches!(error, PreviewFrameError::InvalidJpegMarkers));
        assert!(matches!(
            write_preview_frame(&mut Vec::new(), &preview_metadata(), b"not-a-jpeg"),
            Err(PreviewFrameError::InvalidJpegMarkers)
        ));
    }
}
