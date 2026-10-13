//! AirPlay 2 audio sender.
//!
//! The protocol is not officially documented; this crate follows the community
//! reverse-engineering work listed in `docs/references.md`.
//!
//! Pipeline, in the order a session is established:
//! 1. [`discovery`] — find receivers via mDNS (`_airplay._tcp`).
//! 2. [`pairing`] — transient HomeKit pairing, which encrypts the [`rtsp`] connection.
//! 3. [`rtsp`] — RTSP control channel (SETUP / RECORD / SET_PARAMETER).
//! 4. [`timing`] — answers the receiver's NTP timing requests.
//! 5. [`events`] — event channel the receiver pushes requests on.
//! 6. [`session`] — ties 2–5 together into one playback session.
//! 7. [`stream`] — encode and send audio as encrypted RTP.

pub mod crypto;
pub mod discovery;
pub mod events;
pub mod pairing;
pub mod rtsp;
pub mod session;
pub mod stream;
pub mod timing;
pub mod tlv8;

pub use discovery::{Device, discover};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mDNS error: {0}")]
    Mdns(#[from] mdns_sd::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("{request} returned status {status}")]
    Status { request: String, status: u16 },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("decryption failed (wrong keys or corrupted frame)")]
    Decrypt,
    /// The receiver ended the session itself, usually because another device
    /// started playing to it. Reconnecting would fight that device.
    #[error("the receiver ended the session (another device may have taken over)")]
    SessionEnded,
    #[error("the receiver stopped responding")]
    ReceiverUnreachable,
    #[error("plist error: {0}")]
    Plist(#[from] plist::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Binds a UDP socket on `port`, or any free port if that one is taken (or 0).
async fn bind_udp(ip: std::net::IpAddr, port: u16) -> Result<tokio::net::UdpSocket> {
    use std::net::SocketAddr;
    if port != 0 {
        match tokio::net::UdpSocket::bind(SocketAddr::new(ip, port)).await {
            Ok(socket) => return Ok(socket),
            Err(e) => {
                tracing::warn!(port, error = %e, "preferred UDP port unavailable; using any port")
            }
        }
    }
    Ok(tokio::net::UdpSocket::bind(SocketAddr::new(ip, 0)).await?)
}
