//! Realtime: WebSockets and server-sent events, as requests that stay open.
//!
//! A stream is a request like any other: numbered, in history, held to the
//! sender's policy and asked about if it must be, filled in from its
//! environment, its secrets taken out of whatever comes back. What differs
//! is that its connection stays open after the answer:
//!
//! - A `WEBSOCKET ws://…` request does the handshake itself (`ureq` can't
//!   upgrade a connection), with the same DNS/connect/TLS timing as any
//!   request. Its body's messages go once it's open; more go with
//!   `StreamSend`.
//! - Any request answered with `text/event-stream` is read as server-sent
//!   events, one message an event.
//!
//! Both publish `stream_opened`, a `stream_message` per message either way,
//! `stream_closed` saying why, and then `request_finished` with the
//! handshake's (or the answer's) head, the whole time open and the bytes
//! received. Closing a stream is cancelling it: it ends as a finished
//! request whose `stream_closed` names who closed it.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tracer_protocol::{Actor, Direction, HttpRequest, HttpResponse, StreamKind, StreamMessage, Timing, STREAM_TEXT_MAX, WEBSOCKET};
use ureq::http::Uri;

use crate::hub::Hub;
use crate::ws::{self, BINARY, CLOSE, CONTINUATION, PING, PONG, TEXT};
use crate::envs;

/// How often an open stream looks for a close, or for messages to send.
const POLL: Duration = Duration::from_millis(50);
/// How long a WebSocket may be quiet before it's pinged: servers and the
/// networks in between drop idle connections (echo.websocket.org's after
/// about 35 s), and a ping, answered or not, keeps it in use.
const KEEPALIVE: Duration = Duration::from_secs(25);
/// The longest a connection or a handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The most a handshake's answer, or a message, may be.
const HEAD_MAX: usize = 64 * 1024;
const MESSAGE_MAX: usize = 16 * 1024 * 1024;

pub fn is_websocket(request: &HttpRequest) -> bool {
    request.method.eq_ignore_ascii_case(WEBSOCKET)
}

/// Whether an answer with these headers is a stream of server-sent events.
pub fn is_event_stream(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type") && value.trim().to_ascii_lowercase().starts_with("text/event-stream")
    })
}

/// The messages a `WEBSOCKET` request's body sends once it's open, in
/// order: separated by lines of `===`, each saying whether to wait for the
/// server to send something first (`=== wait-for-server`).
pub fn opening(body: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut wait = false;
    let mut lines: Vec<&str> = Vec::new();
    let mut flush = |lines: &mut Vec<&str>, wait: bool| {
        let text = lines.join("\n").trim().to_string();
        if !text.is_empty() {
            out.push((wait, text));
        }
        lines.clear();
    };
    for line in body.lines() {
        match line.trim() {
            "===" => {
                flush(&mut lines, wait);
                wait = false;
            }
            "=== wait-for-server" => {
                flush(&mut lines, wait);
                wait = true;
            }
            _ => lines.push(line),
        }
    }
    flush(&mut lines, wait);
    out
}

/// One started request's stream, as its worker sees it.
pub struct Link<'a> {
    pub hub: &'a Hub,
    pub actor: &'a Actor,
    pub request_id: u64,
    pub cancelled: &'a AtomicBool,
    pub secrets: &'a [(String, String)],
}

