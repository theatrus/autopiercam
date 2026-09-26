# Chatstronomy image sharing

The current source implements Windows pairing, protected credentials, outbound
HTTPS/WSS transport, Hub-requested snapshots, automatic scene/day-night posts,
periodic images, chat-configurable triggers and telescope-event image bursts,
and Viewer controls, included in **AutoPierCam 0.2.0**.
No live sharing is enabled by installing or upgrading the application.

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
   (for example `https://hub.example.org`) in **1. Pair with your Hub** at the top,
   paste the code, and choose **Pair camera**. Pairing preserves your source
   choices and limits, but keeps the master sharing switch off. Options can be
   configured and saved before pairing. A rejected code does not clear them.
3. In **2. Choose what to share**, select snapshots and a periodic interval;
   expand **Scene and telescope events** or **Chat control** for other options.
   Turn on **Enable image sharing** and choose **Save and enable sharing**.
   Save/status controls stay visible while the settings scroll. Later edits use
   **Save settings**, without restarting capture.
4. Use **Refresh status** to update connection/delivery information without
   discarding edits. **Connection details / change Hub** shows the device ID,
   installation ID, and latest confirmed delivery. The Hub's **Snapshot now**
   button sends the completed preview to that device's selected channels.

Only the entire preview is shared; there is no privacy mask or cropped sharing
region in this version. Never enable sharing if the full preview contains
something you do not want in those channels. The camera's credential does not
authorize telescope commands, and the Hub cannot enable local permissions.

**Stop sharing** disconnects and discards pending images without losing other
unsaved edits. **Forget pairing**, under **Connection details / change Hub**,
requires confirmation and clears permissions and the Windows credential. Revoke
the device credential in the Hub as well: the device protocol has no remote
revocation endpoint. Messages already accepted by Discord cannot be recalled.

Pairing codes are single-use and expire after an hour. If the response is lost
or pairing fails after consuming a code, generate a new code; pairing is never
automatically retried. Credentials never appear in command-line arguments,
ordinary camera configuration, sharing settings, status responses, or logs.

Unsaved changes are marked explicitly; save or **Discard changes** before closing.
If settings changed elsewhere (including chat), refresh preserves your draft and
asks you to either discard it or **Keep my edits** before saving a replacement.
An unconfirmed agent status blocks pairing/saves until a successful refresh.

## Triggered sharing and chat configuration

The Viewer provides a periodic interval (0 disables, otherwise 1–1440 minutes),
a telescope-event opt-in, and an event burst size of 1–3 distinct images spaced
60–600 seconds apart. Periodic sends are single images. The first scheduled send
waits a full interval; reconnects reset the interval and discard unfinished bursts
instead of replaying a backlog. The device-wide 60-second cooldown still applies.

Enable **Allow the camera owner to configure triggers from chat** locally to use
`/piercam triggers` in a channel receiving that camera. Supply the camera's exact
Hub name, interval, scene/day-night/telescope switches, burst count and spacing.
For example, set interval to 10 minutes, telescope events on, count 3, spacing 60.
Chat may only narrow local permissions: it cannot enable a locally disabled
source, send more frequently than the local interval, shorten spacing, increase
the burst cap, or enable snapshots/sharing. To permit periodic chat configuration,
first choose a nonzero local interval. Chat can set it to 0 and later restore it
within that local limit. Only the camera owner may invoke these commands; server
manager privileges alone do not grant access. DMs and unrelated channels fail.

Chat overrides are atomically saved on the camera. **Refresh status** shows the
active rules as well as local permission limits. Any local **Save settings**
clears overrides. A stale Viewer save fails with a revision conflict instead of
overwriting a newer chat update. Pairing preserves trigger permissions but keeps
sharing off; forgetting clears permissions. Both reset chat overrides.
`/piercam snapshot camera:<name>` uses the existing separate local snapshot gate
and posts to all of the camera's configured destinations, not just the command
channel. Triggered images likewise use all configured camera destinations.

Telescope triggers currently cover slew start/end and sequence start/finish.
The Hub admits only fresh (up to 30 seconds), new, chat-enabled events from a
telescope with the **same owner and at least one shared guild/channel route**.
Startup history, disabled events and cross-owner telescopes do not trigger posts.
Camera event bursts wait for a frame completed after receipt; subsequent images
must have new sequence numbers in the same capture session. Long exposures can
delay delivery beyond the selected spacing. There is no pre-event image buffer.
An active burst coalesces additional triggers; the next event cannot create an
unbounded queue. A burst expires after 180 seconds plus its configured spacing
between images; pause/session/consent changes cancel it. Failed delivery can
reduce the number of images actually posted. This is not a security alarm.

Periodic/chat/telescope features require the companion
[Hub protocol-v2 update (#186)](https://github.com/theatrus/chatstronomy/pull/186).
Pairing remains v1; old v1 cameras still connect to the updated Hub. AutoPierCam
uses a v2 WebSocket only when these features are enabled and stops with an
operator-visible protocol error against an older Hub, rather than silently
ignoring the configuration. Neither these sources nor their installers are
deployed automatically.

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
Only one immutable automatic image and one bounded burst plan are retained in
memory; new triggers are coalesced while that burst is active. A retry or lost acknowledgment resends the exact
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
and privacy masks, a non-Windows OS credential store, richer selectable event
filters, and explicitly reviewed semantic detection (for example roof/person labels).
