//! Comparing two responses, past what changes on every call.
//!
//! JSON bodies are compared as values, so key order and spacing don't
//! count, and each difference is named by its path. Other bodies are
//! compared line by line. Headers and body keys that differ on every call
//! (a `Date`, a request id, a `timestamp`) are ignored by default; what was
//! ignored is listed, so a reader can see nothing was hidden.

use serde_json::Value;
use tracer_protocol::{BodyDiff, Change, ChangeKind, DiffLine, DiffSide, HttpResponse, LineOp, ResponseDiff};

/// Headers that differ between any two calls.
const NOISY_HEADERS: [&str; 18] = [
    "header:date",
    "header:age",
    "header:etag",
    "header:last-modified",
    "header:expires",
    "header:content-length",
    "header:set-cookie",
    "header:x-request-id",
    "header:request-id",
    "header:x-correlation-id",
    "header:x-trace-id",
    "header:traceparent",
    "header:cf-ray",
    "header:x-amz*",
    "header:server-timing",
    "header:x-runtime",
    "header:x-response-time",
    "header:report-to",
];

/// Body keys that hold the moment or the call itself.
const NOISY_KEYS: [&str; 10] =
    ["timestamp", "serverTime", "server_time", "generatedAt", "generated_at", "requestId", "request_id", "traceId", "trace_id", "nonce"];

/// The most changes or lines listed; the rest are counted.
const LISTED: usize = 500;
/// Bodies longer than this many lines times lines aren't compared by line.
const LINE_PAIRS: usize = 4_000_000;
/// Values are cut to this in a change.
const VALUE_SHOWN: usize = 200;
/// Unchanged lines kept around each change.
const CONTEXT: usize = 2;

pub fn compare(a: (DiffSide, &HttpResponse), b: (DiffSide, &HttpResponse), ignore: &[String], defaults: bool) -> ResponseDiff {
    let mut patterns: Vec<String> = Vec::new();
    if defaults {
        patterns.extend(NOISY_HEADERS.iter().map(|s| s.to_string()));
        patterns.extend(NOISY_KEYS.iter().map(|s| s.to_string()));
    }
    patterns.extend(ignore.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    let rules: Vec<Rule> = patterns.iter().map(|p| Rule::parse(p)).collect();
    let mut ignored = Vec::new();

    let headers = compare_headers(&a.1.headers, &b.1.headers, &rules, &mut ignored);
    let body = match (serde_json::from_str::<Value>(&a.1.body), serde_json::from_str::<Value>(&b.1.body)) {
        (Ok(x), Ok(y)) => {
            let mut changes = Vec::new();
            let mut more = 0;
            walk(&x, &y, &mut Vec::new(), &rules, &mut changes, &mut more, &mut ignored);
            if changes.is_empty() {
                BodyDiff::Same
            } else {
                BodyDiff::Json { changes, more }
            }
        }
        _ => compare_lines(&a.1.body, &b.1.body),
    };
    ResponseDiff { a: a.0, b: b.0, headers, body, ignored, patterns }
}

#[derive(Clone, Debug, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}

/// A pattern, parsed.
enum Rule {
    /// `header:name`, or `header:prefix*`.
    Header(String),
    /// `$.a[*].b`: a path, matching what's under it too. `None` is `*`.
    Path(Vec<Option<Seg>>),
    /// A bare key, anywhere.
    Key(String),
}

impl Rule {
    fn parse(p: &str) -> Rule {
        if let Some(h) = p.strip_prefix("header:") {
            return Rule::Header(h.trim().to_ascii_lowercase());
        }
        let Some(rest) = p.strip_prefix('$') else { return Rule::Key(p.to_string()) };
        let mut segs = Vec::new();
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '.' => {
                    let mut name = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == '.' || c == '[' {
                            break;
                        }
                        name.push(c);
                        chars.next();
                    }
                    segs.push(if name == "*" { None } else { Some(Seg::Key(name)) });
                }
                '[' => {
                    let mut inner = String::new();
                    for c in chars.by_ref() {
                        if c == ']' {
                            break;
                        }
                        inner.push(c);
                    }
                    let inner = inner.trim();
                    segs.push(match inner {
                        "*" => None,
                        _ => match inner.parse::<usize>() {
                            Ok(i) => Some(Seg::Index(i)),
                            Err(_) => Some(Seg::Key(inner.trim_matches('"').to_string())),
                        },
                    });
                }
                _ => {}
            }
        }
        Rule::Path(segs)
    }

    fn covers_path(&self, path: &[Seg]) -> bool {
        match self {
            Rule::Header(_) => false,
            Rule::Key(k) => path.iter().any(|s| matches!(s, Seg::Key(name) if name == k)),
            Rule::Path(segs) => {
                segs.len() <= path.len() && segs.iter().zip(path).all(|(want, got)| want.as_ref().is_none_or(|w| w == got))
            }
        }
    }

    fn covers_header(&self, name: &str) -> bool {
        match self {
            Rule::Header(h) => match h.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == h,
            },
            _ => false,
        }
    }
}

