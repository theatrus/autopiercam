"""Offline experimental sky baseline. Python trains; production inference is Rust.

Use an explicit, reviewed session split, not a random frame split. Training images
and outputs stay local. This script neither downloads weights nor deploys a model.
"""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import time

LABELS = ["clear", "partly_cloudy", "overcast"]
REPO = Path(__file__).resolve().parents[2]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def select_samples(manifest, split, manifest_hash):
    if manifest.get("schema_version") != 1 or split.get("schema_version") != 1:
        raise ValueError("Unsupported manifest or split schema")
    if split.get("dataset_sha256") != manifest_hash:
        raise ValueError("Split belongs to a different dataset snapshot")
    if not split.get("label_source") or not split.get("session_method"):
        raise ValueError("Record label provenance and session boundaries in the split")
    rows = []
    group_splits = {}
    for sample_id, sample in sorted(manifest["samples"].items()):
        labels = sample["labels"]
        if (labels.get("sky") not in LABELS or labels.get("roof") != "open"
                or labels.get("quality") != "usable"):
            continue
        assignment = split["samples"].get(sample_id)
        if not assignment or assignment.get("split") not in ("train", "test"):
            raise ValueError(f"Missing explicit split: {sample_id}")
        session = assignment.get("session")
        if not isinstance(session, str) or not session.strip():
            raise ValueError("Every image needs an observing session")
        key = (sample["site"], sample["camera"], session)
        if key in group_splits and group_splits[key] != assignment["split"]:
            raise ValueError("Observing session leaks across train/test")
        group_splits[key] = assignment["split"]
        if sample["image"] not in (f"images/{sample_id}.jpg", f"images/{sample_id}.png"):
            raise ValueError("Invalid dataset image path")
        rows.append({"id": sample_id, "image": sample["image"], "session": session,
                     "split": assignment["split"], "label": labels["sky"]})
    for partition in ("train", "test"):
        present = {r["label"] for r in rows if r["split"] == partition}
        if present != set(LABELS):
            raise ValueError(f"{partition} must contain every sky class; found {present}")
    return rows


