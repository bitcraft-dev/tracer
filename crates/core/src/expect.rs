//! Expectations: what a request's answer should be, one line each, checked
//! by the core when the answer arrives.
//!
//! ```text
//! status 2xx                       status == 201, status < 400
//! time < 500                       ttfb < 200, size < 10000 (ms, ms, bytes)
//! header Content-Type contains json
//! header X-Request-Id exists       missing, ==, !=
//! $.user.id == 1                   any JSON value; a bare word is a string
//! $.items[0].name contains "Kam"   a string's text, or an array's items
//! len($.items) >= 3                an array's items, an object's keys, a string's characters
//! body contains ok
//! ```
//!
//! An expectation that can't be read fails, saying why, rather than
//! passing quietly.

use serde_json::Value;
use tracer_protocol::{CheckResult, HttpResponse};

enum Subject {
    Status,
    Time,
    Ttfb,
    Size,
    Header(String),
    Body,
    Path(String),
    Len(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    Exists,
    Missing,
    /// `status 2xx`: the hundreds digit.
    Class(u16),
}

struct Expect {
    subject: Subject,
    op: Op,
    value: Option<Value>,
}

/// Each of `expect` against `response`, or against no response at all
/// (`Err`: why there was none).
pub fn check(expect: &[String], response: Result<&HttpResponse, &str>) -> Vec<CheckResult> {
    let json = response.ok().and_then(|r| serde_json::from_str::<Value>(&r.body).ok());
    expect
        .iter()
        .map(|line| {
            let (pass, actual) = match (parse(line), response) {
                (Err(why), _) => (false, format!("can't read this: {why}")),
                (Ok(_), Err(why)) => (false, format!("no answer: {why}")),
                (Ok(e), Ok(r)) => evaluate(&e, r, json.as_ref()),
            };
            CheckResult { expect: line.clone(), pass, actual }
        })
        .collect()
}

/// Whether `line` is an expectation that can be read, and if not, why.
pub fn readable(line: &str) -> Result<(), String> {
    parse(line).map(|_| ())
}

fn parse(line: &str) -> Result<Expect, String> {
    let line = line.trim();
    let (word, rest) = split_word(line);
    let (subject, rest) = match word {
        "status" => (Subject::Status, rest),
        "time" => (Subject::Time, rest),
        "ttfb" => (Subject::Ttfb, rest),
        "size" => (Subject::Size, rest),
        "body" => (Subject::Body, rest),
        "header" => {
            let (name, rest) = split_word(rest);
            if name.is_empty() {
                return Err("`header` takes a name, such as `header Content-Type exists`".into());
            }
            (Subject::Header(name.to_ascii_lowercase()), rest)
        }
        w if w.starts_with('$') => (Subject::Path(w.to_string()), rest),
        w if w.starts_with("len(") && w.ends_with(')') => (Subject::Len(w[4..w.len() - 1].to_string()), rest),
        "" => return Err("it's empty".into()),
        w => return Err(format!("`{w}` isn't something to check: status, time, ttfb, size, header, body, $.path or len($.path)")),
    };
    let (op_word, value) = split_word(rest);
    let op = match op_word {
        "==" => Op::Eq,
        "!=" => Op::Ne,
        "<" => Op::Lt,
        "<=" => Op::Le,
        ">" => Op::Gt,
        ">=" => Op::Ge,
        "contains" => Op::Contains,
        "exists" => Op::Exists,
        "missing" => Op::Missing,
        w if matches!(subject, Subject::Status) && w.len() == 3 && w.ends_with("xx") => {
            match w[..1].parse::<u16>() {
                Ok(d @ 1..=5) => Op::Class(d),
                _ => return Err(format!("`{w}` is not a status class such as 2xx")),
            }
        }
        "" => return Err("it says what to check but not how: ==, !=, <, <=, >, >=, contains, exists or missing".into()),
        w => return Err(format!("`{w}` is not ==, !=, <, <=, >, >=, contains, exists or missing")),
    };
    let needs_value = !matches!(op, Op::Exists | Op::Missing | Op::Class(_));
    let value = value.trim();
    match (needs_value, value.is_empty()) {
        (true, true) => return Err(format!("`{op_word}` needs a value after it")),
        (false, false) => return Err(format!("nothing comes after `{op_word}`")),
        _ => {}
    }
    let value = needs_value.then(|| serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string())));
    Ok(Expect { subject, op, value })
}

