//! The workspace's cookie jar, when its browser behaviour keeps cookies:
//! `Set-Cookie` answers kept as a browser keeps them (RFC 6265), and sent
//! back with later requests that match their domain, path and `Secure`.
//!
//! The jar lives in `.tracer/cookies.json`, beside the journal and out of
//! git. Values never reach the journal: a request is journalled as
//! written, and the `Cookie` header is added only when it's sent.

use serde::{Deserialize, Serialize};
use tracer_protocol::CookieSeen;

/// A cookie as the jar keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Lowercase, without a leading dot.
    pub domain: String,
    /// Sent only to `domain` itself, not its subdomains: there was no
    /// `Domain` attribute.
    pub host_only: bool,
    pub path: String,
    /// When it expires, in milliseconds since the epoch; `None` for a
    /// session cookie, which lasts as long as the jar.
    pub expires_ms: Option<u64>,
    pub secure: bool,
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Jar {
    pub cookies: Vec<Cookie>,
}

/// A URL's parts the jar needs: https or not, its host, and its path.
struct Target {
    secure: bool,
    host: String,
    path: String,
}

fn target(url: &str) -> Option<Target> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    let secure = matches!(uri.scheme_str(), Some("https") | Some("wss"));
    let host = uri.host()?.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let path = match uri.path() {
        "" => "/".to_string(),
        p => p.to_string(),
    };
    Some(Target { secure, host, path })
}

impl Jar {
    /// Keep what `set_cookie`, a `Set-Cookie` header of an answer from
    /// `url`, says, at time `now_ms`. Ignores one a browser would.
    pub fn store(&mut self, url: &str, set_cookie: &str, now_ms: u64) {
        let Some(at) = target(url) else { return };
        let mut parts = set_cookie.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else { return };
        let (name, value) = (name.trim(), value.trim().trim_matches('"'));
        if name.is_empty() {
            return;
        }
        let mut cookie = Cookie {
            name: name.into(),
            value: value.into(),
            domain: at.host.clone(),
            host_only: true,
            path: default_path(&at.path),
            expires_ms: None,
            secure: false,
            http_only: false,
            same_site: None,
        };
        let mut max_age: Option<i64> = None;
        for attr in parts {
            let (key, val) = match attr.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
                None => (attr.trim().to_ascii_lowercase(), ""),
            };
            match key.as_str() {
                "domain" if !val.is_empty() => {
                    let domain = val.trim_start_matches('.').to_ascii_lowercase();
                    // A server may set its own domain or a parent of it, never another.
                    if !domain_matches(&at.host, &domain) {
                        return;
                    }
                    (cookie.domain, cookie.host_only) = (domain, false);
                }
                "path" if val.starts_with('/') => cookie.path = val.into(),
                "expires" => {
                    if let Some(ms) = http_date_ms(val) {
                        cookie.expires_ms = Some(ms);
                    }
                }
                "max-age" => max_age = val.parse().ok(),
                "secure" => cookie.secure = true,
                "httponly" => cookie.http_only = true,
                "samesite" if !val.is_empty() => cookie.same_site = Some(val.into()),
                _ => {}
            }
        }
        // Max-Age wins over Expires; 0 or less removes it.
        if let Some(secs) = max_age {
            cookie.expires_ms = Some(if secs <= 0 { 0 } else { now_ms + secs as u64 * 1000 });
        }
        // Only a secure page may set a Secure cookie.
        if cookie.secure && !at.secure {
            return;
        }
        self.cookies.retain(|c| !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path));
        if cookie.expires_ms.is_none_or(|e| e > now_ms) {
            self.cookies.push(cookie);
        }
    }

    /// The `Cookie` header a browser would send to `url` at `now_ms`, if
    /// any cookie matches: longer paths first. Drops expired ones.
    pub fn header_for(&mut self, url: &str, now_ms: u64) -> Option<String> {
        self.cookies.retain(|c| c.expires_ms.is_none_or(|e| e > now_ms));
        let at = target(url)?;
        let mut sent: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| if c.host_only { at.host == c.domain } else { domain_matches(&at.host, &c.domain) })
            .filter(|c| path_matches(&at.path, &c.path))
            .filter(|c| !c.secure || at.secure)
            .collect();
        sent.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        let pairs: Vec<String> = sent.iter().map(|c| format!("{}={}", c.name, c.value)).collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// What's in the jar at `now_ms`, with values only if `values`.
    pub fn seen(&self, now_ms: u64, values: bool) -> Vec<CookieSeen> {
        self.cookies
            .iter()
            .filter(|c| c.expires_ms.is_none_or(|e| e > now_ms))
            .map(|c| CookieSeen {
                name: c.name.clone(),
                value: values.then(|| c.value.clone()),
                domain: c.domain.clone(),
                path: c.path.clone(),
                expires_ms: c.expires_ms,
                secure: c.secure,
                http_only: c.http_only,
                same_site: c.same_site.clone(),
            })
            .collect()
    }

    /// Remove the cookies for `domain` and its subdomains, or all of them.
    /// Gives how many went.
    pub fn clear(&mut self, domain: Option<&str>) -> usize {
        let before = self.cookies.len();
        match domain {
            Some(d) => {
                let d = d.trim().trim_start_matches('.').to_ascii_lowercase();
                self.cookies.retain(|c| !domain_matches(&c.domain, &d));
            }
            None => self.cookies.clear(),
        }
        before - self.cookies.len()
    }
}

/// RFC 6265 5.1.4: the directory of the request's path.
fn default_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(at) => path[..at].into(),
    }
}

