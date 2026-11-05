//! A mock server from a collection: the saved requests under a folder are
//! its routes, and each answers with its own last real answer in history.
//!
//! ```text
//! requests/shop/get item.http    GET {{base}}/items/{{id}}
//!   → the mock answers GET /items/42 with what GET {{base}}/items/{{id}}
//!     last answered (the last good answer, else the last of any status)
//! ```
//!
//! A route's path is its URL's after the scheme and host, or after a
//! leading `{{variable}}` (`{{base}}/items`); a segment holding a
//! `{{variable}}` matches any segment, and a route with more literal
//! segments wins. The query isn't matched. A route with no answer yet
//! gets 501 saying so; no route, 404 listing them. Every answer allows
//! any origin, and an `OPTIONS` preflight is answered for any route, so a
//! page in a browser can use it. Each hit is published as `mock_hit`.
//! Routes and answers are looked up on each hit, so a request saved or
//! sent meanwhile counts at once.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tracer_protocol::{Actor, EventKind, HttpRequest, HttpResponse, SavedRequest};

use crate::files;
use crate::hub::Hub;

/// Start a mock of the saved requests under `folder` on 127.0.0.1:`port`
/// (any free port if `None`). Gives its id, port and routes.
pub fn start(hub: &Arc<Hub>, actor: &Actor, folder: &str, port: Option<u16>) -> Result<(u64, u16, Vec<String>), String> {
    let folder = folder.trim().trim_end_matches('/');
    let folder = if folder.is_empty() { String::new() } else { files::normalize(folder)? };
    let routes = routes(hub, &folder);
    if routes.is_empty() {
        return Err(format!("there are no saved requests under `{folder}` to mock"));
    }
    let shown: Vec<String> = routes.iter().map(|r| format!("{} {}", r.method, r.path)).collect();
    listen(hub, actor, port, folder.clone(), shown, Source::Folder(folder))
}

/// Start a mock of OpenAPI spec `name` (see `openapi::mock_answer`).
pub fn start_spec(hub: &Arc<Hub>, actor: &Actor, name: &str, port: Option<u16>) -> Result<(u64, u16, Vec<String>), String> {
    let spec = crate::openapi::load(hub.root(), name)?;
    if spec.ops.is_empty() {
        return Err(format!("the spec `{}` has no operations to mock", spec.name));
    }
    let shown: Vec<String> = spec.ops.iter().map(|o| o.label()).collect();
    listen(hub, actor, port, format!("spec {}", spec.name), shown, Source::Spec(spec.name.clone()))
}

/// What a mock answers from.
#[derive(Clone)]
enum Source {
    /// The saved requests under a folder, with their last answers.
    Folder(String),
    /// An OpenAPI spec in the workspace, by name.
    Spec(String),
}

fn listen(hub: &Arc<Hub>, actor: &Actor, port: Option<u16>, shown_as: String, shown: Vec<String>, source: Source) -> Result<(u64, u16, Vec<String>), String> {
    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).map_err(|e| format!("could not listen on port {}: {e}", port.unwrap_or(0)))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let stop = Arc::new(AtomicBool::new(false));
    let mock_id = hub.start_mock(actor.clone(), shown_as, port, shown.clone(), stop.clone());
    let (hub, actor) = (hub.clone(), actor.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let (hub, actor, source) = (hub.clone(), actor.clone(), source.clone());
            thread::spawn(move || serve(&hub, &actor, mock_id, &source, stream));
        }
    });
    Ok((mock_id, port, shown))
}

/// A route: a saved request's method, the path it answers, and its file.
#[derive(Clone, Debug)]
struct Route {
    method: String,
    /// As shown: `/items/{id}.json`.
    path: String,
    segments: Vec<Segment>,
    saved: SavedRequest,
}

/// One path segment: literal text and `{{variables}}`, each variable
/// matching one or more characters (`{{id}}.json` matches `7.json`).
type Segment = Vec<Piece>;

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Lit(String),
    Var(String),
}

fn routes(hub: &Hub, folder: &str) -> Vec<Route> {
    let (requests, _, _) = hub.list_requests();
    let prefix = format!("{folder}/");
    requests
        .into_iter()
        .filter(|r| folder.is_empty() || r.path == folder || r.path.starts_with(&prefix))
        .filter(|r| !matches!(r.request.method.to_ascii_uppercase().as_str(), "WEBSOCKET" | "GRAPHQL"))
        .map(|saved| {
            let segments = template(&saved.request.url);
            Route { method: saved.request.method.to_ascii_uppercase(), path: shown(&segments), segments, saved }
        })
        .collect()
}

