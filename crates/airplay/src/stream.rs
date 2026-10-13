//! Realtime audio over RTP (stream type 96), as pyatv sends it.
//!
//! Audio packet: `80 60|E0 seq(u16) rtptime(u32) ssrc(u32)` + ChaCha20-Poly1305
//! ciphertext of 352 big-endian 16-bit stereo frames (key = `shk` from SETUP,
//! nonce = 4 zero bytes + 8-byte LE counter, AAD = header bytes 4..12) + the
//! 8 counter bytes. The first packet has the marker bit set (`E0`).
//!
//! Control port traffic: we send sync packets (`D4`) once a second mapping an
//! RTP time to NTP time; the receiver asks for lost packets (`D5`) and we resend
//! them from a backlog wrapped as `80 D6 <seq>` + original packet.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::Result;
use crate::crypto::Cipher;

pub const FRAMES_PER_PACKET: usize = 352;
pub const CHANNELS: usize = 2;
pub const SAMPLES_PER_PACKET: usize = FRAMES_PER_PACKET * CHANNELS;
pub const SAMPLE_RATE: u32 = 44_100;
/// Default gap between a packet being sent and played: 250 ms, the
/// `latencyMin` we announce in SETUP. pyatv uses 1.5 s, which is fine for files
/// but puts live PC audio audibly behind.
pub const DEFAULT_LATENCY: std::time::Duration = std::time::Duration::from_millis(250);

/// Converts a latency to RTP frames at 44.1 kHz.
pub fn latency_frames(latency: std::time::Duration) -> u32 {
    (latency.as_secs_f64() * f64::from(SAMPLE_RATE)).round() as u32
}
const BACKLOG_PACKETS: usize = 1000;

/// One packet of interleaved 16-bit stereo PCM at 44.1 kHz.
pub type PcmPacket = [i16; SAMPLES_PER_PACKET];

/// Supplies audio one packet at a time.
pub trait PcmSource: Send {
    /// Fills `packet` and returns `true`, or returns `false` once exhausted.
    /// A short final read must be zero-padded by the source.
    fn fill(&mut self, packet: &mut PcmPacket) -> bool;
}

pub struct RtpSender {
    socket: Arc<UdpSocket>,
    cipher: Cipher,
    ssrc: u32,
    seq: u16,
    rtptime: u32,
    first: bool,
    backlog: Backlog,
}

impl RtpSender {
    /// `socket` must be connected to the receiver's data port.
    pub fn new(socket: Arc<UdpSocket>, key: &[u8; 32], ssrc: u32, seq: u16, rtptime: u32) -> Self {
        Self {
            socket,
            cipher: Cipher::new(key),
            ssrc,
            seq,
            rtptime,
            first: true,
            backlog: Backlog::new(seq),
        }
    }

    /// Next sequence number and RTP time to be sent.
    pub fn head(&self) -> (u16, u32) {
        (self.seq, self.rtptime)
    }

    pub async fn send(&mut self, pcm: &PcmPacket) -> Result<()> {
        let packet = self.build(pcm);
        self.socket.send(&packet).await?;
        self.backlog.push(self.seq, packet);
        self.seq = self.seq.wrapping_add(1);
        self.rtptime = self.rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
        self.first = false;
        Ok(())
    }

    fn build(&mut self, pcm: &PcmPacket) -> Vec<u8> {
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = if self.first { 0xE0 } else { 0x60 };
        header[2..4].copy_from_slice(&self.seq.to_be_bytes());
        header[4..8].copy_from_slice(&self.rtptime.to_be_bytes());
        header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        let mut payload = Vec::with_capacity(SAMPLES_PER_PACKET * 2);
        for sample in pcm {
            payload.extend_from_slice(&sample.to_be_bytes());
        }
        let nonce = self.cipher.counter().to_le_bytes();
        let sealed = self.cipher.seal(&payload, &header[4..12]);

        let mut packet = Vec::with_capacity(12 + sealed.len() + 8);
        packet.extend_from_slice(&header);
        packet.extend(sealed);
        packet.extend_from_slice(&nonce);
        packet
    }

    /// Answers a retransmit request; returns how many packets were resent.
    pub async fn retransmit(
        &self,
        control: &UdpSocket,
        first: u16,
        count: u16,
        to: std::net::SocketAddr,
    ) -> usize {
        let mut resent = 0;
        for i in 0..count {
            let seq = first.wrapping_add(i);
            let Some(packet) = self.backlog.get(seq) else {
                tracing::debug!(seq, "lost packet no longer in backlog");
                continue;
            };
            let mut resp = Vec::with_capacity(4 + packet.len());
            resp.extend_from_slice(&[0x80, 0xD6]);
            resp.extend_from_slice(&packet[2..4]);
            resp.extend_from_slice(packet);
            if control.send_to(&resp, to).await.is_ok() {
                resent += 1;
            }
        }
        resent
    }
}

/// Sync packet: "RTP time `rtptime - latency` plays at NTP time `ntp`;
/// `rtptime` is the next packet we send". `latency` is in frames.
pub fn sync_packet(first: bool, rtptime: u32, latency: u32, ntp: u64) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[0] = if first { 0x90 } else { 0x80 };
    out[1] = 0xD4;
    out[2..4].copy_from_slice(&7u16.to_be_bytes());
    out[4..8].copy_from_slice(&rtptime.wrapping_sub(latency).to_be_bytes());
    out[8..16].copy_from_slice(&ntp.to_be_bytes());
    out[16..20].copy_from_slice(&rtptime.to_be_bytes());
    out
}

