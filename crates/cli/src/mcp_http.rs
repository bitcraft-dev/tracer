//! `tracer mcp --http <PORT>`: the same MCP server as on stdio, over MCP's
//! Streamable HTTP transport, for clients that connect to a URL.
//!
//! It listens on 127.0.0.1 only. `POST /mcp` takes one JSON-RPC message or
//! a batch and answers with JSON (202 and no body when there's nothing to
//! answer); no server-sent event stream is offered, so `GET /mcp` is 405.
//! `initialize` starts a session, named by the `Mcp-Session-Id` header the
//! answer carries, which later requests send back; each session is its own
//! agent to the core, under the name its client gave. `DELETE /mcp` ends
//! one. A request whose `Origin` isn't this machine is refused, so a web
//! page can't drive Tracer through the browser.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::mcp::Server;

type Sessions = Arc<Mutex<HashMap<String, Arc<Mutex<Server>>>>>;

pub fn serve(workspace: PathBuf, name_override: Option<String>, port: u16) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    eprintln!("tracer mcp: MCP over HTTP at http://127.0.0.1:{port}/mcp (Ctrl+C stops)");
    let sessions: Sessions = Arc::default();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (workspace, name, sessions) = (workspace.clone(), name_override.clone(), sessions.clone());
        thread::spawn(move || {
            if let Err(e) = connection(stream, &workspace, name, &sessions) {
                eprintln!("tracer mcp: {e}");
            }
        });
    }
    Ok(())
}

/// One HTTP request and its answer.
struct Asked {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Asked {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read(stream: &TcpStream) -> io::Result<Option<Asked>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut words = line.split_whitespace();
    let (method, path) = (words.next().unwrap_or_default().to_string(), words.next().unwrap_or_default().to_string());
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((n, v)) = line.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    let length: usize = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if length > 16 * 1024 * 1024 {
        return Err(io::Error::other("a body over 16 MiB"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(Asked { method, path, headers, body }))
}

fn answer(stream: &mut TcpStream, status: &str, headers: &[(&str, String)], body: &str) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (n, v) in headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\nConnection: keep-alive\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Whether `origin` (an `Origin` header) is this machine.
fn local_origin(origin: &str) -> bool {
    let host = origin.split("://").nth(1).unwrap_or(origin);
    let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.bytes().all(|b| b.is_ascii_digit()) { h } else { host });
    matches!(host.trim_matches(['[', ']']), "localhost" | "127.0.0.1" | "::1") || origin == "null"
}

fn new_session_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let a = h.finish();
    let mut h = RandomState::new().build_hasher();
    h.write_u64(a);
    format!("{a:016x}{:016x}", h.finish())
}

fn connection(mut stream: TcpStream, workspace: &PathBuf, name: Option<String>, sessions: &Sessions) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    while let Some(asked) = read(&stream)? {
        let json_type = ("Content-Type", "application/json".to_string());
        if asked.header("origin").is_some_and(|o| !local_origin(o)) {
            answer(&mut stream, "403 Forbidden", &[json_type], r#"{"error":"only pages on this machine may use this"}"#)?;
            continue;
        }
        let path = asked.path.split('?').next().unwrap_or_default();
        if path != "/mcp" {
            answer(&mut stream, "404 Not Found", &[json_type], r#"{"error":"MCP is at /mcp"}"#)?;
            continue;
        }
        let session = asked.header("mcp-session-id").map(String::from);
        match asked.method.as_str() {
            "POST" => {}
            "DELETE" => {
                let gone = session.and_then(|s| sessions.lock().unwrap().remove(&s));
                let status = if gone.is_some() { "204 No Content" } else { "404 Not Found" };
                answer(&mut stream, status, &[], "")?;
                continue;
            }
            _ => {
                answer(&mut stream, "405 Method Not Allowed", &[("Allow", "POST, DELETE".into())], "")?;
                continue;
            }
        }
        let message: Value = match serde_json::from_slice(&asked.body) {
            Ok(m) => m,
            Err(e) => {
                let body = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("not JSON: {e}") } });
                answer(&mut stream, "400 Bad Request", &[json_type], &body.to_string())?;
                continue;
            }
        };
        let messages = match &message {
            Value::Array(all) => all.clone(),
            one => vec![one.clone()],
        };
        let initializing = messages.iter().any(|m| m.get("method").and_then(Value::as_str) == Some("initialize"));
        // A session: a new one for `initialize`, else the one named.
        let (id, server) = if initializing {
            let id = new_session_id();
            let server = Arc::new(Mutex::new(Server::new(workspace.clone(), name.clone())));
            sessions.lock().unwrap().insert(id.clone(), server.clone());
            (id, server)
        } else {
            let found = session.as_ref().and_then(|s| sessions.lock().unwrap().get(s).cloned());
            match (session, found) {
                (Some(id), Some(server)) => (id, server),
                (Some(_), None) => {
                    answer(&mut stream, "404 Not Found", &[json_type], r#"{"error":"no such session: initialize again"}"#)?;
                    continue;
                }
                (None, _) => {
                    answer(&mut stream, "400 Bad Request", &[json_type], r#"{"error":"send Mcp-Session-Id, from initialize's answer"}"#)?;
                    continue;
                }
            }
        };
        let answers: Vec<Value> = {
            let mut server = server.lock().unwrap();
            messages.into_iter().filter_map(|m| server.handle(m)).collect()
        };
        let session_header = ("Mcp-Session-Id", id);
        if answers.is_empty() {
            answer(&mut stream, "202 Accepted", &[session_header], "")?;
        } else {
            let body = if message.is_array() { Value::Array(answers) } else { answers.into_iter().next().unwrap_or(Value::Null) };
            answer(&mut stream, "200 OK", &[json_type, session_header], &body.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert!(local_origin("http://localhost:5173"));
        assert!(local_origin("http://127.0.0.1"));
        assert!(local_origin("http://[::1]:80"));
        assert!(!local_origin("https://evil.test"));
        assert!(!local_origin("http://localhost.evil.test:80"));
        assert_ne!(new_session_id(), new_session_id());
    }
}
