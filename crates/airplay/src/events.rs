//! Event channel: a second TCP connection to the `eventPort` from the first SETUP.
//!
//! The receiver uses it to push RTSP requests to us (e.g. `POST /command` with
//! `updateInfo`). We are the server on this channel, so its HAP keys are the
//! control-channel naming swapped: we write with `Events-Read-Encryption-Key`
//! and read with `Events-Write-Encryption-Key`. Every request gets a bare 200
//! carrying only `Server` and `CSeq`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::Result;
use crate::crypto::HapFraming;
use crate::pairing::SessionKeys;
use crate::rtsp::parse_message;

const EVENT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

pub struct EventChannel {
    received: Arc<AtomicU64>,
    closed_by_receiver: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl EventChannel {
    pub async fn connect(addr: SocketAddr, keys: &SessionKeys) -> Result<Self> {
        let mut stream = timeout(EVENT_CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| crate::Error::Timeout("event TCP connect"))??;
        stream.set_nodelay(true)?;
        let (events_write, events_read) = keys.event_keys();
        // Swapped on purpose: the receiver is the client of this channel.
        let mut framing = HapFraming::new(&events_read, &events_write);
        let received = Arc::new(AtomicU64::new(0));
        let counter = received.clone();
        let closed_by_receiver = Arc::new(AtomicBool::new(false));
        let closed = closed_by_receiver.clone();
        tracing::debug!(%addr, "event channel connected");

        let task = tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            let mut inbox = Vec::new();
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) => {
                        // A clean close means the receiver ended our session,
                        // typically because another device started playing to it.
                        tracing::debug!("event channel closed by receiver");
                        closed.store(true, Ordering::Relaxed);
                        return;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        tracing::debug!(error = %e, "event channel read failed");
                        return;
                    }
                };
                match framing.open(&buf[..n]) {
                    Ok(plain) => inbox.extend(plain),
                    Err(e) => {
                        tracing::warn!(error = %e, "event channel decrypt failed");
                        return;
                    }
                }
                loop {
                    let msg = match parse_message(&inbox) {
                        Ok(Some((msg, used))) => {
                            inbox.drain(..used);
                            msg
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "unparseable event message");
                            return;
                        }
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(request = %msg.start_line, kind = %event_kind(&msg.body), "event");

                    let reply = reply(msg.header("CSeq"), msg.header("Server"));
                    if let Err(e) = stream.write_all(&framing.seal(reply.as_bytes())).await {
                        tracing::debug!(error = %e, "event channel write failed");
                        return;
                    }
                }
            }
        });
        Ok(Self {
            received,
            closed_by_receiver,
            task,
        })
    }

    /// Number of requests the receiver has pushed on this channel.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// True once the receiver has cleanly closed the channel (session ended on its side).
    pub fn closed_by_receiver(&self) -> bool {
        self.closed_by_receiver.load(Ordering::Relaxed)
    }
}

impl Drop for EventChannel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A bare 200. pyatv also adds `Content-Length: 0` and `Audio-Latency: 0`;
/// two independent senders (airplay2-sender-cpp, DeetsAirplay) found those
/// headers caused "connected but silent" or a corrupted realtime timeline on
/// some receivers, and `Audio-Latency` could be read as a latency hint.
fn reply(cseq: Option<&str>, server: Option<&str>) -> String {
    let mut out = String::from("RTSP/1.0 200 OK\r\n");
    if let Some(server) = server {
        out.push_str(&format!("Server: {server}\r\n"));
    }
    if let Some(cseq) = cseq {
        out.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    out.push_str("\r\n");
    out
}

/// The `type` field of a `/command` plist body, for logging.
fn event_kind(body: &[u8]) -> String {
    plist::Value::from_reader(std::io::Cursor::new(body))
        .ok()
        .and_then(|v| {
            v.as_dictionary()?
                .get("type")?
                .as_string()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_echoes_cseq() {
        assert_eq!(
            reply(Some("2"), Some("AirTunes/980.77.2")),
            "RTSP/1.0 200 OK\r\nServer: AirTunes/980.77.2\r\nCSeq: 2\r\n\r\n"
        );
    }
}
