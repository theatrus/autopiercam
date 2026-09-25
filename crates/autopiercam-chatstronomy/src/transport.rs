use crate::{
    media::{self, Detector},
    origin::HubOrigin,
    protocol::{
        ClientMessage, MAX_JPEG_BYTES, MAX_WIRE_BYTES, PROTOCOL_VERSION, PairingRequest,
        PairingResponse, Secret, ServerMessage, requires_operator_action,
    },
    service::{Frame, Settings, Shared, random_uuid},
    snapshot::{LocalState, SnapshotFence},
    triggers::{Scheduler, telescope_summary},
    vault,
};
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    time::{interval, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) async fn pair(
    origin: &HubOrigin,
    installation: &str,
    token: &Secret,
) -> Result<PairingResponse> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| anyhow::anyhow!("Could not initialize HTTPS pairing"))?;
    let body = serde_json::to_vec(&PairingRequest::new(installation, token))?;
    let mut response = client
        .post(origin.pairing_url())
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Pairing failed; the code may have been consumed. Issue a new code in the Hub"
            )
        })?;
    if !response.status().is_success() {
        bail!("Hub rejected pairing; check the origin and issue a new code");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Pairing response lost; issue a new Hub code"))?
    {
        if bytes.len() + chunk.len() > 4096 {
            bail!("Invalid pairing response; issue a new Hub code");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(PairingResponse::parse(&bytes)?)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn state(shared: &Shared, settings: &Settings, frame: Option<&Frame>) -> LocalState {
    LocalState {
        snapshots_allowed: settings.preferences.snapshots,
        privacy_generation: *shared.changes.borrow(),
        session_generation: frame.map(|f| f.session),
    }
}
fn status(shared: &Shared, text: &str) {
    *shared.connection.lock().unwrap() = text.into();
}

/// One immutable automatic event survives reconnects, but never privacy/session
/// changes. Snapshots live only inside their originating WebSocket connection.
struct Outbound {
    id: String,
    wire: String,
    captured_at: u64,
    session: u64,
    retry_at: Instant,
}
impl Outbound {
    fn fresh(&self, frame: Option<&Frame>) -> bool {
        frame.is_some_and(|f| f.session == self.session)
            && now_ms().saturating_sub(self.captured_at) <= 300_000
            && self.captured_at <= now_ms().saturating_add(30_000)
    }
}

pub(crate) async fn run(shared: Arc<Shared>) {
    let mut changes = shared.changes.subscribe();
    let mut outbox = None;
    let mut attempt = 0u32;
    let mut last_new_event = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    loop {
        let epoch = *changes.borrow_and_update();
        if shared.stopped.load(Ordering::Acquire) {
            return;
        }
        let settings = shared.settings.read().unwrap().clone();
        if !settings.preferences.enabled || settings.device_id.is_none() {
            status(
                &shared,
                if settings.device_id.is_none() {
                    "Not paired"
                } else {
                    "Disabled"
                },
            );
            outbox = None;
            attempt = 0;
            let _ = changes.changed().await;
            continue;
        }
        let origin = match HubOrigin::parse(&settings.preferences.hub_origin, shared.policy) {
            Ok(origin) => origin,
            Err(_) => {
                status(&shared, "Invalid Hub origin");
                let _ = changes.changed().await;
                continue;
            }
        };
        let credential = match shared.vault.read(&vault::target(
            &origin.canonical(),
            &settings.installation_id,
        )) {
            Ok(Some(secret)) => secret,
            _ => {
                status(&shared, "Credential unavailable; pair again");
                let _ = changes.changed().await;
                continue;
            }
        };
        status(&shared, "Connecting");
        let connected_at = Instant::now();
        let result = tokio::select! {
            biased;
            _ = changes.changed() => { outbox = None; attempt = 0; continue; }
            result = session(&shared, &settings, &origin, &credential, &mut outbox, epoch, &mut last_new_event) => result
        };
        if matches!(&result, Ok(false))
            || result
                .as_ref()
                .err()
                .is_some_and(|error| error.is::<crate::protocol::InvalidMessage>())
        {
            status(
                &shared,
                "Hub rejected authentication/protocol; pair again or update",
            );
            outbox = None;
            let _ = changes.changed().await;
            continue;
        }
        status(&shared, "Offline; reconnecting");
        if connected_at.elapsed() >= Duration::from_secs(30) {
            attempt = 0;
        }
        attempt = attempt.saturating_add(1).min(6);
        let mut jitter = [0u8; 2];
        let _ = getrandom::fill(&mut jitter);
        let delay = Duration::from_millis(
            ((1u64 << attempt) * 1000 + u64::from(u16::from_le_bytes(jitter)) % 1000).min(60_000),
        );
        tokio::select! {
            biased;
            _ = changes.changed() => { outbox = None; attempt = 0; }
            _ = sleep(delay) => {}
        }
    }
}

async fn send(socket: &mut Socket, text: String) -> Result<()> {
    if text.len() > MAX_WIRE_BYTES {
        bail!("Message exceeds limit");
    }
    timeout(
        Duration::from_secs(10),
        socket.send(Message::Text(text.into())),
    )
    .await??;
    Ok(())
}

async fn next(socket: &mut Socket) -> Result<ServerMessage> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Ok(ServerMessage::parse(text.as_bytes())?),
            Some(Ok(Message::Ping(_))) => {
                socket.flush().await?;
            }
            Some(Ok(Message::Pong(_))) => {}
            _ => bail!("Hub disconnected"),
        }
    }
}