impl Link<'_> {
    fn publish(&self, by: Actor, dir: Direction, bytes: &[u8], binary: bool, event: Option<String>, id: Option<String>) {
        let text = if binary { describe_binary(bytes) } else { cut(String::from_utf8_lossy(bytes).into_owned()) };
        let message = StreamMessage {
            n: 0,
            dir,
            text: envs::redact(&text, self.secrets),
            binary,
            size: bytes.len() as u64,
            event,
            id,
        };
        self.hub.stream_message(by, self.request_id, message);
    }

    /// Why it was closed, once it has been: by whom, if a person or agent.
    fn closed(&self) -> String {
        match self.hub.closed_by(self.request_id) {
            Some(who) => format!("closed by {}", who.name),
            None => "cancelled".into(),
        }
    }

    /// Publish `stream_closed`, and give the head with the whole time open
    /// and the bytes received, for `request_finished`.
    fn end(&self, mut head: HttpResponse, started: Instant, reason: String, counts: Counts) -> HttpResponse {
        self.hub.stream_closed(self.actor.clone(), self.request_id, envs::redact(&reason, self.secrets), counts.received, counts.sent);
        head.timing.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        head.size = counts.bytes;
        head
    }
}

#[derive(Default, Clone, Copy)]
struct Counts {
    received: u64,
    sent: u64,
    bytes: u64,
}

/// Up to [`STREAM_TEXT_MAX`] of `text`, cut at a character.
fn cut(mut text: String) -> String {
    if text.len() > STREAM_TEXT_MAX {
        let mut at = STREAM_TEXT_MAX;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
    }
    text
}

/// `12 bytes: 89 50 4e 47 …`
fn describe_binary(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().take(32).map(|b| format!("{b:02x}")).collect();
    let more = if bytes.len() > 32 { " …" } else { "" };
    format!("{} bytes: {}{more}", bytes.len(), hex.join(" "))
}

