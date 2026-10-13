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
use std::time::{Duration, Instant};

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

/// How the samples are encoded inside the encrypted packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// Raw big-endian 16-bit PCM (`ct` 1, `audioFormat` 0x800). Verified clean
    /// on HomePod (AudioAccessory1,1, OS 27).
    #[default]
    Pcm,
    /// The same samples bit-packed as uncompressed ALAC "escape" frames
    /// (`ct` 2, `audioFormat` 0x40000). Fallback for receivers reported to
    /// decode stream type 96 as ALAC whatever `ct` says (DeetsAirplay,
    /// airplay2-sender-cpp). Lossless; +4 bytes per packet.
    AlacUncompressed,
}

impl Codec {
    /// `ct` in the stream SETUP.
    pub fn compression_type(self) -> i64 {
        match self {
            Codec::Pcm => 1,
            Codec::AlacUncompressed => 2,
        }
    }

    /// `audioFormat` bit in the stream SETUP (44.1 kHz / 16-bit / stereo).
    pub fn audio_format(self) -> i64 {
        match self {
            Codec::Pcm => 0x800,
            Codec::AlacUncompressed => 0x40000,
        }
    }

    pub fn encode(self, pcm: &PcmPacket) -> Vec<u8> {
        match self {
            Codec::Pcm => {
                let mut out = Vec::with_capacity(SAMPLES_PER_PACKET * 2);
                for sample in pcm {
                    out.extend_from_slice(&sample.to_be_bytes());
                }
                out
            }
            Codec::AlacUncompressed => alac_escape_frame(pcm),
        }
    }
}

/// One ALAC frame in "escape" (uncompressed) mode, stereo element:
/// tag `001` (CPE), 4-bit instance, 12 unused bits, partial-frame flag 0,
/// 2-bit shift 0, escape flag 1, then the samples MSB-first as 16-bit
/// big-endian (L, R interleaved), the END tag `111`, and padding to a byte.
fn alac_escape_frame(pcm: &PcmPacket) -> Vec<u8> {
    let mut bits = BitWriter::with_capacity(SAMPLES_PER_PACKET * 2 + 4);
    bits.write(1, 3);
    bits.write(0, 4);
    bits.write(0, 12);
    bits.write(0, 1);
    bits.write(0, 2);
    bits.write(1, 1);
    for &sample in pcm {
        bits.write(u32::from(sample as u16), 16);
    }
    bits.write(0b111, 3);
    bits.finish()
}

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    fn with_capacity(bytes: usize) -> Self {
        Self {
            out: Vec::with_capacity(bytes),
            acc: 0,
            nbits: 0,
        }
    }

    /// Appends the low `n` bits of `value` (n ≤ 32), most significant first.
    /// Only the unflushed bits (< 8 + n) of `acc` matter; older bits shift out.
    fn write(&mut self, value: u32, n: u32) {
        self.acc = (self.acc << n) | (u64::from(value) & ((1u64 << n) - 1));
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push((self.acc << (8 - self.nbits)) as u8);
        }
        self.out
    }
}

/// What happened to a retransmit request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetransmitOutcome {
    pub resent: usize,
    /// Older than the playout delay: the receiver can no longer use them.
    pub expired: usize,
    /// Never sent (skipped slots) or already out of the backlog.
    pub missing: usize,
}

pub struct RtpSender {
    socket: Arc<UdpSocket>,
    cipher: Cipher,
    codec: Codec,
    ssrc: u32,
    seq: u16,
    rtptime: u32,
    first: bool,
    backlog: Backlog,
}

impl RtpSender {
    /// `socket` must be connected to the receiver's data port.
    pub fn new(
        socket: Arc<UdpSocket>,
        key: &[u8; 32],
        codec: Codec,
        ssrc: u32,
        seq: u16,
        rtptime: u32,
    ) -> Self {
        Self {
            socket,
            cipher: Cipher::new(key),
            codec,
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
        self.backlog.push(
            self.seq,
            Some(Sent {
                packet,
                at: Instant::now(),
            }),
        );
        self.advance();
        self.first = false;
        Ok(())
    }

    /// Gives up on one packet slot without sending it: sequence number and RTP
    /// time advance so the timeline stays aligned with the clock. Used when the
    /// sender fell so far behind that the slot would arrive too late anyway —
    /// skipping beats a burst (idea from AirFlash). No nonce is consumed.
    pub fn skip(&mut self) {
        self.backlog.push(self.seq, None);
        self.advance();
    }

    fn advance(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        self.rtptime = self.rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
    }

    fn build(&mut self, pcm: &PcmPacket) -> Vec<u8> {
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = if self.first { 0xE0 } else { 0x60 };
        header[2..4].copy_from_slice(&self.seq.to_be_bytes());
        header[4..8].copy_from_slice(&self.rtptime.to_be_bytes());
        header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        let payload = self.codec.encode(pcm);
        let nonce = self.cipher.counter().to_le_bytes();
        let sealed = self.cipher.seal(&payload, &header[4..12]);

        let mut packet = Vec::with_capacity(12 + sealed.len() + 8);
        packet.extend_from_slice(&header);
        packet.extend(sealed);
        packet.extend_from_slice(&nonce);
        packet
    }

    /// Answers a retransmit request with the original packets (byte-identical,
    /// same nonce). Packets sent longer than `max_age` ago are not resent: past
    /// the playout delay the receiver has already played (or concealed) them,
    /// and resending only adds traffic during the congestion that lost them.
    pub async fn retransmit(
        &self,
        control: &UdpSocket,
        first: u16,
        count: u16,
        to: std::net::SocketAddr,
        max_age: Duration,
    ) -> RetransmitOutcome {
        let mut outcome = RetransmitOutcome::default();
        for i in 0..count {
            let seq = first.wrapping_add(i);
            let Some(sent) = self.backlog.get(seq) else {
                outcome.missing += 1;
                continue;
            };
            if sent.at.elapsed() > max_age {
                outcome.expired += 1;
                continue;
            }
            let mut resp = Vec::with_capacity(4 + sent.packet.len());
            resp.extend_from_slice(&[0x80, 0xD6]);
            resp.extend_from_slice(&sent.packet[2..4]);
            resp.extend_from_slice(&sent.packet);
            if control.send_to(&resp, to).await.is_ok() {
                outcome.resent += 1;
            }
        }
        outcome
    }
}

struct Sent {
    packet: Vec<u8>,
    at: Instant,
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

/// The last `BACKLOG_PACKETS` packet slots, indexed by sequence number
/// (`None` for skipped slots).
struct Backlog {
    first_seq: u16,
    packets: VecDeque<Option<Sent>>,
}

impl Backlog {
    fn new(first_seq: u16) -> Self {
        Self {
            first_seq,
            packets: VecDeque::with_capacity(BACKLOG_PACKETS),
        }
    }

