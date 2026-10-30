//! The local API: HTTP and WebSocket on the core's own port, for anything
//! that isn't the CLI, the window or an MCP agent.
//!
//! A connection's first bytes tell the two apart: the core's protocol opens
//! with `{`, HTTP with a method.
//!
//! - `GET /v1/ws` upgrades to a WebSocket that carries the core's protocol
//!   as it is: one JSON message per text frame, `hello` (with the token)
//!   first. A browser can't set headers on a WebSocket, so the token travels
//!   in `hello`, as it does over TCP.
//! - `POST /v1/command` runs one command, its JSON (`{"cmd": "send", …}`) as
//!   the body, with `Authorization: Bearer <token>`. It connects as
//!   `X-Tracer-Kind` (`agent` unless said otherwise, so the guardrails hold)
//!   named `X-Tracer-Name` (`http`). The answer is the reply's body, or
//!   `{"error": …}` with a 4xx status.
//!
//! Only `Host`s of this machine are answered, and only `Origin`s of this
//! machine, so a web page can't reach the core by renaming itself
//! (DNS rebinding) or from another site; the token is still needed on top.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tracer_protocol::{ClientKind, Outcome, ServerMessage, PROTOCOL_VERSION};

use crate::conn;
use crate::hub::Hub;
use crate::Stop;

/// The most a request's head or body, or a WebSocket message, may be.
const HEAD_MAX: usize = 64 * 1024;
const BODY_MAX: usize = 16 * 1024 * 1024;

pub fn serve(stream: TcpStream, hub: Arc<Hub>, token: &str, stop: &Stop) {
    let mut first = [0u8; 1];
    match stream.peek(&mut first) {
        Ok(1) if first[0].is_ascii_alphabetic() => {}
        Ok(_) => return conn::serve_tcp(stream, hub, token, stop),
        Err(_) => return,
    }
    let Ok(write_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut out = write_half;
    let head = match read_head(&mut reader) {
        Ok(head) => head,
        Err(why) => return respond(&mut out, 400, &json!({ "error": why })),
    };
    if let Err(why) = from_this_machine(&head) {
        return respond(&mut out, 403, &json!({ "error": why }));
    }
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/v1/ws") => websocket(reader, out, &head, hub, token, stop),
        ("POST", "/v1/command") => match read_body(&mut reader, &head) {
            Ok(body) => {
                let (status, answer) = command(&head, &body, hub, token, stop);
                respond(&mut out, status, &answer)
            }
            Err(why) => respond(&mut out, 400, &json!({ "error": why })),
        },
        (_, "/v1/ws" | "/v1/command") => respond(&mut out, 405, &json!({ "error": "GET /v1/ws or POST /v1/command" })),
        _ => respond(&mut out, 404, &json!({ "error": "Tracer answers GET /v1/ws and POST /v1/command" })),
    }
}

struct Head {
    method: String,
    path: String,
    /// Names lowercased.
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn read_head(reader: &mut impl BufRead) -> Result<Head, String> {
    let mut lines = Vec::new();
    let mut size = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        size += n;
        if n == 0 || size > HEAD_MAX {
            return Err("an HTTP request's head ends with an empty line, within 64 KiB".into());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let mut parts = lines.first().map(|l| l.split(' ')).ok_or("no request line")?;
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err("a request line is `METHOD /path HTTP/1.1`".into());
    };
    let path = target.split('?').next().unwrap_or(target).to_string();
    let headers = lines[1..]
        .iter()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok(Head { method: method.to_string(), path, headers })
}

fn read_body(reader: &mut impl Read, head: &Head) -> Result<Vec<u8>, String> {
    let length: usize = match head.header("content-length") {
        Some(n) => n.parse().map_err(|_| format!("Content-Length `{n}` is not a number"))?,
        None => return Err("a command needs a Content-Length".into()),
    };
    if length > BODY_MAX {
        return Err("a command is at most 16 MiB".into());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

/// `localhost`, `127.x.x.x` or `::1`, with or without a port.
fn is_this_machine(host: &str) -> bool {
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, port)| if port.parse::<u16>().is_ok() { h } else { host }),
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn from_this_machine(head: &Head) -> Result<(), String> {
    match head.header("host") {
        Some(host) if is_this_machine(host) => {}
        _ => return Err("Tracer answers only requests addressed to this machine (Host: 127.0.0.1 or localhost)".into()),
    }
    if let Some(origin) = head.header("origin") {
        let host = origin.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or(""));
        if !host.is_some_and(is_this_machine) {
            return Err(format!("Tracer answers pages on this machine only, not {origin}"));
        }
    }
    Ok(())
}

fn respond(out: &mut TcpStream, status: u16, body: &Value) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let body = body.to_string();
    let _ = write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = out.flush();
}

