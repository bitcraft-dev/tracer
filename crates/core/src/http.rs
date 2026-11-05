//! Sending one request and timing it.
//!
//! The phases before the answer are timed by stand-ins in `ureq`'s own
//! pipeline: a resolver that wraps its default one, and marks between the
//! connectors it chains by default (TCP, then TLS). Each request gets a
//! fresh agent, so every request opens its own connection and every phase
//! is its own. With redirects followed, the phases are the first
//! connection's; TTFB and the total cover every hop.
//!
//! A cancelled request stops between reads of its body. The wait for the
//! status line can't be interrupted with `ureq`: that thread runs on until the
//! server answers, and its answer is dropped.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracer_protocol::{HttpRequest, HttpResponse, Timing};
use ureq::config::Config;
use ureq::tls::TlsConfig;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector, TcpConnector, Transport,
};
use ureq::Agent;

/// With `unchecked_tls`, the server's certificate isn't checked (a
/// self-signed one is accepted): see [`tracer_protocol::Browser::unchecked_tls`].
fn config(redirects: bool, unchecked_tls: bool) -> Config {
    let mut config = Agent::config_builder().http_status_as_error(false);
    if !redirects {
        config = config.max_redirects(0);
    }
    if unchecked_tls {
        config = config.tls_config(TlsConfig::builder().disable_verification(true).build());
    }
    config.build()
}

/// Whether `url`'s host is one `browser` trusts without checking its
/// certificate. Only `https:` and `wss:` have one to check.
pub fn tls_unchecked(browser: &tracer_protocol::Browser, url: &str) -> bool {
    if browser.unchecked_tls.is_empty() {
        return false;
    }
    let Ok(uri) = url.trim().parse::<Uri>() else { return false };
    let secure = uri.scheme_str().is_some_and(|s| s.eq_ignore_ascii_case("https") || s.eq_ignore_ascii_case("wss"));
    secure && uri.host().is_some_and(|host| browser.tls_unchecked_for(host))
}

/// An agent that keeps its connections, for a bench's worker: its requests
/// after the first reuse one, as a client under load would.
pub fn keep_alive(redirects: bool, unchecked_tls: bool) -> Agent {
    Agent::new_with_config(config(redirects, unchecked_tls))
}

/// Send `req` with `agent`, reading the whole body. Gives the response with
/// only TTFB and the total timed, the phases being the connection's, which
/// a kept connection doesn't repeat.
///
/// An answer that ends its connection (HTTP/1.0 without keep-alive, or
/// `Connection: close`) has `agent` replaced by a fresh one with
/// `fresh()`: the server has closed that connection, and the next request
/// sent on it would fail.
pub fn execute_with(agent: &mut Agent, fresh: impl Fn() -> Agent, req: &HttpRequest, cancelled: &AtomicBool) -> Result<HttpResponse, String> {
    let request = build(req)?;
    let started = Instant::now();
    let mut res = agent.run(request).map_err(|e| e.to_string())?;
    let ttfb = started.elapsed();
    let bytes = read_body(&mut res, cancelled)?;
    let timing = Marks::default().timing(ttfb.as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0);
    let mut answer = response(&res, bytes, timing);
    answer.tls_unchecked = agent.config().tls_config().disable_verification();
    let connection = res.headers().get("connection").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let ends = connection.contains("close") || (res.version() == ureq::http::Version::HTTP_10 && !connection.contains("keep-alive"));
    if ends {
        *agent = fresh();
    }
    Ok(answer)
}

fn build(req: &HttpRequest) -> Result<ureq::http::Request<Vec<u8>>, String> {
    let mut builder = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = req.body.clone().unwrap_or_default().into_bytes();
    builder.body(body).map_err(|e| e.to_string())
}

fn read_body(res: &mut ureq::http::Response<ureq::Body>, cancelled: &AtomicBool) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut reader = res.body_mut().as_reader();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        match reader.read(&mut chunk).map_err(|e| e.to_string())? {
            0 => return Ok(bytes),
            n => bytes.extend_from_slice(&chunk[..n]),
        }
    }
}

fn response(res: &ureq::http::Response<ureq::Body>, bytes: Vec<u8>, timing: Timing) -> HttpResponse {
    HttpResponse {
        status: res.status().as_u16(),
        headers: res
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect(),
        size: bytes.len() as u64,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        timing,
        tls_unchecked: false,
    }
}

