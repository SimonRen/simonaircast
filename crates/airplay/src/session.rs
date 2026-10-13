//! A playback session with one receiver.
//!
//! Sequence (mirrors pyatv's AirPlay 2 realtime-audio flow, proven on HomePod OS 27):
//! pair → `GET /info` → SETUP (session: timing port, NTP) → connect event
//! channel → SETUP (stream: type 96, PCM 44.1 kHz) → `POST /feedback` every 2 s
//! → RECORD → FLUSH → audio ([`Session::play`]) → TEARDOWN.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use plist::{Dictionary, Value};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::events::EventChannel;
use crate::pairing::{self, SessionKeys};
use crate::rtsp::{Connection, Protocol, Request};
use crate::stream::{
    self, Codec, FRAMES_PER_PACKET, PcmPacket, PcmSource, RtpSender, SAMPLE_RATE,
    SAMPLES_PER_PACKET,
};
use crate::timing::{TimingServer, ntp_now};
use crate::{Error, Result};

const BPLIST: &str = "application/x-apple-binary-plist";
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
const SYNC_INTERVAL: Duration = Duration::from_secs(1);
/// Lag that starts to eat into the receiver's buffer meaningfully.
pub const LATE_THRESHOLD: Duration = Duration::from_millis(100);
/// Further behind than this, missed slots are skipped instead of sent in a
/// burst: they would arrive too late to play, and a burst delays the packets
/// that can still make it (AirFlash uses the same idea).
pub const SKIP_THRESHOLD: Duration = Duration::from_millis(60);

/// Realtime audio stream type (44.1 kHz/16-bit/stereo, codec per [`Codec`]).
const STREAM_TYPE_REALTIME: i64 = 96;

/// Session parameters beyond the receiver's address.
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    pub ports: LocalPorts,
    pub codec: Codec,
    /// `latencyMin` / `latencyMax` announced in the stream SETUP, in frames at
    /// 44.1 kHz. Senders disagree on whether HomePods honour these on top of
    /// the sync-packet delay (docs/research/2026-10-13-open-source-senders.md);
    /// they are configurable so the latency test can measure it.
    pub latency_min: u32,
    pub latency_max: u32,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            ports: LocalPorts::default(),
            codec: Codec::Pcm,
            latency_min: 11_025,
            latency_max: 88_200,
        }
    }
}

/// Counters reported while and after playing.
#[derive(Debug, Clone, Default)]
pub struct PlayStats {
    pub packets_sent: u64,
    pub retransmit_requests: u64,
    pub packets_resent: u64,
    /// Packets sent more than [`LATE_THRESHOLD`] after their due time. Small
    /// lag is normal: Windows timers tick every ~15.6 ms, so packets (8 ms each)
    /// go out in pairs; the receiver's buffer absorbs that.
    pub late_packets: u64,
    /// Worst lag between a packet's due time and when it was sent.
    pub max_lag: Duration,
    /// Slots given up because the sender fell more than [`SKIP_THRESHOLD`] behind.
    pub skipped_packets: u64,
    /// Retransmit requests for packets already past their play time.
    pub expired_retransmits: u64,
    pub elapsed: Duration,
}

/// Local UDP ports the receiver sends to: NTP timing requests and retransmit
/// requests. 0 = any free port.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalPorts {
    pub timing: u16,
    pub control: u16,
}

/// What the receiver told us about the audio stream in the stream SETUP.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// UDP port for RTP audio packets.
    pub data_port: u16,
    /// Receiver UDP port for sync packets / retransmit requests.
    pub control_port: u16,
    pub latency_ms: Option<i64>,
    /// Key for encrypting audio payloads (we choose it and send it in SETUP).
    pub audio_key: [u8; 32],
}

