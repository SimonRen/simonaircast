//! SRP-6a client for HomeKit pair-setup (RFC 5054 3072-bit group, SHA-512).
//!
//! Byte encoding deliberately mirrors `srptools` as used by pyatv, which is
//! proven against HomePod OS 27: integers are hashed as minimal big-endian bytes
//! except where RFC 5054 pads to the length of N (`k` and `u`). Test vectors in
//! `srp_vectors.rs` are generated from srptools by `tools/srp_vectors.py`.

use num_bigint::BigUint;
use num_traits::Zero;
use sha2::{Digest, Sha512};

use crate::{Error, Result};

const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA6",
    "3B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F2411",
    "7C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08",
    "CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9",
    "DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D",
    "04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733",
    "EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143",
    "DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 5;
const N_LEN: usize = 384;
const USERNAME: &str = "Pair-Setup";

pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    a_pub: BigUint,
}

/// Result of processing the server's salt and public key.
pub struct SrpSession {
    /// K = H(S); the 64-byte shared secret all later keys derive from.
    pub session_key: [u8; 64],
    /// M1, sent to the server.
    pub client_proof: [u8; 64],
    expected_server_proof: [u8; 64],
}

impl SrpSession {
    /// Checks the server's M2 = H(A | M1 | K).
    pub fn verify_server(&self, proof: &[u8]) -> bool {
        proof == self.expected_server_proof
    }
}

impl SrpClient {
    /// `private` is the client secret `a` (32 random bytes).
    pub fn new(private: &[u8; 32]) -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid group prime");
        let g = BigUint::from(G);
        let a = BigUint::from_bytes_be(private);
        let a_pub = g.modpow(&a, &n);
        Self { n, g, a, a_pub }
    }

    /// A, as minimal big-endian bytes.
    pub fn public_key(&self) -> Vec<u8> {
        self.a_pub.to_bytes_be()
    }

    pub fn process(&self, password: &str, salt: &[u8], server_public: &[u8]) -> Result<SrpSession> {
        let n = &self.n;
        if server_public.len() > N_LEN {
            return Err(Error::Pairing(
                "server sent an oversized SRP public key".into(),
            ));
        }
        let b_pub = BigUint::from_bytes_be(server_public);
        if (&b_pub % n).is_zero() {
            return Err(Error::Pairing(
                "server sent an invalid SRP public key".into(),
            ));
        }

        let k = int(&h(&[&n.to_bytes_be(), &pad(&self.g)]));
        let x = int(&h(&[
            salt,
            &h(&[format!("{USERNAME}:{password}").as_bytes()]),
        ]));
        let u = int(&h(&[&pad(&self.a_pub), &pad(&b_pub)]));
        let v = self.g.modpow(&x, n);

        // S = (B - k*v) ^ (a + u*x) mod N, with the base reduced into [0, N).
        let kv = (&k * &v) % n;
        let base = ((&b_pub % n) + n - kv) % n;
        let s = base.modpow(&(&self.a + &u * &x), n);
        let session_key = h(&[&s.to_bytes_be()]);

        let hn_xor_hg = int(&h(&[&n.to_bytes_be()])) ^ int(&h(&[&self.g.to_bytes_be()]));
        let h_user = int(&h(&[USERNAME.as_bytes()]));
        let a_bytes = self.a_pub.to_bytes_be();
        let client_proof = h(&[
            &hn_xor_hg.to_bytes_be(),
            &h_user.to_bytes_be(),
            salt,
            &a_bytes,
            &b_pub.to_bytes_be(),
            &session_key,
        ]);
        let expected_server_proof = h(&[&a_bytes, &client_proof, &session_key]);

        Ok(SrpSession {
            session_key,
            client_proof,
            expected_server_proof,
        })
    }
}

fn h(parts: &[&[u8]]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn int(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut out = vec![0u8; N_LEN.saturating_sub(bytes.len())];
    out.extend(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Vector {
        a: &'static str,
        salt: &'static str,
        server_public: &'static str,
        client_public: &'static str,
        session_key: &'static str,
        client_proof: &'static str,
        server_proof: &'static str,
    }

    include!("srp_vectors.rs");

    #[test]
    fn matches_srptools_vectors() {
        for v in VECTORS {
            let a: [u8; 32] = hex::decode(v.a).unwrap().try_into().unwrap();
            let client = SrpClient::new(&a);
            assert_eq!(hex::encode(client.public_key()), v.client_public);

            let session = client
                .process(
                    "3939",
                    &hex::decode(v.salt).unwrap(),
                    &hex::decode(v.server_public).unwrap(),
                )
                .unwrap();
            assert_eq!(hex::encode(session.session_key), v.session_key);
            assert_eq!(hex::encode(session.client_proof), v.client_proof);
            assert!(session.verify_server(&hex::decode(v.server_proof).unwrap()));
            assert!(!session.verify_server(&[0; 64]));
        }
    }

    #[test]
    fn rejects_zero_server_key() {
        let client = SrpClient::new(&[7; 32]);
        assert!(client.process("3939", &[1; 16], &[0]).is_err());
    }

    #[test]
    fn rejects_server_keys_larger_than_the_srp_group() {
        let client = SrpClient::new(&[7; 32]);
        assert!(client.process("3939", &[1; 16], &[1; N_LEN + 1]).is_err());
    }
}