/// The first word and the rest. Spaces inside `[…]`, `(…)` or quotes are
/// part of the word, so `$["odd key"]` is one.
fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let (mut depth, mut quoted) = (0i32, false);
    for (at, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' if !quoted => depth += 1,
            ']' | ')' if !quoted => depth -= 1,
            c if c.is_whitespace() && !quoted && depth <= 0 => return (&s[..at], &s[at..]),
            _ => {}
        }
    }
    (s, "")
}

fn evaluate(e: &Expect, r: &HttpResponse, json: Option<&Value>) -> (bool, String) {
    let number = |n: f64| Some(Value::from(n));
    let found: Option<Value> = match &e.subject {
        Subject::Status => Some(Value::from(r.status)),
        Subject::Time => number((r.timing.total_ms * 10.0).round() / 10.0),
        Subject::Ttfb => number((r.timing.ttfb_ms * 10.0).round() / 10.0),
        Subject::Size => Some(Value::from(r.size)),
        Subject::Body => Some(Value::String(r.body.clone())),
        Subject::Header(name) => {
            let values: Vec<&str> = r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect();
            (!values.is_empty()).then(|| Value::String(values.join(", ")))
        }
        Subject::Path(path) | Subject::Len(path) => {
            let Some(json) = json else { return (false, "the body is not JSON".into()) };
            let segs = match segments(path) {
                Ok(s) => s,
                Err(why) => return (false, format!("can't read this: {why}")),
            };
            let at = segs.iter().try_fold(json, |v, s| match s {
                Seg::Key(k) => v.get(k.as_str()),
                Seg::Index(i) => v.get(*i),
            });
            match (&e.subject, at) {
                (Subject::Len(_), Some(Value::Array(a))) => Some(Value::from(a.len())),
                (Subject::Len(_), Some(Value::Object(o))) => Some(Value::from(o.len())),
                (Subject::Len(_), Some(Value::String(s))) => Some(Value::from(s.chars().count())),
                (Subject::Len(_), Some(other)) => return (false, format!("{} has no length", shown(other))),
                (_, at) => at.cloned(),
            }
        }
    };
    let actual = found.as_ref().map_or("missing".to_string(), shown);
    let pass = match (e.op, &found, &e.value) {
        (Op::Exists, f, _) => f.is_some(),
        (Op::Missing, f, _) => f.is_none(),
        (_, None, _) => false,
        (Op::Class(d), Some(v), _) => v.as_u64().is_some_and(|s| s / 100 == d as u64),
        (Op::Eq, Some(v), Some(want)) => same(v, want),
        (Op::Ne, Some(v), Some(want)) => !same(v, want),
        (Op::Contains, Some(Value::String(s)), Some(want)) => s.contains(&*text_of(want)),
        (Op::Contains, Some(Value::Array(items)), Some(want)) => items.iter().any(|i| same(i, want)),
        (Op::Contains, Some(_), _) => return (false, format!("{actual}: `contains` takes a string or an array")),
        (op, Some(v), Some(want)) => match (v.as_f64(), want.as_f64()) {
            (Some(a), Some(b)) => match op {
                Op::Lt => a < b,
                Op::Le => a <= b,
                Op::Gt => a > b,
                Op::Ge => a >= b,
                _ => false,
            },
            _ => return (false, format!("{actual}: <, <=, > and >= compare numbers")),
        },
        _ => false,
    };
    (pass, actual)
}