pub struct Session {
    conn: Arc<Mutex<Connection>>,
    uri: String,
    session_id: u32,
    local_ip: IpAddr,
    remote_ip: IpAddr,
    info: Dictionary,
    stream: StreamInfo,
    timing: TimingServer,
    events: EventChannel,
    control_socket: Arc<UdpSocket>,
    /// Logs control traffic until [`Session::play`] takes the socket over.
    control_drain: std::sync::Mutex<Option<JoinHandle<()>>>,
    feedback_task: JoinHandle<()>,
    feedback_failures: Arc<AtomicU32>,
    torn_down: AtomicBool,
    options: SessionOptions,
    _keys: SessionKeys,
}

/// Consecutive failed keepalives (2 s apart, 2 s timeout each) after which
/// the receiver is suspected gone…
const SUSPECT_FEEDBACK_FAILURES: u32 = 2;
/// …unless it is still asking us for the time within this window: then the
/// RTSP connection is merely stalled (busy Wi-Fi) and we ride it out
/// (idea from playfruit's two-signal liveness check).
const TIMING_STILL_ALIVE: Duration = Duration::from_secs(5);
/// Failures after which we give up regardless (~12 s, as AirFlash).
const MAX_FEEDBACK_FAILURES: u32 = 6;
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(2);

impl Session {
    pub async fn connect(addr: SocketAddr, sender_name: &str) -> Result<Self> {
        Self::connect_with(addr, sender_name, SessionOptions::default()).await
    }

