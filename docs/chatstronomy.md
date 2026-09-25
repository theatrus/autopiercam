# Chatstronomy image sharing

The current source implements Windows pairing, protected credentials, outbound
HTTPS/WSS transport, Hub-requested snapshots, automatic scene/day-night posts,
and Viewer controls. **The published 0.1.0 installer predates this feature.**
Use a build containing this change; no live sharing is enabled by installing it.

The Hub implementation landed in
[device pairing #184](https://github.com/theatrus/chatstronomy/pull/184) and
[bidirectional transport #185](https://github.com/theatrus/chatstronomy/pull/185).
Deploy a Hub containing both changes before pairing. The authoritative
[wire protocol](https://github.com/theatrus/chatstronomy/blob/main/docs/DEVICE_PROTOCOL.md)
is in the Chatstronomy repository (the local Spacecat checkout). Merging a PR
does not deploy the Hub.

## Setup

1. In the Hub's **Observatory devices** page, create a pier camera and generate
   a one-use pairing code. Select its Discord destination channels. Register the
   server under Discord delivery first; adding destinations requires server
   management permission and a live bot/channel check.
2. In the AutoPierCam Viewer, choose **Chatstronomy**, enter the Hub HTTPS origin
   (for example `https://hub.example.org`), paste the code, and choose **Pair camera**.
   Pairing resets all sharing permissions to off.
3. Explicitly enable **Connect and permit image sharing** and whichever
   permissions you want: Hub snapshot requests, scene-change posts, and/or stable
   day/night transitions. Choose **Save permissions**.
4. Use **Refresh status / reload settings** to see the connection, device ID,
   installation ID, and latest confirmed delivery. The Hub's **Snapshot now**
   button sends the completed preview to that device's selected channels.

Only the entire preview is shared; there is no privacy mask or cropped sharing
region in this version. Never enable sharing if the full preview contains
something you do not want in those channels. The camera's credential does not
authorize telescope commands, and the Hub cannot enable local permissions.

**Stop sharing** disconnects and discards pending images. **Stop and forget local
pairing** also removes the credential from Windows Credential Manager. Revoke
the device credential in the Hub as well: the device protocol has no remote
revocation endpoint. Messages already accepted by Discord cannot be recalled.

Pairing codes are single-use and expire after an hour. If the response is lost
or pairing fails after consuming a code, generate a new code; pairing is never
automatically retried. Credentials never appear in command-line arguments,
ordinary camera configuration, sharing settings, status responses, or logs.

## Runtime and persistence

The tray owns one dedicated network worker, independent of capture and the
generic artifact HTTP uploader. It consumes the existing latest-frame preview
and never opens another SDK handle or changes exposure, gain, ROI, pause state,
or capture timing. Sharing errors do not stop local capture.

Non-secret preferences and the persistent random installation UUID are stored
beside the camera TOML, with its extension replaced by `.chatstronomy.json`.
For example, `autopiercam.toml` uses `autopiercam.chatstronomy.json`. A
`.chatstronomy.lock` lease prevents two sharing workers owning that installation.
Settings saves are atomic and revision-checked through dedicated sharing IPC,
so old config editors cannot silently drop or enable sharing. To change Hub,
forget the existing pairing first.

Windows Credential Manager scopes the credential to both installation UUID
and canonical Hub origin. There is no plaintext fallback on other platforms.
The portable protocol, detector and worker are tested on Linux with an injected
mock credential store; a production non-Windows store remains future work.

Headless `autopiercam run --config <same TOML>` loads the same sharing settings
and credential for the same Windows user. Stop the tray first. When sharing is
enabled at startup it creates the same preview producer used by the tray.
Configure/pair through the Viewer first; headless mode has no live settings UI.
Do not edit the sidecar while a worker is running: runtime changes go through
the Viewer, and manual file edits are read at the next startup.

Production endpoints must be HTTPS origins without credentials, paths, query
strings or fragments. Certificate validation is enabled; HTTP pairing redirects
are not followed. Loopback HTTP/WS is available only through explicit test
dependency injection, not a production Viewer preference.

Connections reconnect with bounded exponential backoff and jitter, capped at
60 seconds. Authentication/protocol rejection stops retries until a local
change. WebSocket messages are capped at 720 KiB; JPEGs are decoded with preview
dimension/allocation limits and resized/re-encoded to at most 512 KiB.

## Snapshot and event behavior

**Snapshot now** uses the latest completed frame, at most 120 seconds old,
labeled with its real capture time. It works with 30–60+ second exposures;
it never interrupts an exposure. Paused, disconnected, stale or superseded
capture sessions return unavailable. This version returns unavailable promptly
rather than waiting for a new exposure when no eligible frame exists. Requests
are bounded by a monotonic deadline of at most 90 seconds.

Automatic events are independent opt-ins:

- **Scene changes:** a 32×24 grayscale grid normalized for overall brightness;
  at least the selected percentage of cells must change materially in three
  distinct frames. Startup, session changes, mode transitions, very dark
  frames and exposure changes over 20% or gain changes over 10 reset/suppress
  the detector. The full preview is the analysis region.
- **Day/night transitions:** use capture-mode metadata with a 30-second dwell,
  not JPEG brightness alone.

These are observations, not classifications of people, animals, meteors,
intrusions, weather or threats. There is a 60-second new-event cooldown.
Only one immutable automatic event is retained in memory; newer events are
dropped while it is pending. A retry or lost acknowledgment resends the exact
event UUID and payload after at least 60 seconds. Events expire after five
minutes. Snapshots and their retries stay within their original request and
connection. Consent changes, pause/restart and session changes clear pending
work. No outbox images are written to disk.

The Hub owns destination routing and per-route receipts. Delivery is
at-least-once, not exactly-once: a Hub crash after Discord accepts an image can
produce a duplicate. Camera messages cannot supply arbitrary image URLs or
channel IDs. Discord mentions are disabled by the Hub.

## Validation and follow-ons

Tests use synthetic JPEGs, injected credentials and a local mock HTTP/WebSocket
Hub. They cover pairing/rejection, redirect refusal, origin binding, persistent
identity, revision conflicts, snapshot consent and freshness, scene detection,
revoked authentication, cancellation and an actual 60-second immutable retry
across reconnect. A separate opt-in test writes and removes only one synthetic
Windows Credential Manager entry. No real camera or Discord channel is needed.

Follow-ons, not prerequisites for image sharing: selectable detection regions
and privacy masks, a non-Windows OS credential store, a Discord snapshot slash
command, richer event routing, and explicitly reviewed semantic detection.
