"""Generate HKDF and HAP-framing test vectors with pyatv's own crypto code.

Usage: uv run --no-project --python 3.12 --with pyatv tools/hap_vectors.py
Prints Rust consts for crates/airplay/src/crypto.rs tests.
"""

from pyatv.auth.hap_session import HAPSession
from pyatv.auth.hap_srp import hkdf_expand

shared = bytes(range(64))
write_key = hkdf_expand("Control-Salt", "Control-Write-Encryption-Key", shared)
read_key = hkdf_expand("Control-Salt", "Control-Read-Encryption-Key", shared)

# Two writes on one session: exercises the 1024-byte frame split and the nonce counter.
session = HAPSession()
session.enable(write_key, read_key)
msg1 = b"GET /info RTSP/1.0\r\nCSeq: 0\r\n\r\n"
msg2 = bytes(i % 251 for i in range(1500))
out1 = session.encrypt(msg1)
out2 = session.encrypt(msg2)

print(f'const SHARED: &str = "{shared.hex()}";')
print(f'const WRITE_KEY: &str = "{write_key.hex()}";')
print(f'const READ_KEY: &str = "{read_key.hex()}";')
print(f'const FRAMED_1: &str = "{out1.hex()}";')
print(f'const FRAMED_2: &str = "{out2.hex()}";')
