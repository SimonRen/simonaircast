//! Transient HomeKit pairing.
//!
//! Receivers with feature bit 48 accept "transient" pair-setup: only messages
//! M1–M4 of HomeKit pair-setup, SRP with the fixed PIN 3939, no long-term keys
//! and no pair-verify. The SRP session key is the shared secret for every key
//! derived afterwards. Flow and headers mirror pyatv's `hap_transient.py`.

mod srp;

use crate::crypto::hkdf_sha512;
use crate::rtsp::{Connection, Protocol, Request};
use crate::tlv8::{self, tag};
use crate::{Error, Result};

pub use srp::{SrpClient, SrpSession};

const TRANSIENT_PIN: &str = "3939";
const FLAG_TRANSIENT: u8 = 0x10;

/// Shared secret from a completed pairing.
pub struct SessionKeys {
    shared: [u8; 64],
}

impl SessionKeys {
    /// Keys for the RTSP control connection: (write, read).
    pub fn control_keys(&self) -> ([u8; 32], [u8; 32]) {
        self.derive(
            "Control-Salt",
            "Control-Write-Encryption-Key",
            "Control-Read-Encryption-Key",
        )
    }

    /// Keys for the event channel the receiver opens later: (write, read).
    pub fn event_keys(&self) -> ([u8; 32], [u8; 32]) {
        self.derive(
            "Events-Salt",
            "Events-Write-Encryption-Key",
            "Events-Read-Encryption-Key",
        )
    }

    pub fn derive(&self, salt: &str, write_info: &str, read_info: &str) -> ([u8; 32], [u8; 32]) {
        (
            hkdf_sha512(salt, write_info, &self.shared),
            hkdf_sha512(salt, read_info, &self.shared),
        )
    }
}

fn pairing_request(path: &str, body: Vec<u8>) -> Request {
    let req = Request::new("POST", path, Protocol::Http11)
        .header("User-Agent", "AirPlay/320.20")
        .header("Connection", "keep-alive")
        .header("X-Apple-HKP", 4);
    if body.is_empty() {
        req.header("Content-Type", "application/octet-stream")
    } else {
        req.body("application/octet-stream", body)
    }
}

/// Runs transient pair-setup on a fresh connection and switches it to encrypted
/// framing. On success the connection is ready for encrypted RTSP.
pub async fn pair_transient(conn: &mut Connection) -> Result<SessionKeys> {
    conn.request_ok(pairing_request("/pair-pin-start", Vec::new()))
        .await?;

    // M1 -> M2: request the server's salt and SRP public key.
    let m1 = tlv8::encode(&[
        (tag::METHOD, &[0]),
        (tag::SEQ_NO, &[1]),
        (tag::FLAGS, &[FLAG_TRANSIENT]),
    ]);
    let m2 = expect_step(conn, m1, 2).await?;
    let salt = field(&m2, tag::SALT)?;
    let server_public = field(&m2, tag::PUBLIC_KEY)?;

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret)
        .map_err(|e| Error::Pairing(format!("no system randomness: {e}")))?;
    let client = SrpClient::new(&secret);
    let session = client.process(TRANSIENT_PIN, salt, server_public)?;

    // M3 -> M4: exchange proofs.
    let public = client.public_key();
    let m3 = tlv8::encode(&[
        (tag::SEQ_NO, &[3]),
        (tag::PUBLIC_KEY, &public),
        (tag::PROOF, &session.client_proof),
    ]);
    let m4 = expect_step(conn, m3, 4).await?;
    if !session.verify_server(field(&m4, tag::PROOF)?) {
        return Err(Error::Pairing("receiver's SRP proof did not verify".into()));
    }
    tracing::debug!("transient pair-setup complete");

    let keys = SessionKeys {
        shared: session.session_key,
    };
    let (write, read) = keys.control_keys();
    conn.enable_encryption(&write, &read);
    Ok(keys)
}

async fn expect_step(
    conn: &mut Connection,
    body: Vec<u8>,
    step: u8,
) -> Result<std::collections::BTreeMap<u8, Vec<u8>>> {
    let resp = conn
        .request_ok(pairing_request("/pair-setup", body))
        .await?;
    let items = tlv8::decode(&resp.body)?;
    if let Some(code) = items.get(&tag::ERROR) {
        return Err(Error::Pairing(format!(
            "receiver returned TLV error {code:?} at M{step}"
        )));
    }
    match items.get(&tag::SEQ_NO).map(Vec::as_slice) {
        Some([s]) if *s == step => Ok(items),
        other => Err(Error::Pairing(format!(
            "expected M{step}, got sequence {other:?}"
        ))),
    }
}

fn field(items: &std::collections::BTreeMap<u8, Vec<u8>>, tag: u8) -> Result<&[u8]> {
    items
        .get(&tag)
        .map(Vec::as_slice)
        .ok_or_else(|| Error::Pairing(format!("pairing response is missing TLV tag {tag:#04x}")))
}
