use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROTOCOL_VERSION: u32 = 1;
pub const TRIGGER_PROTOCOL_VERSION: u32 = 2;
pub const CONTEXT_PROTOCOL_VERSION: u32 = 3;
pub const MAX_WIRE_BYTES: usize = 720 * 1024;
pub const MAX_JPEG_BYTES: usize = 512 * 1024;
/// Independent client disk/network safety ceiling, not a Hub protocol limit.
pub const TIMELAPSE_SAFETY_CEILING: u64 = 512 * 1024 * 1024;

pub fn timelapse_limit(advertised: u64) -> u64 {
    advertised.min(TIMELAPSE_SAFETY_CEILING)
}

/// An immutable observation label, never telescope/camera control or a durable
/// "current target" state. Names are the only free-form information admitted.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TelescopeContext {
    pub telescope_id: i64,
    pub telescope_name: String,
    #[serde(default)]
    pub target_name: Option<String>,
    pub occurred_at: String,
}

impl TelescopeContext {
    fn valid(&self) -> bool {
        fn name(value: &str, limit: usize) -> bool {
            !value.trim().is_empty()
                && value.chars().count() <= limit
                && !value.chars().any(char::is_control)
        }
        self.telescope_id > 0
            && name(&self.telescope_name, 80)
            && self.target_name.as_deref().is_none_or(|v| name(v, 120))
            && self.occurred_at_milliseconds().is_some()
    }

    /// Only UTC RFC3339 is admitted. Build validated time components without
    /// enabling a new parser/dependency in the capture agent.
    fn occurred_at_milliseconds(&self) -> Option<i64> {
        let value = self
            .occurred_at
            .strip_suffix('Z')
            .or_else(|| self.occurred_at.strip_suffix("+00:00"))?;
        if !value.is_ascii() || !(19..=29).contains(&value.len()) {
            return None;
        }
        let (date, clock) = value.split_once('T')?;
        if date.len() != 10
            || &date[4..5] != "-"
            || &date[7..8] != "-"
            || clock.len() < 8
            || &clock[2..3] != ":"
            || &clock[5..6] != ":"
        {
            return None;
        }
        let digits = |v: &str| v.bytes().all(|b| b.is_ascii_digit());
        if ![
            &date[..4],
            &date[5..7],
            &date[8..],
            &clock[..2],
            &clock[3..5],
            &clock[6..8],
        ]
        .into_iter()
        .all(digits)
        {
            return None;
        }
        let nanos = if clock.len() > 8 {
            let fraction = clock.strip_prefix(&clock[..8])?.strip_prefix('.')?;
            if fraction.is_empty() || fraction.len() > 9 || !digits(fraction) {
                return None;
            }
            fraction.parse::<u32>().ok()? * 10_u32.pow(9 - fraction.len() as u32)
        } else {
            0
        };
        let date = time::Date::from_calendar_date(
            date[..4].parse().ok()?,
            time::Month::try_from(date[5..7].parse::<u8>().ok()?).ok()?,
            date[8..].parse().ok()?,
        )
        .ok()?;
        let clock = time::Time::from_hms_nano(
            clock[..2].parse().ok()?,
            clock[3..5].parse().ok()?,
            clock[6..8].parse().ok()?,
            nanos,
        )
        .ok()?;
        (time::PrimitiveDateTime::new(date, clock)
            .assume_utc()
            .unix_timestamp_nanos()
            / 1_000_000)
            .try_into()
            .ok()
    }

    pub(crate) fn fresh_at(&self, now_ms: i64, expires_at: i64) -> bool {
        self.occurred_at_milliseconds().is_some_and(|occurred| {
            (0..=30_000).contains(&now_ms.saturating_sub(occurred))
                && expires_at > now_ms / 1000
                && expires_at <= (occurred / 1000).saturating_add(30)
        })
    }

