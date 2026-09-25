# Chatstronomy integration

Status: Hub implementation proposed; **AutoPierCam 0.1.0 does not yet connect
to Chatstronomy**. Pairing a Hub device is not a working AutoPierCam connection
until the client and local sharing controls below are implemented.

The local `spacecat` checkout is `theatrus/chatstronomy`. Its centralized Hub
lives in `src/hub`, not a separate `chatstronomy-hub` GitHub repository.

Hub work is split into two stacked PRs:

1. [Device pairing and channel management, #184](https://github.com/theatrus/chatstronomy/pull/184).
2. [Bidirectional events and snapshot requests, #185](https://github.com/theatrus/chatstronomy/pull/185).

The exact wire contract is maintained in
[`docs/DEVICE_PROTOCOL.md` on the transport branch](https://github.com/theatrus/chatstronomy/blob/codex/piercam-event-transport/docs/DEVICE_PROTOCOL.md).
The PRs are not deployed by creating them. Merge the foundation before the
transport PR and retarget the transport PR to `main` after that merge.

## Boundaries

An AutoPierCam installation pairs as a user-owned `pier_camera`, not a N.I.N.A.
profile or telescope. It has a separate installation UUID, pairing code, and
credential. Its Discord destinations may share telescope channels, but its
credentials never authorize telescope commands. The Hub selects destinations;
camera messages cannot supply channel IDs or external image URLs.

Pairing uses a one-shot HTTPS exchange. The client then opens an outbound WSS
connection to `/v1/devices`. This supports both event notifications and a Hub
**Snapshot now** request, with no inbound firewall port or public preview server.
Snapshot permission is distinct from automatic event sharing. Neither permission
is enabled by pairing or by receiving a Hub request.

The first Hub implementation supports scene-change and day/night observations,
not semantic claims about meteors, people, animals, intrusions, or clouds.
Actual detector choices remain a user/product decision. It does not add a
Discord snapshot slash command yet; manual requests originate in the Hub UI.

## AutoPierCam implementation sequence

### 1. Client, protected pairing, and on-demand snapshots

- Add a dedicated network worker consuming `PreviewHub::snapshot()`; never use
  another SDK handle, hold a camera mutex during network I/O, or transmit on the
  drain/encoder threads. In headless mode, start the existing preview producer
  when sharing requires it, as video recording already does.
- Keep the Hub HTTPS origin and non-secret preferences in optional validated
  configuration. Reject credentials, fragments, query strings and arbitrary
  paths in the origin; allow loopback HTTP/WS only under explicit development
  configuration. Use certificate validation and refuse cross-origin redirects.
- Persist a random installation UUID. Store the credential in Windows
  Credential Manager, scoped to both installation and Hub origin, outside TOML,
  the config IPC payload, logs, and command-line arguments. On other platforms,
  require an OS credential-store implementation rather than plaintext fallback.
- Add WinUI pairing/revoke/status controls and independent, default-off switches
  for automatic events and requested snapshots. Show the origin and explain
  that images go to the owner's selected Discord channels. Existing strict
  configuration and IPC compatibility tests must cover old clients and omitted
  settings; secrets must never be round-tripped through config editing.
- Complete HTTPS pairing once, securely persist the returned credential, then
  connect over WSS. A lost pair response needs a newly issued code. Reconnect
  with bounded exponential backoff/jitter; do not loop on revoked credentials.
- Advertise current local snapshot consent. Recheck consent and capture-session
  identity before every upload and after every await. Turning off sharing must
  cancel in-flight work, discard queued images, and reconnect with updated
  consent; already posted Discord messages cannot be recalled.
- For a snapshot, return the latest completed frame (at most 120 seconds old),
  with its actual capture timestamp; optionally wait for the next frame within
  the 90-second deadline. Never interrupt a 30–60+ second exposure, alter gain,
  exposure or ROI, or treat a remote request as `capture.resume`/`capture.now`.
  Paused/disconnected/stale sessions return `snapshot_unavailable`.
- Bound transmitted previews to 512 KiB. Resize/re-encode on the network worker
  if necessary. The Hub has a 720 KiB JSON limit and no RAW16/PNG/MP4 ingestion.
  Do not upload full-resolution stills just because a preview exceeded the cap.

### 2. Conservative local event detection

- Day/night transitions should use the existing capture mode with a stable
  dwell period, not JPEG brightness alone (auto exposure compensates brightness).
- For scene changes, use a small grayscale analysis grid, a configurable region
  of interest and threshold, consecutive-frame confirmation, and cooldowns.
  Suppress startup, reconnect, large exposure/gain changes and mode transitions.
  Label results as scene changes, not object or threat classifications.
- Analyze only new sequence numbers from the current preview session. Reset
  the baseline on a session change, pause, camera change, or privacy change.
- Make thresholds and event categories opt-in local settings. A remote Hub
  request must not enable detection or widen the region being shared.
- Keep a bounded immutable event outbox, separate from the existing generic
  artifact HTTP uploader. Retry with the same event UUID and exact payload;
  drop terminal failures, stale events (five minutes) and consent-revoked work.
  Start with a bounded memory queue; disk persistence of images should be a
  separately disclosed choice rather than implied by existing upload settings.
- Honor the Hub's 60-second new-event cooldown and retry acknowledgments.
  Per-route receipts suppress duplicates across reconnects/restarts, but delivery
  remains at-least-once at the Discord acceptance/receipt-commit boundary.

### 3. Follow-on features

Separately review a Discord snapshot command with explicit device/channel
authorization, richer per-event routing, and any semantic/ML event detection.
None should inherit a telescope's hardware-control permissions.

## Acceptance tests before enabling a real camera

- Local mock Hub: valid/rejected/expired pairing, origin validation, credentials
  redacted, wrong installation, reconnect/revocation, duplicate connections.
- Fake preview producer: 30/60/120-second exposures, stale frame rejection,
  session fencing, pause/shutdown, snapshot denial and cancellation.
- Synthetic image sequences: stationary noise, exposure ramps, mode transitions,
  persistent scene changes, ROI masking and cooldown/hysteresis behavior.
- Delivery: bounded memory, slow/offline Hub, dropped acknowledgment, partial
  destination failure, immutable IDs, consent disabled while queued/in flight.
- Existing Rust workspace, WinUI and N.I.N.A. compatibility/build tests continue
  to pass. No automatic live posts; a real Discord smoke test needs an explicitly
  selected test device, channel and local image-sharing consent.

The Hub PR tests use mock camera WebSockets and local fake Discord endpoints.
No physical camera was opened and no production chat messages were sent while
developing this integration design.