/// What reads an answer of server-sent events once its head has come
/// (see [`crate::realtime::sse`]): the head, timed to TTFB; the body as it
/// arrives; when the request was sent.
pub type EventsReader<'a> = &'a dyn Fn(HttpResponse, Box<dyn Read + Send>, Instant) -> Result<HttpResponse, String>;

/// An error status is an answer, not a failure: only a request that got no
/// answer at all is an `Err`. Without `redirects`, a redirect is the answer.
/// With `unchecked_tls`, the server's certificate isn't checked, and the
/// answer says so; redirects then aren't followed here (the caller follows
/// them a hop at a time, judging each host's trust on its own).
/// An answer of server-sent events goes to `events`, if given, rather than
/// being read to an end it may never have.
pub fn execute(
    req: &HttpRequest,
    redirects: bool,
    unchecked_tls: bool,
    cancelled: &AtomicBool,
    events: Option<EventsReader>,
) -> Result<HttpResponse, String> {
    let marks = Arc::new(Mutex::new(Marks::default()));
    let connector = ()
        .chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(Mark { marks: marks.clone(), phase: Phase::Connected })
        .chain(RustlsConnector::default())
        .chain(Mark { marks: marks.clone(), phase: Phase::Secured });
    let resolver = TimedResolver { inner: DefaultResolver::default(), marks: marks.clone() };
    let agent = Agent::with_parts(config(redirects && !unchecked_tls, unchecked_tls), connector, resolver);
    let request = build(req)?;

    let started = Instant::now();
    let mut res = agent.run(request).map_err(|e| e.to_string())?;
    let ttfb = started.elapsed();
    if let Some(read_events) = events {
        let ttfb_ms = ttfb.as_secs_f64() * 1000.0;
        let mut head = response(&res, Vec::new(), marks.lock().unwrap().timing(ttfb_ms, ttfb_ms));
        head.tls_unchecked = unchecked_tls;
        if crate::realtime::is_event_stream(&head.headers) {
            return read_events(head, Box::new(res.into_body().into_reader()), started);
        }
    }
    let bytes = read_body(&mut res, cancelled)?;
    let total = started.elapsed();
    let timing = marks.lock().unwrap().timing(ttfb.as_secs_f64() * 1000.0, total.as_secs_f64() * 1000.0);
    let mut answer = response(&res, bytes, timing);
    answer.tls_unchecked = unchecked_tls;
    Ok(answer)
}

/// When each phase of the first connection ended.
#[derive(Debug, Default)]
struct Marks {
    resolving: Option<Instant>,
    resolved: Option<Instant>,
    connected: Option<Instant>,
    secured: Option<Instant>,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Resolving,
    Resolved,
    Connected,
    Secured,
}

impl Marks {
    /// The first time only: a redirect's connection doesn't move them.
    fn mark(&mut self, phase: Phase) {
        let at = match phase {
            Phase::Resolving => &mut self.resolving,
            Phase::Resolved => &mut self.resolved,
            Phase::Connected => &mut self.connected,
            Phase::Secured => &mut self.secured,
        };
        at.get_or_insert_with(Instant::now);
    }

    fn timing(&self, ttfb_ms: f64, total_ms: f64) -> Timing {
        let between = |a: Option<Instant>, b: Option<Instant>| Some(b?.checked_duration_since(a?)?.as_secs_f64() * 1000.0);
        Timing {
            ttfb_ms,
            total_ms,
            dns_ms: between(self.resolving, self.resolved),
            connect_ms: between(self.resolved, self.connected),
            tls_ms: between(self.connected, self.secured),
        }
    }
}

/// `ureq`'s own resolver, timed.
#[derive(Debug)]
struct TimedResolver {
    inner: DefaultResolver,
    marks: Arc<Mutex<Marks>>,
}

impl Resolver for TimedResolver {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        self.marks.lock().unwrap().mark(Phase::Resolving);
        let addrs = self.inner.resolve(uri, config, timeout)?;
        self.marks.lock().unwrap().mark(Phase::Resolved);
        Ok(addrs)
    }
}

/// A connector that passes on what the one before it made, noting when.
#[derive(Debug)]
struct Mark {
    marks: Arc<Mutex<Marks>>,
    phase: Phase,
}

impl<In: Transport> Connector<In> for Mark {
    type Out = In;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<In>, ureq::Error> {
        // Past the TLS connector, a plain `http:` connection had no handshake.
        let tls = details.uri.scheme().is_some_and(|s| s.as_str().eq_ignore_ascii_case("https"));
        if chained.is_some() && (tls || !matches!(self.phase, Phase::Secured)) {
            self.marks.lock().unwrap().mark(self.phase);
        }
        Ok(chained)
    }
}
