# Icon tooling

`assets/branding/autopiercam.svg` is the canonical AutoPierCam artwork. Render
the checked-in PNG and ICO derivatives with:

```powershell
python tools/icons/build_icons.py
python tools/icons/build_icons.py --check
```

The renderer intentionally checks exact tool versions. Install the build-only
dependencies from `requirements.txt` in a Python 3.12 environment when needed.
The ICO contains independently rendered 32-bit PNG frames at 16, 20, 24, 32,
40, 48, 64, 128, and 256 pixels; it never downsamples a single master frame.

CairoSVG is LGPL-3.0-or-later, cairocffi is BSD-3-Clause, Pillow is HPND, and
the Cairo rendering library is available under LGPL-2.1-only or MPL-1.1. They
are development tools only and are not distributed with AutoPierCam. The
generated artwork and its SVG source are licensed under Apache-2.0 with the
rest of this repository.
