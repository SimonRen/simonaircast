//! NTP-style timing responder for realtime audio sessions.
//!
//! We advertise `timingProtocol: NTP` and our port in the first SETUP. The
//! receiver then sends 32-byte timing requests (type 0x52) and **does not answer
//! the stream SETUP until they are answered** — it hangs silently instead of
//! failing. On Windows the firewall drops these by default (see docs/findings).
//!
//! Packet layout (big-endian): `0x80 | type | seq (u16) | pad (u32) |
//! reference time | receive time | send time`, times as 64-bit NTP timestamps.
//! Replies mirror pyatv's `TimingServer`.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;

use crate::Result;

const TIMING_REQUEST: u8 = 0x52;
const TIMING_REPLY: u8 = 0x53 | 0x80;
const PACKET_LEN: usize = 32;
/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: u64 = 0x83AA_7E80;

pub struct TimingServer {
    port: u16,
    answered: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl TimingServer {
    /// Binds `preferred_port` (0 = any); falls back to any port if it's taken.
    pub async fn bind(local_ip: IpAddr, preferred_port: u16) -> Result<Self> {
        let socket = crate::bind_udp(local_ip, preferred_port).await?;
        let port = socket.local_addr()?.port();
        let answered = Arc::new(AtomicU64::new(0));
        let counter = answered.clone();

        let task = tokio::spawn(async move {
            let mut buf = [0u8; 128];
            loop {
                let (n, peer) = match socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    // Windows reports ICMP port-unreachable as a recv error; keep serving.
                    Err(e) => {
                        tracing::debug!(error = %e, "timing socket recv error");
                        continue;
                    }
                };
                let Some(resp) = reply(&buf[..n], ntp_now()) else {
                    tracing::trace!(len = n, "ignoring non-timing packet");
                    continue;
                };
                if socket.send_to(&resp, peer).await.is_ok() {
                    let total = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if total == 1 {
                        tracing::debug!(%peer, "answered first timing request");
                    }
                }
            }
        });
        tracing::debug!(port, "timing server listening");
        Ok(Self {
            port,
            answered,
            task,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Number of timing requests answered so far. Zero a second after SETUP
    /// almost always means a firewall is dropping the receiver's packets.
    pub fn answered(&self) -> u64 {
        self.answered.load(Ordering::Relaxed)
    }
}

impl Drop for TimingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Current wall-clock time as a 64-bit NTP timestamp (32.32 fixed point).
pub fn ntp_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + NTP_UNIX_OFFSET;
    let frac = (u64::from(now.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// Builds the reply to a timing request: the request's send time becomes our
/// reference time; receive and send time are both `now`.
fn reply(request: &[u8], now: u64) -> Option<[u8; PACKET_LEN]> {
    if request.len() < PACKET_LEN || request[1] & 0x7F != TIMING_REQUEST {
        return None;
    }
    let mut out = [0u8; PACKET_LEN];
    out[0] = request[0];
    out[1] = TIMING_REPLY;
    out[2..4].copy_from_slice(&7u16.to_be_bytes());
    out[8..16].copy_from_slice(&request[24..32]);
    out[16..24].copy_from_slice(&now.to_be_bytes());
    out[24..32].copy_from_slice(&now.to_be_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timing request captured from HomePod 书房 (OS 27.0).
    const HOMEPOD_REQUEST: [u8; 32] = [
        0x80, 0xd2, 0x00, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x83,
        0xaa, 0x7e, 0x80, 0x37, 0xaa, 0x01, 0x7e,
    ];

    #[test]
    fn answers_homepod_request_with_its_send_time_as_reference() {
        let now = 0x1122_3344_5566_7788;
        let resp = reply(&HOMEPOD_REQUEST, now).unwrap();
        assert_eq!(&resp[..4], &[0x80, 0xd3, 0x00, 0x07]);
        assert_eq!(&resp[4..8], &[0; 4]);
        assert_eq!(&resp[8..16], &HOMEPOD_REQUEST[24..32]);
        assert_eq!(&resp[16..24], &now.to_be_bytes());
        assert_eq!(&resp[24..32], &now.to_be_bytes());
    }

    #[test]
    fn ignores_other_packets() {
        let mut sync = HOMEPOD_REQUEST;
        sync[1] = 0xd4;
        assert!(reply(&sync, 0).is_none());
        assert!(reply(&HOMEPOD_REQUEST[..16], 0).is_none());
    }

    #[test]
    fn ntp_now_is_after_2026() {
        // 2026-01-01T00:00:00Z in NTP seconds.
        assert!(ntp_now() >> 32 > 1_767_225_600 + NTP_UNIX_OFFSET);
    }
}
