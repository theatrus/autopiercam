use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_WIRE_BYTES: usize = 720 * 1024;
pub const MAX_JPEG_BYTES: usize = 512 * 1024;

/// Secret-bearing types are deliberately not Serialize/Clone/Debug by default.
/// Explicit wire requests below are the only serialization boundary.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose_for_transport(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

#[derive(Serialize)]
pub struct PairingRequest<'a> {
    protocol_version: u32,
    kind: &'static str,
    installation_id: &'a str,
    pairing_token: &'a str,
}

impl<'a> PairingRequest<'a> {
    pub fn new(installation_id: &'a str, token: &'a Secret) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            kind: "pier_camera",
            installation_id,
            pairing_token: token.expose_for_transport(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingResponse {
    pub protocol_version: u32,
    pub device_id: i64,
    pub credential: Secret,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage<'a> {
    Authenticate {
        protocol_version: u32,
        installation_id: &'a str,
        credential: &'a str,
        snapshots: bool,
    },
    SnapshotUnavailable {
        request_id: &'a str,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Ready {
        protocol_version: u32,
        device_id: i64,
    },
    Error {
        code: String,
    },
    SnapshotRequest {
        request_id: String,
        expires_at: i64,
        max_jpeg_bytes: usize,
        max_frame_age_seconds: u64,
    },
    EventAck {
        event_id: String,
        status: String,
        retry_after_seconds: u64,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("invalid or unsupported Hub message")]
pub struct InvalidMessage;

impl ServerMessage {
    /// Enforce size before parsing and never forward parser errors, which may
    /// quote untrusted response text or accidentally reflected credentials.
    pub fn parse(bytes: &[u8]) -> Result<Self, InvalidMessage> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(InvalidMessage);
        }
        let message: Self = serde_json::from_slice(bytes).map_err(|_| InvalidMessage)?;
        match &message {
            Self::Ready {
                protocol_version,
                device_id,
            } if *protocol_version != PROTOCOL_VERSION || *device_id <= 0 => {
                return Err(InvalidMessage);
            }
            Self::SnapshotRequest {
                request_id,
                max_jpeg_bytes,
                max_frame_age_seconds,
                ..
            } if !valid_uuid(request_id) || *max_jpeg_bytes == 0 || *max_frame_age_seconds == 0 => {
                return Err(InvalidMessage);
            }
            Self::EventAck { event_id, .. } if !valid_uuid(event_id) => return Err(InvalidMessage),
            _ => {}
        }
        Ok(message)
    }
}

impl PairingResponse {
    pub fn parse(bytes: &[u8]) -> Result<Self, InvalidMessage> {
        if bytes.len() > 4096 {
            return Err(InvalidMessage);
        }
        let response: Self = serde_json::from_slice(bytes).map_err(|_| InvalidMessage)?;
        let credential = response.credential.expose_for_transport();
        if response.protocol_version != PROTOCOL_VERSION
            || response.device_id <= 0
            || !credential.starts_with("csdc_")
            || credential.len() <= 5
            || credential
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err(InvalidMessage);
        }
        Ok(response)
    }
}

pub fn requires_operator_action(code: &str) -> bool {
    // Unknown errors fail closed; only documented transient errors reconnect.
    !matches!(code, "rate_limited" | "already_connected")
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, ch)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                ch == b'-'
            } else {
                ch.is_ascii_hexdigit()
            }
        })
        && value.bytes().any(|ch| ch != b'0' && ch != b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "5fe70313-0a01-43b2-97a8-bab512a45ba7";

    #[test]
    fn pairing_and_authentication_match_hub_contract() {
        let secret = Secret::new("csdp_test".to_owned());
        let json = serde_json::to_value(PairingRequest::new(ID, &secret)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"protocol_version":1,"kind":"pier_camera","installation_id":ID,"pairing_token":"csdp_test"})
        );
        let response = PairingResponse::parse(
            br#"{"protocol_version":1,"device_id":42,"credential":"csdc_test"}"#,
        )
        .unwrap();
        assert!(!format!("{response:?}").contains("csdc_test"));
        let auth = ClientMessage::Authenticate {
            protocol_version: PROTOCOL_VERSION,
            installation_id: ID,
            credential: response.credential.expose_for_transport(),
            snapshots: false,
        };
        assert_eq!(
            serde_json::to_value(auth).unwrap(),
            serde_json::json!({"type":"authenticate","protocol_version":1,"installation_id":ID,"credential":"csdc_test","snapshots":false})
        );
    }

    #[test]
    fn decodes_snapshot_and_ack_fixtures() {
        let request = serde_json::json!({"type":"snapshot_request","request_id":ID,"expires_at":1790355690_i64,"max_jpeg_bytes":524288,"max_frame_age_seconds":120});
        assert!(matches!(
            ServerMessage::parse(request.to_string().as_bytes()).unwrap(),
            ServerMessage::SnapshotRequest {
                max_jpeg_bytes: MAX_JPEG_BYTES,
                ..
            }
        ));
        let ack = serde_json::json!({"type":"event_ack","event_id":ID,"status":"retry","retry_after_seconds":60});
        assert!(matches!(
            ServerMessage::parse(ack.to_string().as_bytes()).unwrap(),
            ServerMessage::EventAck {
                retry_after_seconds: 60,
                ..
            }
        ));
    }

    #[test]
    fn rejects_unknown_commands_fields_versions_and_oversized_input() {
        for message in [
            r#"{"type":"capture_resume"}"#,
            r#"{"type":"ready","protocol_version":2,"device_id":42}"#,
            r#"{"type":"ready","protocol_version":1,"device_id":42,"channel_id":"1"}"#,
            r#"{"type":"snapshot_request","request_id":"not-uuid","expires_at":1,"max_jpeg_bytes":1,"max_frame_age_seconds":120}"#,
            r#"{"type":"ready","protocol_version":1,"device_id":0}"#,
        ] {
            assert!(ServerMessage::parse(message.as_bytes()).is_err());
        }
        assert!(ServerMessage::parse(&vec![b' '; MAX_WIRE_BYTES + 1]).is_err());
        assert!(
            PairingResponse::parse(
                br#"{"protocol_version":1,"device_id":42,"credential":"csdc_secret","extra":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn permanent_or_unknown_auth_errors_do_not_retry() {
        for code in [
            "authentication_failed",
            "unsupported_version",
            "invalid_message",
            "future_error",
        ] {
            assert!(requires_operator_action(code));
        }
        assert!(!requires_operator_action("rate_limited"));
        assert!(!requires_operator_action("already_connected"));
    }
}
