# AutoPierCam Vision

Portable Rust CPU inference and local training-image preparation. **No trained
roof or cloud model ships yet.** This crate is not connected to capture, the
Viewer, or Chatstronomy; it never opens cameras or sends images.

Requires Rust 1.91+. Dataset tools are enabled by default. `--features onnx`
enables tract 0.23.8's CPU backend, with transformer/GPU features disabled. There
is no Python, ONNX Runtime DLL, CUDA, DirectML, or network dependency at runtime.
CI runs the same generated ONNX convolution/pooling test on Windows x64,
Linux x64/ARM64, and macOS ARM64. This tests the runtime contract, not detector accuracy.

## Start a local dataset

```powershell
cargo run -p autopiercam-vision -- import `
  --source P:/_Incoming/starfront-redcat61/captures `
  --output datasets/starfront-redcat61 `
  --site starfront-redcat61 --camera piercam --group initial-archive `
  --interval-seconds 300 --limit 200

cargo run -p autopiercam-vision -- review --dataset datasets/starfront-redcat61
cargo run -p autopiercam-vision -- check --dataset datasets/starfront-redcat61
```

Import again to pick up files still copying. A run takes a bounded snapshot of
the directory; it does not watch it. Files modified in the last 30 seconds,
files changing during the read, and incomplete JPEG/PNG files are skipped for
the next run. Images are fully decoded with size limits before copying. Source
files are never modified. SHA256-addressed copies deduplicate identical images;
existing annotations are preserved. Corrupt files are listed in the report.

Sampling uses capture time from AutoPierCam's generated filename, not copy time:
one image per five-minute bucket, site, camera, and group. Unknown filenames
have no inferred timestamp and are not time-sampled. `--interval-seconds 0`
imports every distinct complete image up to `--limit`. Use that for a **separate
transition batch** so partial roofs and brief obstructions are not missed.
No exposure or gain is guessed from the JPEG; keep original sidecars separately
if available for a future metadata importer.

Choose a consistent `--group` for each observing night/session. The initial
archive can conservatively be one group until its sessions are curated. Training
and evaluation must hold out whole groups, never randomly split adjacent frames.
Do not claim accuracy from a single-group dataset. Site/camera identify a fixed
view; use a new camera/profile ID after moving the camera or lens.

Datasets contain private images and absolute source paths. `/datasets/` is
ignored by the repository, and the importer writes a local `.gitignore` too.
Do not commit datasets, model weights, or screenshots without explicit review.

## Label images

Open the generated `review.html` in a local browser. It contains no remote scripts
or network calls. Choose labels, then **Download labels.json** before closing.
Changes are not automatically saved. Apply the downloaded file:

```powershell
cargo run -p autopiercam-vision -- label `
  --dataset datasets/starfront-redcat61 --file C:/path/to/labels.json
```

The download contains only edited images and their original labels. New imports
do not invalidate your work. Conflicting newer labels on the same image reject
the whole batch rather than overwrite it; unchanged replays are safe. Regenerate
the review after applying. All edits are validated before an atomic manifest
replacement. Imports and annotation updates hold an exclusive dataset lock.

- Roof: `open`, `partial`, `closed`, `uncertain`.
- Sky: `clear`, `partly_cloudy`, `overcast`, `not_visible`, `uncertain`.
- Quality: `usable`, `too_dark`, `saturated`, `obscured`, `uncertain`.
- Blank: not reviewed, not a negative example.

Use `not_visible`, not `overcast`, when the roof hides the sky. Keep ambiguous
frames for review rather than invent ground truth. Automatic labels are not
generated. `check` verifies image hashes and summarizes coverage; it deliberately
does not declare a dataset ready to train.

## Model contract and inference

Train offline, export a self-contained ONNX file, and validate it in this runtime
before choosing an architecture or quantization scheme. A framework exporting
ONNX does not guarantee that every operator is supported by tract.

Contract v1 uses **one model per task**, allowing different roof/sky regions:

- One RGB float32 input `[1, 3, height, width]` (NCHW), fixed dimensions 16–512.
- A normalized ROI `{left, top, right, bottom}`, defaulting to the whole image.
  Crop first, then aspect-preserving triangle resize and centered black letterbox.
  Odd padding puts the extra pixel at bottom/right. No automatic contrast stretch.
- RGB bytes become `(channel / 255 - mean[channel]) / std[channel]`.
- One float32 output `[1, classes]` containing **logits**, not softmax probabilities.
- Class order: roof `[open, partial, closed]`; sky `[clear, partly_cloudy, overcast]`;
  quality `[usable, too_dark, saturated, obscured]`.

`uncertain` is a confidence rejection, not a training class. `not_visible` is a
sky applicability label, not a weather class. Exclude unreviewed, uncertain, and
not-visible sky samples from weather training. A future application coordinator
must suppress weather inference when the sky is hidden or the image unusable;
the standalone `infer` command does not infer other tasks or control equipment.

Example manifest (replace the digest and ID with those of a real trained model):

```json
{
  "schema_version": 1,
  "model_id": "roof-site-v1",
  "sha256": "<64 hexadecimal characters>",
  "task": "roof",
  "width": 224,
  "height": 224,
  "mean": [0.485, 0.456, 0.406],
  "std": [0.229, 0.224, 0.225],
  "min_confidence": 0.8
}
```

```powershell
cargo run --release -p autopiercam-vision --features onnx -- infer `
  --model C:/models/roof.onnx --spec C:/models/roof.json --image C:/images/frame.jpg
# Optional: --roi C:/models/roof-roi.json
```

One image is classified independently; no averaging, confirmation frames, or
temporal smoothing. The result includes the model ID, task, class-order scores,
and `label: null` below the confidence threshold. Softmax scores are **not
calibrated confidence** until evaluated/calibrated on held-out sessions.

Only load trusted, locally provisioned models and manifests. Hash verification
detects a mismatched file, not a malicious graph; tract is not a sandbox. No
models are downloaded automatically. Validate latency, memory, and false alerts
on real held-out days/nights before integrating the worker with AutoPierCam.
The initial inference fixture is hand-authored and synthetic, not a trained
detector. Runtime integration, model training, calibration, and event posting
remain separate work.

## Tests

```text
cargo test -p autopiercam-vision --features onnx
cargo clippy -p autopiercam-vision --all-targets --features onnx -- -D warnings
```
