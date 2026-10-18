//! Control connection to a receiver (TCP port 7000).
//!
//! The same TCP connection carries HTTP/1.1 (pairing) and RTSP/1.0 (session
//! control). After pairing it switches to HAP framing ([`crate::crypto::HapFraming`]).

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::crypto::HapFraming;
use crate::{Error, Result};

/// OS 27 receivers reject requests whose User-Agent doesn't look like Apple
/// software (TuneBlade's `TuneBlade/1.0` got 403). Version as sent by pyatv.
pub const USER_AGENT: &str = "AirPlay/550.10";

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// A TCP handshake on the local network should complete quickly. Keeping this
/// shorter than the RTSP response timeout makes a stale mDNS address fail fast
/// so the engine can reconnect without waiting ten seconds per attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// AirPlay control and event messages are small plist/parameter messages. A
/// bound keeps a peer that never completes a message from growing the inbox
/// without limit.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http11,
    Rtsp10,
}

impl Protocol {
    fn as_str(self) -> &'static str {
        match self {
            Protocol::Http11 => "HTTP/1.1",
            Protocol::Rtsp10 => "RTSP/1.0",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub protocol: Protocol,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// How long to wait for the response (default 10 s).
    pub timeout: Duration,
}

impl Request {
    pub fn new(method: &str, uri: &str, protocol: Protocol) -> Self {
        Self {
            method: method.into(),
            uri: uri.into(),
            protocol,
            headers: Vec::new(),
            body: Vec::new(),
            timeout: RESPONSE_TIMEOUT,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn header(mut self, name: &str, value: impl ToString) -> Self {
        self.headers.push((name.into(), value.to_string()));
        self
    }

    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".into(), content_type.into()));
        self.body = body;
        self
    }

    fn describe(&self) -> String {
        format!("{} {}", self.method, self.uri)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn plist(&self) -> Result<plist::Value> {
        Ok(plist::Value::from_reader(std::io::Cursor::new(&self.body))?)
    }
}

pub struct Connection {
    stream: TcpStream,
    framing: Option<HapFraming>,
    /// Decrypted (or plain) bytes received but not yet parsed into a response.
    inbox: Vec<u8>,
    cseq: u32,
    session_headers: Vec<(String, String)>,
}

impl Connection {
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Timeout("TCP connect"))??;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            framing: None,
            inbox: Vec::new(),
            cseq: 0,
            session_headers: Vec::new(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream.local_addr()?)
    }

    pub fn peer_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream.peer_addr()?)
    }

    /// Switches both directions to HAP framing. Call right after pairing.
    pub fn enable_encryption(&mut self, write_key: &[u8; 32], read_key: &[u8; 32]) {
        self.framing = Some(HapFraming::new(write_key, read_key));
    }

    pub fn is_encrypted(&self) -> bool {
        self.framing.is_some()
    }

    /// Headers added to every RTSP request (e.g. `DACP-ID`, `Active-Remote`).
    pub fn set_session_headers(&mut self, headers: Vec<(String, String)>) {
        self.session_headers = headers;
    }

    /// Sends a request and waits for its response. RTSP requests get `CSeq`,
    /// the session headers and `User-Agent` added automatically.
    pub async fn request(&mut self, mut req: Request) -> Result<Response> {
        if req.protocol == Protocol::Rtsp10 {
            req.headers
                .insert(0, ("CSeq".into(), self.cseq.to_string()));
            self.cseq += 1;
            for (i, header) in self.session_headers.iter().enumerate() {
                req.headers.insert(1 + i, header.clone());
            }
            if !req
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("User-Agent"))
            {
                req.headers.push(("User-Agent".into(), USER_AGENT.into()));
            }
        }
        let wire = serialize(&req);
        tracing::debug!(request = %req.describe(), encrypted = self.is_encrypted(), "send");
        tracing::trace!(bytes = %String::from_utf8_lossy(&wire), "send raw");

        let out = match &mut self.framing {
            Some(framing) => framing.seal(&wire),
            None => wire,
        };
        self.stream.write_all(&out).await?;

        let resp = timeout(req.timeout, self.read_response())
            .await
            .map_err(|_| Error::Timeout("response"))??;
        tracing::debug!(request = %req.describe(), status = resp.status, len = resp.body.len(), "recv");
        Ok(resp)
    }

    /// Like [`request`](Self::request) but fails on any non-2xx status.
    pub async fn request_ok(&mut self, req: Request) -> Result<Response> {
        let described = req.describe();
        let resp = self.request(req).await?;
        if !(200..300).contains(&resp.status) {
            return Err(Error::Status {
                request: described,
                status: resp.status,
            });
        }
        Ok(resp)
    }

    async fn read_response(&mut self) -> Result<Response> {
        let mut buf = [0u8; 8192];
        loop {
            if let Some((resp, used)) = parse_response(&self.inbox)? {
                self.inbox.drain(..used);
                return Ok(resp);
            }
            let n = self.stream.read(&mut buf).await?;
            if n == 0 {
                return Err(Error::Protocol("connection closed by receiver".into()));
            }
            match &mut self.framing {
                Some(framing) => self.inbox.extend(framing.open(&buf[..n])?),
                None => self.inbox.extend_from_slice(&buf[..n]),
            }
        }
    }
}