    pub(crate) fn caption(&self, summary: &str) -> String {
        let suffix = match &self.target_name {
            Some(target) => format!(" · {} · Target: {target}", self.telescope_name),
            None => format!(" · {}", self.telescope_name),
        };
        format!("{summary}{suffix}").chars().take(300).collect()
    }
}

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
    MediaReady {
        max_timelapse_bytes: u64,
    },
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
    ConfigureTriggers {
        request_id: String,
        rules: crate::triggers::TriggerRules,
    },
    TelescopeEvent {
        event: String,
        expires_at: i64,
        #[serde(default)]
        context: Option<TelescopeContext>,
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
            Self::MediaReady {
                max_timelapse_bytes,
            } if *max_timelapse_bytes == 0 => {
                return Err(InvalidMessage);
            }
            Self::Ready {
                protocol_version,
                device_id,
            } if ![
                PROTOCOL_VERSION,
                TRIGGER_PROTOCOL_VERSION,
                CONTEXT_PROTOCOL_VERSION,
            ]
            .contains(protocol_version)
                || *device_id <= 0 =>
            {
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
            Self::ConfigureTriggers { request_id, .. } if !valid_uuid(request_id) => {
                return Err(InvalidMessage);
            }
            Self::TelescopeEvent {
                context: Some(context),
                ..
            } if !context.valid() => {
                return Err(InvalidMessage);
            }
            _ => {}
        }
        Ok(message)
    }

    /// Context is negotiated, not an additive field sent to strict v2 peers.
    pub(crate) fn parse_for_version(bytes: &[u8], version: u32) -> Result<Self, InvalidMessage> {
        let message = Self::parse(bytes)?;
        if matches!(&message, Self::MediaReady { .. }) && version != CONTEXT_PROTOCOL_VERSION {
            return Err(InvalidMessage);
        }
        if let Self::TelescopeEvent { event, context, .. } = &message
            && ((version != CONTEXT_PROTOCOL_VERSION && context.is_some())
                || (context.is_none()
                    && !matches!(
                        event.as_str(),
                        "mount_slew_started"
                            | "mount_slewed"
                            | "sequence_started"
                            | "sequence_finished"
                    )))
        {
            return Err(InvalidMessage);
        }
        if matches!(&message, Self::TelescopeEvent { .. }) && version != CONTEXT_PROTOCOL_VERSION {
            let value: serde_json::Value =
                serde_json::from_slice(bytes).map_err(|_| InvalidMessage)?;
            if value.get("context").is_some() {
                return Err(InvalidMessage);
            }
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

pub(crate) fn valid_uuid(value: &str) -> bool {
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
    fn trigger_protocol_is_strict_and_pairing_stays_v1() {
        let rules = crate::triggers::TriggerRules::local(&crate::service::Preferences::default());
        let message =
            serde_json::json!({"type":"configure_triggers","request_id":ID,"rules":rules});
        assert!(matches!(
            ServerMessage::parse(message.to_string().as_bytes()),
            Ok(ServerMessage::ConfigureTriggers { .. })
        ));
        let mut invalid = message.clone();
        invalid["rules"]["enable_sharing"] = true.into();
        assert!(ServerMessage::parse(invalid.to_string().as_bytes()).is_err());
        invalid = message;
        invalid["request_id"] = "00000000-0000-0000-0000-000000000000".into();
        assert!(ServerMessage::parse(invalid.to_string().as_bytes()).is_err());
        assert!(matches!(
            ServerMessage::parse(br#"{"type":"ready","protocol_version":2,"device_id":42}"#),
            Ok(ServerMessage::Ready { .. })
        ));
        assert!(
            PairingResponse::parse(
                br#"{"protocol_version":2,"device_id":42,"credential":"csdc_test"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_commands_fields_versions_and_oversized_input() {
        for message in [
            r#"{"type":"capture_resume"}"#,
            r#"{"type":"ready","protocol_version":4,"device_id":42}"#,
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

    fn context_message() -> serde_json::Value {
        serde_json::json!({"type":"telescope_event","event":"autofocus_started","expires_at":1792708830,
            "context":{"telescope_id":17,"telescope_name":"UC131","target_name":"NGC 7023","occurred_at":"2026-10-22T20:00:00.123Z"}})
    }

    #[test]
    fn observation_context_is_negotiated_without_changing_legacy_messages() {
        let rich = context_message().to_string();
        assert!(
            ServerMessage::parse_for_version(rich.as_bytes(), CONTEXT_PROTOCOL_VERSION).is_ok()
        );
        assert!(
            ServerMessage::parse_for_version(rich.as_bytes(), TRIGGER_PROTOCOL_VERSION).is_err()
        );
        let legacy =
            br#"{"type":"telescope_event","event":"mount_slewed","expires_at":1792708830}"#;
        assert!(ServerMessage::parse_for_version(legacy, TRIGGER_PROTOCOL_VERSION).is_ok());
        // Video-only v3 cameras may retain the four legacy triggers without
        // opting into target-name transmission.
        assert!(ServerMessage::parse_for_version(legacy, CONTEXT_PROTOCOL_VERSION).is_ok());
        let explicit_null = br#"{"type":"telescope_event","event":"mount_slewed","expires_at":1792708830,"context":null}"#;
        assert!(ServerMessage::parse_for_version(explicit_null, TRIGGER_PROTOCOL_VERSION).is_err());
        assert!(
            ServerMessage::parse(br#"{"type":"ready","protocol_version":3,"device_id":42}"#)
                .is_ok()
        );
    }

    #[test]
    fn observation_context_is_bounded_strict_and_utc_only() {
        for (field, value) in [
            ("telescope_id", serde_json::json!(0)),
            ("telescope_name", serde_json::json!("x".repeat(81))),
            ("target_name", serde_json::json!("x".repeat(121))),
            ("target_name", serde_json::json!("name\nsecret")),
            ("target_name", serde_json::json!(" ")),
            ("occurred_at", serde_json::json!("2026-02-30T20:00:00Z")),
            (
                "occurred_at",
                serde_json::json!("2026-10-22T20:00:00-05:00"),
            ),
            ("occurred_at", serde_json::json!("2026-10-22T20:00:00.")),
            ("coordinates", serde_json::json!({"latitude":38.5})),
        ] {
            let mut message = context_message();
            message["context"][field] = value;
            assert!(
                ServerMessage::parse(message.to_string().as_bytes()).is_err(),
                "accepted {field}"
            );
        }
        let mut without_target = context_message();
        without_target["context"]
            .as_object_mut()
            .unwrap()
            .remove("target_name");
        assert!(ServerMessage::parse(without_target.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn context_expiry_cannot_extend_the_original_observation() {
        let ServerMessage::TelescopeEvent {
            context: Some(context),
            ..
        } = ServerMessage::parse(context_message().to_string().as_bytes()).unwrap()
        else {
            panic!()
        };
        let occurred = context.occurred_at_milliseconds().unwrap();
        assert!(context.fresh_at(occurred, occurred / 1000 + 30));
        assert!(context.fresh_at(occurred + 1000, occurred / 1000 + 30));
        assert!(!context.fresh_at(occurred - 1, occurred / 1000 + 30));
        assert!(!context.fresh_at(occurred + 30_001, occurred / 1000 + 30));
        assert!(!context.fresh_at(occurred, occurred / 1000 + 31));
        let mut equivalent = context.clone();
        equivalent.occurred_at = "2026-10-22T20:00:00.123+00:00".into();
        assert_eq!(equivalent.occurred_at_milliseconds(), Some(occurred));
        equivalent.telescope_name = "é".repeat(80);
        equivalent.target_name = Some("星".repeat(120));
        let caption = equivalent.caption(&"long summary ".repeat(20));
        assert_eq!(caption.chars().count(), 300);
        assert!(!caption.chars().any(char::is_control));
    }
}