    /// Like [`connect`](Self::connect) with explicit options: preferred local
    /// UDP ports (so a firewall rule can name them), codec and SETUP latency.
    pub async fn connect_with(
        addr: SocketAddr,
        sender_name: &str,
        options: SessionOptions,
    ) -> Result<Self> {
        let ports = options.ports;
        let mut conn = Connection::connect(addr).await?;
        let local_ip = conn.local_addr()?.ip();
        let keys = pairing::pair_transient(&mut conn).await?;

        let session_id = random_u32()?;
        let dacp_id = format!("{:016X}", random_u64()?);
        conn.set_session_headers(vec![
            ("DACP-ID".into(), dacp_id.clone()),
            ("Active-Remote".into(), random_u32()?.to_string()),
            ("Client-Instance".into(), dacp_id),
        ]);
        let uri = format!("rtsp://{local_ip}/{session_id}");

        let info = conn
            .request_ok(Request::new("GET", "/info", Protocol::Rtsp10))
            .await?
            .plist()?
            .into_dictionary()
            .unwrap_or_default();

        // Must be listening before the first SETUP: the receiver starts sending
        // timing requests right away and blocks the stream SETUP until answered.
        let timing = TimingServer::bind(local_ip, ports.timing).await?;

        let setup = dict([
            ("deviceID", "AA:BB:CC:DD:EE:FF".into()),
            ("sessionUUID", uuid_upper()?.into()),
            ("timingPort", int(timing.port().into())),
            ("timingProtocol", "NTP".into()),
            ("isMultiSelectAirPlay", true.into()),
            ("groupContainsGroupLeader", false.into()),
            ("macAddress", "AA:BB:CC:DD:EE:FF".into()),
            // Sender identity as pyatv sends it; receivers may branch on these.
            ("model", "iPhone14,3".into()),
            ("name", sender_name.into()),
            ("osBuildVersion", "20F66".into()),
            ("osName", "iPhone OS".into()),
            ("osVersion", "16.5".into()),
            ("senderSupportsRelay", false.into()),
            ("sourceVersion", "690.7.1".into()),
            ("statsCollectionEnabled", false.into()),
        ]);
        let resp = conn
            .request_ok(Request::new("SETUP", &uri, Protocol::Rtsp10).body(BPLIST, bplist(&setup)?))
            .await?
            .plist()?;
        let event_port = get_int(&resp, "eventPort")
            .ok_or_else(|| Error::Protocol("session SETUP reply has no eventPort".into()))?;

        let events = connect_events(SocketAddr::new(addr.ip(), event_port as u16), &keys).await?;

        let control_socket = Arc::new(crate::bind_udp(local_ip, ports.control).await?);
        let control_task = spawn_control_drain(control_socket.clone());

        let audio_key: [u8; 32] = random_bytes()?;
        let stream_setup = dict([(
            "streams",
            Value::Array(vec![dict([
                ("audioFormat", int(options.codec.audio_format())),
                ("audioMode", "default".into()),
                (
                    "controlPort",
                    int(control_socket.local_addr()?.port().into()),
                ),
                ("ct", int(options.codec.compression_type())),
                ("isMedia", true.into()),
                ("latencyMax", int(options.latency_max.into())),
                ("latencyMin", int(options.latency_min.into())),
                ("shk", Value::Data(audio_key.to_vec())),
                ("spf", int(FRAMES_PER_PACKET as i64)),
                ("sr", int(SAMPLE_RATE.into())),
                ("type", int(STREAM_TYPE_REALTIME)),
                ("supportsDynamicStreamID", false.into()),
                ("streamConnectionID", int(session_id.into())),
            ])]),
        )]);
        let resp = match conn
            .request_ok(
                Request::new("SETUP", &uri, Protocol::Rtsp10).body(BPLIST, bplist(&stream_setup)?),
            )
            .await
        {
            Err(Error::Timeout(_)) if timing.answered() == 0 => {
                return Err(Error::Protocol(format!(
                    "stream SETUP got no reply and no timing request reached us; \
                     a firewall is probably dropping UDP from {} to port {}",
                    addr.ip(),
                    timing.port()
                )));
            }
            other => other?.plist()?,
        };
        let stream = resp
            .as_dictionary()
            .and_then(|d| d.get("streams")?.as_array()?.first().cloned())
            .ok_or_else(|| Error::Protocol("stream SETUP reply has no streams".into()))?;
        let stream = StreamInfo {
            data_port: get_int(&stream, "dataPort")
                .ok_or_else(|| Error::Protocol("stream SETUP reply has no dataPort".into()))?
                as u16,
            control_port: get_int(&stream, "controlPort").unwrap_or(0) as u16,
            latency_ms: get_int(&stream, "arrivalToRenderLatencyMs"),
            audio_key,
        };
        tracing::debug!(
            ?stream,
            timing_answered = timing.answered(),
            "stream set up"
        );

        let conn = Arc::new(Mutex::new(conn));
        let feedback_failures = Arc::new(AtomicU32::new(0));
        let feedback_task = spawn_feedback(conn.clone(), feedback_failures.clone());

        Ok(Self {
            conn,
            uri,
            session_id,
            local_ip,
            remote_ip: addr.ip(),
            info,
            stream,
            timing,
            events,
            control_socket,
            control_drain: std::sync::Mutex::new(Some(control_task)),
            feedback_task,
            feedback_failures,
            torn_down: AtomicBool::new(false),
            options,
            _keys: keys,
        })
    }

    /// Why the session can no longer play, if it can't.
    fn health(&self) -> Result<()> {
        if self.events.closed_by_receiver() {
            return Err(Error::SessionEnded);
        }
        let failures = self.feedback_failures.load(Ordering::Relaxed);
        let timing_alive = self
            .timing
            .since_last_request()
            .is_some_and(|age| age < TIMING_STILL_ALIVE);
        if receiver_gone(failures, timing_alive) {
            return Err(Error::ReceiverUnreachable);
        }
        Ok(())
    }