fn serialize(req: &Request) -> Vec<u8> {
    let mut head = format!("{} {} {}\r\n", req.method, req.uri, req.protocol.as_str());
    for (k, v) in &req.headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    if !req.body.is_empty() {
        let _ = write!(head, "Content-Length: {}\r\n", req.body.len());
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&req.body);
    out
}

/// A parsed HTTP/RTSP message (request or response) before interpretation.
pub(crate) struct Message {
    pub start_line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parses one complete message from the start of `data`, returning it and the
/// number of bytes used; `None` if more bytes are needed.
pub(crate) fn parse_message(data: &[u8]) -> Result<Option<(Message, usize)>> {
    let Some(head_end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
        if data.len() > MAX_HEADER_BYTES {
            return Err(Error::Protocol("message headers too large".into()));
        }
        return Ok(None);
    };
    if head_end > MAX_HEADER_BYTES {
        return Err(Error::Protocol("message headers too large".into()));
    }
    let head = std::str::from_utf8(&data[..head_end])
        .map_err(|_| Error::Protocol("message head is not UTF-8".into()))?;
    let mut lines = head.split("\r\n");
    let start_line = lines.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let msg = Message {
        start_line,
        headers,
        body: Vec::new(),
    };
    let len = msg
        .header("Content-Length")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| Error::Protocol("bad Content-Length".into()))?
        .unwrap_or(0);
    if len > MAX_BODY_BYTES {
        return Err(Error::Protocol("message body too large".into()));
    }

    let body_start = head_end + 4;
    let body_end = body_start
        .checked_add(len)
        .ok_or_else(|| Error::Protocol("message body length overflow".into()))?;
    if data.len() < body_end {
        return Ok(None);
    }
    let body = data[body_start..body_end].to_vec();
    Ok(Some((Message { body, ..msg }, body_end)))
}

fn parse_response(data: &[u8]) -> Result<Option<(Response, usize)>> {
    let Some((msg, used)) = parse_message(data)? else {
        return Ok(None);
    };
    let mut parts = msg.start_line.splitn(3, ' ');
    let (Some(_proto), Some(code)) = (parts.next(), parts.next()) else {
        return Err(Error::Protocol(format!(
            "bad status line: {:?}",
            msg.start_line
        )));
    };
    let status = code
        .parse()
        .map_err(|_| Error::Protocol(format!("bad status line: {:?}", msg.start_line)))?;
    let reason = parts.next().unwrap_or_default().to_string();
    Ok(Some((
        Response {
            status,
            reason,
            headers: msg.headers,
            body: msg.body,
        },
        used,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_like_pyatv_pairing_request() {
        let req = Request::new("POST", "/pair-setup", Protocol::Http11)
            .header("User-Agent", "AirPlay/320.20")
            .body("application/octet-stream", vec![1, 2, 3]);
        assert_eq!(
            serialize(&req),
            b"POST /pair-setup HTTP/1.1\r\nUser-Agent: AirPlay/320.20\r\nContent-Type: application/octet-stream\r\nContent-Length: 3\r\n\r\n\x01\x02\x03"
        );
    }

    #[test]
    fn parses_response_only_when_body_complete() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nServer: AirTunes/980.77.2\r\n\r\nab";
        assert!(parse_response(raw).unwrap().is_none());

        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nServer: AirTunes/980.77.2\r\n\r\nabcRTSP/1.0";
        let (resp, used) = parse_response(raw).unwrap().unwrap();
        assert_eq!(
            (resp.status, resp.reason.as_str(), resp.body.as_slice()),
            (200, "OK", &b"abc"[..])
        );
        assert_eq!(resp.header("server"), Some("AirTunes/980.77.2"));
        assert_eq!(used, raw.len() - "RTSP/1.0".len());
    }

    #[test]
    fn parses_bodyless_403() {
        let (resp, _) = parse_response(b"RTSP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(resp.status, 403);
    }

    #[test]
    fn rejects_headers_larger_than_the_protocol_limit() {
        let mut raw = b"RTSP/1.0 200 OK\r\nX-Long: ".to_vec();
        raw.extend(std::iter::repeat_n(b'x', 70 * 1024));
        raw.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(
            parse_response(&raw),
            Err(Error::Protocol(message)) if message.contains("headers too large")
        ));
    }

    #[test]
    fn rejects_bodies_larger_than_the_protocol_limit() {
        let raw = format!(
            "RTSP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n",
            2 * 1024 * 1024
        );
        assert!(matches!(
            parse_response(raw.as_bytes()),
            Err(Error::Protocol(message)) if message.contains("body too large")
        ));
    }
}