/// RFC 6265 5.1.3: `host` is `domain` or a name under it (not an IP).
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host.ends_with(domain)
            && host[..host.len() - domain.len()].ends_with('.')
            && host.parse::<std::net::IpAddr>().is_err())
}

/// RFC 6265 5.1.4.
fn path_matches(path: &str, cookie_path: &str) -> bool {
    path == cookie_path
        || (path.starts_with(cookie_path) && (cookie_path.ends_with('/') || path[cookie_path.len()..].starts_with('/')))
}

/// An HTTP date (`Wed, 21 Oct 2015 07:28:00 GMT`, or with dashes, or
/// two-digit years) in milliseconds since the epoch.
fn http_date_ms(text: &str) -> Option<u64> {
    let words: Vec<&str> = text.split(|c: char| c == ' ' || c == '-' || c == ',').filter(|w| !w.is_empty()).collect();
    let (mut day, mut month, mut year, mut time) = (None, None, None, None);
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    for w in words {
        let lower = w.to_ascii_lowercase();
        if w.contains(':') {
            time = Some(w);
        } else if let Some(m) = MONTHS.iter().position(|m| lower.starts_with(m)) {
            month = Some(m as u32 + 1);
        } else if let Ok(n) = w.parse::<i64>() {
            if day.is_none() && n <= 31 && w.len() <= 2 && year.is_none() {
                day = Some(n as u32);
            } else {
                year = Some(match n {
                    0..=69 => n + 2000,
                    70..=99 => n + 1900,
                    _ => n,
                });
            }
        }
    }
    let (day, month, year, time) = (day?, month?, year?, time?);
    let hms: Vec<u64> = time.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let [h, m, s] = <[u64; 3]>::try_from(hms).ok()?;
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return Some(0);
    }
    Some(((days as u64 * 24 + h) * 60 + m) * 60_000 + s * 1000)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn a_cookie_goes_back_to_its_host_and_path_and_not_elsewhere() {
        let mut jar = Jar::default();
        jar.store("https://api.example.com/v1/login", "sid=abc; Path=/v1; HttpOnly", NOW);
        jar.store("https://api.example.com/v1/login", "theme=dark", NOW);
        assert_eq!(jar.header_for("https://api.example.com/v1/me", NOW).as_deref(), Some("sid=abc; theme=dark"));
        // Its default path is /v1, the login's directory.
        assert_eq!(jar.header_for("https://api.example.com/other", NOW), None);
        assert_eq!(jar.header_for("https://example.com/v1/me", NOW), None);
        assert_eq!(jar.header_for("https://other.test/v1/me", NOW), None);
        assert_eq!(jar.header_for("https://api.example.com/v10", NOW), None);
    }

    #[test]
    fn a_domain_cookie_reaches_subdomains_and_only_its_own_parents_may_be_named() {
        let mut jar = Jar::default();
        jar.store("https://login.example.com/", "a=1; Domain=.example.com", NOW);
        jar.store("https://login.example.com/", "evil=1; Domain=other.test", NOW);
        assert_eq!(jar.header_for("https://api.example.com/", NOW).as_deref(), Some("a=1"));
        assert_eq!(jar.header_for("https://example.com/", NOW).as_deref(), Some("a=1"));
        assert_eq!(jar.header_for("https://other.test/", NOW), None);
    }

    #[test]
    fn secure_cookies_go_only_over_https_and_are_set_only_from_it() {
        let mut jar = Jar::default();
        jar.store("https://x.test/", "s=1; Secure", NOW);
        jar.store("http://x.test/", "t=1; Secure", NOW);
        assert_eq!(jar.header_for("https://x.test/", NOW).as_deref(), Some("s=1"));
        assert_eq!(jar.header_for("http://x.test/", NOW), None);
    }

    #[test]
    fn expiry_and_max_age_end_a_cookie_and_a_later_one_replaces_it() {
        let mut jar = Jar::default();
        jar.store("https://x.test/", "a=1; Max-Age=60", NOW);
        jar.store("https://x.test/", "b=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", NOW);
        assert_eq!(jar.header_for("https://x.test/", NOW).as_deref(), Some("a=1"));
        assert_eq!(jar.header_for("https://x.test/", NOW + 61_000), None);
        jar.store("https://x.test/", "c=1", NOW);
        jar.store("https://x.test/", "c=2", NOW);
        assert_eq!(jar.header_for("https://x.test/", NOW).as_deref(), Some("c=2"));
        jar.store("https://x.test/", "c=; Max-Age=0", NOW);
        assert_eq!(jar.header_for("https://x.test/", NOW), None);
    }

    #[test]
    fn http_dates_read_in_their_usual_shapes() {
        let at = 1_445_412_480_000; // 2015-10-21 07:28:00 UTC
        assert_eq!(http_date_ms("Wed, 21 Oct 2015 07:28:00 GMT"), Some(at));
        assert_eq!(http_date_ms("Wednesday, 21-Oct-15 07:28:00 GMT"), Some(at));
        assert_eq!(http_date_ms("not a date"), None);
    }

    #[test]
    fn clearing_a_domain_takes_its_subdomains_and_leaves_the_rest() {
        let mut jar = Jar::default();
        jar.store("https://a.example.com/", "a=1", NOW);
        jar.store("https://example.com/", "b=1", NOW);
        jar.store("https://other.test/", "c=1", NOW);
        assert_eq!(jar.clear(Some("example.com")), 2);
        assert_eq!(jar.seen(NOW, false).len(), 1);
        assert_eq!(jar.seen(NOW, false)[0].value, None);
        assert_eq!(jar.clear(None), 1);
    }
}
