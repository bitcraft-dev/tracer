//! Finding the servers running on this machine: the TCP ports the system
//! says are listening (or, where it can't say, the ports dev servers
//! usually take), each tried for HTTP and then HTTPS, and looked over for
//! an OpenAPI document at the usual paths.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::thread;
use std::time::Duration;

use tracer_protocol::LocalServer;
use ureq::tls::TlsConfig;
use ureq::Agent;

/// Ports dev servers usually take, tried even when the system's list of
/// listening ports can't be read.
const USUAL: &[u16] = &[
    80, 443, 1337, 3000, 3001, 3002, 3003, 4000, 4200, 4321, 5000, 5001, 5173, 5174, 5500, 6006, 7000, 7070, 8000, 8001, 8008,
    8080, 8081, 8082, 8088, 8443, 8787, 8888, 9000, 9090, 9200, 9229,
];

/// Where an API describes itself, tried in turn.
const OPENAPI_PATHS: &[&str] = &[
    "/openapi.json", "/swagger.json", "/v3/api-docs", "/api-docs", "/swagger/v1/swagger.json", "/docs/openapi.json", "/api/openapi.json",
    "/openapi.yaml",
];

const CONNECT: Duration = Duration::from_millis(300);
const ANSWER: Duration = Duration::from_millis(1500);

/// The servers answering on this machine, by port, skipping `own` (the
/// core's). `extra` ports are tried too.
pub fn local_servers(own: u16, extra: &[u16]) -> Vec<LocalServer> {
    let listening = listening();
    let mut ports: BTreeMap<u16, (Option<u32>, Option<String>)> = BTreeMap::new();
    let known = listening.is_some();
    for (port, pid) in listening.unwrap_or_default() {
        // The system's own services below 1024 (RPC, SMB) aren't dev servers.
        if port >= 1024 || port == 80 || port == 443 {
            ports.insert(port, (pid, None));
        }
    }
    if !known {
        ports.extend(USUAL.iter().map(|&p| (p, (None, None))));
    }
    ports.extend(extra.iter().map(|&p| (p, (None, None))));
    ports.remove(&own);
    let names = process_names();
    for (pid, name) in ports.values_mut() {
        *name = pid.and_then(|pid| names.get(&pid).cloned());
    }

    let handles: Vec<_> = ports
        .into_iter()
        .map(|(port, (pid, process))| thread::spawn(move || probe(port, pid, process)))
        .collect();
    let mut found: Vec<LocalServer> = handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect();
    found.sort_by_key(|s| s.port);
    found
}

/// What answers on `port`, if anything does over HTTP.
fn probe(port: u16, pid: Option<u32>, process: Option<String>) -> Option<LocalServer> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    let host = if TcpStream::connect_timeout(&addr, CONNECT).is_ok() {
        "127.0.0.1"
    } else if TcpStream::connect_timeout(&v6, CONNECT).is_ok() {
        "[::1]"
    } else {
        return None;
    };
    let agent = Agent::new_with_config(
        Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(ANSWER))
            // A local dev server's certificate is usually its own.
            .tls_config(TlsConfig::builder().disable_verification(true).build())
            .build(),
    );
    for scheme in ["http", "https"] {
        let base = format!("{scheme}://{host}:{port}");
        let Ok(mut res) = agent.get(&format!("{base}/")).call() else { continue };
        let status = res.status().as_u16();
        let header = |name: &str| res.headers().get(name).and_then(|v| v.to_str().ok()).map(String::from);
        let server = header("server").or_else(|| header("x-powered-by"));
        let content_type = header("content-type");
        let body = res.body_mut().with_config().limit(64 * 1024).read_to_string().unwrap_or_default();
        let title = title_of(&body);
        let openapi = OPENAPI_PATHS.iter().find_map(|path| {
            let mut res = agent.get(&format!("{base}{path}")).call().ok()?;
            if res.status().as_u16() != 200 {
                return None;
            }
            let text = res.body_mut().with_config().limit(256 * 1024).read_to_string().ok()?;
            let head: String = text.chars().take(2048).collect();
            (head.contains("\"openapi\"") || head.contains("\"swagger\"") || head.contains("openapi:") || head.contains("swagger:"))
                .then(|| path.to_string())
        });
        // The address that answered: `localhost` may try the other one first.
        let url = format!("{base}/");
        return Some(LocalServer { port, url, status, server, content_type, title, openapi, pid, process });
    }
    None
}

/// An HTML page's `<title>`.
fn title_of(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open = start + lower[start..].find('>')? + 1;
    let end = open + lower[open..].find("</title>")?;
    let title = body.get(open..end)?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(120).collect())
}

