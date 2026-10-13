"""Generate SRP-6a test vectors with srptools, exactly as pyatv drives it.

pyatv's transient pairing (proven against HomePod OS 27) uses srptools with the
RFC 5054 3072-bit group, SHA-512, I="Pair-Setup", P="3939". Our Rust SRP client
must produce byte-identical A, K, M1 and M2 for the same inputs.

Usage: uv run --no-project --python 3.12 --with srptools tools/srp_vectors.py
Prints Rust test-vector consts for crates/airplay/src/pairing/srp.rs.
"""

import hashlib
import random

from srptools import SRPClientSession, SRPContext, constants

rng = random.Random(3939)


def vector(i: int) -> str:
    a = rng.randbytes(32)
    salt = rng.randbytes(16)
    b = rng.getrandbits(256)

    ctx = SRPContext("Pair-Setup", "3939", prime=constants.PRIME_3072,
                     generator=constants.PRIME_3072_GEN, hash_func=hashlib.sha512)
    # Server side (stands in for the HomePod): B = k*v + g^b mod N.
    v = ctx.get_common_password_verifier(ctx.get_common_password_hash(salt))
    server_public = ctx.get_server_public(v, b)
    b_bytes = bytes.fromhex(f"{server_public:x}".zfill(len(f"{server_public:x}") + len(f"{server_public:x}") % 2))

    client = SRPClientSession(ctx, a.hex())
    client.process(b_bytes.hex(), salt.hex())

    fields = {
        "a": a.hex(),
        "salt": salt.hex(),
        "server_public": b_bytes.hex(),
        "client_public": client.public,
        "session_key": client.key,
        "client_proof": client.key_proof,
        "server_proof": client.key_proof_hash,
    }
    # srptools returns some hex values as bytes (hexlify), others as str.
    as_str = lambda v: v.decode() if isinstance(v, bytes) else v
    body = "\n".join(f'        {k}: "{as_str(v)}",' for k, v in fields.items())
    return f"    Vector {{\n{body}\n    }},"


print("const VECTORS: &[Vector] = &[")
for i in range(3):
    print(vector(i))
print("];")
