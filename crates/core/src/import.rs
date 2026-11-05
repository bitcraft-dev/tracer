//! A request from a pasted command: a `curl` command line, as a browser's
//! "Copy as cURL" or a person writes one, or a JavaScript `fetch(…)` call,
//! as "Copy as fetch" gives it.
//!
//! What can't be carried over into a request (a multipart form, a file to
//! send, `credentials: "include"`) is named in the notes rather than
//! dropped without a word.

use std::fmt;

use tracer_protocol::HttpRequest;

use crate::ws::base64;

/// A JavaScript literal, its objects' keys in the order written (as
/// `JSON.stringify` keeps them).
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    /// As JSON writes it.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

/// As JSON, which is what `JSON.stringify` gives.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let string = |s: &str| serde_json::to_string(s).expect("a string serializes");
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => f.write_str(n),
            Value::String(s) => f.write_str(&string(s)),
            Value::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Value::Object(pairs) => {
                f.write_str("{")?;
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{}:{value}", string(key))?;
                }
                f.write_str("}")
            }
        }
    }
}

/// A request read from `text`, and notes on what of it was left out.
pub fn import(text: &str) -> Result<(HttpRequest, Vec<String>), String> {
    let trimmed = text.trim().trim_start_matches('$').trim_start();
    if trimmed.starts_with("curl") {
        curl(trimmed)
    } else if let Some(at) = trimmed.find("fetch(") {
        fetch(&trimmed[at + "fetch(".len()..])
    } else {
        Err("not a curl command or a fetch(…) call".into())
    }
}

fn request(method: &str, url: String, headers: Vec<(String, String)>, body: Option<String>) -> HttpRequest {
    HttpRequest { method: method.to_ascii_uppercase(), url, headers, body, expect: vec![], capture: vec![] }
}

// ---- curl ----

/// The command's words. A command for Windows' cmd (its `^` line joins
/// and `^"` quotes, as a browser's "Copy as cURL (cmd)" writes) is read as
/// cmd and then curl.exe would; anything else as a POSIX shell would.
fn words(text: &str) -> Result<Vec<String>, String> {
    if text.contains("^\n") || text.contains("^\r\n") || text.contains("^\"") {
        Ok(cmd_words(text))
    } else {
        posix_words(text)
    }
}

/// cmd takes `^` as escaping the next character (a line end, to join
/// lines) outside its own quotes; then curl.exe splits its line as
/// Windows' C runtime does: `"` toggles quoting, `\"` is a quote, and
/// backslashes before a quote pair up.
fn cmd_words(text: &str) -> Vec<String> {
    let mut line = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '^' if !quoted => match chars.next() {
                Some('\r') => {
                    chars.next_if_eq(&'\n');
                }
                Some('\n') | None => {}
                Some(next) => line.push(next),
            },
            '"' => {
                quoted = !quoted;
                line.push(c);
            }
            c => line.push(c),
        }
    }
    let mut out = Vec::new();
    let mut word = String::new();
    let (mut started, mut quoted) = (false, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut n = 1;
                while chars.next_if_eq(&'\\').is_some() {
                    n += 1;
                }
                started = true;
                if chars.peek() == Some(&'"') {
                    word.extend(std::iter::repeat_n('\\', n / 2));
                    if n % 2 == 1 {
                        chars.next();
                        word.push('"');
                    }
                } else {
                    word.extend(std::iter::repeat_n('\\', n));
                }
            }
            '"' => {
                started = true;
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// A POSIX shell's words: '…', "…" (with \ before $ ` " \ and a newline),
/// $'…' (C escapes), \ outside quotes, and a \ at a line's end joining
/// lines.
fn posix_words(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\r') => {
                    chars.next_if_eq(&'\n');
                }
                Some('\n') => {}
                Some(next) => {
                    word.push(next);
                    started = true;
                }
                None => {}
            },
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return Err("a ' quote isn't closed".into()),
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => word.push('\n'),
                            Some('r') => word.push('\r'),
                            Some('t') => word.push('\t'),
                            Some('x') => {
                                let hex: String = (0..2).filter_map(|_| chars.next_if(char::is_ascii_hexdigit)).collect();
                                word.push(u8::from_str_radix(&hex, 16).map(char::from).map_err(|_| "a \\x escape needs hex digits")?);
                            }
                            Some('u') => {
                                let hex: String = (0..4).filter_map(|_| chars.next_if(char::is_ascii_hexdigit)).collect();
                                let code = u32::from_str_radix(&hex, 16).map_err(|_| "a \\u escape needs hex digits")?;
                                word.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            }
                            Some(ch) => word.push(ch),
                            None => return Err("a $' quote isn't closed".into()),
                        },
                        Some(ch) => word.push(ch),
                        None => return Err("a $' quote isn't closed".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('$' | '`' | '"' | '\\')) => word.push(ch),
                            Some('\n') => {}
                            Some(ch) => {
                                word.push('\\');
                                word.push(ch);
                            }
                            None => return Err("a \" quote isn't closed".into()),
                        },
                        Some(ch) => word.push(ch),
                        None => return Err("a \" quote isn't closed".into()),
                    }
                }
            }
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    Ok(out)
}

