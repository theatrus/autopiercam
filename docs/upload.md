# Best-effort HTTP upload

The current uploader sends completed JPEGs to an HTTP endpoint without putting
network work on the camera or still-writer threads. It is intentionally
best-effort: finalized images are durable on disk, while upload intents and
retry state exist only in the running process. SQLite-backed recovery is the
next milestone.

## Configuration and credentials

Enable `[upload]`, set an exact absolute endpoint, and choose a positive bounded
`queue_capacity`. The endpoint must have a host and cannot embed credentials or
a URL fragment. Use HTTPS. When `bearer_token_env` is present, HTTPS is required
and the named environment variable must contain the token when the camera
worker starts. The token itself is not stored in TOML or included in errors and
logs.

HTTPS certificate validation uses the platform verifier. Redirect following is
disabled, so the validated URL, after standard URL normalization and including
its path and query, is the only request target. Connect timeout is five seconds
and total request timeout is fifteen seconds.

## Artifact and request contract

The still writer encodes to a unique temporary file, flushes and syncs it, and
atomically publishes it without replacing another artifact. Only after that
publication succeeds does it try to enqueue the path. Enqueue never waits for
capacity, so a full or stopped upload queue cannot delay capture.

The dedicated upload thread opens and streams the file; it does not copy the
whole JPEG into memory. Each attempt sends:

    PUT <the normalized configured endpoint>
    Content-Type: image/jpeg
    Content-Length: <finalized file size>
    X-AutoPierCam-Filename: <artifact filename>
    Idempotency-Key: autopiercam-<artifact filename>
    Authorization: Bearer <token>  # only when configured

The filename-derived idempotency key is stable for every retry of that artifact.
Any 2xx response is success. The uploader does not delete the local file after
success, failure, or queue rejection.

## Retry and shutdown behavior

Transport/protocol failures and HTTP 408, 425, 429, and 5xx responses retry the
same file. Base delays are 1, 2, 5, 10, 30, 60, then 300 seconds. A deterministic
80–120 percent jitter derived from filename and attempt is applied, with a
five-minute cap. A decimal-seconds `Retry-After` value acts as a minimum delay
and is also capped at five minutes; HTTP-date values are not interpreted. Other
non-2xx statuses are permanent failures for that in-memory intent.
Retryable failures continue at the capped delay until success, a permanent
response, or shutdown.

A full queue or permanent failure leaves the published JPEG untouched for
inspection or later recovery. On orderly stop, the worker lets the current
request finish within its timeout, interrupts any retry wait, and abandons
pending in-memory intents. A crash or restart likewise loses unacknowledged
intent state; the agent does not yet scan existing files and replay them.

The planned durable slice will record artifacts, attempts, acknowledgements,
and retention state in SQLite, reconcile the spool at startup, and resume
unacknowledged uploads with the same stable idempotency keys.