    fn push(&mut self, seq: u16, packet: Option<Sent>) {
        debug_assert_eq!(seq, self.first_seq.wrapping_add(self.packets.len() as u16));
        if self.packets.len() == BACKLOG_PACKETS {
            self.packets.pop_front();
            self.first_seq = self.first_seq.wrapping_add(1);
        }
        self.packets.push_back(packet);
    }

    fn get(&self, seq: u16) -> Option<&Sent> {
        self.packets
            .get(seq.wrapping_sub(self.first_seq) as usize)?
            .as_ref()
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
            Codec::Pcm,
            0xDEAD_BEEF,
            0xFFFF,
            LATENCY_FRAMES,
        )
    }

    fn sent(byte: u8) -> Option<Sent> {
        Some(Sent {
            packet: vec![byte],
            at: Instant::now(),
        })
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
        let second = &s.backlog.get(0).unwrap().packet;
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
            b.push(10 + i, sent(i as u8));
        }
        assert!(b.get(14).is_none());
        assert_eq!(b.get(15).map(|s| s.packet.clone()), Some(vec![5]));
    }

    #[tokio::test]
    async fn skipped_slots_advance_the_timeline_but_cannot_be_resent() {
        let mut s = sender().await;
        let pcm = [0i16; SAMPLES_PER_PACKET];
        s.send(&pcm).await.unwrap(); // seq 0xFFFF
        s.skip(); // seq 0
        s.skip(); // seq 1
        s.send(&pcm).await.unwrap(); // seq 2
        assert_eq!(s.head(), (3, LATENCY_FRAMES + 4 * FRAMES_PER_PACKET as u32));
        assert!(s.backlog.get(0).is_none() && s.backlog.get(1).is_none());
        let last = &s.backlog.get(2).unwrap().packet;
        assert_eq!(
            &last[last.len() - 8..],
            &1u64.to_le_bytes(),
            "skips consume no nonce"
        );
    }

    #[tokio::test]
    async fn retransmit_resends_fresh_packets_and_drops_expired_ones() {
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let to = receiver.local_addr().unwrap();
        let mut s = sender().await;
        let pcm = [0i16; SAMPLES_PER_PACKET];
        s.send(&pcm).await.unwrap(); // seq 0xFFFF
        s.skip(); // seq 0
        s.send(&pcm).await.unwrap(); // seq 1

        let fresh = s
            .retransmit(&control, 0xFFFF, 3, to, Duration::from_secs(5))
            .await;
        assert_eq!(
            fresh,
            RetransmitOutcome {
                resent: 2,
                expired: 0,
                missing: 1
            }
        );
        let mut buf = [0u8; 2048];
        let n = receiver.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..4], &[0x80, 0xD6, 0xFF, 0xFF]);
        assert_eq!(&buf[4..n], &s.backlog.get(0xFFFF).unwrap().packet[..]);

        tokio::time::sleep(Duration::from_millis(5)).await;
        let stale = s
            .retransmit(&control, 0xFFFF, 1, to, Duration::from_millis(1))
            .await;
        assert_eq!(stale.expired, 1);
    }

    #[test]
    fn alac_escape_frame_layout() {
        let mut pcm = [0i16; SAMPLES_PER_PACKET];
        let silent = Codec::AlacUncompressed.encode(&pcm);
        // 23 header bits + 704 × 16 sample bits + 3 end bits = 11290 → 1412 bytes.
        assert_eq!(silent.len(), 1412);
        assert_eq!(&silent[..3], &[0x20, 0x00, 0x02], "CPE tag, escape flag");
        assert_eq!(&silent[1410..], &[0x01, 0xC0], "END tag 111 then padding");

        pcm[0] = i16::MIN; // 0x8000: its MSB lands in the last bit of byte 2.
        pcm[1] = 0x1234;
        let frame = Codec::AlacUncompressed.encode(&pcm);
        assert_eq!(frame[2], 0x03);
        // Left = 0x8000 spans bits 23..39, right = 0x1234 bits 39..55.
        assert_eq!(&frame[3..7], &[0x00, 0x00, 0x24, 0x68]);
    }

    #[test]
    fn codec_setup_values() {
        assert_eq!(
            (Codec::Pcm.compression_type(), Codec::Pcm.audio_format()),
            (1, 0x800)
        );
        assert_eq!(
            (
                Codec::AlacUncompressed.compression_type(),
                Codec::AlacUncompressed.audio_format()
            ),
            (2, 0x40000)
        );
        let pcm = [0x0102i16; SAMPLES_PER_PACKET];
        assert_eq!(&Codec::Pcm.encode(&pcm)[..2], &[0x01, 0x02]);
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
