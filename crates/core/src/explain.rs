//! "Explain this failure": one past request, everything about it that
//! helps a person or any AI say why it went as it did, as Markdown to
//! paste. Secret values are never in it: the request is as written
//! (`{{token}}`), the environment's secrets are named only, and a response
//! already had any secret value that came back replaced by its name.

use std::fmt::Write as _;

use tracer_protocol::{Browser, CheckResult, ClientKind, Ended, Environment, HistoryEntry, HttpResponse};

/// The most of a body put in: enough to see its shape and its error.
const BODY_MAX: usize = 8 * 1024;

/// What the bundle is made of.
pub struct Case<'a> {
    pub entry: &'a HistoryEntry,
    pub response: Option<&'a HttpResponse>,
    pub checks: &'a [CheckResult],
    /// Its environment as it is now, if it had one and it's still there.
    pub env: Option<&'a Environment>,
    pub browser: &'a Browser,
    /// The last earlier request to the same method and URL (as written)
    /// that answered below 400 with every check passed.
    pub last_good: Option<&'a HistoryEntry>,
}

/// Whether it went wrong, and how, in a line.
pub fn verdict(entry: &HistoryEntry, checks: &[CheckResult]) -> (bool, String) {
    let failed: Vec<&CheckResult> = checks.iter().filter(|c| !c.pass).collect();
    match &entry.ended {
        Ended::Failed { error } => (true, format!("no answer: {error}")),
        Ended::Cancelled { by } => (true, format!("cancelled by {}", who(&by.kind, &by.name))),
        Ended::AwaitingApproval { reason } => (true, format!("not sent: waiting for a person to allow it ({reason})")),
        Ended::Running => (false, "still running".into()),
        Ended::Done { status, .. } if *status >= 400 => (true, format!("answered {status}{}", reason(*status))),
        Ended::Done { status, .. } if !failed.is_empty() => {
            (true, format!("answered {status}, but {} of {} checks failed", failed.len(), checks.len()))
        }
        Ended::Done { status, .. } => (false, format!("answered {status}{}; nothing failed", reason(*status))),
    }
}

