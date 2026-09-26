use crate::{origin::TransportPolicy, protocol::Secret, service::*, transport::now_ms, vault};
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

#[derive(Default)]
struct MemoryStore(Mutex<HashMap<String, String>>);
impl SecretStore for MemoryStore {
    fn read(&self, key: &str) -> Result<Option<Secret>> {
        Ok(self.0.lock().unwrap().get(key).cloned().map(Secret::new))
    }
    fn write(&self, key: &str, value: &Secret) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_owned(), value.expose_for_transport().to_owned());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

pub(crate) fn frame(sequence: u64, changed: bool) -> Frame {
    let image = RgbImage::from_fn(96, 64, |x, _| {
        if changed && x < 48 {
            Rgb([220, 220, 220])
        } else {
            Rgb([70, 70, 70])
        }
    });
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&image)
        .unwrap();
    Frame {
        session: 1,
        sequence,
        captured_at_unix_ms: now_ms(),
        exposure_us: Some(60_000_000),
        gain: Some(100),
        mode: "night".into(),
        jpeg: jpeg.into(),
    }
}

async fn text(socket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let message = timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn http_pair(listener: &TcpListener, status: &str, response: &str) -> Value {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    let (body_start, length) = loop {
        let mut b = [0; 1024];
        let count = socket.read(&mut b).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&b[..count]);
        assert!(bytes.len() <= 8192);
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..index]).to_lowercase();
            assert!(headers.starts_with("post /v1/devices/pair "));
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            break (index + 4, length);
        }
    };
    while bytes.len() < body_start + length {
        let mut b = [0; 1024];
        let count = socket.read(&mut b).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&b[..count]);
    }
    let request = serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
    request
}

struct Fixture {
    _directory: tempfile::TempDir,
    service: SharingService,
    frames: Arc<RwLock<Option<Frame>>>,
    vault: Arc<MemoryStore>,
    listener: TcpListener,
}
impl Fixture {
    async fn paired() -> Self {
        Self::paired_with(Preferences::default()).await
    }

