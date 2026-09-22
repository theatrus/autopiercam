# Security video

AutoPierCam can record preview-resolution H.264 MP4 segments without blocking
camera acquisition. Install a trusted FFmpeg build with **libx264** support
separately, then set its full executable path and enable **Security video** in
the Viewer. FFmpeg is not included in the MSI. The official
[FFmpeg download page](https://ffmpeg.org/download.html) links Windows builds;
their licenses apply separately. AutoPierCam invokes the executable directly,
never through a shell or an automatic PATH search.

Equivalent configuration (Windows TOML literal string):

```toml
[video]
enabled = true
ffmpeg_path = 'C:\Tools\ffmpeg\bin\ffmpeg.exe'
segment_seconds = 60
frames_per_second = 2
```

Segments accept 1–600 seconds and sampling rates 1–30 fps, but the shared preview
producer currently caps useful new samples at **2 fps**, and long exposures
reduce that further. This is not full-resolution planetary video. Actual elapsed
gaps between sampled frames determine playback timing; no synthetic intermediate
frames are created. Encoding may introduce sampling gaps. Completed segments
use yuv420p H.264 with CRF 28 and include fast-start MP4 metadata.

Pause and retention pressure suspend new sampling while exposure and live
preview continue. An active segment is finalized on duration/size limits or
orderly shutdown. Each private JPEG spool is bounded to 256 MiB and 1,200
samples; encoding is limited to two threads and a 20-second deadline. Encoder
failure faults the capture attempt with an actionable log message. Uploads and
retention stay alive until final segment publication has drained.

Completed files are atomically published without overwrite as
`frame-<seconds>-<milliseconds>-<session nonce>-<sequence>.mp4`. They use the
same durable upload ledger and protected-retention rules as JPEG/PNG stills;
HTTP PUT carries `video/mp4` (RAW16 stills carry `image/png`). Still-image counters
exclude video segments; logs report every completed segment. Receivers must
accept these media types before you enable their upload.

Only completed MP4 files are queued for upload. A power loss can lose the active
segment and leave its private `.autopiercam-video-*` scratch directory; recovery
of unfinished segments is not implemented. Previously completed files remain
usable. Configure disk limits and leave room for a spool and its encoded output.

Developer encoder tests use a pinned, hash-verified external build:

```powershell
$env:AUTOPIERCAM_TEST_FFMPEG = ./scripts/Get-FFmpegForTests.ps1
cargo test -p autopiercam video::tests -- --include-ignored
```