pub fn bundle(case: &Case) -> String {
    let e = case.entry;
    let (failed, line) = verdict(e, case.checks);
    let mut out = String::new();
    let r = &e.request;
    let _ = writeln!(out, "# {} #{}: {line}\n", if failed { "Failed request" } else { "Request" }, e.request_id);
    let _ = writeln!(out, "`{} {}`, sent by {} at {} (UTC).", r.method, r.url, who(&e.actor.kind, &e.actor.name), utc(e.at_ms));
    let mut context = Vec::new();
    if let Some(of) = e.rerun_of {
        context.push(format!("a re-run of #{of}"));
    }
    if let Some(flow) = e.flow {
        context.push(format!("a step of flow {flow}"));
    }
    if let Some(bench) = e.bench {
        context.push(format!("the first request of bench {bench}"));
    }
    if !context.is_empty() {
        let _ = writeln!(out, "It was {}.", context.join(", "));
    }

    out.push_str("\n## Request, as written\n\n");
    match (&e.env, case.env) {
        (None, _) => out.push_str("No environment: `{{variables}}` weren't filled.\n\n"),
        (Some(name), None) => {
            let _ = writeln!(out, "Environment `{name}`, which no longer exists.\n");
        }
        (Some(name), Some(env)) => {
            let _ = writeln!(out, "Environment `{name}` (as it is now):\n");
            for (k, v) in &env.vars {
                if !env.secrets.contains(k) {
                    let _ = writeln!(out, "- `{k}` = `{v}`");
                }
            }
            for k in &env.secrets {
                let _ = writeln!(out, "- `{k}`: a secret, value withheld");
            }
            out.push('\n');
        }
    }
    out.push_str("```http\n");
    let _ = writeln!(out, "{} {}", r.method, r.url);
    for (name, value) in &r.headers {
        let _ = writeln!(out, "{name}: {value}");
    }
    if let Some(body) = r.body.as_deref().filter(|b| !b.is_empty()) {
        let _ = write!(out, "\n{}", cut(body, BODY_MAX));
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("```\n");
    if !r.expect.is_empty() || !r.capture.is_empty() {
        out.push_str("\nExpectations and captures:\n\n");
        for x in &r.expect {
            let _ = writeln!(out, "- expect `{x}`");
        }
        for c in &r.capture {
            let _ = writeln!(out, "- capture `{c}`");
        }
    }

    if let Some(res) = case.response {
        let _ = writeln!(out, "\n## Response\n\n{}{}", res.status, reason(res.status));
        let t = &res.timing;
        let mut phases = Vec::new();
        for (name, ms) in [("DNS", t.dns_ms), ("connect", t.connect_ms), ("TLS", t.tls_ms)] {
            if let Some(ms) = ms {
                phases.push(format!("{name} {ms:.0} ms"));
            }
        }
        if !phases.is_empty() {
            phases.push(format!("server wait {:.0} ms", t.wait_ms()));
        }
        let _ = writeln!(
            out,
            "\nTime to first byte {:.0} ms, total {:.0} ms, {} bytes.{}{}",
            t.ttfb_ms,
            t.total_ms,
            res.size,
            if phases.is_empty() { String::new() } else { format!(" ({})", phases.join(", ")) },
            if res.tls_unchecked { " The server's TLS certificate was not checked." } else { "" }
        );
        out.push_str("\n```http\n");
        for (name, value) in &res.headers {
            let _ = writeln!(out, "{name}: {value}");
        }
        if !res.body.is_empty() {
            let _ = write!(out, "\n{}", cut(&res.body, BODY_MAX));
            if !res.body.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str("```\n");
        if res.body.len() > BODY_MAX {
            let _ = writeln!(out, "\nThe body is cut at {} KiB of {} bytes.", BODY_MAX / 1024, res.body.len());
        }
    } else if let Ended::Failed { error } = &e.ended {
        let _ = writeln!(out, "\n## No response\n\n```\n{error}\n```");
    }

    if !case.checks.is_empty() {
        out.push_str("\n## Checks\n\n");
        for c in case.checks {
            let _ = writeln!(out, "- {} `{}`: found {}", if c.pass { "passed" } else { "**failed**" }, c.expect, c.actual);
        }
    }

    let b = case.browser;
    let mut shaped = Vec::new();
    if b.cookies {
        shaped.push("cookies are kept and sent back from the workspace's jar (a `Cookie` header may have been added)".to_string());
    }
    if let Some(origin) = &b.origin {
        shaped.push(format!("requests go as from a page at `{origin}`: `Origin` is sent, CORS preflights run first, and answers are checked for CORS"));
    }
    if !b.unchecked_tls.is_empty() {
        shaped.push(format!("TLS certificates aren't checked for {}", b.unchecked_tls.join(", ")));
    }
    if !shaped.is_empty() {
        out.push_str("\n## Workspace settings (as they are now)\n\n");
        for s in shaped {
            let _ = writeln!(out, "- {s}");
        }
    }

    out.push_str("\n## Last time it worked\n\n");
    match case.last_good {
        Some(good) => {
            let status = match good.ended {
                Ended::Done { status, .. } => status.to_string(),
                _ => "?".into(),
            };
            let env = good.env.as_deref().map(|e| format!(" with environment `{e}`")).unwrap_or_default();
            let _ = writeln!(
                out,
                "#{} answered {status} at {}{env}. `tracer diff {} {}` shows how the answers differ.",
                good.request_id,
                utc(good.at_ms),
                good.request_id,
                e.request_id
            );
        }
        None => out.push_str("No earlier request to the same URL, with the same method, answered without an error.\n"),
    }
    out
}

/// Who sent it, as the bundle says it.
fn who(kind: &ClientKind, name: &str) -> String {
    let kind = match kind {
        ClientKind::Window => "the window",
        ClientKind::Cli => "the CLI",
        ClientKind::Agent => "agent",
        ClientKind::Disk => "a file change",
    };
    format!("{kind} ({name})")
}

/// A few words on what a status means, for the statuses that come up.
fn reason(status: u16) -> &'static str {
    match status {
        400 => " Bad Request",
        401 => " Unauthorized: no credentials, or they weren't accepted",
        403 => " Forbidden: the credentials were understood but aren't allowed this",
        404 => " Not Found",
        405 => " Method Not Allowed",
        409 => " Conflict",
        415 => " Unsupported Media Type: check Content-Type",
        422 => " Unprocessable Content: the body was read but not accepted",
        429 => " Too Many Requests: rate limited",
        500 => " Internal Server Error",
        502 => " Bad Gateway",
        503 => " Service Unavailable",
        504 => " Gateway Timeout",
        _ => "",
    }
}

fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[..at]
}

