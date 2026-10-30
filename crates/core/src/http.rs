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
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector, TcpConnector, Transport,
};
use ureq::Agent;

/// An error status is an answer, not a failure: only a request that got no
/// answer at all is an `Err`. Without `redirects`, a redirect is the answer.
pub fn execute(req: &HttpRequest, redirects: bool, cancelled: &AtomicBool) -> Result<HttpResponse, String> {
    let mut config = Agent::config_builder().http_status_as_error(false);
    if !redirects {
        config = config.max_redirects(0);
    }
    let marks = Arc::new(Mutex::new(Marks::default()));
    let connector = ()
        .chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(Mark { marks: marks.clone(), phase: Phase::Connected })
        .chain(RustlsConnector::default())
        .chain(Mark { marks: marks.clone(), phase: Phase::Secured });
    let resolver = TimedResolver { inner: DefaultResolver::default(), marks: marks.clone() };
    let agent = Agent::with_parts(config.build(), connector, resolver);
    let mut builder = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = req.body.clone().unwrap_or_default().into_bytes();
    let request = builder.body(body).map_err(|e| e.to_string())?;

    let started = Instant::now();
    let mut res = agent.run(request).map_err(|e| e.to_string())?;
    let ttfb = started.elapsed();
    let mut bytes = Vec::new();
    let mut reader = res.body_mut().as_reader();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        match reader.read(&mut chunk).map_err(|e| e.to_string())? {
            0 => break,
            n => bytes.extend_from_slice(&chunk[..n]),
        }
    }
    let total = started.elapsed();
    let timing = marks.lock().unwrap().timing(ttfb.as_secs_f64() * 1000.0, total.as_secs_f64() * 1000.0);

    Ok(HttpResponse {
        status: res.status().as_u16(),
        headers: res
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect(),
        size: bytes.len() as u64,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        timing,
    })
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