async fn session(
    shared: &Shared,
    settings: &Settings,
    origin: &HubOrigin,
    credential: &Secret,
    outbox: &mut Option<Outbound>,
    epoch: u64,
    last_new_event: &mut Instant,
) -> Result<bool> {
    let version = if settings.preferences.interval_minutes > 0
        || settings.preferences.telescope_events
        || settings.preferences.chat_configuration
    {
        crate::protocol::TRIGGER_PROTOCOL_VERSION
    } else {
        PROTOCOL_VERSION
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_BYTES))
        .max_frame_size(Some(MAX_WIRE_BYTES))
        .max_write_buffer_size(MAX_WIRE_BYTES * 2);
    let (mut socket, _) = timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(
            origin.websocket_url().as_str(),
            Some(config),
            false,
        ),
    )
    .await??;
    send(
        &mut socket,
        serde_json::to_string(&ClientMessage::Authenticate {
            protocol_version: version,
            installation_id: &settings.installation_id,
            credential: credential.expose_for_transport(),
            snapshots: settings.preferences.snapshots,
        })?,
    )
    .await?;
    match timeout(Duration::from_secs(10), next(&mut socket)).await?? {
        ServerMessage::Ready {
            device_id,
            protocol_version,
        } if Some(device_id) == settings.device_id && protocol_version == version => {}
        ServerMessage::Error { code } => return Ok(!requires_operator_action(&code)),
        _ => return Ok(false),
    }
    status(shared, "Connected");
    if version == crate::protocol::TRIGGER_PROTOCOL_VERSION {
        send(&mut socket, serde_json::json!({"type":"trigger_capabilities", "chat_configuration":settings.preferences.chat_configuration, "telescope_events":settings.preferences.telescope_events}).to_string()).await?;
    }
    let mut rules = settings.rules();
    let mut scheduler = Scheduler::new(rules.clone(), Instant::now());
    let mut ticks = interval(Duration::from_millis(100));
    let mut last_rx = Instant::now();
    let mut detector = Detector::default();
    let mut request: Option<SnapshotFence> = None;
    let mut snapshot_outbox: Option<Outbound> = None;
    let mut last_sequence = 0;
    loop {
        if shared.stopped.load(Ordering::Acquire) || *shared.changes.borrow() != epoch {
            bail!("Local sharing permission changed");
        }
        tokio::select! {
            incoming = socket.next() => {
                last_rx = Instant::now();
                let incoming = match incoming {
                    Some(Ok(Message::Text(text))) => match ServerMessage::parse(text.as_bytes()) { Ok(value) => value, Err(_) => return Ok(false) },
                    Some(Ok(Message::Ping(_))) => { socket.flush().await?; continue; }
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => bail!("Hub disconnected"),
                };
                match incoming {
                    ServerMessage::ConfigureTriggers { request_id, rules: proposed } if version == crate::protocol::TRIGGER_PROTOCOL_VERSION => {
                        let accepted = shared.configure_triggers(proposed.clone(), epoch).is_ok();
                        if accepted {
                            rules = proposed;
                            scheduler = Scheduler::new(rules.clone(), Instant::now());
                            detector = Detector::default();
                            // Clear reconnect-retained bytes before any awaited write.
                            *outbox = None; snapshot_outbox = None;
                            if let Some(fence) = request.take() {
                                send(&mut socket, serde_json::to_string(&ClientMessage::SnapshotUnavailable { request_id: fence.request_id() })?).await?;
                            }
                        }
                        send(&mut socket, serde_json::json!({"type":"trigger_configuration_result","request_id":request_id,"accepted":accepted}).to_string()).await?;
                    }
                    ServerMessage::TelescopeEvent { event, expires_at } if version == crate::protocol::TRIGGER_PROTOCOL_VERSION => {
                        let now_seconds = (now_ms() / 1000) as i64;
                        if rules.telescope_events && expires_at > now_seconds && expires_at <= now_seconds + 90
                            && let Some(summary) = telescope_summary(&event)
                            && let Some(frame) = (shared.source)() {
                            scheduler.trigger("telescope_event", summary, &frame, Instant::now(), true);
                        }
                    }
                    ServerMessage::SnapshotRequest { ref request_id, .. } => {
                        let frame = (shared.source)();
                        if request.is_some() || outbox.is_some() {
                            send(&mut socket, serde_json::to_string(&ClientMessage::SnapshotUnavailable { request_id })?).await?;
                        } else {
                            match SnapshotFence::new(&incoming, state(shared, settings, frame.as_ref()), now_ms(), Instant::now()) {
                                Ok(fence) => request = Some(fence),
                                Err(_) => send(&mut socket, serde_json::to_string(&ClientMessage::SnapshotUnavailable { request_id })?).await?,
                            }
                        }
                    }
                    ServerMessage::EventAck { event_id, status: ack, retry_after_seconds } => {
                        let slot = if snapshot_outbox.as_ref().is_some_and(|v| v.id == event_id) { &mut snapshot_outbox } else { &mut *outbox };
                        if let Some(item) = slot.as_mut().filter(|v| v.id == event_id) {
                            if ack == "retry" || ack == "rate_limited" {
                                item.retry_at = Instant::now() + Duration::from_secs(retry_after_seconds.clamp(60, 3600));
                            } else {
                                if ack == "delivered" { *shared.last_delivery.lock().unwrap() = Some(now_ms()); status(shared, "Connected; image delivered"); }
                                else { status(shared, match ack.as_str() { "no_destinations" => "Connected; select destination channels in the Hub", _ => "Connected; Hub declined the image" }); }
                                *slot = None;
                                if request.as_ref().is_some() && snapshot_outbox.is_none() { request = None; }
                            }
                        }
                    }
                    ServerMessage::Error { code } => return Ok(!requires_operator_action(&code)),
                    _ => return Ok(false),
                }
            }
            _ = ticks.tick() => {
                if last_rx.elapsed() > Duration::from_secs(100) { bail!("Hub idle"); }
                let frame = (shared.source)();
                if outbox.as_ref().is_some_and(|item| !item.fresh(frame.as_ref())) { *outbox = None; }
                if let Some(fence) = &request {
                    let valid = frame.as_ref().is_some_and(|f| fence.check_frame(state(shared, settings, Some(f)), f.session, f.captured_at_unix_ms, now_ms(), Instant::now()).is_ok());
                    if !valid {
                        send(&mut socket, serde_json::to_string(&ClientMessage::SnapshotUnavailable { request_id: fence.request_id() })?).await?;
                        request = None; snapshot_outbox = None;
                    } else if snapshot_outbox.is_none() {
                        let f = frame.as_ref().unwrap().clone();
                        let cap = fence.max_jpeg_bytes();
                        let encoded = tokio::task::spawn_blocking(move || media::jpeg(&f, cap)).await?;
                        if let Ok(jpeg) = encoded {
                            snapshot_outbox = Some(event(frame.as_ref().unwrap(), "snapshot", "Requested pier-camera snapshot", Some(fence.request_id()), jpeg)?);
                        } else {
                            send(&mut socket, serde_json::to_string(&ClientMessage::SnapshotUnavailable { request_id: fence.request_id() })?).await?;
                            request = None;
                        }
                    }
                }
                if let Some(f) = frame.as_ref().filter(|f| f.sequence != last_sequence) {
                    last_sequence = f.sequence;
                    if rules.scene_changes || rules.day_night {
                        // Decoding is bounded to a small preview and never holds a camera lock.
                        let mut detection = settings.preferences.clone();
                        detection.scene_changes = rules.scene_changes;
                        detection.day_night = rules.day_night;
                        if let Some((kind, summary)) = detector.observe(f, &detection, Instant::now()).unwrap_or(None) {
                            scheduler.trigger(kind, summary, f, Instant::now(), false);
                        }
                    }
                } else if frame.is_none() { detector = Detector::default(); last_sequence = 0; }
                let due = scheduler.due(frame.as_ref(), Instant::now(), now_ms());
                if let Some((kind, summary)) = due && let Some(f) = frame.as_ref()
                    && outbox.is_none() && request.is_none() && last_new_event.elapsed() >= Duration::from_secs(60) {
                    let encoded_frame = f.clone();
                    if let Ok(jpeg) = tokio::task::spawn_blocking(move || media::jpeg(&encoded_frame, MAX_JPEG_BYTES)).await? {
                        *outbox = Some(event(f, kind, summary, None, jpeg)?);
                        scheduler.queued(f, Instant::now());
                        *last_new_event = Instant::now();
                    }
                }
                let current = (shared.source)();
                if shared.stopped.load(Ordering::Acquire) || *shared.changes.borrow() != epoch {
                    bail!("Local sharing permission changed");
                }
                let pending = if snapshot_outbox.is_some() { &mut snapshot_outbox } else { &mut *outbox };
                if let Some(item) = pending.as_mut() {
                    if !item.fresh(current.as_ref()) { *pending = None; continue; }
                    if let Some(fence) = &request {
                        let Some(f) = current.as_ref() else { continue; };
                        if fence.check_frame(state(shared, settings, Some(f)), item.session, item.captured_at, now_ms(), Instant::now()).is_err() { continue; }
                    }
                    if Instant::now() >= item.retry_at {
                        // Cancel stalled sends when capture stops or changes session.
                        let session = item.session;
                        tokio::select! {
                            biased;
                            _ = source_invalidated(shared, session) => { bail!("Capture changed during image delivery"); }
                            result = send(&mut socket, item.wire.clone()) => result?,
                        }
                        item.retry_at = Instant::now() + Duration::from_secs(60);
                        *last_new_event = Instant::now();
                    }
                }
            }
        }
    }
}

async fn source_invalidated(shared: &Shared, session: u64) {
    loop {
        if (shared.source)().is_none_or(|f| f.session != session) {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

fn event(
    frame: &Frame,
    kind: &str,
    summary: &str,
    request_id: Option<&str>,
    jpeg: Vec<u8>,
) -> Result<Outbound> {
    let id = random_uuid()?;
    let captured_at = time::OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(frame.captured_at_unix_ms) * 1_000_000,
    )?
    .format(&time::format_description::well_known::Rfc3339)?;
    let wire = serde_json::json!({"type":"event", "event": {
        "event_id":id, "kind":kind, "captured_at":captured_at, "summary":summary,
        "jpeg_base64":STANDARD.encode(jpeg), "request_id":request_id,
    }})
    .to_string();
    Ok(Outbound {
        id,
        wire,
        captured_at: frame.captured_at_unix_ms,
        session: frame.session,
        retry_at: Instant::now(),
    })
}
