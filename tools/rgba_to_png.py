"""Combine raw 32x32 RGBA icon dumps into one enlarged PNG sheet for review.

Usage: uv run --no-project --python 3.12 --with pillow tools/rgba_to_png.py <dir> <out.png>
(dumps come from: SIMONAIRCAST_ICON_DUMP=<dir> cargo test -p simonaircast dump_icons)
"""

import pathlib
import sys

from PIL import Image

src, out = pathlib.Path(sys.argv[1]), sys.argv[2]
names = ["idle", "connecting", "streaming", "retrying"]
scale, size, pad = 6, 32, 16
sheet = Image.new("RGBA", (len(names) * (size * scale + pad) + pad, size * scale + 2 * pad), (240, 240, 240, 255))
dark = Image.new("RGBA", sheet.size, (32, 32, 32, 255))
for i, name in enumerate(names):
    icon = Image.frombytes("RGBA", (size, size), (src / f"{name}.rgba").read_bytes())
    big = icon.resize((size * scale, size * scale), Image.NEAREST)
    pos = (pad + i * (size * scale + pad), pad)
    sheet.alpha_composite(big, pos)
    dark.alpha_composite(big, pos)
combined = Image.new("RGBA", (sheet.width, sheet.height * 2))
combined.paste(sheet, (0, 0))
combined.paste(dark, (0, sheet.height))
combined.save(out)
print(out)
