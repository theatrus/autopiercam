# Windows-native video and daily timelapses

Windows-native security video and daily timelapses are available in 0.2.27.
Version 0.2.28 adds local-clock and sunset-to-sunrise schedules.

Both video modes use Windows Media Foundation to produce H.264 MP4, without an
external executable or codec download. Hardware transforms are allowed; Windows
may use its software encoder. Encoding runs in an isolated AutoPierCam child
process, not the camera thread. Media DLLs are loaded from System32 only when
encoding/decoding is requested. Windows N editions need the Media Feature Pack
for video; missing media support does not prevent ordinary capture or settings.

Enable either mode independently in Viewer Settings → Imaging:

```toml
[video]
enabled = true              # real-time security clips from preview
daily_timelapse = true      # accelerated daily clips from saved stills
segment_seconds = 60
frames_per_second = 2        # security sampling only
```

Both are disabled by default. An obsolete `ffmpeg_path` setting is accepted when
reading an old configuration but is never executed and is omitted on the next
configuration save. The Viewer no longer offers an external encoder path.
Video recording on non-Windows capture agents is rejected explicitly.

## Security video

Samples the latest preview JPEG, using real elapsed gaps between samples.
Segments accept 1–600 seconds and sampling rates of 1–30 fps; the available
preview rate and exposure duration may limit new samples further. It does not
increase acquisition rate or provide full-resolution planetary video.

Pause and retention pressure suspend sampling while exposure/live preview
continue. Segments finish at duration/size limits or orderly shutdown.
The private JPEG spool is limited to 256 MiB and 1,200 samples. Encoding has a
20-second deadline; a failed encoder faults the recording attempt with a log
message. An interrupted active security segment is not recovered after a crash.
Encoding on the sampler thread can introduce sampling gaps.

## Daily timelapse

The still writer takes the first successfully saved still in each UTC minute.
It makes a private JPEG copy, scaled to fit a 1280 × 720 black canvas without
stretching. RAW8 JPEG and RAW16 PNG captures are supported. Sampling does not
depend on preview FPS and never requests, aborts, or restarts an exposure.