    async fn paired_with(preferences: Preferences) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let frames = Arc::new(RwLock::new(Some(frame(1, false))));
        let source = frames.clone();
        let vault = Arc::new(MemoryStore::default());
        let service = SharingService::start_with_store(
            &directory.path().join("agent.toml"),
            Arc::new(move || source.read().unwrap().clone()),
            vault.clone(),
            TransportPolicy::AllowLoopbackHttp,
        )
        .unwrap();
        let client = service.client();
        let status = client
            .update(
                client.status().revision,
                Preferences {
                    hub_origin: origin,
                    ..preferences.clone()
                },
            )
            .unwrap();
        let pairing = tokio::task::spawn_blocking(move || {
            client.pair(status.revision, Secret::new("csdp_test_only".into()))
        });
        let body = http_pair(
            &listener,
            "200 OK",
            r#"{"protocol_version":1,"device_id":42,"credential":"csdc_test_only"}"#,
        )
        .await;
        assert_eq!(body["kind"], "pier_camera");
        let status = pairing.await.unwrap().unwrap();
        assert_eq!(body["installation_id"], status.installation_id);
        assert!(!status.preferences.enabled);
        assert_eq!(
            status.preferences,
            Preferences {
                hub_origin: status.preferences.hub_origin.clone(),
                enabled: false,
                ..preferences
            }
        );
        assert!(
            !std::fs::read_to_string(directory.path().join("agent.chatstronomy.json"))
                .unwrap()
                .contains("csdc_")
        );
        assert!(!serde_json::to_string(&status).unwrap().contains("csdc_"));
        assert_eq!(vault.0.lock().unwrap().len(), 1);
        Self {
            _directory: directory,
            service,
            frames,
            vault,
            listener,
        }
    }
    async fn connect(&self, snapshots: bool, scene_changes: bool) -> WebSocketStream<TcpStream> {
        let client = self.service.client();
        let status = client.status();
        client
            .update(
                status.revision,
                Preferences {
                    enabled: true,
                    snapshots,
                    scene_changes,
                    ..status.preferences
                },
            )
            .unwrap();
        let (socket, _) = timeout(Duration::from_secs(5), self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let auth = text(&mut socket).await;
        assert_eq!(auth["type"], "authenticate");
        assert_eq!(auth["snapshots"], snapshots);
        assert_eq!(auth["credential"], "csdc_test_only");
        socket
            .send(Message::Text(
                json!({"type":"ready","protocol_version":auth["protocol_version"],"device_id":42})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        if auth["protocol_version"] == 2 {
            assert_eq!(text(&mut socket).await["type"], "trigger_capabilities");
        }
        socket
    }
}

#[tokio::test]
async fn pairing_preserves_choices_but_never_starts_sharing() {
    let fixture = Fixture::paired_with(Preferences {
        enabled: true,
        snapshots: true,
        scene_changes: true,
        day_night: true,
        scene_threshold_percent: 35,
        interval_minutes: 15,
        telescope_events: true,
        chat_configuration: true,
        burst_count: 3,
        spacing_seconds: 120,
        ..Preferences::default()
    })
    .await;
    let client = fixture.service.client();
    let status = client.status();
    let stored: Settings = serde_json::from_slice(
        &std::fs::read(fixture._directory.path().join("agent.chatstronomy.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored.preferences, status.preferences);
    assert!(stored.remote_rules.is_none());
    assert!(
        timeout(Duration::from_millis(300), fixture.listener.accept())
            .await
            .is_err(),
        "pairing must not connect or send frames until explicitly enabled"
    );
    let error = client
        .pair(status.revision, Secret::new("csdp_second".into()))
        .unwrap_err();
    assert!(error.to_string().contains("already paired"));
    assert_eq!(client.status().revision, status.revision);
    assert_eq!(client.status().preferences, status.preferences);
    assert_eq!(fixture.vault.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn trigger_configuration_is_durable_bounded_and_reset_by_local_save() {
    use crate::triggers::TriggerRules;
    let fixture = Fixture::paired().await;
    let client = fixture.service.client();
    let old = client.status();
    client
        .update(
            old.revision,
            Preferences {
                chat_configuration: true,
                telescope_events: true,
                interval_minutes: 5,
                burst_count: 3,
                ..old.preferences
            },
        )
        .unwrap();
    let mut socket = fixture.connect(true, true).await;
    let before = client.status();
    let mut rules = TriggerRules::local(&before.preferences);
    rules.interval_minutes = 10;
    rules.burst_count = 2;
    let id = random_uuid().unwrap();
    socket
        .send(Message::Text(
            json!({"type":"configure_triggers","request_id":id,"rules":rules})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        text(&mut socket).await,
        json!({"type":"trigger_configuration_result","request_id":id,"accepted":true})
    );
    assert_eq!(client.status().active_triggers, rules);
    assert_eq!(client.status().revision, before.revision + 1);
    let stored: Settings = serde_json::from_slice(
        &std::fs::read(fixture._directory.path().join("agent.chatstronomy.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored.rules(), rules);
    rules.interval_minutes = 1; // faster than local permission
    socket
        .send(Message::Text(
            json!({"type":"configure_triggers","request_id":id,"rules":rules})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(text(&mut socket).await["accepted"], false);
    assert_eq!(client.status().revision, before.revision + 1);
    assert!(
        client
            .update(before.revision, before.preferences.clone())
            .is_err()
    );
    let current = client.status();
    client.update(current.revision, before.preferences).unwrap();
    assert_eq!(client.status().active_triggers.interval_minutes, 5);
}

#[tokio::test]
async fn telescope_trigger_waits_for_an_updated_frame_and_local_consent_wins() {
    let fixture = Fixture::paired().await;
    let client = fixture.service.client();
    let old = client.status();
    client
        .update(
            old.revision,
            Preferences {
                telescope_events: true,
                ..old.preferences
            },
        )
        .unwrap();
    let mut socket = fixture.connect(false, false).await;
    let id = random_uuid().unwrap();
    let rules = crate::triggers::TriggerRules::local(&client.status().preferences);
    socket
        .send(Message::Text(
            json!({"type":"configure_triggers","request_id":id,"rules":rules})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(text(&mut socket).await["accepted"], false); // chat gate off
    socket.send(Message::Text(json!({"type":"telescope_event","event":"mount_slew_started","expires_at":now_ms()/1000+30}).to_string().into())).await.unwrap();
    assert!(
        timeout(Duration::from_millis(250), socket.next())
            .await
            .is_err()
    );
    *fixture.frames.write().unwrap() = Some(frame(2, true));
    let event = text(&mut socket).await;
    assert_eq!(event["event"]["kind"], "telescope_event");
    assert_eq!(event["event"]["summary"], "Pier camera: mount slew started");
    let current = client.status();
    client
        .update(
            current.revision,
            Preferences {
                enabled: false,
                ..current.preferences
            },
        )
        .unwrap();
    assert!(!matches!(
        timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap(),
        Some(Ok(Message::Text(_)))
    ));
}

async fn request(socket: &mut WebSocketStream<TcpStream>) -> String {
    let id = random_uuid().unwrap();
    socket.send(Message::Text(json!({"type":"snapshot_request","request_id":id,"expires_at":now_ms()/1000+90,"max_jpeg_bytes":524288,"max_frame_age_seconds":120}).to_string().into())).await.unwrap();
    id
}

#[tokio::test]
async fn pairing_snapshot_delivery_and_disable_round_trip() {
    let fixture = Fixture::paired().await;
    let mut socket = fixture.connect(true, false).await;
    let id = request(&mut socket).await;
    let event = text(&mut socket).await;
    assert_eq!(event["type"], "event");
    assert_eq!(event["event"]["kind"], "snapshot");
    assert_eq!(event["event"]["request_id"], id);
    let jpeg = STANDARD
        .decode(event["event"]["jpeg_base64"].as_str().unwrap())
        .unwrap();
    assert!(jpeg.len() <= 524288);
    image::load_from_memory(&jpeg).unwrap();
    let capture = time::OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(
            fixture
                .frames
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .captured_at_unix_ms,
        ) * 1_000_000,
    )
    .unwrap()
    .format(&time::format_description::well_known::Rfc3339)
    .unwrap();
    assert_eq!(event["event"]["captured_at"], capture);
    socket.send(Message::Text(json!({"type":"event_ack","event_id":event["event"]["event_id"],"status":"delivered","retry_after_seconds":0}).to_string().into())).await.unwrap();
    let client = fixture.service.client();
    timeout(Duration::from_secs(3), async {
        while client.status().last_delivery_unix_ms.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = client.status();
    client
        .update(
            status.revision,
            Preferences {
                enabled: false,
                ..status.preferences
            },
        )
        .unwrap();
    assert!(timeout(Duration::from_secs(2), socket.next()).await.is_ok());
    assert!(
        timeout(Duration::from_millis(300), fixture.listener.accept())
            .await
            .is_err()
    );
    let status = client.status();
    client.forget(status.revision).unwrap();
    assert!(fixture.vault.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_local_snapshot_consent_denies_even_a_hub_request() {
    let fixture = Fixture::paired().await;
    let mut socket = fixture.connect(false, false).await;
    let id = request(&mut socket).await;
    assert_eq!(
        text(&mut socket).await,
        json!({"type":"snapshot_unavailable","request_id":id})
    );
}

#[tokio::test]
async fn paused_stale_and_changed_session_frames_are_not_shared() {
    let fixture = Fixture::paired().await;
    let mut socket = fixture.connect(true, false).await;
    *fixture.frames.write().unwrap() = None;
    let id = request(&mut socket).await;
    assert_eq!(text(&mut socket).await["request_id"], id);
    let mut stale = frame(2, false);
    stale.captured_at_unix_ms -= 121_000;
    *fixture.frames.write().unwrap() = Some(stale);
    request(&mut socket).await;
    assert_eq!(text(&mut socket).await["type"], "snapshot_unavailable");
}

#[tokio::test]
async fn scene_change_posts_once_and_queued_retry_is_cancelled_on_disable() {
    let fixture = Fixture::paired().await;
    let mut socket = fixture.connect(false, true).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    for sequence in 2..=4 {
        *fixture.frames.write().unwrap() = Some(frame(sequence, true));
        tokio::time::sleep(Duration::from_millis(180)).await;
    }
    let event = text(&mut socket).await;
    assert_eq!(event["event"]["kind"], "scene_change");
    assert!(event["event"]["request_id"].is_null());
    socket.send(Message::Text(json!({"type":"event_ack","event_id":event["event"]["event_id"],"status":"retry","retry_after_seconds":60}).to_string().into())).await.unwrap();
    assert!(
        timeout(Duration::from_millis(300), socket.next())
            .await
            .is_err()
    );
    let client = fixture.service.client();
    let status = client.status();
    client
        .update(
            status.revision,
            Preferences {
                enabled: false,
                ..status.preferences
            },
        )
        .unwrap();
    assert!(timeout(Duration::from_secs(2), socket.next()).await.is_ok());
}

#[tokio::test]
async fn elided_ack_drops_the_image_without_retrying() {
    let fixture = Fixture::paired().await;
    let mut socket = fixture.connect(false, true).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    for sequence in 2..=4 {
        *fixture.frames.write().unwrap() = Some(frame(sequence, true));
        tokio::time::sleep(Duration::from_millis(180)).await;
    }
    let event = text(&mut socket).await;
    assert_eq!(event["event"]["kind"], "scene_change");
    socket.send(Message::Text(json!({"type":"event_ack","event_id":event["event"]["event_id"],"status":"elided","retry_after_seconds":0}).to_string().into())).await.unwrap();
    // Terminal: the same event is never resent.
    if let Ok(Some(Ok(Message::Text(next)))) =
        timeout(Duration::from_millis(300), socket.next()).await
    {
        let next: serde_json::Value = serde_json::from_str(&next).unwrap();
        assert_ne!(next["event"]["event_id"], event["event"]["event_id"]);
    }
    let status = fixture.service.client().status();
    assert_eq!(
        status.connection,
        "Connected; image skipped (the Hub posts at most one per minute)"
    );
    assert!(status.last_delivery_unix_ms.is_none());
}

#[tokio::test]
async fn revoked_authentication_stops_automatic_reconnection() {
    let fixture = Fixture::paired().await;
    let client = fixture.service.client();
    let status = client.status();
    client
        .update(
            status.revision,
            Preferences {
                enabled: true,
                ..status.preferences
            },
        )
        .unwrap();
    let (stream, _) = fixture.listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    text(&mut socket).await;
    socket
        .send(Message::Text(
            json!({"type":"error","code":"authentication_failed"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while !client.status().connection.contains("rejected") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(1200), fixture.listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn identity_is_persistent_and_credentials_are_scoped_to_origin_and_installation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.toml");
    let vault = Arc::new(MemoryStore::default());
    let service = SharingService::start_with_store(
        &path,
        Arc::new(|| None),
        vault.clone(),
        TransportPolicy::HttpsOnly,
    )
    .unwrap();
    let id = service.client().status().installation_id;
    assert!(
        SharingService::start_with_store(
            &path,
            Arc::new(|| None),
            vault.clone(),
            TransportPolicy::HttpsOnly
        )
        .is_err()
    );
    drop(service);
    let service = SharingService::start_with_store(
        &path,
        Arc::new(|| None),
        vault,
        TransportPolicy::HttpsOnly,
    )
    .unwrap();
    assert_eq!(id, service.client().status().installation_id);
    assert_ne!(
        vault::target("https://one", &id),
        vault::target("https://two", &id)
    );
    assert_ne!(
        vault::target("https://one", &id),
        vault::target("https://one", &random_uuid().unwrap())
    );
    let client = service.client();
    let status = client.status();
    client
        .update(status.revision, status.preferences.clone())
        .unwrap();
    assert!(client.update(status.revision, status.preferences).is_err());
}

#[tokio::test]
async fn rejected_pairing_does_not_persist_credentials_or_retry() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vault = Arc::new(MemoryStore::default());
    let service = SharingService::start_with_store(
        &directory.path().join("agent.toml"),
        Arc::new(|| None),
        vault.clone(),
        TransportPolicy::AllowLoopbackHttp,
    )
    .unwrap();
    let client = service.client();
    let status = client
        .update(
            1,
            Preferences {
                hub_origin: format!("http://{}", listener.local_addr().unwrap()),
                snapshots: true,
                scene_changes: true,
                interval_minutes: 12,
                telescope_events: true,
                chat_configuration: true,
                burst_count: 2,
                ..Preferences::default()
            },
        )
        .unwrap();
    let expected_preferences = status.preferences.clone();
    let pairing = tokio::task::spawn_blocking(move || {
        client.pair(status.revision, Secret::new("csdp_expired".into()))
    });
    http_pair(&listener, "401 Unauthorized", "csdp_expired").await;
    let error = pairing.await.unwrap().unwrap_err().to_string();
    assert!(!error.contains("csdp_expired"));
    assert!(vault.0.lock().unwrap().is_empty());
    assert_eq!(service.client().status().preferences, expected_preferences);
    assert!(service.client().status().device_id.is_none());
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn automatic_retry_retains_exact_payload_across_reconnect() {
    let fixture = Fixture::paired().await;
    let mut socket = fixture.connect(false, true).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    for sequence in 2..=4 {
        *fixture.frames.write().unwrap() = Some(frame(sequence, true));
        tokio::time::sleep(Duration::from_millis(180)).await;
    }
    let original = text(&mut socket).await;
    let sent = std::time::Instant::now();
    // Simulate a lost acknowledgment, not a new event.
    drop(socket);
    let (stream, _) = timeout(Duration::from_secs(5), fixture.listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    text(&mut socket).await;
    socket
        .send(Message::Text(
            json!({"type":"ready","protocol_version":1,"device_id":42})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let retry = timeout(Duration::from_secs(65), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(retry) = retry else {
        panic!("expected immutable event retry");
    };
    assert_eq!(serde_json::from_str::<Value>(&retry).unwrap(), original);
    assert!(sent.elapsed() >= Duration::from_secs(59));
}

#[tokio::test]
async fn pair_redirect_is_never_followed_and_origin_cannot_change_while_paired() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vault = Arc::new(MemoryStore::default());
    let service = SharingService::start_with_store(
        &directory.path().join("agent.toml"),
        Arc::new(|| None),
        vault,
        TransportPolicy::AllowLoopbackHttp,
    )
    .unwrap();
    let client = service.client();
    let status = client
        .update(
            1,
            Preferences {
                hub_origin: format!("http://{}", listener.local_addr().unwrap()),
                ..Preferences::default()
            },
        )
        .unwrap();
    let pairing = tokio::task::spawn_blocking(move || {
        client.pair(status.revision, Secret::new("csdp_redirect".into()))
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut bytes = [0u8; 4096];
    assert!(stream.read(&mut bytes).await.unwrap() > 0);
    stream.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap()).as_bytes()).await.unwrap();
    assert!(pairing.await.unwrap().is_err());
    assert!(
        timeout(Duration::from_millis(300), target.accept())
            .await
            .is_err()
    );
    let fixture = Fixture::paired().await;
    let client = fixture.service.client();
    let status = client.status();
    assert!(
        client
            .update(
                status.revision,
                Preferences {
                    hub_origin: "https://other.example".into(),
                    ..status.preferences
                }
            )
            .is_err()
    );
}

#[test]
fn detector_suppresses_exposure_ramps_and_requires_stable_mode_and_distinct_frames() {
    use crate::media::Detector;
    use std::time::Instant;
    let now = Instant::now();
    let prefs = Preferences {
        scene_changes: true,
        day_night: true,
        ..Preferences::default()
    };
    let mut detector = Detector::default();
    let initial = frame(1, false);
    assert!(detector.observe(&initial, &prefs, now).unwrap().is_none());
    let mut ramp = frame(2, true);
    ramp.exposure_us = Some(30_000_000);
    assert!(detector.observe(&ramp, &prefs, now).unwrap().is_none());
    for _ in 0..10 {
        assert!(detector.observe(&ramp, &prefs, now).unwrap().is_none());
    }
    let mut day = frame(3, false);
    day.mode = "day".into();
    assert!(detector.observe(&day, &prefs, now).unwrap().is_none());
    day.sequence = 4;
    assert!(
        detector
            .observe(&day, &prefs, now + Duration::from_secs(29))
            .unwrap()
            .is_none()
    );
    day.sequence = 5;
    assert_eq!(
        detector
            .observe(&day, &prefs, now + Duration::from_secs(30))
            .unwrap()
            .unwrap()
            .0,
        "day_night_transition"
    );
    day.session = 2;
    day.sequence = 6;
    assert!(
        detector
            .observe(&day, &prefs, now + Duration::from_secs(60))
            .unwrap()
            .is_none()
    );
}

#[test]
fn sharing_accepts_full_hd_input_and_still_bounds_network_images() {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 75)
        .encode(
            &vec![100; 1920 * 1080 * 3],
            1920,
            1080,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    let input = Frame {
        jpeg: bytes.into(),
        ..frame(1, false)
    };
    let encoded = crate::media::jpeg(&input, 524288).unwrap();
    assert!(encoded.len() <= 524288);
    let decoded = image::load_from_memory(&encoded).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1280, 720));
}

#[test]
fn jpeg_encoder_respects_remote_cap_and_rejects_malformed_input() {
    let valid = frame(1, true);
    let encoded = crate::media::jpeg(&valid, 1500).unwrap();
    assert!(encoded.len() <= 1500);
    assert!(crate::media::jpeg(&valid, 4).is_err());
    let invalid = Frame {
        jpeg: Arc::from(b"not jpeg".as_slice()),
        ..valid
    };
    assert!(crate::media::jpeg(&invalid, 524288).is_err());
}

#[tokio::test]
async fn privacy_change_cancels_a_late_pairing_response_before_credential_storage() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vault = Arc::new(MemoryStore::default());
    let service = SharingService::start_with_store(
        &directory.path().join("agent.toml"),
        Arc::new(|| None),
        vault.clone(),
        TransportPolicy::AllowLoopbackHttp,
    )
    .unwrap();
    let client = service.client();
    let status = client
        .update(
            1,
            Preferences {
                hub_origin: format!("http://{}", listener.local_addr().unwrap()),
                ..Preferences::default()
            },
        )
        .unwrap();
    let pair_client = client.clone();
    let pairing = tokio::task::spawn_blocking(move || {
        pair_client.pair(status.revision, Secret::new("csdp_cancelled".into()))
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut bytes = [0u8; 4096];
    assert!(stream.read(&mut bytes).await.unwrap() > 0);
    let current = client.status();
    client
        .update(current.revision, current.preferences)
        .unwrap();
    let body = r#"{"protocol_version":1,"device_id":42,"credential":"csdc_late_response"}"#;
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert!(pairing.await.unwrap().is_err());
    assert!(vault.0.lock().unwrap().is_empty());
    assert!(client.status().device_id.is_none());
    assert!(!client.status().preferences.enabled);
}

#[tokio::test]
async fn unsupported_hub_ready_version_requires_operator_action() {
    let fixture = Fixture::paired().await;
    let client = fixture.service.client();
    let status = client.status();
    client
        .update(
            status.revision,
            Preferences {
                enabled: true,
                ..status.preferences
            },
        )
        .unwrap();
    let (stream, _) = fixture.listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    text(&mut socket).await;
    socket
        .send(Message::Text(
            json!({"type":"ready","protocol_version":999,"device_id":42})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while !client.status().connection.contains("rejected") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(1200), fixture.listener.accept())
            .await
            .is_err()
    );
}