/// `2026-11-05 05:49:12` from milliseconds since the epoch.
fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::{Actor, HttpRequest, Timing};

    fn entry(id: u64, ended: Ended) -> HistoryEntry {
        HistoryEntry {
            request_id: id,
            at_ms: 1_793_857_752_000,
            actor: Actor { kind: ClientKind::Agent, name: "claude-code".into() },
            request: HttpRequest {
                method: "POST".into(),
                url: "{{base}}/users".into(),
                headers: vec![("Authorization".into(), "Bearer {{token}}".into())],
                body: Some(r#"{"name":"Ada"}"#.into()),
                expect: vec!["status 2xx".into()],
                capture: vec![],
            },
            env: Some("local".into()),
            rerun_of: None,
            flow: Some(3),
            bench: None,
            ended,
            checks: None,
        }
    }

    #[test]
    fn a_failure_with_its_request_answer_checks_and_last_success() {
        let failed = entry(12, Ended::Done { status: 422, total_ms: 40.0, size: 30 });
        let good = entry(9, Ended::Done { status: 201, total_ms: 30.0, size: 30 });
        let response = HttpResponse {
            status: 422,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: r#"{"error":"email is required"}"#.into(),
            size: 30,
            timing: Timing { ttfb_ms: 38.0, total_ms: 40.0, dns_ms: Some(1.0), connect_ms: Some(10.0), tls_ms: None },
            tls_unchecked: false,
        };
        let checks = vec![CheckResult { expect: "status 2xx".into(), pass: false, actual: "422".into() }];
        let env = Environment {
            name: "local".into(),
            vars: vec![("base".into(), "http://localhost:3000".into()), ("token".into(), "x".into())],
            secrets: vec!["token".into()],
        };
        let browser = Browser { cookies: true, ..Browser::default() };
        let case = Case { entry: &failed, response: Some(&response), checks: &checks, env: Some(&env), browser: &browser, last_good: Some(&good) };
        let text = bundle(&case);
        assert!(text.starts_with("# Failed request #12: answered 422 Unprocessable Content"), "{text}");
        assert!(text.contains("sent by agent (claude-code) at 2026-11-05 "), "{text}");
        assert!(text.contains("It was a step of flow 3."));
        assert!(text.contains("- `base` = `http://localhost:3000`"));
        assert!(text.contains("- `token`: a secret, value withheld"));
        assert!(!text.contains("`token` = "), "a secret's value never shows: {text}");
        assert!(text.contains("POST {{base}}/users\nAuthorization: Bearer {{token}}\n\n{\"name\":\"Ada\"}\n```"));
        assert!(text.contains(r#"{"error":"email is required"}"#));
        assert!(text.contains("(DNS 1 ms, connect 10 ms, server wait 27 ms)"), "{text}");
        assert!(text.contains("- **failed** `status 2xx`: found 422"));
        assert!(text.contains("cookies are kept"));
        assert!(text.contains("#9 answered 201 at 2026-11-05 "));
        assert!(text.contains("`tracer diff 9 12`"));
    }

    #[test]
    fn verdicts() {
        assert!(verdict(&entry(1, Ended::Failed { error: "connection refused".into() }), &[]).1.contains("no answer: connection refused"));
        let (bad, line) = verdict(&entry(1, Ended::Done { status: 200, total_ms: 1.0, size: 0 }), &[]);
        assert!(!bad && line == "answered 200; nothing failed", "{line}");
        let ok_but = [CheckResult { expect: "$.id == 1".into(), pass: false, actual: "2".into() }];
        assert!(verdict(&entry(1, Ended::Done { status: 200, total_ms: 1.0, size: 0 }), &ok_but).0);
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00");
    }
}
