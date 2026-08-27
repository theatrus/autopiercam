# Local protocol v1

Protocol version 1 uses unsigned 32-bit little-endian message length followed by
one UTF-8 JSON object. Every control request has:

    {
      "version": 1,
      "request_id": "01J...",
      "method": "status.get",
      "payload": {}
    }

Every response echoes version and request_id and contains exactly one of result
or error. Frames larger than 1 MiB are rejected before allocation.

The Windows control endpoint is `\\.\pipe\autopiercam-control-v1`. The tray
creates it with a protected DACL granting the current logon SID access and sets
`PIPE_REJECT_REMOTE_CLIENTS`. It also requests the first pipe instance so a
second tray process cannot silently claim the same endpoint. A fresh listening
instance is created before each connected client is serviced, allowing
short-lived Viewer requests to reconnect without a gap. Protocol v1 serves one
request per connection, and disconnects a client that does not finish sending
that request within five seconds so an idle connection cannot monopolize the
control endpoint.

Implemented methods:

- status.get
- capture.pause
- capture.resume
- capture.now
- config.get
- config.replace
- agent.shutdown

Reserved methods currently return a structured `not_implemented` error:

- cameras.list
- artifacts.list

`status.get` returns:

    {
      "state": "starting|idle|capturing|paused|faulted|stopping",
      "camera": { "id": 0, "name": "ZWO ASI676MC" },
      "frames_captured": 42,
      "frames_saved": 4,
      "last_artifact": "captures/frame-....jpg"
    }

`camera`, `last_artifact`, and `last_error` are omitted when unavailable.

The Viewer opens one pipe connection per request and serializes its requests.
It enforces a 2-second connection timeout, 30-second response timeout, matching
request ids, protocol version 1, and the result/error exclusivity rule.

## Configuration methods

`config.get` returns a content-derived revision and the complete validated
configuration document:

    {
      "revision": 11968056021619285017,
      "config": {
        "camera": { "...": "..." },
        "capture": { "...": "..." },
        "upload": { "...": "..." },
        "video": { "...": "..." },
        "api": { "...": "..." }
      }
    }

`config.replace` carries `expected_revision` and a complete configuration
document. Complete means every field emitted by the current schema is present,
including optional fields whose value is null; unknown fields are rejected.
The agent validates the document and expected revision, syncs a unique temporary
file, atomically replaces the TOML file, and schedules a controlled camera
restart. Success is precise about those two completed/accepted steps:

    { "revision": 42, "saved": true, "restart_scheduled": true }

The revision is derived from canonical configuration content, so a no-op save
can retain the same revision. A stale write returns `revision_conflict` with
`expected_revision` and `current_revision` details. If persistence succeeds but
the worker has already stopped, the response is the structured
`config_saved_agent_stopped` error with the persisted revision; clients must
refresh before another edit.

## Preview stream v1

Preview uses the separate outbound-only endpoint
`\\.\pipe\autopiercam-preview-v1`; it is not a control method. The tray applies
the same protected current-logon-SID DACL and remote-client rejection as the
control pipe, requests the first pipe instance, and never accepts bytes from a
preview client. Keeping preview separate means a stalled image consumer cannot
delay status, configuration, or shutdown commands.

Each record is:

    uint32 little-endian metadata length (1..4096)
    UTF-8 metadata JSON
    uint32 little-endian JPEG length (1..4194304)
    JPEG bytes

Readers reject zero or oversized lengths before allocating payload storage.
Metadata is strict: unknown or missing fields, unsupported enum values, and
invalid dimensions are rejected. JPEG data must carry the JPEG start and end
markers. The complete metadata object is:

    {
      "version": 1,
      "session_generation": 4,
      "sequence": 128,
      "captured_at_unix_ms": 1725000000123,
      "width": 1280,
      "height": 960,
      "exposure_us": 12500,
      "gain": 120,
      "content_type": "image/jpeg",
      "mode": "unknown",
      "dropped_frames": 3
    }

`exposure_us` and `gain` are required but may be null when reliable telemetry
is unavailable. `mode` is `unknown`, `day`, or `night`; the current SDK-auto
controller reports `unknown`. Width and height must each be at most 1280 and
their product must not exceed 1,638,400 pixels.

The producer samples no more often than every 500 milliseconds, including while
scheduled still capture is paused. A one-slot queue replaces pending work with
the newest RAW8 frame and counts discarded candidates. A dedicated thread
debayers and aspect-preserving downscales the image to a 1280-pixel edge, then
encodes JPEG at quality 75. Session generation changes for each camera attempt,
sequence increases across the tray process, and `dropped_frames` is cumulative
for the active preview session. Ending or restarting a camera session clears
the published frame and prevents an old session from publishing later.

A newly connected client receives the newest published frame, then only newer
frames. The server keeps a replacement listener present while one client is
streaming and allows two seconds for an entire frame write; a slow or broken
consumer is disconnected without stopping the listener. Clearing a session
closes an established active stream. The Viewer reconnects with bounded 250 ms, 500 ms,
1 s, 2 s, then 5 s delays, uses a two-second connection timeout, and requires a
record to finish within five seconds after its first byte arrives. Waiting for
the next record's first byte is intentionally unbounded.

The Viewer exposes connecting, waiting, live, and reconnecting stream states,
clears an image when the connection epoch changes, and verifies metadata,
monotonic sequence/session values, JPEG markers, and decoded dimensions. A live
image is marked stale after five seconds without a newer frame; malformed or
undecodable data is shown as a frame error.