    /// Starts playback and streams `source` in real time until it is exhausted,
    /// followed by `latency` of silence so the tail plays out. Each packet plays
    /// `latency` after it is sent (plus the receiver's own render delay).
    /// Calls `progress` about once a second.
    pub async fn play(
        &self,
        source: &mut dyn PcmSource,
        latency: Duration,
        mut progress: impl FnMut(&PlayStats),
    ) -> Result<PlayStats> {
        let latency_frames = stream::latency_frames(latency);
        self.stop_control_drain();
        let data = UdpSocket::bind(SocketAddr::new(self.local_ip, 0)).await?;
        data.connect(SocketAddr::new(self.remote_ip, self.stream.data_port))
            .await?;
        let seq = u16::from_le_bytes(random_bytes()?);
        let mut sender = RtpSender::new(
            Arc::new(data),
            &self.stream.audio_key,
            self.options.codec,
            self.session_id,
            seq,
            latency_frames,
        );
        // A resend is useless once the packet's play time has passed; the
        // play time is at least the larger of our delay and latencyMin.
        let resend_window = latency.max(Duration::from_secs_f64(
            f64::from(self.options.latency_min) / f64::from(SAMPLE_RATE),
        ));
        let packet_duration =
            Duration::from_secs_f64(FRAMES_PER_PACKET as f64 / f64::from(SAMPLE_RATE));
        let control = self.control_socket.clone();
        let receiver_control = SocketAddr::new(self.remote_ip, self.stream.control_port);

        // Clock model: RTP time latency_frames + n is sent at start + n samples,
        // and the sync packets tell the receiver that RTP time n plays at
        // start_ntp + n samples, i.e. `latency` after it was sent.
        self.record(seq, latency_frames).await?;
        let start_ntp = ntp_now();
        let start = Instant::now();

        let mut stats = PlayStats::default();
        let mut pcm: PcmPacket = [0; SAMPLES_PER_PACKET];
        let mut sent_frames: u64 = 0;
        let mut source_done = false;
        let mut padding_left = u64::from(latency_frames);
        let mut sync = tokio::time::interval(SYNC_INTERVAL);
        let mut first_sync = true;
        let mut buf = [0u8; 2048];

        loop {
            let due = start + Duration::from_secs_f64(sent_frames as f64 / f64::from(SAMPLE_RATE));
            // Order matters: sync first (cheap, keeps the receiver's clock
            // mapping fresh), then new audio, and only then retransmits — new
            // media must never wait behind resends (AirFlash does the same).
            tokio::select! {
                biased;
                _ = sync.tick() => {
                    self.health()?;
                    let (_, head) = sender.head();
                    let ntp = start_ntp + ((sent_frames << 32) / u64::from(SAMPLE_RATE));
                    let packet = stream::sync_packet(first_sync, head, latency_frames, ntp);
                    if let Err(e) = control.send_to(&packet, receiver_control).await {
                        tracing::warn!(error = %e, "sync packet send failed");
                    }
                    first_sync = false;
                    stats.elapsed = start.elapsed();
                    progress(&stats);
                }
                _ = tokio::time::sleep_until(due) => {
                    let lag = Instant::now().saturating_duration_since(due);
                    stats.max_lag = stats.max_lag.max(lag);
                    if lag > LATE_THRESHOLD {
                        stats.late_packets += 1;
                    }
                    if lag > SKIP_THRESHOLD {
                        // Fell far behind (e.g. the PC stalled): give up the slots
                        // that would arrive too late, discarding their audio, and
                        // re-sync right away so the receiver re-anchors.
                        let missed = (lag.as_nanos() / packet_duration.as_nanos()) as u64;
                        for _ in 0..missed {
                            if !source_done && !source.fill(&mut pcm) {
                                source_done = true;
                            }
                            sender.skip();
                            sent_frames += FRAMES_PER_PACKET as u64;
                        }
                        stats.skipped_packets += missed;
                        sync.reset_immediately();
                        tracing::warn!(lag_ms = lag.as_millis() as u64, missed, "sender fell behind; skipped late slots");
                        continue;
                    }
                    if !source_done && !source.fill(&mut pcm) {
                        source_done = true;
                    }
                    if source_done {
                        if padding_left == 0 {
                            break;
                        }
                        pcm = [0; SAMPLES_PER_PACKET];
                        padding_left = padding_left.saturating_sub(FRAMES_PER_PACKET as u64);
                    }
                    sender.send(&pcm).await?;
                    sent_frames += FRAMES_PER_PACKET as u64;
                    stats.packets_sent += 1;
                }
                received = control.recv_from(&mut buf) => {
                    // Windows surfaces ICMP errors as recv failures; ignore them.
                    let Ok((n, from)) = received else { continue };
                    if let Some((first, count)) = stream::parse_retransmit_request(&buf[..n]) {
                        stats.retransmit_requests += 1;
                        let outcome = sender.retransmit(&control, first, count, from, resend_window).await;
                        stats.packets_resent += outcome.resent as u64;
                        stats.expired_retransmits += outcome.expired as u64;
                        tracing::debug!(first, count, ?outcome, "retransmit request");
                    } else {
                        tracing::debug!(len = n, kind = format!("{:#04x}", buf[1]), "unhandled control packet");
                    }
                }
            }
        }
        stats.elapsed = start.elapsed();
        Ok(stats)
    }