Optional [observation overlays](chatstronomy.md#daily-timelapse-overlays)
add the target, saved LIGHT exposure count and a selected focuser/external
temperature to future private samples. Values are captured with each frame and
omitted when unknown or stale; original stills remain unchanged.

Samples play at 24 source frames per second. A fully sampled UTC day produces
about 60 seconds of video. Missing minutes are omitted rather than filled in;
a partial day produces a shorter clip. Existing configurations still close at
midnight UTC. The background worker checks closed windows on startup and every
30 seconds while recording services run; encoding and network cooldown can delay
delivery beyond that check.
It also finishes days missed while AutoPierCam was closed after the next enabled
start. Disabling the feature leaves unfinished samples on disk for a later
enabled run; it does not create a partial-day movie or backfill older stills.

Private samples live in `.autopiercam-timelapse/` under the capture directory,
keyed by UTC day for legacy schedules or by immutable window metadata for new
schedules. Their total is capped at 1 GiB, each sample at 512 KiB, and each window
at 1,560 samples (including 25-hour daylight-saving days). Both private video
spools, metadata, abandoned staging files and
in-progress movie output count toward the shared retention byte quota as
protected storage. Retention never deletes unfinished samples to make room.
Video writers check current usage and the minimum-free-space rule before new
samples; encoders check before starting, once a second, and before publication.
Pressure skips samples, discards an unfinished security segment, or defers a
daily clip while retaining its samples. Failed daily output is removed before
retry. These are retention limits, not hard filesystem reservations: concurrent
writes, encoder buffering and the checking interval can briefly overshoot.
Sampling also stops with a warning when the private sample cap is full.
Failures never remove the already saved still.
Stills can be retained/deleted independently once their sampled pixels are copied.

The spool survives restart, skips occupied minute slots, and seals closed days
against backfill/clock rollback. A failed daily encoder keeps its samples and
retries on a later check; its process has a ten-minute deadline and is cancelled
on shutdown. After successful encoding, a synced SHA-256 receipt allows recovery
from a crash between movie publication and completion bookkeeping. Conflicting
existing movies are never replaced. Samples are removed only after publication
and upload-ledger registration (when enabled). Small completion markers remain
so retained-away movies are not regenerated.

### Record a night and send it at noon

In **Settings → Imaging**, enable daily timelapses and enter the observing
latitude and longitude. Select **Night — sunset to sunrise**, keep **Sunset to
sunrise** as the night boundary, and enter an IANA timezone such as
`America/Los_Angeles`. Choosing Night presets **At a local clock time**, **12:00**;
you can change the time or choose immediate sending. Enable **Share finalized
daily timelapses** separately in Chatstronomy settings, then save both tabs.
No machine settings or sharing permissions are changed merely by upgrading.

```toml
[video.timelapse_schedule]
window = "dusk_to_dawn"       # utc_day, local_noon, or dusk_to_dawn
timezone = "America/Los_Angeles"
twilight = "sunset"           # sunset, civil (-6°), astronomical (-18°)
delivery = "local_time"       # immediate or local_time
send_minute = 720             # minutes after local midnight: 12:00
```

Night windows run from estimated sunset to the following sunrise, crossing
midnight as one clip. The calculation uses the observing coordinates, not the
computer's location, and includes civil twilight by default. Solar boundaries
use the existing NOAA approximation rounded to a minute, without terrain or
weather correction. No complete pair of crossings, including polar day/night,
means no night samples or clip. **Local noon to noon** instead includes daylight
and night in one local-calendar window.

The selected IANA timezone determines calendar boundaries and send times. A
local-noon window can be 23 or 25 hours across DST. An ambiguous send time uses
its first occurrence; a nonexistent time moves to the first valid minute after
the gap (up to three hours). Exceptional skipped dates fail closed. Send time
means the first chosen local clock time at or after the window closes, never a
promise to upload before encoding finishes. AutoPierCam recording services and
an active sharing session must be available; it does not wake a sleeping PC or
start a stopped camera. Changes reload recording services without aborting an
exposure or reopening the camera.

New schedules save a bounded delivery intent with the finished movie. It can
wait across a restart or offline period, but expires **24 hours after window
close**, matching the existing Hub timestamp limit. Choosing a later send time
leaves less catch-up time; a boundary beyond that limit leaves the movie local.
Changes to the schedule, coordinates, sharing revision or pairing cancel the
old intent. Enabling sharing later does not scan old movies. Existing UTC-day
immediate schedules retain their one-time, completion-only sharing behavior.

Pending movies are protected from retention until their send is attempted,
cancelled or expired, but still count toward storage limits. No space means
recording/encoding can pause rather than purge that movie. After an attempt,
ordinary retention applies to the original; the existing immutable upload
snapshot protects in-flight bytes. A durable claim prevents duplicate posts
after restart: a crash between claiming and queueing may lose a send, never
replay it. HTTP retries remain bounded to identical bytes, UUID and timestamp.
Disabling timelapse recording stops its scheduler; resuming only catches up
within the same expiry bound. Original stills are never deleted by this queue.

## Publication and timing

Both modes sync and publish completed movies atomically without overwrite.
They use the existing `frame-<seconds>-<milliseconds>-<nonce>-<sequence>.mp4`
grammar, upload ledger, and protected retention rules. Daily movies use the
window-end timestamp and a timelapse namespace nonce. HTTP PUT uses
`video/mp4`; receivers must accept it before uploads are enabled. Still-image
counters exclude movies; worker logs report completed clips and errors.

Daily clips can also be posted to Discord through Chatstronomy with the separate
**Share finalized daily timelapses** permission. This does not share security
recording segments. Clips above the Hub's advertised limit (or the independent
512 MiB client safety ceiling) remain local. Uploads stream a private immutable
file snapshot; no movie is resized, split, or deleted to fit. Discord can still
reject a clip. See [Chatstronomy sharing](chatstronomy.md#daily-timelapses).
The local-clock send schedule applies to Chatstronomy only; a separately enabled
generic HTTP PUT uploader still receives the movie immediately after publication.

The native pipeline disables the video processor's frame-rate conversion.
It retains source timestamps and adds one terminal hold sample where necessary
to preserve the final frame's duration through the Windows H.264 encoder.
This does not invent intermediate scene motion or expand sparse recordings to
constant-rate duplicate frames. Resolution changes within a security segment
fail rather than stretch or silently mix incompatible frames.

## Hardware-free validation

```powershell
cargo test -p autopiercam native_video --lib --locked
cargo test -p autopiercam timelapse --lib --locked
cargo test -p autopiercam --test native_video --locked
```

Synthetic tests exercise the actual Windows encoder and decoder, colours and
orientation, elapsed-time and daily playback duration, process deadlines,
failed-encode cleanup, no-overwrite publication, UTC rollover, bounded caching,
restart, and publication recovery. They do not open cameras, install anything,
or upload images.

The design follows the native-media approach in
[Seiza's video exporter](https://github.com/theatrus/seiza-win/blob/main/src/Seiza.App/Services/ParallaxVideoExporter.cs).
AutoPierCam uses the lower-level Media Foundation sink writer from Rust;
see [Microsoft's sink writer documentation](https://learn.microsoft.com/en-us/windows/win32/medfound/using-the-sink-writer).