/// Equal as JSON, numbers by value (`1` is `1.0`). A bare word on the
/// right compares with a number or boolean's text too, so `status == 200`
/// and `$.ok == true` read as written.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn shown(v: &Value) -> String {
    let s = v.to_string();
    if s.len() <= 120 {
        return s;
    }
    let mut cut = 120;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

enum Seg {
    Key(String),
    Index(usize),
}

/// `$.a.b[0]["odd key"]`.
fn segments(path: &str) -> Result<Vec<Seg>, String> {
    let rest = path.strip_prefix('$').ok_or("a path starts with $")?;
    let mut out = Vec::new();
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
                if name.is_empty() {
                    return Err(format!("`{path}` has an empty key"));
                }
                out.push(Seg::Key(name));
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
                match inner.parse::<usize>() {
                    Ok(i) => out.push(Seg::Index(i)),
                    Err(_) if inner.starts_with('"') && inner.ends_with('"') && inner.len() >= 2 => {
                        out.push(Seg::Key(inner[1..inner.len() - 1].to_string()))
                    }
                    Err(_) => return Err(format!("`[{inner}]` is not an index or a \"key\" (no wildcards here)")),
                }
            }
            c => return Err(format!("`{c}` in `{path}`: a path goes on with .key or [index]")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::Timing;

    fn response() -> HttpResponse {
        let body = r#"{"user":{"id":1,"name":"Ada"},"items":[{"name":"Kampala"},{"name":"Lagos"}],"ok":true,"odd key":2}"#;
        HttpResponse {
            status: 201,
            headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())],
            body: body.into(),
            size: body.len() as u64,
            timing: Timing { ttfb_ms: 40.0, total_ms: 42.5, dns_ms: None, connect_ms: None, tls_ms: None },
        }
    }

    fn results(lines: &[&str]) -> Vec<(bool, String)> {
        let lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        check(&lines, Ok(&response())).into_iter().map(|r| (r.pass, r.actual)).collect()
    }

    #[test]
    fn what_holds_passes() {
        let lines = [
            "status 2xx",
            "status == 201",
            "status < 400",
            "time < 500",
            "ttfb <= 40",
            "size > 10",
            "header content-type contains json",
            "header X-Missing missing",
            "$.user.id == 1",
            "$.user.id == 1.0",
            "$.user.name == Ada",
            "$.user.name == \"Ada\"",
            "$.ok == true",
            "$.items[1].name contains Lag",
            "$.items contains {\"name\":\"Lagos\"}",
            "len($.items) == 2",
            "len($.user) == 2",
            "$[\"odd key\"] == 2",
            "$.user exists",
            "$.nothing missing",
            "body contains Kampala",
        ];
        for (line, (pass, actual)) in lines.iter().zip(results(&lines)) {
            assert!(pass, "{line} (found {actual})");
        }
    }

    #[test]
    fn what_doesnt_hold_fails_and_says_what_was_there() {
        assert_eq!(results(&["status 4xx"]), [(false, "201".to_string())]);
        assert_eq!(results(&["$.user.id == 2"]), [(false, "1".to_string())]);
        assert_eq!(results(&["$.user.email exists"]), [(false, "missing".to_string())]);
        assert_eq!(results(&["time < 10"]), [(false, "42.5".to_string())]);
    }

    #[test]
    fn what_cant_be_read_fails_saying_why() {
        for (line, why) in [
            ("statu 200", "isn't something to check"),
            ("status", "but not how"),
            ("status ==", "needs a value"),
            ("$.a exists now", "nothing comes after"),
            ("$.items[*].name exists", "no wildcards"),
            ("$.user.name < 3", "compare numbers"),
            ("len($.ok) == 1", "has no length"),
        ] {
            let got = results(&[line]);
            assert!(!got[0].0 && got[0].1.contains(why), "{line}: {:?}", got[0].1);
        }
        let none = check(&["status 2xx".to_string()], Err("connection refused"));
        assert_eq!((none[0].pass, none[0].actual.as_str()), (false, "no answer: connection refused"));
    }
}
