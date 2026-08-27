# Local protocol sketch

Protocol version 1 uses unsigned 32-bit little-endian message length followed by
one UTF-8 JSON object. Every control request has:

    {
      "version": 1,
      "request_id": "01J...",
      "method": "status.get",
      "payload": {}
    }

Every response echoes version and request_id and contains exactly one of result
or error. Initial methods:

- status.get
- cameras.list
- config.get
- config.replace
- capture.pause
- capture.resume
- capture.now
- artifacts.list
- agent.shutdown

config.replace carries expected_revision and a complete configuration document.
The agent validates first, writes a temporary file, atomically replaces the
previous file, increments the revision, and then applies changes through the
camera state machine.

The preview pipe emits:

    4-byte metadata length
    metadata JSON
    4-byte JPEG length
    JPEG bytes

Preview metadata includes sequence, capture time, dimensions, exposure, gain,
night/day mode, and dropped-frame counters. Consumers may reconnect at any time;
the next available preview is always the newest one.