/// A URL's path segments: after the scheme and host, or after a leading
/// `{{variable}}` standing for them (`{{base}}/items`); no query.
fn template(url: &str) -> Vec<Segment> {
    let url = url.trim();
    let rest = if let Some(at) = url.find("://") {
        let after = &url[at + 3..];
        after.find('/').map_or("", |i| &after[i..])
    } else if url.starts_with("{{") {
        url.find("}}").map_or("", |i| &url[i + 2..])
    } else {
        url
    };
    let path = rest.split(['?', '#']).next().unwrap_or("");
    path.split('/').filter(|s| !s.is_empty()).map(pieces).collect()
}

fn pieces(segment: &str) -> Segment {
    let mut out = Vec::new();
    let mut rest = segment;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start..].find("}}") else { break };
        if start > 0 {
            out.push(Piece::Lit(rest[..start].to_string()));
        }
        out.push(Piece::Var(rest[start + 2..start + len].trim().to_string()));
        rest = &rest[start + len + 2..];
    }
    if !rest.is_empty() {
        out.push(Piece::Lit(rest.to_string()));
    }
    out
}

/// `/items/{id}.json`.
fn shown(segments: &[Segment]) -> String {
    let parts: Vec<String> = segments
        .iter()
        .map(|s| {
            s.iter()
                .map(|p| match p {
                    Piece::Lit(t) => t.clone(),
                    Piece::Var(v) => format!("{{{v}}}"),
                })
                .collect()
        })
        .collect();
    format!("/{}", parts.join("/"))
}

/// Whether `text` is `segment`, each variable standing for one or more
/// characters.
fn matches(segment: &[Piece], text: &str) -> bool {
    match segment.split_first() {
        None => text.is_empty(),
        Some((Piece::Lit(lit), rest)) => text.strip_prefix(lit.as_str()).is_some_and(|t| matches(rest, t)),
        Some((Piece::Var(_), rest)) => (1..=text.len()).filter(|&i| text.is_char_boundary(i)).any(|i| matches(rest, &text[i..])),
    }
}

/// How much of a route is literal: the more, the more particular it is.
fn literal(route: &Route) -> usize {
    route.segments.iter().flatten().map(|p| if let Piece::Lit(t) = p { t.len() } else { 0 }).sum()
}

/// The route that answers `method` `path`: the most literal one, then the
/// first by file path.
fn find<'a>(routes: &'a [Route], method: &str, path: &str) -> Option<&'a Route> {
    let wanted: Vec<&str> = path.split(['?', '#']).next().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
    routes
        .iter()
        .filter(|r| r.method == method && r.segments.len() == wanted.len())
        .filter(|r| r.segments.iter().zip(&wanted).all(|(s, w)| matches(s, w)))
        .max_by_key(|r| (literal(r), std::cmp::Reverse(r.saved.path.clone())))
}

fn serve(hub: &Hub, actor: &Actor, mock_id: u64, source: &Source, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut words = line.split_whitespace();
    let (Some(method), Some(target)) = (words.next(), words.next()) else { return };
    let (method, target) = (method.to_ascii_uppercase(), target.to_string());
    let mut length = 0usize;
    let mut asked_headers = String::new();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "access-control-request-headers" => asked_headers = value.trim().to_string(),
                _ => {}
            }
        }
    }
    // Read whole so the client sees its answer; a spec mock checks it.
    let mut sent = Vec::new();
    let _ = reader.by_ref().take(length.min(16 * 1024 * 1024) as u64).read_to_end(&mut sent);
    let sent = String::from_utf8_lossy(&sent).into_owned();

    let folder = match source {
        Source::Folder(f) => f.as_str(),
        Source::Spec(_) => "",
    };
    let (status, headers, body, route) = if method == "OPTIONS" {
        let allowed = if asked_headers.is_empty() { "*".to_string() } else { asked_headers };
        let headers = vec![
            ("Access-Control-Allow-Methods".into(), "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS".into()),
            ("Access-Control-Allow-Headers".into(), allowed),
            ("Access-Control-Max-Age".into(), "600".into()),
        ];
        (204, headers, String::new(), None)
    } else if let Source::Spec(name) = source {
        match crate::openapi::load(hub.root(), name) {
            Ok(spec) => crate::openapi::mock_answer(&spec, &method, &target, &headers, &sent),
            Err(e) => (500, json_type(), serde_json::json!({ "error": e }).to_string(), None),
        }
    } else {
        let routes = routes(hub, folder);
        match find(&routes, &method, &target) {
            Some(route) => match hub.last_answer(&route.saved.request) {
                Some(answer) => (answer.status, kept_headers(&answer), answer.body, Some(route.saved.path.clone())),
                None => {
                    let body = format!(
                        "{{\"error\":\"no answer yet for requests/{}.http: send it once and the mock answers with that\"}}",
                        route.saved.path
                    );
                    (501, json_type(), body, Some(route.saved.path.clone()))
                }
            },
            None => {
                let list: Vec<String> = routes.iter().map(|r| format!("{} {}", r.method, r.path)).collect();
                let body = serde_json::json!({ "error": format!("no saved request answers {method} {target}"), "routes": list }).to_string();
                (404, json_type(), body, None)
            }
        }
    };
    let _ = write_answer(&mut writer, status, &headers, if method == "HEAD" { "" } else { &body });
    hub.publish(actor.clone(), EventKind::MockHit { mock_id, method, path: target, status, route });
}

