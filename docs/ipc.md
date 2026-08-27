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
short-lived Viewer requests to reconnect without a gap.

Implemented methods:

- status.get
- capture.pause
- capture.resume
- capture.now
- agent.shutdown

Reserved methods currently return a structured `not_implemented` error:

- cameras.list
- config.get
- config.replace
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

## Planned configuration and preview methods

config.replace carries expected_revision and a complete configuration document.
The agent validates first, writes a temporary file, atomically replaces the
previous file, increments the revision, and then applies changes through the
camera state machine.

The separate preview pipe will emit:

    4-byte metadata length
    metadata JSON
    4-byte JPEG length
    JPEG bytes

Preview metadata includes sequence, capture time, dimensions, exposure, gain,
night/day mode, and dropped-frame counters. Consumers may reconnect at any time;
the next available preview is always the newest one.
