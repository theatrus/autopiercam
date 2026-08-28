# Durable HTTP upload outbox

The uploader sends finalized JPEGs without putting filesystem, database, or
network work on the camera thread. SQLite is the authoritative outbox: artifact
identity, attempts, retry deadlines, successes, and terminal failures survive
an orderly shutdown, process crash, or machine restart.

## Configuration and ledger identity

Enable `[upload]`, set an absolute HTTP or HTTPS endpoint, and choose a positive
`queue_capacity`. The endpoint must have a host and cannot contain embedded
credentials or a fragment. Use HTTPS for deployments outside a trusted
loopback network. When `bearer_token_env` is present, HTTPS is required and the
named environment variable must contain the token when the camera worker
starts. The token itself is never written to TOML or SQLite and is not included
in errors or logs. SQLite stores only a domain-separated SHA-256 fingerprint of
the effective authorization identity; anonymous upload has a distinct
fingerprint.

The endpoint is normalized to its canonical ASCII URL before use. Redirects
are disabled, so that normalized URL, including its path and query, is the only
request target; AutoPierCam never appends a filename. HTTPS certificate
validation uses the platform verifier. Connect timeout is five seconds and the
total request timeout is fifteen seconds.

When upload is first enabled, AutoPierCam creates a sidecar by replacing the
configuration filename's extension with `upload.sqlite3`. The schema-creation
transaction also inventories every existing direct child with a generated
capture name; those paths form the activation boundary. For example:

    C:\pier\autopiercam.toml
    C:\pier\autopiercam.upload.sqlite3

The ledger stores and validates the canonical capture directory, normalized
destination, and one-way authorization fingerprint. It will not open with a
different root or destination. Under exclusive ledger ownership, a different
authorization identity is accepted transactionally only when there are no
`pending`, `in_progress`, or `retrying` rows and no eligible unledgered JPEG in
the publish/record crash gap; otherwise startup fails closed. This prevents
pending or not-yet-reconciled artifacts from being silently sent to another
endpoint, tenant, or account. Completed and permanently failed history does
not block normal credential rotation. Moving or renaming the configuration
file selects a different sidecar path.

Only one active upload worker may own a ledger. It uses SQLite WAL mode,
`synchronous=FULL`, a five-second busy timeout, an exclusive live connection,
and a strict versioned schema. An unrecognized, modified, or newer schema is a
startup error rather than something the agent tries to repair automatically.

## Publication, recording, and startup reconciliation

The still writer encodes to a unique temporary file in the capture directory,
flushes and syncs it, then atomically publishes it without replacing an
existing artifact. Windows publication uses a write-through move. Only after
publication does the writer record the artifact in SQLite. Recording captures
its canonical path, filename, size, SHA-256 digest, and stable idempotency key
before a nonblocking wake is attempted.

There is necessarily a small filesystem/database boundary between publishing a
JPEG and inserting its row. Startup reconciliation closes that crash window.
It scans direct children of the configured capture directory and records
eligible generated JPEGs that have neither a job row nor a preactivation
inventory entry.

New captures use the exact generated grammar
`frame-<unix-seconds>-<three-digit-milliseconds>-<32-lowercase-hex-session-nonce>-<six-or-more-digit-sequence>.jpg`.
The 128-bit OS-random nonce makes names collision-resistant across process
restart and wall-clock correction. Reconciliation also recognizes the legacy
form without a nonce and accepts `.jpeg` for recovery compatibility.

Because activation records the paths that actually existed rather than
comparing filename timestamps, enabling upload does not adopt old captures
whose clocks happen to be in the future, and a backdated file published after
activation is still recoverable. Once a ledger exists, later startup scans
recover qualifying artifacts created after activation, including artifacts
published while upload was temporarily disabled.

Reconciliation ignores nested paths, symlinks, non-files, unrelated names, and
preactivation inventory paths. Already recorded paths are skipped before file
hashing. Direct recording returns `AlreadyRecorded` only when filename, size,
SHA-256, and idempotency identity all match; a reused path with different bytes
faults rather than silently losing the new intent.

## Durable state and recovery

Rows are never deleted and their artifact identity is immutable. Their mutable
delivery state moves through:

| State | Meaning | Restart behavior |
| --- | --- | --- |
| `pending` | Recorded and waiting for its first due attempt | Remains due |
| `in_progress` | Fenced to one active HTTP attempt | Recovered to `pending`; the consumed attempt number is retained |
| `retrying` | A retryable failure has a persisted deadline | Waits until the same deadline, then resumes |
| `completed` | A 2xx response was received | Retained as acknowledged history |
| `permanently_failed` | The endpoint or local artifact made automatic retry unsafe | Retained for operator inspection; not retried automatically |

Claims include an incrementing attempt number so stale outcomes cannot update a
later claim. Attempt results are committed before the worker reacts to a
shutdown request. A worker failure faults the capture attempt so the supervisor
can restart it; the durable rows remain available to the next worker.

