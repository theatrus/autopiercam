# Capture retention and storage pressure

AutoPierCam retention is a separate background worker. It never runs on the
camera thread, never deletes upload-ledger rows, and does not treat every JPEG
in the capture directory as application-owned data. A synchronous initial
sweep completes before the camera is opened. Later sweeps run every 60 seconds
and after each still has been atomically published and, when upload is enabled,
durably recorded in SQLite. Wake notifications use a one-slot coalescing queue.

## Policy

The `[capture]` table accepts these retention fields:

    [capture]
    keep_latest = true
    retention_days = 14
    # retention_max_bytes = 107374182400
    # retention_min_free_bytes = 10737418240

- `retention_days` deletes eligible captures at or beyond that age. Zero
  disables the age rule.
- `retention_max_bytes` is an optional maximum for all managed captures,
  including protected captures. Omit it to disable that limit.
- `retention_min_free_bytes` is an optional minimum available-byte target on
  the capture volume. Omit it to disable that limit.
- `keep_latest` protects the newest managed capture even when another rule
  would otherwise select it.

The two optional byte limits must be greater than zero when present. If both
byte limits are set, both must be satisfied. Age-expired eligible files are
selected first, then the oldest remaining eligible files are selected until
the byte targets are met. No retention worker is started when the age rule and
both byte rules are disabled; `status.get` then omits storage sweep data even
though the build still advertises its retention capability.

The Viewer exposes the two optional byte limits in MiB. It advertises and edits
them only when the agent reports the `storage.retention` capability; with an
older agent, already loaded values remain read-only and are preserved.

## Managed and protected artifacts

The inventory includes only regular, non-reparse-point direct children of the
capture directory whose names exactly match AutoPierCam's current or legacy
generated JPEG grammar. Nested files, symbolic links/reparse points, unrelated
filenames, temporary files, and other extensions are ignored rather than
deleted.

When no upload ledger exists, managed captures are locally reclaimable. Once a
ledger exists, it is authoritative for deletion safety:

| Artifact relationship | Retention decision |
| --- | --- |
| Completed upload row with current size, digest, revision, and path | Reclaimable |
| Preactivation artifact recorded when upload was first enabled | Reclaimable |
| Pending, in-progress, or retrying upload row | Protected |
| Permanently failed upload row | Protected so an operator can investigate or requeue it |
| Generated file in the publish/record crash gap or otherwise unknown to the ledger | Protected |

The retention worker copies a classification from the already-open ledger, but
that is not deletion authority by itself. Immediately before deleting, it holds
the same publication/store synchronization used by the still writer. A
completed row must still have the exact state, job revision, path, size, and
SHA-256 identity. A preactivation marker must still exist with no job recorded
for that path. The Windows deleter opens the file without following reparse
points, denies concurrent writes, verifies the opened handle and authorized
size, hashes completed artifacts, and marks that handle for deletion. A
pathname replacement, content change, state transition, or failed recheck
retains the artifact.

If upload is disabled while the ledger database, WAL/SHM sidecar, or maintenance
marker still exists, AutoPierCam cannot consult the authoritative state and
protects every managed capture. This is deliberately fail-closed; do not delete
or move ledger files while the agent is running to make space.

## Pressure and capture behavior

`status.get` publishes the latest sweep as a `storage` object with:

- managed, protected, and reclaimable byte counts;
- capture-volume free bytes when available;
- sweep time and the files/bytes reclaimed by that sweep;
- `ok`, `cleanup_needed`, or `blocked` pressure;
- `capture_suspended` and an optional error description.

`cleanup_needed` means a cleanup action could not be completed but configured
byte targets are currently satisfied. `blocked` means a configured managed-byte
or free-space target remains unsatisfied after every currently authorized
deletion, or the worker cannot safely establish the information needed to
enforce a byte target.

While pressure is blocked, scheduled still persistence is suspended so it does
not continually add data that cannot be reclaimed. Camera draining and preview
continue, and Capture now remains an explicit operator override. A later
successful sweep clears the suspension automatically. An unexpected retention
worker stop faults the active camera attempt so the supervisor can restart it
instead of continuing without the configured guard.

## Shutdown

The still writer drains before retention is stopped, so each accepted still is
fully published, entered in the upload ledger when enabled, and followed by a
retention wake. Shutdown then completes the queued final sweep, stops retention,
revokes and drains upload-administration access to the shared ledger, and only
then stops the uploader. This order prevents retention or an operator request
from racing ledger teardown.