/// The TCP ports listening on this machine's loopback or every address,
/// with the process holding each where the system says; `None` if the
/// list can't be read.
fn listening() -> Option<Vec<(u16, Option<u32>)>> {
    if cfg!(windows) {
        let out = Command::new("netstat").args(["-ano", "-p", "tcp"]).output().ok()?;
        let mut v6 = Command::new("netstat").args(["-ano", "-p", "tcpv6"]).output().ok().map(|o| o.stdout).unwrap_or_default();
        let mut text = out.stdout;
        text.append(&mut v6);
        Some(parse_netstat(&String::from_utf8_lossy(&text)))
    } else {
        let mut found = Vec::new();
        let mut any = false;
        for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(text) = std::fs::read_to_string(file) {
                any = true;
                found.extend(parse_proc_net(&text).into_iter().map(|p| (p, None)));
            }
        }
        any.then_some(found)
    }
}

/// `netstat -ano`'s LISTENING lines on 127.0.0.1, 0.0.0.0, [::1] or [::].
fn parse_netstat(text: &str) -> Vec<(u16, Option<u32>)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 || !cols[0].eq_ignore_ascii_case("tcp") || cols[3] != "LISTENING" {
            continue;
        }
        let Some((addr, port)) = cols[1].rsplit_once(':') else { continue };
        if !matches!(addr, "127.0.0.1" | "0.0.0.0" | "[::1]" | "[::]") {
            continue;
        }
        if let Ok(port) = port.parse::<u16>() {
            if !out.iter().any(|(p, _)| *p == port) {
                out.push((port, cols[4].parse().ok()));
            }
        }
    }
    out
}

/// `/proc/net/tcp`'s listening sockets (state 0A) on loopback or any.
fn parse_proc_net(text: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 || cols[3] != "0A" {
            continue;
        }
        let Some((addr, port)) = cols[1].split_once(':') else { continue };
        let local = addr.chars().all(|c| c == '0') || addr == "0100007F" || addr == "00000000000000000000000001000000";
        if let (true, Ok(port)) = (local, u16::from_str_radix(port, 16)) {
            if !out.contains(&port) {
                out.push(port);
            }
        }
    }
    out
}

/// Process names by id, where the system lists them (Windows' tasklist).
fn process_names() -> BTreeMap<u32, String> {
    let mut names = BTreeMap::new();
    if !cfg!(windows) {
        return names;
    }
    let Ok(out) = Command::new("tasklist").args(["/FO", "CSV", "/NH"]).output() else { return names };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split("\",\"").map(|c| c.trim_matches('"')).collect();
        if let [name, pid, ..] = cols.as_slice() {
            if let Ok(pid) = pid.parse() {
                names.insert(pid, name.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn netstat_and_proc_net_are_read() {
        let netstat = "  Proto  Local Address          Foreign Address        State           PID\n  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1708\n  TCP    127.0.0.1:5173         0.0.0.0:0              LISTENING       42\n  TCP    192.168.1.5:139        0.0.0.0:0              LISTENING       4\n  TCP    127.0.0.1:5173         127.0.0.1:50000        ESTABLISHED     42\n  TCP    [::]:8080              [::]:0                 LISTENING       7\n";
        assert_eq!(parse_netstat(netstat), vec![(135, Some(1708)), (5173, Some(42)), (8080, Some(7))]);
        let proc = "  sl  local_address rem_address   st\n   0: 0100007F:0BB8 00000000:0000 0A 00000000\n   1: 0100007F:0BB8 0100007F:D431 01 00000000\n   2: 0A00000F:1F90 00000000:0000 0A 00000000\n";
        assert_eq!(parse_proc_net(proc), vec![3000]);
        assert_eq!(title_of("<html><head><TITLE>\n  My  App </title>").as_deref(), Some("My App"));
    }

    #[test]
    fn a_local_http_server_and_its_openapi_are_found() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let body = if head.starts_with("GET /openapi.json ") {
                    r#"{"openapi":"3.0.0","info":{"title":"t"}}"#.to_string()
                } else if head.starts_with("GET / ") {
                    "<title>Todo API</title>".to_string()
                } else {
                    String::new()
                };
                let status = if body.is_empty() { "404 Not Found" } else { "200 OK" };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nServer: test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let found = local_servers(0, &[port]);
        let server = found.iter().find(|s| s.port == port).expect("found");
        assert_eq!((server.status, server.server.as_deref(), server.title.as_deref()), (200, Some("test"), Some("Todo API")));
        assert_eq!(server.openapi.as_deref(), Some("/openapi.json"));
        assert_eq!(server.url, format!("http://127.0.0.1:{port}/"));
    }
}
