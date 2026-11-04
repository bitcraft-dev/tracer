//! CORS as a browser does it, for a workspace whose browser behaviour
//! names a page's origin: which requests are cross-origin, which of them a
//! browser preflights with an `OPTIONS` first, and whether an answer lets
//! the page read it (the Fetch standard, in the parts that decide that).

use tracer_protocol::{CheckResult, HttpRequest, HttpResponse};

/// `scheme://host[:port]` of `url`, lowercase, without a default port.
pub fn origin_of(url: &str) -> Option<String> {
    let uri: ureq::http::Uri = url.trim().parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let host = uri.host()?.to_ascii_lowercase();
    let port = match (scheme.as_str(), uri.port_u16()) {
        ("http", Some(80)) | ("https", Some(443)) | (_, None) => String::new(),
        (_, Some(p)) => format!(":{p}"),
    };
    Some(format!("{scheme}://{host}{port}"))
}

/// Whether a page at `origin` sending to `url` is a cross-origin request.
pub fn cross_origin(origin: &str, url: &str) -> bool {
    origin_of(url).is_none_or(|o| o != origin)
}

/// Headers a page's script can't set, which a browser sends itself; they
/// neither make a request need a preflight nor are listed in one.
const FORBIDDEN: [&str; 21] = [
    "accept-charset",
    "accept-encoding",
    "access-control-request-headers",
    "access-control-request-method",
    "connection",
    "content-length",
    "cookie",
    "cookie2",
    "date",
    "dnt",
    "expect",
    "host",
    "keep-alive",
    "origin",
    "referer",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "via",
];

fn forbidden(name: &str) -> bool {
    FORBIDDEN.contains(&name) || name.starts_with("proxy-") || name.starts_with("sec-")
}

/// A header a page may send without a preflight (Fetch's CORS-safelisted
/// request headers, their values' finer limits aside).
fn safelisted(name: &str, value: &str) -> bool {
    match name {
        "accept" | "accept-language" | "content-language" | "range" => true,
        "content-type" => {
            let essence = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            matches!(essence.as_str(), "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain")
        }
        _ => false,
    }
}

/// What a browser's preflight for `request` asks: its method, and its
/// headers that aren't safelisted (lowercase, sorted). `None` when it
/// needs none.
pub fn needs_preflight(request: &HttpRequest) -> Option<(String, Vec<String>)> {
    let method = request.method.to_ascii_uppercase();
    let mut headers: Vec<String> = request
        .headers
        .iter()
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.as_str()))
        .filter(|(n, v)| !forbidden(n) && !safelisted(n, v))
        .map(|(n, _)| n)
        .collect();
    headers.sort();
    headers.dedup();
    let simple_method = matches!(method.as_str(), "GET" | "HEAD" | "POST");
    (!simple_method || !headers.is_empty()).then_some((method, headers))
}

/// The `OPTIONS` a browser at `origin` sends before `request`.
pub fn preflight_of(request: &HttpRequest, origin: &str, method: &str, headers: &[String]) -> HttpRequest {
    let mut sent = vec![("Origin".to_string(), origin.to_string()), ("Access-Control-Request-Method".to_string(), method.to_string())];
    if !headers.is_empty() {
        sent.push(("Access-Control-Request-Headers".into(), headers.join(",")));
    }
    HttpRequest { method: "OPTIONS".into(), url: request.url.clone(), headers: sent, ..Default::default() }
}

fn header<'a>(response: &'a HttpResponse, name: &str) -> Option<&'a str> {
    response.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim())
}

fn list(value: Option<&str>) -> Vec<String> {
    value.unwrap_or("").split(',').map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty()).collect()
}

/// Whether `response` lets a page at `origin` read it: its
/// `Access-Control-Allow-Origin`, and with `credentials` (cookies were
/// sent), `Access-Control-Allow-Credentials`. Why not, if it doesn't.
fn allows_origin(response: &HttpResponse, origin: &str, credentials: bool) -> Result<(), String> {
    match header(response, "access-control-allow-origin") {
        None => return Err("no Access-Control-Allow-Origin".into()),
        Some("*") if credentials => return Err("Access-Control-Allow-Origin is *, which a request with cookies can't use".into()),
        Some("*") => {}
        Some(o) if o.eq_ignore_ascii_case(origin) => {}
        Some(o) => return Err(format!("Access-Control-Allow-Origin is {o}, not {origin}")),
    }
    if credentials && header(response, "access-control-allow-credentials") != Some("true") {
        return Err("cookies were sent, but Access-Control-Allow-Credentials isn't true".into());
    }
    Ok(())
}

/// Whether a preflight's answer lets the request go: a 2xx, the origin
/// allowed, and the method and each header in its allow lists (`*` counts
/// only without cookies). Why not, if it doesn't.
pub fn preflight_allows(response: &HttpResponse, origin: &str, method: &str, headers: &[String], credentials: bool) -> Result<(), String> {
    if !(200..300).contains(&response.status) {
        return Err(format!("the preflight was answered {}", response.status));
    }
    allows_origin(response, origin, credentials)?;
    let methods = list(header(response, "access-control-allow-methods"));
    let any = |l: &[String]| !credentials && l.iter().any(|v| v == "*");
    let simple = matches!(method, "GET" | "HEAD" | "POST");
    if !simple && !any(&methods) && !methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
        return Err(format!("{method} isn't in Access-Control-Allow-Methods"));
    }
    let allowed = list(header(response, "access-control-allow-headers"));
    let missing: Vec<&String> = headers.iter().filter(|h| !any(&allowed) && !allowed.contains(h)).collect();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|h| h.as_str()).collect();
        return Err(format!("Access-Control-Allow-Headers doesn't allow {}", names.join(", ")));
    }
    Ok(())
}

