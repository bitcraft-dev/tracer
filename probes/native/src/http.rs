//! Probe 3: a real HTTP request, awaited from a page.
//!
//! The request runs off the UI thread as an `async` export; an error status
//! is an answer, not a failure, and only a transport failure is thrown.

use std::fmt;
use std::time::Instant;

use ureq::Agent;

#[rux::export]
pub struct HttpResult {
    pub status: u32,
    pub ms: f64,
    pub bytes: u32,
    pub headers: Vec<String>,
    /// The body's first 4 KiB, as text.
    pub preview: String,
}

#[derive(Debug)]
pub struct TransportError(String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `method` is GET, POST, PUT, PATCH, DELETE or HEAD; `body` is sent for the
/// methods that take one.
#[rux::export]
pub async fn send(method: String, url: String, body: String) -> Result<HttpResult, TransportError> {
    let agent: Agent = Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let started = Instant::now();
    let sent = match method.as_str() {
        "GET" => agent.get(&url).call(),
        "HEAD" => agent.head(&url).call(),
        "DELETE" => agent.delete(&url).call(),
        "POST" => agent.post(&url).send(body.as_bytes()),
        "PUT" => agent.put(&url).send(body.as_bytes()),
        "PATCH" => agent.patch(&url).send(body.as_bytes()),
        other => return Err(TransportError(format!("unknown method {other}"))),
    };
    let mut res = sent.map_err(|e| TransportError(e.to_string()))?;
    let status = res.status().as_u16() as u32;
    let headers = res
        .headers()
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("<binary>")))
        .collect();
    let text = res
        .body_mut()
        .read_to_string()
        .map_err(|e| TransportError(e.to_string()))?;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut cut = text.len().min(4096);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok(HttpResult {
        status,
        ms,
        bytes: text.len() as u32,
        headers,
        preview: text[..cut].to_string(),
    })
}