/// curl's options that take a value, short and long.
const WITH_VALUE: &[&str] = &[
    "-X", "--request", "-H", "--header", "-d", "--data", "--data-raw", "--data-binary", "--data-ascii", "--data-urlencode",
    "--json", "-u", "--user", "-A", "--user-agent", "-e", "--referer", "-b", "--cookie", "-F", "--form", "--form-string",
    "--url", "-o", "--output", "-m", "--max-time", "--connect-timeout", "-x", "--proxy", "-T", "--upload-file", "-w",
    "--write-out", "--retry", "-c", "--cookie-jar", "--cert", "--key", "--cacert", "-r", "--range", "-E", "--resolve",
    "--max-redirs", "--limit-rate", "-K", "--config", "--oauth2-bearer", "--aws-sigv4",
];

fn curl(text: &str) -> Result<(HttpRequest, Vec<String>), String> {
    let words = words(text)?;
    // A form or an upload makes curl's method POST or PUT, even though
    // what they send isn't carried over.
    let given = |names: &[&str]| words.iter().any(|w| names.iter().any(|n| w == n || (n.len() == 2 && w.starts_with(n))));
    let (form, upload) = (given(&["-F", "--form", "--form-string"]), given(&["-T", "--upload-file"]));
    let mut args = words.into_iter().skip(1).peekable();
    let mut method: Option<String> = None;
    let mut url: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut data: Vec<String> = Vec::new();
    let mut get = false;
    let mut head = false;
    let mut notes = Vec::new();
    let mut note = |n: String| {
        if !notes.contains(&n) {
            notes.push(n);
        }
    };

    while let Some(arg) = args.next() {
        // `-XPOST`, `-sSL`, `-HName: v`: a short option's value may be
        // joined to it, and short flags without values may be grouped.
        let (flag, joined) = if arg.starts_with("--") {
            match arg.split_once('=') {
                Some((f, v)) if WITH_VALUE.contains(&f) => (f.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            }
        } else if arg.starts_with('-') && arg.len() > 2 {
            let short = &arg[..2];
            if WITH_VALUE.contains(&short) {
                (short.to_string(), Some(arg[2..].to_string()))
            } else {
                // Grouped flags: every letter but a last one that takes a value.
                let letters: Vec<char> = arg[1..].chars().collect();
                for (i, ch) in letters.iter().enumerate() {
                    let f = format!("-{ch}");
                    if WITH_VALUE.contains(&f.as_str()) {
                        let rest: String = letters[i + 1..].iter().collect();
                        let value = if rest.is_empty() { args.next() } else { Some(rest) };
                        take(&f, value, &mut method, &mut url, &mut headers, &mut data, &mut note)?;
                        break;
                    }
                    flag_only(&f, &mut get, &mut head, &mut note);
                }
                continue;
            }
        } else if arg.starts_with('-') && arg != "-" {
            (arg.clone(), None)
        } else {
            if url.is_none() {
                url = Some(arg);
            } else {
                note(format!("only the first URL is kept; `{arg}` is left out"));
            }
            continue;
        };
        if WITH_VALUE.contains(&flag.as_str()) {
            let value = joined.or_else(|| args.next());
            take(&flag, value, &mut method, &mut url, &mut headers, &mut data, &mut note)?;
        } else {
            flag_only(&flag, &mut get, &mut head, &mut note);
        }
    }

    let mut url = url.ok_or("the curl command names no URL")?;
    if !url.contains("://") {
        url = format!("http://{url}");
    }
    let mut body = (!data.is_empty()).then(|| data.join("&"));
    if get {
        if let Some(query) = body.take() {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&query);
        }
    }
    if body.is_some() && !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")) {
        // What curl sends with -d.
        headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
    }
    let method = method.unwrap_or_else(|| {
        if head {
            "HEAD".into()
        } else if upload {
            "PUT".into()
        } else if body.is_some() || form {
            "POST".into()
        } else {
            "GET".into()
        }
    });
    Ok((request(&method, url, headers, body), notes))
}