/// Run one command as a client that says hello, sends it and leaves.
fn command(head: &Head, body: &[u8], hub: Arc<Hub>, token: &str, stop: &Stop) -> (u16, Value) {
    let Some(given) = head.header("authorization").and_then(|a| a.strip_prefix("Bearer ")) else {
        return (401, json!({ "error": "send the token from .tracer/core.json as `Authorization: Bearer <token>`" }));
    };
    let kind = match head.header("x-tracer-kind").unwrap_or("agent") {
        "agent" => ClientKind::Agent,
        "cli" => ClientKind::Cli,
        "window" => ClientKind::Window,
        other => return (400, json!({ "error": format!("X-Tracer-Kind is agent, cli or window, not `{other}`") })),
    };
    let name = head.header("x-tracer-name").unwrap_or("http").to_string();
    let mut command: Value = match serde_json::from_slice(body) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return (400, json!({ "error": "the body is one command as JSON, such as {\"cmd\": \"status\"}" })),
    };
    match command.get("cmd").and_then(Value::as_str) {
        Some("hello") => return (400, json!({ "error": "no hello: the headers say who you are" })),
        Some("subscribe") => return (400, json!({ "error": "events come over the WebSocket at /v1/ws" })),
        _ => {}
    }
    command["id"] = json!(2);
    let hello = json!({ "id": 1, "cmd": "hello", "protocol": PROTOCOL_VERSION, "token": given, "kind": kind, "name": name });

    let written = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = written.clone();
    let lines = [Ok(hello.to_string()), Ok(command.to_string())];
    conn::serve(lines, move |line: &str| Ok(sink.lock().unwrap().push(line.to_string())), hub, token, stop);

    let mut answer = (400, json!({ "error": "the core gave no answer" }));
    for line in written.lock().unwrap().iter() {
        let Ok(ServerMessage::Reply(reply)) = serde_json::from_str(line) else { continue };
        answer = match (reply.id, reply.outcome) {
            (1, Outcome::Error(e)) => return (401, json!({ "error": e })),
            (2, Outcome::Ok(body)) => (200, serde_json::to_value(body).unwrap_or_default()),
            (2, Outcome::Error(e)) => (400, json!({ "error": e })),
            _ => continue,
        };
    }
    answer
}

/// The core's protocol over a WebSocket (RFC 6455), a message per frame.
fn websocket(reader: BufReader<TcpStream>, mut out: TcpStream, head: &Head, hub: Arc<Hub>, token: &str, stop: &Stop) {
    let upgrade = head.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    let Some(key) = head.header("sec-websocket-key").filter(|_| upgrade) else {
        return respond(&mut out, 400, &json!({ "error": "/v1/ws is a WebSocket: send Upgrade: websocket" }));
    };
    let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if out.write_all(handshake.as_bytes()).is_err() {
        return;
    }
    // Shared by the writer and the reader's pongs and close, a frame at a time.
    let out = Arc::new(Mutex::new(out));
    let write = out.clone();
    let write_line = move |line: &str| write_frame(&mut write.lock().unwrap(), TEXT, line.as_bytes());
    conn::serve(Frames { reader, out }, write_line, hub, token, stop);
}

const TEXT: u8 = 0x1;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

fn write_frame(out: &mut TcpStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    out.write_all(&frame)?;
    out.flush()
}

/// A client's messages, each whole: frames joined, pings answered. Ends at
/// a close, which it answers, or when the connection goes.
struct Frames {
    reader: BufReader<TcpStream>,
    out: Arc<Mutex<TcpStream>>,
}

impl Frames {
    fn frame(&mut self) -> io::Result<(bool, u8, Vec<u8>)> {
        let mut two = [0u8; 2];
        self.reader.read_exact(&mut two)?;
        let (fin, opcode, masked) = (two[0] & 0x80 != 0, two[0] & 0x0F, two[1] & 0x80 != 0);
        let len = match two[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                self.reader.read_exact(&mut b)?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                self.reader.read_exact(&mut b)?;
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        if !masked || len > BODY_MAX {
            return Err(io::Error::other("a client's frames are masked, and at most 16 MiB"));
        }
        let mut mask = [0u8; 4];
        self.reader.read_exact(&mut mask)?;
        let mut payload = vec![0; len];
        self.reader.read_exact(&mut payload)?;
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        Ok((fin, opcode, payload))
    }
}

impl Iterator for Frames {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<io::Result<String>> {
        let mut message = Vec::new();
        loop {
            let (fin, opcode, payload) = match self.frame() {
                Ok(frame) => frame,
                Err(_) => return None,
            };
            match opcode {
                CLOSE => {
                    let _ = write_frame(&mut self.out.lock().unwrap(), CLOSE, &payload[..payload.len().min(2)]);
                    return None;
                }
                PING => {
                    let _ = write_frame(&mut self.out.lock().unwrap(), PONG, &payload);
                    continue;
                }
                PONG => continue,
                _ => message.extend_from_slice(&payload),
            }
            if message.len() > BODY_MAX {
                return None;
            }
            if fin {
                return Some(String::from_utf8(message).map_err(io::Error::other));
            }
        }
    }
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_answers_as_rfc_6455_says() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
    }

    #[test]
    fn only_this_machine_is_answered() {
        for host in ["127.0.0.1:5000", "localhost:5000", "LOCALHOST", "[::1]:5000", "127.1.2.3"] {
            assert!(is_this_machine(host), "{host}");
        }
        for host in ["evil.com", "evil.com:5000", "localhost.evil.com", "10.0.0.1:80", "[2001:db8::1]:80"] {
            assert!(!is_this_machine(host), "{host}");
        }
    }
}