/// `$.data[3].name`; keys that aren't plain words are quoted.
fn path_text(path: &[Seg]) -> String {
    let mut out = String::from("$");
    for s in path {
        match s {
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
            Seg::Key(k) if !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') => {
                out.push('.');
                out.push_str(k);
            }
            Seg::Key(k) => out.push_str(&format!("[{}]", Value::String(k.clone()))),
        }
    }
    out
}

fn shown(text: String) -> String {
    if text.len() <= VALUE_SHOWN {
        return text;
    }
    let mut cut = VALUE_SHOWN;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

fn json_text(v: &Value) -> String {
    shown(v.to_string())
}

fn walk(
    a: &Value,
    b: &Value,
    path: &mut Vec<Seg>,
    rules: &[Rule],
    out: &mut Vec<Change>,
    more: &mut u64,
    ignored: &mut Vec<String>,
) {
    if a == b {
        return;
    }
    if rules.iter().any(|r| r.covers_path(path)) {
        ignored.push(path_text(path));
        return;
    }
    let push = |change: Change, out: &mut Vec<Change>, more: &mut u64| {
        if out.len() < LISTED {
            out.push(change);
        } else {
            *more += 1;
        }
    };
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, va) in x {
                path.push(Seg::Key(k.clone()));
                match y.get(k) {
                    Some(vb) => walk(va, vb, path, rules, out, more, ignored),
                    None if rules.iter().any(|r| r.covers_path(path)) => ignored.push(path_text(path)),
                    None => push(
                        Change { at: path_text(path), kind: ChangeKind::Removed, before: Some(json_text(va)), after: None },
                        out,
                        more,
                    ),
                }
                path.pop();
            }
            for (k, vb) in y.iter().filter(|(k, _)| !x.contains_key(*k)) {
                path.push(Seg::Key(k.clone()));
                if rules.iter().any(|r| r.covers_path(path)) {
                    ignored.push(path_text(path));
                } else {
                    push(Change { at: path_text(path), kind: ChangeKind::Added, before: None, after: Some(json_text(vb)) }, out, more);
                }
                path.pop();
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            for i in 0..x.len().max(y.len()) {
                path.push(Seg::Index(i));
                match (x.get(i), y.get(i)) {
                    (Some(va), Some(vb)) => walk(va, vb, path, rules, out, more, ignored),
                    (Some(va), None) => push(
                        Change { at: path_text(path), kind: ChangeKind::Removed, before: Some(json_text(va)), after: None },
                        out,
                        more,
                    ),
                    (None, Some(vb)) => push(
                        Change { at: path_text(path), kind: ChangeKind::Added, before: None, after: Some(json_text(vb)) },
                        out,
                        more,
                    ),
                    (None, None) => {}
                }
                path.pop();
            }
        }
        _ => push(
            Change { at: path_text(path), kind: ChangeKind::Changed, before: Some(json_text(a)), after: Some(json_text(b)) },
            out,
            more,
        ),
    }
}