/// A WebSocket's connection, TLS or not.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => &s.sock,
        }
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)?;
        self.flush()
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// The system's roots, as `ureq` trusts them for any other request.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring has TLS 1.2 and 1.3")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Open `request`'s WebSocket and keep it until it's closed. An answer to
/// the handshake other than `101` is an ordinary finished request.
pub fn websocket(link: &Link, request: &HttpRequest) -> Result<HttpResponse, String> {
    let uri: Uri = request.url.parse().map_err(|e| format!("`{}` is not a URL: {e}", request.url))?;
    let tls = match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("wss" | "https") => true,
        Some("ws" | "http") => false,
        _ => return Err(format!("a WebSocket's URL starts with ws:// or wss://, and `{}` doesn't", request.url)),
    };
    let host = uri.host().ok_or_else(|| format!("`{}` names no host", request.url))?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let path = uri.path_and_query().map_or("/", |p| p.as_str());

    let started = Instant::now();
    let addrs: Vec<SocketAddr> = (host.as_str(), port).to_socket_addrs().map_err(|e| format!("{host}: {e}"))?.collect();
    let resolved = started.elapsed();
    let tcp = addrs
        .iter()
        .find_map(|a| TcpStream::connect_timeout(a, CONNECT_TIMEOUT).ok())
        .ok_or_else(|| format!("could not connect to {host}:{port}"))?;
    let connected = started.elapsed();
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    let mut conn = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| format!("{host}: {e}"))?;
        let mut session = rustls::ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?;
        let mut tcp = tcp;
        while session.is_handshaking() {
            session.complete_io(&mut tcp).map_err(|e| format!("TLS with {host}: {e}"))?;
        }
        Conn::Tls(Box::new(rustls::StreamOwned::new(session, tcp)))
    } else {
        Conn::Plain(tcp)
    };
    let secured = tls.then(|| started.elapsed());

    let key = ws::new_key();
    let shown = if host.contains(':') { format!("[{host}]") } else { host.clone() };
    let host_header = if uri.port_u16().is_some() { format!("{shown}:{port}") } else { shown };
    let mut head = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    const OURS: [&str; 5] = ["host", "upgrade", "connection", "sec-websocket-key", "sec-websocket-version"];
    for (name, value) in &request.headers {
        if !OURS.contains(&name.to_ascii_lowercase().as_str()) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    conn.send(head.as_bytes()).map_err(|e| format!("sending the handshake: {e}"))?;

    let mut buf = Vec::new();
    let (status, headers) = read_head(&mut conn, &mut buf, link.cancelled)?;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let ttfb = ms(started.elapsed());
    let timing = Timing {
        ttfb_ms: ttfb,
        total_ms: ttfb,
        dns_ms: Some(ms(resolved)),
        connect_ms: Some(ms(connected - resolved)),
        tls_ms: secured.map(|s| ms(s - connected)),
    };
    let header = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    if status != 101 {
        let body = read_rest(&mut conn, buf, header("content-length").and_then(|l| l.trim().parse().ok()));
        let mut timing = timing;
        timing.total_ms = ms(started.elapsed());
        let size = body.len() as u64;
        return Ok(HttpResponse { status, headers, size, body: String::from_utf8_lossy(&body).into_owned(), timing });
    }
    if header("sec-websocket-accept").map(str::trim) != Some(ws::accept_key(&key).as_str()) {
        return Err("the server agreed to a WebSocket but answered the handshake's key wrongly".into());
    }
    let head = HttpResponse { status, headers, size: 0, body: String::new(), timing };
    let Some(outbox) = link.hub.open_stream(link.actor.clone(), link.request_id, StreamKind::Websocket, head.clone()) else {
        return Err("cancelled".into());
    };
    conn.tcp().set_read_timeout(Some(POLL)).map_err(|e| e.to_string())?;

    let mut queue: VecDeque<(bool, String)> = opening(request.body.as_deref().unwrap_or("")).into();
    // Whether the server has sent anything since we last did.
    let mut heard = false;
    let mut counts = Counts::default();
    // A message arriving in fragments: its opcode and what has come so far.
    let mut partial: Option<(u8, Vec<u8>)> = None;
    let mut chunk = vec![0u8; 16 * 1024];
    let mut last_heard_or_sent = Instant::now();
    let reason = 'open: loop {
        if link.cancelled.load(Ordering::SeqCst) {
            let _ = conn.send(&ws::frame(CLOSE, &1000u16.to_be_bytes(), Some(ws::new_mask())));
            break link.closed();
        }
        let mut sends: Vec<(Actor, String)> = Vec::new();
        while queue.front().is_some_and(|(wait, _)| !*wait || heard) {
            let (_, text) = queue.pop_front().unwrap();
            sends.push((link.actor.clone(), text));
            heard = false;
        }
        sends.extend(outbox.try_iter());
        for (by, text) in sends {
            if let Err(e) = conn.send(&ws::frame(TEXT, text.as_bytes(), Some(ws::new_mask()))) {
                break 'open format!("the connection failed: {e}");
            }
            counts.sent += 1;
            last_heard_or_sent = Instant::now();
            link.publish(by, Direction::Out, text.as_bytes(), false, None, None);
        }
        if last_heard_or_sent.elapsed() >= KEEPALIVE {
            if let Err(e) = conn.send(&ws::frame(PING, b"tracer", Some(ws::new_mask()))) {
                break 'open format!("the connection failed: {e}");
            }
            last_heard_or_sent = Instant::now();
        }

        match conn.read(&mut chunk) {
            Ok(0) => break "the server closed the connection".into(),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                last_heard_or_sent = Instant::now();
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break "the server closed the connection".into(),
            Err(e) => break format!("the connection failed: {e}"),
        }
        loop {
            let frame = match ws::take_frame(&mut buf, MESSAGE_MAX) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => break 'open e,
            };
            match frame.opcode {
                PING => {
                    let _ = conn.send(&ws::frame(PONG, &frame.payload, Some(ws::new_mask())));
                }
                PONG => {}
                CLOSE => {
                    let _ = conn.send(&ws::frame(CLOSE, &frame.payload[..frame.payload.len().min(2)], Some(ws::new_mask())));
                    break 'open closed_by_server(&frame.payload);
                }
                TEXT | BINARY => partial = Some((frame.opcode, frame.payload)),
                CONTINUATION => match partial.as_mut() {
                    Some((_, so_far)) if so_far.len() + frame.payload.len() <= MESSAGE_MAX => so_far.extend(frame.payload),
                    Some(_) => break 'open format!("a message over the {MESSAGE_MAX} bytes allowed"),
                    None => break 'open "the server continued a message it never began".into(),
                },
                other => break 'open format!("the server sent a frame of unknown kind {other}"),
            }
            if frame.fin {
                if let Some((opcode, message)) = partial.take() {
                    counts.received += 1;
                    counts.bytes += message.len() as u64;
                    heard = true;
                    link.publish(link.actor.clone(), Direction::In, &message, opcode == BINARY, None, None);
                }
            }
        }
    };
    Ok(link.end(head, started, reason, counts))
}