fn flag_only(flag: &str, get: &mut bool, head: &mut bool, note: &mut impl FnMut(String)) {
    match flag {
        "-G" | "--get" => *get = true,
        "-I" | "--head" => *head = true,
        "-k" | "--insecure" => note("curl's --insecure isn't carried over: to accept a host's self-signed certificate, add it to the browser behaviour's TLS hosts".into()),
        "-L" | "--location" => {}
        "--compressed" | "-s" | "--silent" | "-S" | "--show-error" | "-v" | "--verbose" | "-i" | "--include" | "-f" | "--fail"
        | "-N" | "--no-buffer" | "--http1.1" | "--http2" | "-#" | "--progress-bar" | "-O" | "--remote-name" => {}
        other => note(format!("curl's `{other}` isn't carried over")),
    }
}

#[allow(clippy::too_many_arguments)]
fn take(
    flag: &str,
    value: Option<String>,
    method: &mut Option<String>,
    url: &mut Option<String>,
    headers: &mut Vec<(String, String)>,
    data: &mut Vec<String>,
    note: &mut impl FnMut(String),
) -> Result<(), String> {
    let value = value.ok_or_else(|| format!("curl's `{flag}` needs a value"))?;
    match flag {
        "-X" | "--request" => *method = Some(value),
        "--url" => *url = Some(value),
        "-H" | "--header" => match value.split_once(':') {
            Some((name, v)) => headers.push((name.trim().to_string(), v.trim().to_string())),
            None => note(format!("the header `{value}` has no value and is left out")),
        },
        "-d" | "--data" | "--data-ascii" | "--data-binary" | "--data-raw" => {
            if value.starts_with('@') && flag != "--data-raw" {
                note(format!("`{flag} {value}` sends a file, which isn't carried over; the body is left without it"));
            } else if flag == "--data-binary" || flag == "--data-raw" {
                data.push(value);
            } else {
                // -d strips newlines, as curl does.
                data.push(value.replace(['\r', '\n'], ""));
            }
        }
        "--data-urlencode" => data.push(urlencoded(&value)),
        "--json" => {
            data.push(value);
            if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")) {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("accept")) {
                headers.push(("Accept".into(), "application/json".into()));
            }
        }
        "-u" | "--user" => {
            let pair = if value.contains(':') { value } else { format!("{value}:") };
            headers.push(("Authorization".into(), format!("Basic {}", base64(pair.as_bytes()))));
        }
        "--oauth2-bearer" => headers.push(("Authorization".into(), format!("Bearer {value}"))),
        "-A" | "--user-agent" => headers.push(("User-Agent".into(), value)),
        "-e" | "--referer" => headers.push(("Referer".into(), value)),
        "-b" | "--cookie" => {
            if value.contains('=') {
                headers.push(("Cookie".into(), value));
            } else {
                note(format!("`{flag} {value}` reads cookies from a file, which isn't carried over"));
            }
        }
        "-F" | "--form" | "--form-string" => note(format!("multipart forms aren't supported yet: `{flag} {value}` is left out")),
        "-T" | "--upload-file" => note(format!("`{flag} {value}` uploads a file, which isn't carried over")),
        _ => note(format!("curl's `{flag} {value}` isn't carried over")),
    }
    Ok(())
}

