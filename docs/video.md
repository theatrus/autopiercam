# Windows-native video and daily timelapses

These are development changes after 0.2.26; the published 0.2.26 installer
predates the native encoder.

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
a partial day produces a shorter clip. Clips close at midnight UTC, not local
midnight (therefore no daylight-saving ambiguity). The background worker checks
closed days on startup and every five minutes while recording services run.
It also finishes days missed while AutoPierCam was closed after the next enabled
start. Disabling the feature leaves unfinished samples on disk for a later
enabled run; it does not create a partial-day movie or backfill older stills.

Private samples live in `.autopiercam-timelapse/<UTC epoch day>/` under the capture
directory. Their total is capped at 1 GiB, each sample at 512 KiB, and each day at
1,440 samples. Both private video spools, metadata, abandoned staging files and
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
retries after five minutes; its process has a ten-minute deadline and is cancelled
on shutdown. After successful encoding, a synced SHA-256 receipt allows recovery
from a crash between movie publication and completion bookkeeping. Conflicting
existing movies are never replaced. Samples are removed only after publication
and upload-ledger registration (when enabled). Small completion markers remain
so retained-away movies are not regenerated.

## Publication and timing

Both modes sync and publish completed movies atomically without overwrite.
They use the existing `frame-<seconds>-<milliseconds>-<nonce>-<sequence>.mp4`
grammar, upload ledger, and protected retention rules. Daily movies use the UTC
day-end timestamp and a fixed timelapse namespace nonce. HTTP PUT uses
`video/mp4`; receivers must accept it before uploads are enabled. Still-image
counters exclude movies; worker logs report completed clips and errors.

Daily clips can also be posted to Discord through Chatstronomy with the separate
**Share finalized daily timelapses** permission. This does not share security
recording segments. Clips above the Hub's advertised limit (or the independent
512 MiB client safety ceiling) remain local. Uploads stream a private immutable
file snapshot; no movie is resized, split, or deleted to fit. Discord can still
reject a clip. See [Chatstronomy sharing](chatstronomy.md#daily-timelapses).

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