/// By name, ignoring case; a name given twice compares as its values
/// joined, in order.
fn compare_headers(a: &[(String, String)], b: &[(String, String)], rules: &[Rule], ignored: &mut Vec<String>) -> Vec<Change> {
    let gather = |hs: &[(String, String)]| {
        let mut out: Vec<(String, String)> = Vec::new();
        for (k, v) in hs {
            let k = k.to_ascii_lowercase();
            match out.iter_mut().find(|(name, _)| *name == k) {
                Some((_, joined)) => {
                    joined.push_str(", ");
                    joined.push_str(v);
                }
                None => out.push((k, v.clone())),
            }
        }
        out
    };
    let (x, y) = (gather(a), gather(b));
    let mut changes = Vec::new();
    let mut names: Vec<&String> = x.iter().map(|(k, _)| k).collect();
    names.extend(y.iter().map(|(k, _)| k).filter(|k| !x.iter().any(|(n, _)| n == *k)));
    for name in names {
        let before = x.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let after = y.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        if before == after {
            continue;
        }
        if rules.iter().any(|r| r.covers_header(name)) {
            ignored.push(format!("header:{name}"));
            continue;
        }
        let kind = match (&before, &after) {
            (None, _) => ChangeKind::Added,
            (_, None) => ChangeKind::Removed,
            _ => ChangeKind::Changed,
        };
        changes.push(Change { at: name.clone(), kind, before: before.map(shown), after: after.map(shown) });
    }
    changes
}