fn json_type() -> Vec<(String, String)> {
    vec![("Content-Type".into(), "application/json".into())]
}

/// An answer's headers as the mock sends them: without those that
/// describe the original connection or encoding (the body is the decoded
/// text) or the original time.
fn kept_headers(answer: &HttpResponse) -> Vec<(String, String)> {
    const DROPPED: &[&str] = &["content-length", "transfer-encoding", "connection", "keep-alive", "content-encoding", "date", "server"];
    let mut kept: Vec<(String, String)> = answer.headers.iter().filter(|(n, _)| !DROPPED.contains(&n.to_ascii_lowercase().as_str())).cloned().collect();
    kept.retain(|(n, _)| !n.to_ascii_lowercase().starts_with("access-control-"));
    kept
}

fn write_answer(out: &mut TcpStream, status: u16, headers: &[(String, String)], body: &str) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Access-Control-Allow-Origin: *\r\nServer: tracer-mock\r\nConnection: close\r\n");
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.write_all(head.as_bytes())?;
    out.write_all(body.as_bytes())?;
    out.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Which saved request (as written) a past request was: the same method
/// and URL.
pub fn same(a: &HttpRequest, b: &HttpRequest) -> bool {
    a.method.eq_ignore_ascii_case(&b.method) && a.url.trim() == b.url.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, url: &str, path: &str) -> Route {
        let saved = SavedRequest { path: path.into(), request: HttpRequest { method: method.into(), url: url.into(), ..Default::default() } };
        Route { method: method.into(), path: String::new(), segments: template(url), saved }
    }

    #[test]
    fn templates() {
        let lit = |s: &str| Piece::Lit(s.into());
        let var = |s: &str| Piece::Var(s.into());
        assert_eq!(template("{{base}}/items/{{id}}?full=1"), vec![vec![lit("items")], vec![var("id")]]);
        assert_eq!(template("https://api.test:8443/v1/users"), vec![vec![lit("v1")], vec![lit("users")]]);
        assert_eq!(template("https://api.test"), Vec::<Segment>::new());
        assert_eq!(template("{{base}}/f/{{id}}.json"), vec![vec![lit("f")], vec![var("id"), lit(".json")]]);
        assert_eq!(shown(&template("{{base}}/items/{{ id }}/x-{{v}}.json")), "/items/{id}/x-{v}.json");
    }

    #[test]
    fn the_most_literal_route_wins() {
        let routes = vec![
            route("GET", "{{base}}/items/{{id}}", "a"),
            route("GET", "{{base}}/items/new", "b"),
            route("POST", "{{base}}/items", "c"),
            route("GET", "{{base}}/items/{{id}}.json", "d"),
        ];
        assert_eq!(find(&routes, "GET", "/items/42?x=1").unwrap().saved.path, "a");
        assert_eq!(find(&routes, "GET", "/items/new").unwrap().saved.path, "b");
        assert_eq!(find(&routes, "POST", "/items/").unwrap().saved.path, "c");
        assert_eq!(find(&routes, "GET", "/items/42.json").unwrap().saved.path, "d");
        assert!(find(&routes, "DELETE", "/items/1").is_none());
        assert!(find(&routes, "GET", "/items/1/more").is_none());
        // A variable stands for at least one character.
        assert!(!matches(&pieces("{{id}}.json"), ".json"));
        assert!(matches(&pieces("v{{n}}-{{m}}"), "v1-2"));
    }
}