    /// Starts playback: RECORD, then FLUSH positioned at `seq`/`rtptime`.
    pub async fn record(&self, seq: u16, rtptime: u32) -> Result<()> {
        let mut conn = self.conn.lock().await;
        conn.request_ok(Request::new("RECORD", &self.uri, Protocol::Rtsp10))
            .await?;
        conn.request_ok(
            Request::new("FLUSH", &self.uri, Protocol::Rtsp10)
                .header("Range", "npt=0-")
                .header("Session", 0)
                .header("RTP-Info", format!("seq={seq};rtptime={rtptime}")),
        )
        .await?;
        Ok(())
    }

    /// Volume in dB: 0.0 is full, -30.0 is quietest, -144.0 is mute.
    pub async fn set_volume(&self, db: f64) -> Result<()> {
        self.conn
            .lock()
            .await
            .request_ok(
                Request::new("SET_PARAMETER", &self.uri, Protocol::Rtsp10)
                    .body("text/parameters", format!("volume: {db:.6}").into_bytes()),
            )
            .await?;
        Ok(())
    }

    pub async fn teardown(self) -> Result<()> {
        self.torn_down.store(true, Ordering::Relaxed);
        self.feedback_task.abort();
        self.stop_control_drain();
        self.conn
            .lock()
            .await
            .request_ok(Request::new("TEARDOWN", &self.uri, Protocol::Rtsp10).header("Session", 0))
            .await?;
        Ok(())
    }

    /// The receiver's `GET /info` dictionary (name, model, initialVolume, ...).
    pub fn info(&self) -> &Dictionary {
        &self.info
    }

    pub fn stream(&self) -> &StreamInfo {
        &self.stream
    }

    pub fn remote_ip(&self) -> IpAddr {
        self.remote_ip
    }

    /// Local UDP socket whose port was announced as our control port.
    pub fn control_socket(&self) -> &Arc<UdpSocket> {
        &self.control_socket
    }

    pub fn timing_requests_answered(&self) -> u64 {
        self.timing.answered()
    }

    pub fn events_received(&self) -> u64 {
        self.events.received()
    }
}

/// Some receivers open the event port shortly after replying; retry briefly.
async fn connect_events(addr: SocketAddr, keys: &SessionKeys) -> Result<EventChannel> {
    let mut attempt = 0;
    loop {
        match EventChannel::connect(addr, keys).await {
            Err(Error::Io(e))
                if e.kind() == std::io::ErrorKind::ConnectionRefused && attempt < 5 =>
            {
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            other => return other,
        }
    }
}

/// Liveness decision from the two signals: failed keepalives and whether the
/// receiver still sends timing requests.
fn receiver_gone(feedback_failures: u32, timing_alive: bool) -> bool {
    if feedback_failures >= MAX_FEEDBACK_FAILURES {
        return true;
    }
    feedback_failures >= SUSPECT_FEEDBACK_FAILURES && !timing_alive
}

/// Keepalive; counts consecutive failures so `play` can notice a dead receiver.
fn spawn_feedback(conn: Arc<Mutex<Connection>>, failures: Arc<AtomicU32>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(FEEDBACK_INTERVAL);
        loop {
            tick.tick().await;
            let result = conn
                .lock()
                .await
                .request_ok(
                    Request::new("POST", "/feedback", Protocol::Rtsp10).timeout(FEEDBACK_TIMEOUT),
                )
                .await;
            match result {
                Ok(_) => failures.store(0, Ordering::Relaxed),
                Err(e) => {
                    let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::warn!(error = %e, consecutive = n, "feedback keepalive failed");
                }
            }
        }
    })
}