/// `closed by the server (1001 going away)`
fn closed_by_server(payload: &[u8]) -> String {
    if payload.len() < 2 {
        return "closed by the server".into();
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    let why = String::from_utf8_lossy(&payload[2..]);
    if why.trim().is_empty() {
        format!("closed by the server ({code})")
    } else {
        format!("closed by the server ({code} {})", why.trim())
    }
}

/// Read an HTTP answer's head off `conn`, leaving whatever came after it
/// in `buf`. Its status, and its headers in order.
fn read_head(conn: &mut Conn, buf: &mut Vec<u8>, cancelled: &AtomicBool) -> Result<(u16, Vec<(String, String)>), String> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > HEAD_MAX {
            return Err("the handshake's answer is too long".into());
        }
        if cancelled.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        if Instant::now() > deadline {
            return Err(format!("no answer to the handshake within {} s", CONNECT_TIMEOUT.as_secs()));
        }
        match conn.read(&mut chunk) {
            Ok(0) => return Err("the server closed the connection during the handshake".into()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(format!("reading the handshake's answer: {e}")),
        }
    };
    let head: Vec<u8> = buf.drain(..end + 4).collect();
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("`{status_line}` is not an HTTP status line"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok((status, headers))
}

/// The rest of a refused handshake's answer: `length` bytes, or until the
/// server closes or goes quiet.
fn read_rest(conn: &mut Conn, mut body: Vec<u8>, length: Option<usize>) -> Vec<u8> {
    let _ = conn.tcp().set_read_timeout(Some(Duration::from_secs(5)));
    let mut chunk = [0u8; 16 * 1024];
    while length.is_none_or(|l| body.len() < l) && body.len() < MESSAGE_MAX {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    if let Some(l) = length {
        body.truncate(l);
    }
    body
}

/// Read an answer of server-sent events from `reader` until the server
/// ends it, the connection goes, or the stream is closed. `head` is the
/// answer's status, headers and timing so far.
pub fn sse(link: &Link, head: HttpResponse, reader: Box<dyn Read + Send>, started: Instant) -> Result<HttpResponse, String> {
    if link.hub.open_stream(link.actor.clone(), link.request_id, StreamKind::Sse, head.clone()).is_none() {
        return Err("cancelled".into());
    }
    // Reads block until the server sends something, so they happen on a
    // thread of their own and a close needn't wait for the next event. That
    // thread ends at its next read once nobody listens.
    let (tx, rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(64);
    thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let read = reader.read(&mut chunk).map(|n| chunk[..n].to_vec());
            let last = !matches!(&read, Ok(bytes) if !bytes.is_empty());
            if tx.send(read).is_err() || last {
                return;
            }
        }
    });
    let mut parser = SseParser::default();
    let mut counts = Counts::default();
    let reason = loop {
        if link.cancelled.load(Ordering::SeqCst) {
            break link.closed();
        }
        match rx.recv_timeout(POLL) {
            Ok(Ok(bytes)) if bytes.is_empty() => break "the server ended the stream".to_string(),
            Ok(Ok(bytes)) => {
                counts.bytes += bytes.len() as u64;
                for event in parser.feed(&bytes) {
                    counts.received += 1;
                    link.publish(link.actor.clone(), Direction::In, event.data.as_bytes(), false, event.event, event.id);
                }
            }
            Ok(Err(e)) => break format!("the connection failed: {e}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break "the connection went".to_string(),
        }
    };
    Ok(link.end(head, started, reason, counts))
}

/// One server-sent event.
#[derive(Debug, PartialEq, Eq)]
struct Sse {
    event: Option<String>,
    id: Option<String>,
    data: String,
}

/// Server-sent events as the HTML spec reads them: lines ending in CR, LF
/// or CRLF; `data:` lines joined with newlines; a blank line ends an event,
/// and one with no data is no event; `:` lines are comments; one space
/// after the colon is dropped.
#[derive(Default)]
struct SseParser {
    line: Vec<u8>,
    /// The last byte was a CR, so an LF now ends nothing.
    after_cr: bool,
    data: Option<String>,
    event: Option<String>,
    id: Option<String>,
}

impl SseParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Sse> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.after_cr && b == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = b == b'\r';
            if b == b'\r' || b == b'\n' {
                let line = std::mem::take(&mut self.line);
                out.extend(self.line_ended(&String::from_utf8_lossy(&line)));
            } else {
                self.line.push(b);
            }
        }
        out
    }

    fn line_ended(&mut self, line: &str) -> Option<Sse> {
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if line.is_empty() {
            let (event, id) = (self.event.take(), self.id.take());
            let mut data = self.data.take()?;
            if data.ends_with('\n') {
                data.pop();
            }
            return Some(Sse { event, id, data });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
        match field {
            "data" => {
                let data = self.data.get_or_insert_with(String::new);
                data.push_str(value);
                data.push('\n');
            }
            "event" => self.event = Some(value.to_string()),
            "id" => self.id = Some(value.to_string()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_websockets_body_is_the_messages_it_sends_on_open() {
        let body = "===\n{\"hi\": 1}\n=== wait-for-server\nsecond\nline\n===\n\n===\nthird";
        assert_eq!(
            opening(body),
            vec![(false, "{\"hi\": 1}".into()), (true, "second\nline".into()), (false, "third".into())]
        );
        assert_eq!(opening("just one"), vec![(false, "just one".into())]);
        assert!(opening("").is_empty());
    }

    #[test]
    fn server_sent_events_are_read_as_the_spec_says_in_any_chunks() {
        let stream = ": hello\r\nevent: tick\r\nid: 7\r\ndata: one\r\ndata:two\r\n\r\ndata: x\n\nevent: empty\n\nretry: 5\ndata\n\r";
        let want = vec![
            Sse { event: Some("tick".into()), id: Some("7".into()), data: "one\ntwo".into() },
            Sse { event: None, id: None, data: "x".into() },
            Sse { event: None, id: None, data: "".into() },
        ];
        for size in [1, 2, 3, 7, stream.len()] {
            let mut parser = SseParser::default();
            let got: Vec<Sse> = stream.as_bytes().chunks(size).flat_map(|c| parser.feed(c)).collect();
            assert_eq!(got, want, "chunks of {size}");
        }
    }

    #[test]
    fn only_an_event_stream_is_streamed() {
        let h = |v: &str| vec![("Content-Type".to_string(), v.to_string())];
        assert!(is_event_stream(&h("text/event-stream")));
        assert!(is_event_stream(&h("Text/Event-Stream; charset=utf-8")));
        assert!(!is_event_stream(&h("application/json")));
        assert!(!is_event_stream(&[]));
    }

    #[test]
    fn a_binary_message_is_told_by_its_size_and_first_bytes() {
        assert_eq!(describe_binary(&[0x89, 0x50]), "2 bytes: 89 50");
        assert!(describe_binary(&[0; 40]).ends_with(" …"));
        assert_eq!(cut("é".repeat(STREAM_TEXT_MAX)).len(), STREAM_TEXT_MAX);
    }
}