/// A check on a cross-origin answer: whether a page at `origin` could
/// read it.
pub fn answer_check(response: &HttpResponse, origin: &str, credentials: bool) -> CheckResult {
    let (pass, actual) = match allows_origin(response, origin, credentials) {
        Ok(()) => (true, "the page may read it".to_string()),
        Err(why) => (false, format!("a browser would keep it from the page: {why}")),
    };
    CheckResult { expect: format!("CORS from {origin}"), pass, actual }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::Timing;

    fn request(method: &str, headers: &[(&str, &str)]) -> HttpRequest {
        let headers = headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        HttpRequest { method: method.into(), url: "https://api.test/x".into(), headers, ..Default::default() }
    }

    fn answer(status: u16, headers: &[(&str, &str)]) -> HttpResponse {
        let headers = headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        let timing = Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None };
        HttpResponse { status, headers, body: String::new(), size: 0, timing }
    }

    #[test]
    fn origins_compare_without_default_ports_or_case() {
        assert_eq!(origin_of("HTTPS://App.Test:443/page").as_deref(), Some("https://app.test"));
        assert_eq!(origin_of("http://localhost:5173/").as_deref(), Some("http://localhost:5173"));
        assert!(!cross_origin("https://app.test", "https://app.test/api"));
        assert!(cross_origin("https://app.test", "https://api.test/"));
        assert!(cross_origin("https://app.test", "http://app.test/"));
    }

    #[test]
    fn a_simple_request_goes_without_a_preflight_and_others_with_one() {
        assert_eq!(needs_preflight(&request("GET", &[("Accept", "application/json")])), None);
        assert_eq!(needs_preflight(&request("POST", &[("Content-Type", "text/plain; charset=utf-8")])), None);
        // A header the browser sets itself changes nothing.
        assert_eq!(needs_preflight(&request("GET", &[("Cookie", "a=1"), ("User-Agent", "x")])).map(|p| p.1), Some(vec!["user-agent".to_string()]));
        assert_eq!(needs_preflight(&request("POST", &[("Content-Type", "application/json")])), Some(("POST".into(), vec!["content-type".into()])));
        assert_eq!(needs_preflight(&request("delete", &[])), Some(("DELETE".into(), vec![])));
        let (method, headers) = needs_preflight(&request("PUT", &[("X-Token", "1"), ("Authorization", "b")])).unwrap();
        let pre = preflight_of(&request("PUT", &[]), "https://app.test", &method, &headers);
        assert_eq!(pre.method, "OPTIONS");
        assert!(pre.headers.contains(&("Access-Control-Request-Headers".into(), "authorization,x-token".into())));
    }

    #[test]
    fn a_preflight_answer_must_allow_the_origin_the_method_and_each_header() {
        let ok = answer(204, &[
            ("Access-Control-Allow-Origin", "https://app.test"),
            ("Access-Control-Allow-Methods", "GET, PUT"),
            ("Access-Control-Allow-Headers", "Authorization, X-Token"),
        ]);
        let hs = vec!["authorization".to_string(), "x-token".to_string()];
        assert!(preflight_allows(&ok, "https://app.test", "PUT", &hs, false).is_ok());
        assert!(preflight_allows(&ok, "https://app.test", "DELETE", &[], false).unwrap_err().contains("DELETE"));
        assert!(preflight_allows(&ok, "https://evil.test", "PUT", &[], false).unwrap_err().contains("not https://evil.test"));
        assert!(preflight_allows(&ok, "https://app.test", "PUT", &["x-other".into()], false).unwrap_err().contains("x-other"));
        assert!(preflight_allows(&answer(404, &[]), "https://app.test", "PUT", &[], false).unwrap_err().contains("404"));
        // A wildcard serves only a request without cookies.
        let star = answer(200, &[("Access-Control-Allow-Origin", "*"), ("Access-Control-Allow-Methods", "*"), ("Access-Control-Allow-Headers", "*")]);
        assert!(preflight_allows(&star, "https://app.test", "PUT", &hs, false).is_ok());
        assert!(preflight_allows(&star, "https://app.test", "PUT", &hs, true).is_err());
    }

    #[test]
    fn an_answer_without_the_origin_allowed_fails_its_cors_check() {
        let none = answer(200, &[]);
        assert!(!answer_check(&none, "https://app.test", false).pass);
        let mine = answer(200, &[("Access-Control-Allow-Origin", "https://app.test")]);
        assert!(answer_check(&mine, "https://app.test", false).pass);
        assert!(!answer_check(&mine, "https://app.test", true).pass);
        let with = answer(200, &[("Access-Control-Allow-Origin", "https://app.test"), ("Access-Control-Allow-Credentials", "true")]);
        assert!(answer_check(&with, "https://app.test", true).pass);
    }
}
