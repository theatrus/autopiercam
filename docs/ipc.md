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

## Planned preview method

The separate preview pipe will emit:

    4-byte metadata length
    metadata JSON
    4-byte JPEG length
    JPEG bytes

Preview metadata includes sequence, capture time, dimensions, exposure, gain,
night/day mode, and dropped-frame counters. Consumers may reconnect at any time;
the next available preview is always the newest one.
