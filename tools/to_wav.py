"""Convert audio files (FLAC/OGG/WAV, any rate) to 44.1 kHz 16-bit stereo WAV.

Usage: uv run --no-project --python 3.12 --with soundfile --with soxr tools/to_wav.py <out_dir> <files...>
"""

import pathlib
import sys

import numpy as np
import soundfile as sf
import soxr

TARGET_RATE = 44_100

out_dir = pathlib.Path(sys.argv[1])
out_dir.mkdir(parents=True, exist_ok=True)

for src in map(pathlib.Path, sys.argv[2:]):
    data, rate = sf.read(src, dtype="float32", always_2d=True)
    if data.shape[1] == 1:
        data = np.repeat(data, 2, axis=1)
    data = data[:, :2]
    if rate != TARGET_RATE:
        data = soxr.resample(data, rate, TARGET_RATE, quality="HQ")
    dst = out_dir / (src.stem + ".wav")
    sf.write(dst, np.clip(data, -1.0, 1.0), TARGET_RATE, subtype="PCM_16")
    print(f"{dst.name}: {rate} Hz -> {TARGET_RATE} Hz, {len(data) / TARGET_RATE:.0f} s")
