#!/usr/bin/env python3
"""Render and verify AutoPierCam's committed application icon assets."""

from __future__ import annotations

import argparse
import hashlib
import io
import shutil
import struct
import sys
import tempfile
from pathlib import Path

import cairocffi
import cairosvg
from PIL import Image, __version__ as pillow_version


ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
EXPECTED_TOOL_VERSIONS = {
    "CairoSVG": "2.8.2",
    "cairocffi": "1.7.1",
    "Pillow": "10.4.0",
    "Cairo": "1.18.4",
}
REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
BRANDING_ROOT = REPOSITORY_ROOT / "assets" / "branding"
SVG_SOURCE = BRANDING_ROOT / "autopiercam.svg"


def tool_versions() -> dict[str, str]:
    return {
        "CairoSVG": cairosvg.__version__,
        "cairocffi": cairocffi.__version__,
        "Pillow": pillow_version,
        "Cairo": cairocffi.cairo_version_string(),
    }


def require_reproducible_tools() -> None:
    actual = tool_versions()
    mismatches = [
        f"{name}={actual[name]} (expected {expected})"
        for name, expected in EXPECTED_TOOL_VERSIONS.items()
        if actual[name] != expected
    ]
    if mismatches:
        raise RuntimeError(
            "Icon rendering tool versions have drifted: " + ", ".join(mismatches)
        )


def render_png(svg: bytes, size: int, destination: Path) -> None:
    cairosvg.svg2png(
        bytestring=svg,
        write_to=str(destination),
        output_width=size,
        output_height=size,
    )
    validate_png(destination.read_bytes(), size, destination.name)


def validate_png(payload: bytes, size: int, label: str) -> None:
    with Image.open(io.BytesIO(payload)) as image:
        if image.format != "PNG" or image.size != (size, size):
            raise RuntimeError(
                f"{label} must be a {size}x{size} PNG; got {image.format} {image.size}"
            )
        rgba = image.convert("RGBA")
        alpha = rgba.getchannel("A")
        alpha_min, alpha_max = alpha.getextrema()
        if alpha_min != 0 or alpha_max != 255:
            raise RuntimeError(f"{label} must contain transparent and opaque pixels")
        if any(rgba.getpixel(point)[3] != 0 for point in ((0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1))):
            raise RuntimeError(f"{label} must keep all four corners transparent")


def pack_ico(frames: list[tuple[int, bytes]]) -> bytes:
    header = struct.pack("<HHH", 0, 1, len(frames))
    offset = len(header) + 16 * len(frames)
    entries: list[bytes] = []
    payloads: list[bytes] = []
    for size, payload in frames:
        dimension = 0 if size == 256 else size
        entries.append(
            struct.pack(
                "<BBBBHHII",
                dimension,
                dimension,
                0,
                0,
                1,
                32,
                len(payload),
                offset,
            )
        )
        payloads.append(payload)
        offset += len(payload)
    return header + b"".join(entries) + b"".join(payloads)


def validate_ico(payload: bytes) -> None:
    if len(payload) < 6:
        raise RuntimeError("autopiercam.ico is truncated")
    reserved, icon_type, count = struct.unpack_from("<HHH", payload)
    if (reserved, icon_type, count) != (0, 1, len(ICON_SIZES)):
        raise RuntimeError("autopiercam.ico has an invalid header or frame count")

    seen_sizes: list[int] = []
    for index in range(count):
        entry_offset = 6 + index * 16
        width, height, colors, reserved_byte, planes, bit_count, length, offset = (
            struct.unpack_from("<BBBBHHII", payload, entry_offset)
        )
        decoded_width = 256 if width == 0 else width
        decoded_height = 256 if height == 0 else height
        if decoded_width != decoded_height:
            raise RuntimeError(f"ICO frame {index} is not square")
        if colors != 0 or reserved_byte != 0 or planes != 1 or bit_count != 32:
            raise RuntimeError(f"ICO frame {index} is not a 32-bit true-color frame")
        if offset < 6 + count * 16 or offset + length > len(payload):
            raise RuntimeError(f"ICO frame {index} points outside the file")
        frame = payload[offset : offset + length]
        validate_png(frame, decoded_width, f"ICO frame {decoded_width}")
        seen_sizes.append(decoded_width)

    if tuple(seen_sizes) != ICON_SIZES:
        raise RuntimeError(
            f"ICO sizes must be {ICON_SIZES}; got {tuple(seen_sizes)}"
        )


def build(destination: Path) -> list[Path]:
    svg = SVG_SOURCE.read_bytes()
    png_root = destination / "png"
    png_root.mkdir(parents=True, exist_ok=True)

    outputs: list[Path] = []
    frames: list[tuple[int, bytes]] = []
    for size in ICON_SIZES:
        png_path = png_root / f"autopiercam-{size}.png"
        render_png(svg, size, png_path)
        payload = png_path.read_bytes()
        frames.append((size, payload))
        outputs.append(png_path)

    icon_path = destination / "autopiercam.ico"
    icon_payload = pack_ico(frames)
    validate_ico(icon_payload)
    icon_path.write_bytes(icon_payload)
    outputs.append(icon_path)

    for size, name in (
        (512, "autopiercam-featured.png"),
        (1024, "autopiercam-logo.png"),
    ):
        output = destination / name
        render_png(svg, size, output)
        outputs.append(output)

    return outputs


def relative_outputs(root: Path, outputs: list[Path]) -> list[Path]:
    return [path.relative_to(root) for path in outputs]


def check_or_copy(check: bool) -> None:
    with tempfile.TemporaryDirectory(prefix="autopiercam-icons-") as temporary:
        temporary_root = Path(temporary)
        generated = build(temporary_root)
        relatives = relative_outputs(temporary_root, generated)

        if check:
            failures: list[str] = []
            for relative in relatives:
                expected = BRANDING_ROOT / relative
                actual = temporary_root / relative
                if not expected.is_file():
                    failures.append(f"missing {relative}")
                elif expected.read_bytes() != actual.read_bytes():
                    failures.append(f"stale {relative}")
            if failures:
                raise RuntimeError(
                    "Committed icon assets do not match the SVG source: "
                    + ", ".join(failures)
                )
            action = "Verified"
        else:
            for relative in relatives:
                destination = BRANDING_ROOT / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(temporary_root / relative, destination)
            action = "Rendered"

    icon_hash = hashlib.sha256(
        (BRANDING_ROOT / "autopiercam.ico").read_bytes()
    ).hexdigest()
    print(
        f"{action} {len(ICON_SIZES)} ICO frames, 512px featured image, and "
        f"1024px logo (ICO SHA-256 {icon_hash})."
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="render into a temporary directory and verify committed assets byte-for-byte",
    )
    args = parser.parse_args()

    require_reproducible_tools()
    check_or_copy(args.check)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