/// Until audio streaming exists, just log what the receiver sends to our control port.
impl Session {
    fn stop_control_drain(&self) {
        if let Some(drain) = self.control_drain.lock().ok().and_then(|mut d| d.take()) {
            drain.abort();
        }
    }
}

/// A session dropped without [`Session::teardown`] (early `?` return, task
/// cancelled) must not leave its keepalive running against the receiver —
/// that leaked one task per failed attempt in the first tray build. Stop the
/// background work and send a best-effort TEARDOWN so the receiver is freed.
impl Drop for Session {
    fn drop(&mut self) {
        self.feedback_task.abort();
        self.stop_control_drain();
        if self.torn_down.swap(true, Ordering::Relaxed) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let conn = self.conn.clone();
        let request = Request::new("TEARDOWN", &self.uri, Protocol::Rtsp10).header("Session", 0);
        runtime.spawn(async move {
            let teardown = async { conn.lock().await.request_ok(request).await };
            let _ = tokio::time::timeout(Duration::from_secs(2), teardown).await;
        });
    }
}

fn spawn_control_drain(socket: Arc<UdpSocket>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
            tracing::debug!(%peer, len = n, kind = format!("{:#04x}", buf.get(1).copied().unwrap_or(0)), "control packet");
        }
    })
}

fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Dictionary(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn int(v: i64) -> Value {
    Value::Integer(v.into())
}

fn get_int(v: &Value, key: &str) -> Option<i64> {
    v.as_dictionary()?.get(key)?.as_signed_integer()
}

fn bplist(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    v.to_writer_binary(&mut out)?;
    Ok(out)
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|e| Error::Protocol(format!("no system randomness: {e}")))?;
    Ok(out)
}

fn random_u32() -> Result<u32> {
    Ok(u32::from_le_bytes(random_bytes()?))
}

fn random_u64() -> Result<u64> {
    Ok(u64::from_le_bytes(random_bytes()?))
}

/// Random (v4-style) UUID, uppercase, as senders put in `sessionUUID`.
fn uuid_upper() -> Result<String> {
    let mut b: [u8; 16] = random_bytes()?;
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h = hex::encode_upper(b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_setup_plist_round_trips() {
        let v = dict([(
            "streams",
            Value::Array(vec![dict([
                ("type", int(96)),
                ("shk", Value::Data(vec![1; 32])),
            ])]),
        )]);
        let bytes = bplist(&v).unwrap();
        assert!(bytes.starts_with(b"bplist00"));
        let back = Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
        let stream = back.as_dictionary().unwrap()["streams"].as_array().unwrap()[0].clone();
        assert_eq!(get_int(&stream, "type"), Some(96));
    }

    #[test]
    fn liveness_rides_out_rtsp_stalls_while_timing_continues() {
        assert!(!receiver_gone(0, false));
        assert!(!receiver_gone(1, false));
        assert!(
            receiver_gone(2, false),
            "keepalives failing, no timing: gone"
        );
        assert!(
            !receiver_gone(2, true),
            "keepalives failing, timing alive: stall"
        );
        assert!(!receiver_gone(5, true));
        assert!(receiver_gone(6, true), "hard limit regardless of timing");
    }

    #[test]
    fn uuid_has_canonical_shape() {
        let u = uuid_upper().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(
            u.chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
    }
}
