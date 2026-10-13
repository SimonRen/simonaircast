"""Decode binary-plist bodies from a pyatv debug log (RTSP SETUP, event channel).

Usage: uv run --no-project --python 3.12 tools/decode_pyatv_log.py captures/pyatv-spike2.log
"""

import ast
import plistlib
import pprint
import re
import sys

MSG = re.compile(r"(Sending RTSP/1\.0 message|Received|Got message on event channel): (b'.*'|HttpRequest\(.*)$")


def first_line(raw: bytes) -> str:
    return raw.split(b"\r\n", 1)[0].decode(errors="replace")


for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    m = MSG.search(line)
    if not m:
        continue
    kind, payload = m.groups()
    if payload.startswith("HttpRequest"):
        body = re.search(r"body=(b'.*?')\)?$", payload)
        print(f"\n=== EVENT {re.search(r'path=.([^\']+)', payload).group(1)}")
        raw_body = ast.literal_eval(body.group(1)) if body else b""
    else:
        raw = ast.literal_eval(payload)
        print(f"\n=== {'>>' if kind.startswith('Sending') else '<<'} {first_line(raw)}")
        raw_body = raw.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in raw else b""
    if raw_body.startswith(b"bplist00"):
        try:
            obj = plistlib.loads(raw_body)
            text = pprint.pformat(obj, width=110, compact=True)
            print(text if len(text) < 2500 else text[:2500] + " ...")
        except Exception as exc:  # truncated bodies in the log
            print(f"(plist decode failed: {exc})")