/// Parses `80 D5 seq lost_seq(u16) count(u16)`; returns (first lost seq, count).
pub fn parse_retransmit_request(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 8 || data[1] & 0x7F != 0x55 {
        return None;
    }
    Some((
        u16::from_be_bytes([data[4], data[5]]),
        u16::from_be_bytes([data[6], data[7]]),
    ))
}

/// The last `BACKLOG_PACKETS` sent packets, indexed by sequence number.
struct Backlog {
    first_seq: u16,
    packets: VecDeque<Vec<u8>>,
}

impl Backlog {
    fn new(first_seq: u16) -> Self {
        Self {
            first_seq,
            packets: VecDeque::with_capacity(BACKLOG_PACKETS),
        }
    }

    fn push(&mut self, seq: u16, packet: Vec<u8>) {
        debug_assert_eq!(seq, self.first_seq.wrapping_add(self.packets.len() as u16));
        if self.packets.len() == BACKLOG_PACKETS {
            self.packets.pop_front();
            self.first_seq = self.first_seq.wrapping_add(1);
        }
        self.packets.push_back(packet);
    }

    fn get(&self, seq: u16) -> Option<&Vec<u8>> {
        self.packets.get(seq.wrapping_sub(self.first_seq) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};

    const LATENCY_FRAMES: u32 = 11_025;

    #[test]
    fn default_latency_is_250_ms_in_frames() {
        assert_eq!(latency_frames(DEFAULT_LATENCY), LATENCY_FRAMES);
    }

    async fn sender() -> RtpSender {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect("127.0.0.1:9").await.unwrap();
        RtpSender::new(
            Arc::new(socket),
            &[7; 32],
            0xDEAD_BEEF,
            0xFFFF,
            LATENCY_FRAMES,
        )
    }

    #[tokio::test]
    async fn packets_have_rtp_header_and_decrypt_with_trailing_nonce() {
        let mut s = sender().await;
        let mut pcm = [0i16; SAMPLES_PER_PACKET];
        pcm[0] = 0x0102;
        pcm[1] = -2;

        let first = s.build(&pcm);
        assert_eq!(&first[..4], &[0x80, 0xE0, 0xFF, 0xFF]);
        assert_eq!(&first[4..8], &LATENCY_FRAMES.to_be_bytes());
        assert_eq!(&first[8..12], &0xDEAD_BEEFu32.to_be_bytes());
        assert_eq!(first.len(), 12 + SAMPLES_PER_PACKET * 2 + 16 + 8);

        let (body, tail) = first[12..].split_at(first.len() - 12 - 8);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(tail);
        let plain = ChaCha20Poly1305::new(&[7u8; 32].into())
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: body,
                    aad: &first[4..12],
                },
            )
            .unwrap();
        assert_eq!(
            &plain[..4],
            &[0x01, 0x02, 0xFF, 0xFE],
            "samples are big-endian"
        );
        assert_eq!(tail, &0u64.to_le_bytes());
    }

    #[tokio::test]
    async fn sequence_and_time_advance_and_wrap() {
        let mut s = sender().await;
        let pcm = [0i16; SAMPLES_PER_PACKET];
        s.send(&pcm).await.unwrap();
        s.send(&pcm).await.unwrap();
        assert_eq!(s.head(), (1, LATENCY_FRAMES + 2 * FRAMES_PER_PACKET as u32));
        let second = s.backlog.get(0).unwrap();
        assert_eq!(
            &second[..4],
            &[0x80, 0x60, 0x00, 0x00],
            "marker only on first packet"
        );
        assert_eq!(&second[second.len() - 8..], &1u64.to_le_bytes());
        assert!(s.backlog.get(0xFFFF).is_some());
        assert!(s.backlog.get(1).is_none());
    }

    #[test]
    fn backlog_keeps_only_recent_packets() {
        let mut b = Backlog::new(10);
        for i in 0..(BACKLOG_PACKETS as u16 + 5) {
            b.push(10 + i, vec![i as u8]);
        }
        assert!(b.get(14).is_none());
        assert_eq!(b.get(15), Some(&vec![5]));
    }

    #[test]
    fn sync_packet_layout() {
        let p = sync_packet(
            true,
            LATENCY_FRAMES + 352,
            LATENCY_FRAMES,
            0x0102_0304_0506_0708,
        );
        assert_eq!(&p[..4], &[0x90, 0xD4, 0x00, 0x07]);
        assert_eq!(&p[4..8], &352u32.to_be_bytes());
        assert_eq!(&p[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&p[16..20], &(LATENCY_FRAMES + 352).to_be_bytes());
        assert_eq!(sync_packet(false, 0, 0, 0)[0], 0x80);
    }

    #[test]
    fn parses_retransmit_request() {
        assert_eq!(
            parse_retransmit_request(&[0x80, 0xD5, 0, 1, 0x12, 0x34, 0, 3]),
            Some((0x1234, 3))
        );
        assert_eq!(
            parse_retransmit_request(&[0x80, 0xD4, 0, 1, 0, 0, 0, 0]),
            None
        );
    }
}
