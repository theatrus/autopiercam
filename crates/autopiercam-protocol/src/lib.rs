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
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot<T> {
    pub revision: u64,
    pub config: T,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigReplace<T> {
    pub expected_revision: u64,
    pub config: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigApplied {
    pub revision: u64,
    pub applied: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionConflictDetails {
    pub expected_revision: u64,
    pub current_revision: u64,
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

        let applied = ConfigApplied {
            revision: 42,
            applied: true,
        };
        assert_eq!(
            serde_json::to_value(applied).unwrap(),
            json!({"revision": 42, "applied": true})
        );
        let conflict = RevisionConflictDetails {
            expected_revision: 40,
            current_revision: 41,
        };
        assert_eq!(conflict.current_revision, 41);
    }
}