Before every request, the uploader copies the source through a bounded 64 KiB
buffer into a private anonymous temporary file. It verifies the copied size and
SHA-256 against the recorded identity before HTTP can begin, closes the source,
and streams only that verified snapshot. Missing, replaced, symlinked,
non-regular, or identity-mismatched artifacts become permanent failures rather
than sending different bytes under an existing idempotency key. Later source
mutation or replacement cannot change the bytes in flight on Windows or Unix.

The upload worker never deletes a local JPEG or removes a ledger row. A separate
retention worker can reclaim only a preactivation artifact or an artifact whose
row is still `completed` with the exact recorded path, revision, size, and
SHA-256. Pending, in-progress, retrying, permanently failed, unknown, and
publish/record crash-gap artifacts are protected. The final state and identity
are rechecked while the ledger is synchronized immediately before the opened
file handle is deleted. See [Capture retention and storage
pressure](retention.md) for age, byte-quota, and upload-disabled behavior.

## Request and idempotency contract

Each attempt streams the verified file and sends:

    PUT <the normalized configured endpoint>
    Content-Type: image/jpeg
    Content-Length: <recorded finalized file size>
    X-AutoPierCam-Filename: <artifact filename>
    Idempotency-Key: autopiercam-sha256-<64 lowercase SHA-256 hex characters>-<artifact filename>
    Authorization: Bearer <token>  # only when configured

The idempotency key is stable across retries and restarts and binds both the
content digest and generated filename. Receivers should implement idempotent
PUT handling because a response can be lost after the server accepts a body.
Any 2xx response completes the row.

Transport/protocol failures and HTTP 408, 425, 429, and 5xx responses are
retryable. Base delays are 1, 2, 5, 10, 30, 60, then 300 seconds. Deterministic
80–120 percent jitter derived from the idempotency key and attempt number is
applied, with a five-minute cap. A decimal-seconds `Retry-After` value acts as a
minimum delay and is also capped at five minutes; HTTP-date values are not
interpreted. Retryable failures continue indefinitely at the capped delay.

Other non-2xx statuses are permanent failures. Local identity failures such as
a missing or changed file are also permanent.

## Operator inspection and requeue

Current agents advertise `uploads.list` and `uploads.requeue` in the status
capability list. The Viewer uses those methods for its Manage outbox dialog; it
does not open SQLite directly.

`uploads.list` returns a path-free, newest-first projection containing the
ledger id, job and revision ids, generated filename, state, recorded byte size,
attempt and requeue counts, audit timestamps, most recent HTTP/error details,
and current requeue eligibility. Requests can filter by state and select a page
size from 1 through 100. Pagination cursors are opaque and bind the ledger,
filter, page size, high-water job, and global ledger revision. Any durable
mutation invalidates an outstanding cursor, so clients must refresh rather than
combine rows from different ledger states. The Viewer does this automatically.

`uploads.requeue` accepts exactly one ledger id, job id, and expected job
revision. It succeeds only for a `permanently_failed` row whose immutable
delivery binding still matches the running worker. While holding the publication
and ledger fences, the agent rechecks the row and verifies that the canonical,
regular local file still has its recorded size and SHA-256. A missing, replaced,
changed, wrong-ledger, wrong-state, or stale-revision artifact is rejected
without changing durable state.

An accepted requeue moves the row to `pending`, increments its job revision and
requeue count, records the requeue time, and sends a coalesced worker wake when
the uploader is available. Attempt and failure history are retained. Completed
rows are immutable, and pending/in-progress/retrying rows cannot be manually
requeued. The Viewer requires a confirmation before submitting the revision-
fenced request and refreshes after either success or conflict.

## Backpressure and status telemetry

`upload.queue_capacity` bounds only a coalesced in-process wake channel. It
does not cap the number of durable rows. The still writer first commits the row
and then tries to send a wake without waiting. A full channel means a wake is
already pending; the worker polls the ledger and drains every due row, so the
artifact is not dropped. Disk capacity is the practical outbox bound.

When upload is enabled, `status.get` includes an `upload` object and the Viewer
shows the same aggregate state:

    {
      "pending": 2,
      "active": 1,
      "retrying": 3,
      "completed": 120,
      "permanently_failed": 1,
      "last_success_unix_ms": 1787952000000,
      "last_failure_unix_ms": 1787951940000,
      "last_error": "HTTP endpoint requested a retry"
    }

The three last-detail fields are omitted until available. `last_error`
describes the most recently recorded failure and is not cleared merely because
a later upload succeeds. When upload is disabled, the optional `upload` object
is absent because no ledger worker is active. Outbox methods then return a
service-unavailable error, and the capability-aware Viewer leaves Manage outbox
visible but disabled so an operator can distinguish an unavailable worker from
an older agent.

## Shutdown and operator caveats

Ordered shutdown drains the still-writer queue first, so every published JPEG
it completes is recorded and its retention wake is queued. Retention completes
that queued final sweep and exits while the ledger is still available. The
agent then revokes upload-administration access and waits for any already
accepted list/requeue operation to leave the shared ledger before stopping the
uploader. The uploader:

1. finishes an HTTP request already in flight and persists its outcome;
2. releases a claim that was fenced but not yet submitted back to `pending`;
3. interrupts retry waiting and exits without discarding pending rows.

A crash can leave `in_progress` rows; opening the ledger converts them back to
`pending`. Because the server may have accepted the earlier request, restart
delivery relies on the stable idempotency key.

Treat the ledger and its `-wal`/`-shm` sidecars as application data:

- Stop the agent before inspecting, backing up, moving, or archiving them.
- Do not delete or edit a ledger to clear an error; that can discard the only
  record of unacknowledged delivery.
- To intentionally change the capture root or endpoint, first drain or account
  for every nonterminal row, stop the agent, archive the existing ledger as a
  unit, then change configuration. The next start creates a fresh activation
  inventory and will not adopt captures already present at that point. Saving a
  different endpoint in the Viewer does not migrate the old ledger; the
  restarted worker will report a destination mismatch.
- A bearer token may rotate in place after every row is terminal and startup
  finds no eligible unledgered crash-gap JPEG. If a row is pending, active, or
  retrying—or a finalized artifact still needs reconciliation—a different
  token is rejected because the agent cannot prove that it represents the same
  remote account. Restore the prior credential, let it reconcile and drain the
  outbox, or deliberately migrate the outbox before retrying. Renaming the
  environment variable while preserving the exact token does not change the
  authorization identity.
- If the configuration file is moved, keep track of its old sidecar. The new
  path will not automatically discover or resume the old outbox.
- A permanently failed row remains terminal if its local file is merely
  restored. Preserve the ledger and artifact, inspect it in Manage outbox, and
  explicitly requeue it only after the cause is understood. The requeue still
  requires the original bytes and current ledger/delivery identity; it cannot
  substitute a repaired or regenerated file under the old idempotency key.

## Offline ledger lifecycle

Ledger maintenance is a camera-independent CLI path. Both commands dispatch
before the ZWO SDK is loaded, but the tray/console agent must be stopped. Every
configured capture run holds a shared operating-system lifecycle lease through
still-writer and retention shutdown, including when uploads are disabled;
maintenance requires the incompatible exclusive lease:

    cargo run --release -p autopiercam -- upload-ledger migrate --config autopiercam.toml
    cargo run --release -p autopiercam -- upload-ledger archive --config autopiercam.toml --expected-ledger-id <32-lowercase-hex>

`migrate` accepts only the exact released v3 schema or the exact current v4
schema. An exact v3 ledger is transformed transactionally to v4, assigned a
random immutable ledger ID, and given per-row delivery bindings and revisions.
Before schema changes and again on every v4 verification path, each persisted
job and preactivation path is decoded against the same file-independent
identity and state invariants used by the live store. Completed artifact bytes
need not remain on disk. The complete v4 schema and derived fields are verified
before the migration transaction can commit. V4 is a verified no-op that
prints its existing ledger ID. Every other version, malformed row, altered
schema, aggregate mismatch, wrong capture root, unsafe database path, or failed
integrity check is rejected without migration.

V3 did not store a delivery identity per job and allowed authorization rotation
after live work drained. A migrated v3 permanently failed row therefore keeps
a deliberately non-current binding: it remains visible for inspection, but
explicit requeue is denied rather than risking delivery to a different account.

`archive` accepts exact v4 only and requires the operator to repeat the ledger
ID shown by `migrate` or Manage outbox. It refuses to archive while any row is
pending, in progress, retrying, or permanently failed, or when a generated JPEG
is neither a durable job nor known preactivation history. Generated-name
directories, symlinks, and Windows reparse points fail closed. Retained
preactivation JPEGs are known exclusions and do not prevent a drained archive.

After a full WAL checkpoint, archive uses SQLite's backup API to create and
verify a same-directory temporary database. It compares a logical digest with
the still-locked active database, publishes without replacing an existing
name, flushes data and name changes, and prints the archive's SHA-256. Only then
does it durably move the active database to the retired name:

    autopiercam.upload.<ledger-id>.archive.sqlite3
    autopiercam.upload.<ledger-id>.retired.sqlite3

No active `autopiercam.upload.sqlite3` remains after success, so the next
upload-enabled start creates a new ledger. The retired source is retained as a
recoverable second copy; neither output is silently overwritten.

Archive recovery is state-aware. If a verified archive exists beside the
unchanged active ledger but retirement did not happen, rerunning the same
command verifies their exact logical contents and finishes retirement. If both
verified archive and retired files exist and the active file is absent, rerun
is an idempotent verification; JPEGs captured after retirement belong to the
next ledger lifecycle and do not invalidate that completed report. A changed
active ledger, an orphan archive or retired file, any archive/retired SQLite
WAL, SHM, or rollback-journal sidecar, or all three database names existing is
reported as an incomplete state and left untouched for inspection.

The persistent `<database>.maintenance.lock` file is only a coordination inode;
the held shared/exclusive OS lock is authoritative. Leaving the unlocked file
in place avoids replacement races, and it is not considered a prior ledger by
retention. Do not copy it as ledger data or use its mere presence to infer that
maintenance is running.
