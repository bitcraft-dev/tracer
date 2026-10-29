//! Sending one request and timing it.
//!
//! Phase 1 times what `ureq` can tell apart: the status line and headers
//! (TTFB) and the whole body. DNS, connect and TLS need a lower-level client
//! and come with the timing waterfall.

use std::time::Instant;

use tracer_protocol::{HttpRequest, HttpResponse, Timing};
use ureq::Agent;

/// An error status is an answer, not a failure: only a request that got no
/// answer at all is an `Err`.
pub fn execute(req: &HttpRequest) -> Result<HttpResponse, String> {
    let agent: Agent = Agent::config_builder().http_status_as_error(false).build().into();
    let mut builder = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = req.body.clone().unwrap_or_default().into_bytes();
    let request = builder.body(body).map_err(|e| e.to_string())?;

    let started = Instant::now();
    let mut res = agent.run(request).map_err(|e| e.to_string())?;
    let ttfb = started.elapsed();
    let bytes = res.body_mut().read_to_vec().map_err(|e| e.to_string())?;
    let total = started.elapsed();

    Ok(HttpResponse {
        status: res.status().as_u16(),
        headers: res
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect(),
        size: bytes.len() as u64,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        timing: Timing { ttfb_ms: ttfb.as_secs_f64() * 1000.0, total_ms: total.as_secs_f64() * 1000.0 },
    })
}