def metrics(truth, predicted, confidence, threshold=0.8):
    matrix = [[0] * 3 for _ in LABELS]
    for actual, guess in zip(truth, predicted):
        matrix[int(actual)][int(guess)] += 1
    recalls = [matrix[i][i] / sum(matrix[i]) if sum(matrix[i]) else None for i in range(3)]
    accepted = [i for i, value in enumerate(confidence) if value >= threshold]
    return {"class_order": LABELS, "confusion_rows_actual_columns_predicted": matrix,
            "accuracy": sum(matrix[i][i] for i in range(3)) / len(truth),
            "recall": dict(zip(LABELS, recalls)),
            "balanced_accuracy": sum(v for v in recalls if v is not None) / sum(v is not None for v in recalls),
            "threshold": threshold, "accepted": len(accepted), "total": len(truth),
            "coverage": len(accepted) / len(truth),
            "accepted_accuracy": (sum(truth[i] == predicted[i] for i in accepted) / len(accepted)
                                  if accepted else None)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--split", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="New directory, never overwritten")
    parser.add_argument("--epochs", type=int, default=120)
    parser.add_argument("--seed", type=int, default=1729)
    args = parser.parse_args()
    if not 1 <= args.epochs <= 1000:
        parser.error("epochs must be 1–1000")
    source_hashes = {"trainer": digest(Path(__file__).read_bytes()),
                     "preprocessor": digest((REPO / "crates/autopiercam-vision/examples/training_export.rs").read_bytes())}
    import numpy as np
    import onnx
    import torch
    from torch import nn

    torch.set_num_threads(4)
    torch.manual_seed(args.seed)
    torch.use_deterministic_algorithms(True)
    np.random.seed(args.seed)
    dataset = args.dataset.resolve()
    manifest_bytes = (dataset / "manifest.json").read_bytes()
    manifest = json.loads(manifest_bytes)
    split_bytes = args.split.read_bytes()
    split = json.loads(split_bytes)
    rows = select_samples(manifest, split, digest(manifest_bytes))
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    (output / ".gitignore").write_text("*\n", encoding="utf-8")
    write_json(output / "split.json", split)
    spec = {"schema_version": 1, "model_id": "experimental-sky-" + digest(manifest_bytes)[:12],
            "sha256": "0" * 64, "task": "sky", "width": 96, "height": 96,
            "mean": [0., 0., 0.], "std": [1., 1., 1.], "min_confidence": 0.8}
    roi = {"left": 0., "top": 0., "right": 1., "bottom": 1.}
    job = {"spec": spec, "roi": roi,
           "images": [{"id": r["id"], "path": str(dataset / r["image"])} for r in rows]}
    write_json(output / "preprocess-job.json", job)
    write_json(output / "roi.json", roi)
    subprocess.run(["cargo", "run", "-p", "autopiercam-vision", "--example", "training_export",
                    "--", str(output / "preprocess-job.json"), str(output / "tensors")], cwd=REPO, check=True)
    inputs = torch.from_numpy(np.stack([np.fromfile(output / "tensors" / (r["id"] + ".f32"),
                                                    dtype="<f4").reshape(3, 96, 96) for r in rows]))
    targets = torch.tensor([LABELS.index(r["label"]) for r in rows])
    train_indices = [i for i, r in enumerate(rows) if r["split"] == "train"]
    test_indices = [i for i, r in enumerate(rows) if r["split"] == "test"]
    x_train, y_train = inputs[train_indices], targets[train_indices]
    counts = torch.bincount(y_train, minlength=3)
    # Sampling/augmentation use only the training partition. No test-driven tuning.
    sampler_weights = 1.0 / counts[y_train].float()
    model = nn.Sequential(
        nn.Conv2d(3, 8, 3, stride=2, padding=1), nn.ReLU(),
        nn.Conv2d(8, 16, 3, stride=2, padding=1), nn.ReLU(),
        nn.Conv2d(16, 24, 3, stride=2, padding=1), nn.ReLU(),
        nn.AvgPool2d(4), nn.Flatten(), nn.Dropout(0.2), nn.Linear(24 * 3 * 3, 3))
    optimizer = torch.optim.AdamW(model.parameters(), lr=0.001, weight_decay=0.01)
    loss_fn = nn.CrossEntropyLoss()
    losses = []
    started = time.perf_counter()
    model.train()
    for epoch in range(args.epochs):
        indices = torch.multinomial(sampler_weights, len(train_indices), replacement=True)
        total_loss = 0.0
        for batch in indices.split(16):
            x = x_train[batch].clone()
            shape = (len(batch), 1, 1, 1)
            # Brightness/gamma and mild channel variation, no geometric flips:
            # retain camera orientation while discouraging exposure-only shortcuts.
            x = x.pow(torch.empty(shape).uniform_(0.75, 1.35))
            x = (x * torch.empty(shape).uniform_(0.7, 1.3)
                 * torch.empty((len(batch), 3, 1, 1)).uniform_(0.9, 1.1)).clamp(0, 1)
            optimizer.zero_grad()
            loss = loss_fn(model(x), y_train[batch])
            loss.backward()
            optimizer.step()
            total_loss += loss.item() * len(batch)
        losses.append(total_loss / len(indices))
        if (epoch + 1) % 20 == 0:
            print(f"Epoch {epoch + 1}/{args.epochs}: training loss {losses[-1]:.4f}", flush=True)
    model.eval()
    with torch.no_grad():
        probabilities = model(inputs).softmax(1).numpy()
    model_path = output / "sky.onnx"
    # Fixed, small opset-17 graph; tract validation below is the compatibility gate.
    torch.onnx.export(model, torch.zeros(1, 3, 96, 96), str(model_path),
                      input_names=["input"], output_names=["logits"],
                      opset_version=17, dynamo=False)
    onnx.checker.check_model(str(model_path))
    spec["sha256"] = digest(model_path.read_bytes())
    spec["model_id"] = "experimental-sky-" + spec["sha256"][:12]
    write_json(output / "sky.json", spec)
    report = {"status": "experimental_not_for_deployment", "human_verified_labels": False,
              "model_id": spec["model_id"], "model_sha256": spec["sha256"],
              "dataset_sha256": digest(manifest_bytes), "split_sha256": digest(split_bytes),
              "label_source": split["label_source"], "session_method": split["session_method"],
              "seed": args.seed, "epochs": args.epochs, "model_parameters": sum(p.numel() for p in model.parameters()),
              "onnx_bytes": model_path.stat().st_size, "training_seconds": time.perf_counter() - started,
              "versions": {"python": platform.python_version(), "torch": torch.__version__,
                           "numpy": np.__version__, "onnx": onnx.__version__},
              "source_sha256": source_hashes,
              "git_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
              "training_loss": losses, "partitions": {}, "predictions": [],
              "limitations": ["AI-assisted labels, not independent ground truth.",
                              "One fixed camera/site, few nights and rare sky classes.",
                              "Full-frame baseline may learn telescope/lighting shortcuts.",
                              "Threshold 0.8 is fixed, not calibrated.",
                              "No test-driven checkpoint selection; final fixed epoch evaluated once.",
                              "No roof applicability/quality gate in standalone inference; do not deploy."]}
    for name, indices in (("train", train_indices), ("test", test_indices)):
        probs = probabilities[indices]
        truth = targets[indices].tolist()
        report["partitions"][name] = {
            "counts": {label: truth.count(i) for i, label in enumerate(LABELS)},
            "sessions": sorted({rows[i]["session"] for i in indices}),
            **metrics(truth, probs.argmax(1).tolist(), probs.max(1).tolist())}
    majority = int(counts.argmax())
    report["training_majority_class"] = LABELS[majority]
    report["majority_baseline_test_accuracy"] = sum(int(targets[i]) == majority for i in test_indices) / len(test_indices)
    for row, probs in zip(rows, probabilities):
        report["predictions"].append({**row, "probabilities": probs.tolist(),
                                      "predicted": LABELS[int(probs.argmax())]})
    report["rust_parity"] = {"verified": False}
    write_json(output / "report.json", report)
    subprocess.run(["cargo", "build", "-p", "autopiercam-vision", "--features", "onnx"], cwd=REPO, check=True)
    # Resolve Cargo's configured target directory, including CARGO_TARGET_DIR.
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=REPO))
    binary = Path(metadata["target_directory"]) / "debug" / ("autopiercam-vision.exe" if sys.platform == "win32" else "autopiercam-vision")
    max_error = 0.0
    for i in test_indices:
        result = json.loads(subprocess.check_output([
            str(binary), "infer", "--model", str(model_path), "--spec", str(output / "sky.json"),
            "--image", str(dataset / rows[i]["image"]), "--roi", str(output / "roi.json")], cwd=REPO))
        error = float(np.max(np.abs(np.asarray(result["probabilities"]) - probabilities[i])))
        if not np.isfinite(error) or error > 1e-5:
            raise RuntimeError(f"Rust/torch parity failure for {rows[i]['id']}: {error}")
        max_error = max(max_error, error)
    if digest((dataset / "manifest.json").read_bytes()) != digest(manifest_bytes):
        raise RuntimeError("Dataset changed during training; keep this run as an obsolete snapshot")
    report["rust_parity"] = {"verified": True, "images": len(test_indices), "max_probability_error": max_error}
    write_json(output / "report.json", report)
    print(json.dumps({"output": str(output), "test": report["partitions"]["test"],
                      "rust_parity": report["rust_parity"]}, indent=2))


if __name__ == "__main__":
    main()
