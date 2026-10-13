"""Build assets/simonaircast.ico from the code-drawn 256 px icon.

Usage (from the repo root):
  $env:SIMONAIRCAST_ICON_DUMP = "<dir>"; cargo test -p simonaircast icon::tests::dump
  uv run --no-project --python 3.12 --with pillow tools/make_icon.py <dir> assets/simonaircast.ico
"""

import pathlib
import sys

from PIL import Image

src, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
icon = Image.frombytes("RGBA", (256, 256), (src / "brand-256.rgba").read_bytes())
out.parent.mkdir(parents=True, exist_ok=True)
icon.save(out, format="ICO", sizes=[(16, 16), (20, 20), (24, 24), (32, 32), (40, 40), (48, 48), (64, 64), (128, 128), (256, 256)])
print(out, out.stat().st_size, "bytes")