/// `--data-urlencode`'s forms: `content`, `=content`, `name=content`.
fn urlencoded(value: &str) -> String {
    let (name, content) = match value.split_once('=') {
        Some(("", content)) => (None, content),
        Some((name, content)) => (Some(name), content),
        None => (None, value),
    };
    let mut out = String::new();
    for b in content.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    match name {
        Some(name) => format!("{name}={out}"),
        None => out,
    }
}

// ---- fetch ----

/// `rest` is what follows `fetch(`: the URL, then perhaps the options.
fn fetch(rest: &str) -> Result<(HttpRequest, Vec<String>), String> {
    let mut p = Js { s: rest.as_bytes(), i: 0, text: rest };
    let url = match p.value()? {
        Value::String(url) => url,
        _ => return Err("fetch's first argument isn't a string: write the URL out".into()),
    };
    p.space();
    let options = if p.eat(b',') {
        p.space();
        if p.peek() == Some(b')') {
            Value::Null
        } else {
            p.value()?
        }
    } else {
        Value::Null
    };
    let mut notes = Vec::new();
    let mut method = "GET".to_string();
    let mut headers = Vec::new();
    let mut body = None;
    if let Value::Object(options) = options {
        for (key, value) in options {
            match (key.as_str(), value) {
                ("method", Value::String(m)) => method = m,
                ("headers", Value::Object(h)) => {
                    for (name, v) in h {
                        headers.push((name, scalar(&v)));
                    }
                }
                ("headers", Value::Array(pairs)) => {
                    for pair in pairs {
                        if let Value::Array(nv) = pair {
                            if let [n, v] = nv.as_slice() {
                                headers.push((scalar(n), scalar(v)));
                            }
                        }
                    }
                }
                ("body", Value::Null) => {}
                ("body", Value::String(b)) => body = Some(b),
                ("body", other) => body = Some(other.to_string()),
                ("credentials", Value::String(c)) if c == "include" => {
                    notes.push("`credentials: \"include\"` isn't carried over: turn on cookies in the browser behaviour to send the jar's".into())
                }
                ("mode" | "credentials" | "referrerPolicy" | "cache" | "redirect" | "keepalive" | "priority" | "integrity", _) => {}
                ("referrer", Value::String(r)) => headers.push(("Referer".into(), r)),
                (other, _) => notes.push(format!("fetch's `{other}` option isn't carried over")),
            }
        }
    }
    Ok((request(&method, url, headers, body), notes))
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Just enough of JavaScript's literals for a fetch call: strings in any
/// of the three quotes (a template's `${…}` kept as written), numbers,
/// `true`/`false`/`null`/`undefined`, arrays, objects with bare or quoted
/// keys and trailing commas, and `JSON.stringify(…)` of any of those,
/// which becomes the JSON text.
struct Js<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl Js<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn space(&mut self) {
        loop {
            while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"//") {
                while self.peek().is_some_and(|b| b != b'\n') {
                    self.i += 1;
                }
            } else if self.s[self.i..].starts_with(b"/*") {
                match self.text[self.i + 2..].find("*/") {
                    Some(end) => self.i += end + 4,
                    None => self.i = self.s.len(),
                }
            } else {
                return;
            }
        }
    }

    fn fail<T>(&self, what: &str) -> Result<T, String> {
        let near: String = self.text[self.i.min(self.text.len())..].chars().take(20).collect();
        Err(format!("{what} at `{near}`"))
    }

    fn value(&mut self) -> Result<Value, String> {
        self.space();
        match self.peek() {
            Some(q @ (b'"' | b'\'' | b'`')) => self.string(q).map(Value::String),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => {
                let word = self.word();
                match word.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" | "undefined" => Ok(Value::Null),
                    "JSON.stringify" => {
                        self.space();
                        if !self.eat(b'(') {
                            return self.fail("expected ( after JSON.stringify");
                        }
                        let inner = self.value()?;
                        self.space();
                        // JSON.stringify(x, null, 2) and the like.
                        while self.eat(b',') {
                            self.value()?;
                            self.space();
                        }
                        if !self.eat(b')') {
                            return self.fail("expected )");
                        }
                        Ok(Value::String(inner.to_string()))
                    }
                    other => self.fail(&format!("`{other}` is a name, not a value; write its value out")),
                }
            }
            _ => self.fail("expected a value"),
        }
    }

    fn word(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'.') {
            self.i += 1;
        }
        self.text[start..self.i].to_string()
    }

    fn string(&mut self, quote: u8) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(ch) = self.text[self.i..].chars().next() else { return self.fail("a string isn't closed") };
            self.i += ch.len_utf8();
            match ch {
                c if c as u32 == quote as u32 => return Ok(out),
                '\\' => {
                    let Some(e) = self.text[self.i..].chars().next() else { return self.fail("a string isn't closed") };
                    self.i += e.len_utf8();
                    match e {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex = self.text.get(self.i..self.i + 4).unwrap_or("");
                            let code = u32::from_str_radix(hex, 16).map_err(|_| "a \\u escape needs four hex digits".to_string())?;
                            self.i += 4;
                            // A surrogate pair, as JSON.stringify writes one.
                            if (0xD800..0xDC00).contains(&code) && self.text[self.i..].starts_with("\\u") {
                                let low = u32::from_str_radix(self.text.get(self.i + 2..self.i + 6).unwrap_or(""), 16).unwrap_or(0);
                                if (0xDC00..0xE000).contains(&low) {
                                    self.i += 6;
                                    out.push(char::from_u32(0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)).unwrap_or('\u{fffd}'));
                                    continue;
                                }
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        '\n' => {}
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E')) {
            self.i += 1;
        }
        match serde_json::from_str::<serde_json::Number>(&self.text[start..self.i]) {
            Ok(n) => Ok(Value::Number(n.to_string())),
            Err(_) => self.fail("not a number"),
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.space();
            if self.eat(b']') {
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.space();
            if !self.eat(b',') {
                self.space();
                if self.eat(b']') {
                    return Ok(Value::Array(items));
                }
                return self.fail("expected , or ]");
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.i += 1;
        let mut map: Vec<(String, Value)> = Vec::new();
        loop {
            self.space();
            if self.eat(b'}') {
                return Ok(Value::Object(map));
            }
            let key = match self.peek() {
                Some(q @ (b'"' | b'\'' | b'`')) => self.string(q)?,
                Some(b) if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' => self.word(),
                _ => return self.fail("expected a key"),
            };
            self.space();
            if !self.eat(b':') {
                return self.fail("expected :");
            }
            let value = self.value()?;
            map.retain(|(k, _)| *k != key);
            map.push((key, value));
            self.space();
            if !self.eat(b',') {
                self.space();
                if self.eat(b'}') {
                    return Ok(Value::Object(map));
                }
                return self.fail("expected , or }");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(r: &HttpRequest, name: &str) -> Option<String> {
        r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }

    #[test]
    fn a_browsers_bash_curl() {
        let text = "curl 'https://api.example.com/v1/users?page=2' \\\n  -H 'accept: application/json' \\\n  -H 'authorization: Bearer abc' \\\n  --data-raw $'{\"name\":\"it\\'s\"}' \\\n  --compressed";
        let (r, notes) = import(text).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://api.example.com/v1/users?page=2");
        assert_eq!(h(&r, "authorization").as_deref(), Some("Bearer abc"));
        assert_eq!(r.body.as_deref(), Some(r#"{"name":"it's"}"#));
        assert_eq!(h(&r, "content-type").as_deref(), Some("application/x-www-form-urlencoded"));
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn short_flags_joined_and_grouped() {
        let (r, notes) = import(r#"curl -sSL -XPUT -H"X-A: 1" -u ada:pw "http://localhost:8080/x" -d "a=1" -d b=2 -k"#).unwrap();
        assert_eq!(r.method, "PUT");
        assert_eq!(h(&r, "x-a").as_deref(), Some("1"));
        assert_eq!(h(&r, "authorization").as_deref(), Some("Basic YWRhOnB3"));
        assert_eq!(r.body.as_deref(), Some("a=1&b=2"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("--insecure"));
    }

    #[test]
    fn get_moves_data_to_the_query_and_json_sets_headers() {
        let (r, _) = import("curl -G https://x.test/search?a=1 --data-urlencode 'q=hello world' -d n=5").unwrap();
        assert_eq!((r.method.as_str(), r.url.as_str(), r.body.as_deref()), ("GET", "https://x.test/search?a=1&q=hello%20world&n=5", None));
        let (r, _) = import(r#"curl --json '{"a":1}' x.test/api"#).unwrap();
        assert_eq!((r.method.as_str(), r.url.as_str()), ("POST", "http://x.test/api"));
        assert_eq!(h(&r, "content-type").as_deref(), Some("application/json"));
        assert_eq!(h(&r, "accept").as_deref(), Some("application/json"));
        let (r, notes) = import("curl -F file=@a.png https://x.test/up").unwrap();
        assert_eq!((r.method.as_str(), r.body.as_deref()), ("POST", None));
        assert_eq!(import("curl -T a.txt https://x.test/up").unwrap().0.method, "PUT");
        assert!(notes[0].contains("multipart"), "{notes:?}");
    }

    #[test]
    fn windows_cmd_curl() {
        let text = "curl ^\"https://api.example.com/x^\" ^\r\n  -H ^\"accept: */*^\" ^\r\n  --data-raw ^\"^{^\\^\"a^\\^\":1^}^\"";
        let (r, _) = import(text).unwrap();
        assert_eq!(r.url, "https://api.example.com/x");
        assert_eq!(h(&r, "accept").as_deref(), Some("*/*"));
        assert_eq!(r.body.as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn a_browsers_fetch() {
        let text = r#"fetch("https://api.example.com/v1/users", {
  "headers": {
    "accept": "application/json",
    "content-type": "application/json"
  },
  "referrer": "https://app.example.com/",
  "body": "{\"name\":\"Ada\"}",
  "method": "POST",
  "mode": "cors",
  "credentials": "include"
});"#;
        let (r, notes) = import(text).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body.as_deref(), Some(r#"{"name":"Ada"}"#));
        assert_eq!(h(&r, "referer").as_deref(), Some("https://app.example.com/"));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("credentials"));
    }

    #[test]
    fn a_hand_written_fetch() {
        let text = "const res = await fetch('http://localhost:3000/todos', {\n  method: 'PATCH', // edit\n  headers: { Authorization: `Bearer ${token}`, },\n  body: JSON.stringify({ done: true, tags: ['a', \"b\"], n: -1.5 }),\n});";
        let (r, _) = import(text).unwrap();
        assert_eq!(r.method, "PATCH");
        assert_eq!(h(&r, "authorization").as_deref(), Some("Bearer ${token}"));
        assert_eq!(r.body.as_deref(), Some(r#"{"done":true,"tags":["a","b"],"n":-1.5}"#));
        let (r, _) = import("fetch(\"https://x.test/\")").unwrap();
        assert_eq!((r.method.as_str(), r.body.as_deref()), ("GET", None));
    }

    #[test]
    fn neither_is_an_error() {
        assert!(import("wget https://x.test").is_err());
        assert!(import("curl -H 'a: b'").unwrap_err().contains("no URL"));
        assert!(import("fetch(url)").unwrap_err().contains("not a value"));
    }
}
