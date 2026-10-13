//! Key derivation and the ChaCha20-Poly1305 framing used on paired channels.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha512;

use crate::{Error, Result};

/// HKDF-SHA512 with string salt/info, 32-byte output (all AirPlay keys use this).
pub fn hkdf_sha512(salt: &str, info: &str, ikm: &[u8]) -> [u8; 32] {
    let mut okm = [0u8; 32];
    Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm)
        .expand(info.as_bytes(), &mut okm)
        .expect("32 bytes is a valid HKDF-SHA512 output length");
    okm
}

/// ChaCha20-Poly1305 with a per-direction message counter as nonce:
/// 4 zero bytes followed by the counter as 8 little-endian bytes.
pub struct Cipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(&(*key).into()),
            counter: 0,
        }
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        nonce.into()
    }

    /// Counter the next `seal`/`open` will use as its nonce.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    pub fn seal(&mut self, msg: &[u8], aad: &[u8]) -> Vec<u8> {
        let nonce = self.next_nonce();
        self.aead
            .encrypt(&nonce, Payload { msg, aad })
            .expect("ChaCha20-Poly1305 encryption cannot fail for in-memory buffers")
    }

    pub fn open(&mut self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = self.next_nonce();
        self.aead
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| Error::Decrypt)
    }
}

/// HAP stream framing: plaintext is cut into frames of at most 1024 bytes, each
/// sent as `len (u16 LE) | ciphertext | tag (16)` with the length bytes as AAD.
pub struct HapFraming {
    write: Cipher,
    read: Cipher,
    pending: Vec<u8>,
}

const FRAME_LEN: usize = 1024;
const TAG_LEN: usize = 16;

impl HapFraming {
    pub fn new(write_key: &[u8; 32], read_key: &[u8; 32]) -> Self {
        Self {
            write: Cipher::new(write_key),
            read: Cipher::new(read_key),
            pending: Vec::new(),
        }
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(plain.len() + plain.len().div_ceil(FRAME_LEN) * (2 + TAG_LEN));
        for frame in plain.chunks(FRAME_LEN) {
            let len = (frame.len() as u16).to_le_bytes();
            out.extend_from_slice(&len);
            out.extend(self.write.seal(frame, &len));
        }
        out
    }

    /// Feeds received bytes; returns whatever plaintext complete frames yield.
    pub fn open(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.pending.extend_from_slice(data);
        let mut plain = Vec::new();
        let mut consumed = 0;
        while let Some(header) = self.pending.get(consumed..consumed + 2) {
            let len = u16::from_le_bytes([header[0], header[1]]) as usize;
            let end = consumed + 2 + len + TAG_LEN;
            if self.pending.len() < end {
                break;
            }
            let aad = [header[0], header[1]];
            plain.extend(self.read.open(&self.pending[consumed + 2..end], &aad)?);
            consumed = end;
        }
        self.pending.drain(..consumed);
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("crypto_vectors.rs");

    fn key(hex_str: &str) -> [u8; 32] {
        hex::decode(hex_str).unwrap().try_into().unwrap()
    }

    fn messages() -> (Vec<u8>, Vec<u8>) {
        let msg1 = b"GET /info RTSP/1.0\r\nCSeq: 0\r\n\r\n".to_vec();
        let msg2 = (0..1500).map(|i| (i % 251) as u8).collect();
        (msg1, msg2)
    }

    #[test]
    fn control_keys_match_pyatv() {
        let shared = hex::decode(SHARED).unwrap();
        assert_eq!(
            hkdf_sha512("Control-Salt", "Control-Write-Encryption-Key", &shared),
            key(WRITE_KEY)
        );
        assert_eq!(
            hkdf_sha512("Control-Salt", "Control-Read-Encryption-Key", &shared),
            key(READ_KEY)
        );
    }

    #[test]
    fn framing_matches_pyatv_byte_for_byte() {
        let (msg1, msg2) = messages();
        let mut framing = HapFraming::new(&key(WRITE_KEY), &key(READ_KEY));
        assert_eq!(hex::encode(framing.seal(&msg1)), FRAMED_1);
        assert_eq!(hex::encode(framing.seal(&msg2)), FRAMED_2);
    }

    #[test]
    fn peer_decrypts_frames_split_at_arbitrary_points() {
        let (msg1, msg2) = messages();
        let wire = [
            hex::decode(FRAMED_1).unwrap(),
            hex::decode(FRAMED_2).unwrap(),
        ]
        .concat();
        // The receiving side reads with the sender's write key.
        let mut peer = HapFraming::new(&key(READ_KEY), &key(WRITE_KEY));
        let mut plain = Vec::new();
        for chunk in wire.chunks(7) {
            plain.extend(peer.open(chunk).unwrap());
        }
        assert_eq!(plain, [msg1, msg2].concat());
    }

    #[test]
    fn tampered_frame_is_rejected() {
        let mut wire = hex::decode(FRAMED_1).unwrap();
        wire[5] ^= 1;
        let mut peer = HapFraming::new(&key(READ_KEY), &key(WRITE_KEY));
        assert!(matches!(peer.open(&wire), Err(Error::Decrypt)));
    }
}