/// A longest-common-subsequence line diff, kept to the changes and their
/// context.
fn compare_lines(a: &str, b: &str) -> BodyDiff {
    let (x, y): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    if x == y {
        return BodyDiff::Same;
    }
    if x.len().saturating_mul(y.len()) > LINE_PAIRS {
        return BodyDiff::TooLarge { a_lines: x.len() as u64, b_lines: y.len() as u64 };
    }
    // lcs[i][j]: the longest common run of x[i..] and y[j..].
    let w = y.len() + 1;
    let mut lcs = vec![0u32; (x.len() + 1) * w];
    for i in (0..x.len()).rev() {
        for j in (0..y.len()).rev() {
            lcs[i * w + j] = if x[i] == y[j] { lcs[(i + 1) * w + j + 1] + 1 } else { lcs[(i + 1) * w + j].max(lcs[i * w + j + 1]) };
        }
    }
    let mut ops: Vec<DiffLine> = Vec::new();
    let (mut i, mut j) = (0, 0);
    let line = |op, a: Option<usize>, b: Option<usize>, text: &str| DiffLine {
        op,
        a: a.map(|n| n as u64 + 1),
        b: b.map(|n| n as u64 + 1),
        text: text.to_string(),
    };
    while i < x.len() || j < y.len() {
        if i < x.len() && j < y.len() && x[i] == y[j] {
            ops.push(line(LineOp::Same, Some(i), Some(j), x[i]));
            i += 1;
            j += 1;
        } else if i < x.len() && (j == y.len() || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
            // A line's removal before what replaces it.
            ops.push(line(LineOp::Removed, Some(i), None, x[i]));
            i += 1;
        } else {
            ops.push(line(LineOp::Added, None, Some(j), y[j]));
            j += 1;
        }
    }
    // Keep changes and the lines within CONTEXT of one.
    let near: Vec<bool> = (0..ops.len())
        .map(|k| {
            let lo = k.saturating_sub(CONTEXT);
            let hi = (k + CONTEXT).min(ops.len() - 1);
            (lo..=hi).any(|m| ops[m].op != LineOp::Same)
        })
        .collect();
    let mut lines = Vec::new();
    let mut more = 0;
    let mut skipped = false;
    for (k, op) in ops.into_iter().enumerate() {
        if !near[k] {
            skipped = true;
            continue;
        }
        if lines.len() >= LISTED {
            more += 1;
            continue;
        }
        if skipped && !lines.is_empty() {
            lines.push(DiffLine { op: LineOp::Gap, a: None, b: None, text: String::new() });
        }
        skipped = false;
        lines.push(op);
    }
    BodyDiff::Text { lines, more }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::Timing;

    fn response(headers: &[(&str, &str)], body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.into(),
            size: body.len() as u64,
            timing: Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None },
        }
    }

    fn side(id: u64) -> DiffSide {
        DiffSide { request_id: id, method: "GET".into(), url: "http://x/".into(), status: 200 }
    }

    fn diff(a: &HttpResponse, b: &HttpResponse, ignore: &[&str]) -> ResponseDiff {
        let ignore: Vec<String> = ignore.iter().map(|s| s.to_string()).collect();
        compare((side(1), a), (side(2), b), &ignore, true)
    }

    #[test]
    fn json_compares_as_values_and_names_each_change_by_its_path() {
        let a = response(&[], r#"{"a":1,"list":[{"n":"x"},{"n":"y"}],"gone":true,"meta":{"timestamp":5}}"#);
        let b = response(&[], r#"{"list":[{"n":"x"},{"n":"Y"},{"n":"z"}],"a":1,"new":null,"meta":{"timestamp":6}}"#);
        let d = diff(&a, &b, &[]);
        let BodyDiff::Json { changes, more: 0 } = d.body else { panic!("{:?}", d.body) };
        let got: Vec<(String, ChangeKind)> = changes.iter().map(|c| (c.at.clone(), c.kind)).collect();
        assert_eq!(
            got,
            // Keys in order, as serde_json keeps them.
            [
                ("$.gone".to_string(), ChangeKind::Removed),
                ("$.list[1].n".to_string(), ChangeKind::Changed),
                ("$.list[2]".to_string(), ChangeKind::Added),
                ("$.new".to_string(), ChangeKind::Added),
            ]
        );
        assert_eq!(changes[1].before.as_deref(), Some("\"y\""));
        assert_eq!(d.ignored, ["$.meta.timestamp"], "ignored, and said so");
    }

    #[test]
    fn patterns_ignore_a_path_a_key_anywhere_or_headers() {
        let a = response(&[("Date", "Mon"), ("X-Amzn-Trace", "1"), ("Content-Type", "a")], r#"{"d":[{"at":1,"v":1}],"k":{"updatedAt":1}}"#);
        let b = response(&[("date", "Tue"), ("X-Amzn-Trace", "2"), ("Content-Type", "b")], r#"{"d":[{"at":2,"v":1}],"k":{"updatedAt":2}}"#);
        let d = diff(&a, &b, &["$.d[*].at", "updatedAt"]);
        assert_eq!(d.body, BodyDiff::Same);
        assert_eq!(d.headers.len(), 1);
        assert_eq!((d.headers[0].at.as_str(), d.headers[0].after.as_deref()), ("content-type", Some("b")));
        assert_eq!(d.ignored, ["header:date", "header:x-amzn-trace", "$.d[0].at", "$.k.updatedAt"]);
        let all = compare((side(1), &a), (side(2), &b), &[], false);
        assert_eq!(all.headers.len(), 3, "no defaults, every header counts");
    }

    #[test]
    fn text_is_compared_by_line_with_context() {
        let a = response(&[], "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12");
        let b = response(&[], "1\nTWO\n3\n4\n5\n6\n7\n8\n9\n10\nELEVEN\n12");
        let BodyDiff::Text { lines, .. } = diff(&a, &b, &[]).body else { panic!() };
        let shown: Vec<String> = lines
            .iter()
            .map(|l| match l.op {
                LineOp::Same => format!(" {}", l.text),
                LineOp::Removed => format!("-{}", l.text),
                LineOp::Added => format!("+{}", l.text),
                LineOp::Gap => "…".into(),
            })
            .collect();
        assert_eq!(shown, [" 1", "-2", "+TWO", " 3", " 4", "…", " 9", " 10", "-11", "+ELEVEN", " 12"]);
        assert_eq!((lines[1].a, lines[2].b), (Some(2), Some(2)));
        assert_eq!(diff(&a, &a, &[]).body, BodyDiff::Same);
    }
}
