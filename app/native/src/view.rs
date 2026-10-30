//! The response viewer: a body as numbered lines the page can colour and
//! fold.
//!
//! The page asks for a finished request's body by id. The whole body comes
//! from the core (events carry only the first 64 KiB), is laid out once and
//! kept for the request on show, and each ask gives the lines left visible
//! by the folds, up to a limit: a page draws a thousand lines well, not a
//! hundred thousand.
//!
//! JSON is pretty-printed from its own text, so keys keep their order and
//! numbers read exactly as sent. Anything else is shown as its lines.

use std::sync::{Arc, Mutex};

use tracer_protocol::{Command, ReplyBody};

use crate::tracer::{shared, CoreError};

#[rux::export]
#[derive(Clone)]
pub struct BodyLine {
    /// 1, 2, 3, … over the whole laid-out body, folded or not.
    pub no: u32,
    pub indent: u32,
    /// `"name": `, or "" on a line with no key.
    pub key: String,
    /// The value, bracket or text, with its trailing comma.
    pub value: String,
    /// `string`, `number`, `bool`, `null`, `punct` or `plain`: its colour.
    pub kind: String,
    /// How many lines folding this one hides; 0 if it doesn't fold.
    pub folds: u32,
    pub folded: bool,
    /// What a folded line stands for, such as `3 keys`.
    pub note: String,
}

#[rux::export]
#[derive(Clone)]
pub struct BodyView {
    pub request_id: u64,
    /// The body parsed as JSON; otherwise it is shown as text.
    pub json: bool,
    pub lines: Vec<BodyLine>,
    /// Lines the folds leave visible, of which `lines` are the first.
    pub visible: u32,
    /// Lines in all, nothing folded.
    pub total: u32,
}

/// One laid-out line, before folding.
struct Line {
    indent: u32,
    key: String,
    value: String,
    kind: &'static str,
    /// For an opening bracket: the index of its closing line, and how many
    /// entries it holds.
    close: Option<(usize, usize)>,
}

/// The request on show and its layout, so folding doesn't fetch again.
static LAID_OUT: Mutex<Option<(u64, Arc<(bool, Vec<Line>)>)>> = Mutex::new(None);

/// Request `request_id`'s body, with the lines numbered in `folded` folded,
/// at most `limit` lines of it.
#[rux::export]
pub async fn body_view(request_id: u64, folded: Vec<u32>, limit: u32) -> Result<BodyView, CoreError> {
    let cached = LAID_OUT.lock().unwrap().as_ref().filter(|(id, _)| *id == request_id).map(|(_, l)| l.clone());
    let laid = match cached {
        Some(laid) => laid,
        None => {
            let body = match shared()?.call(Command::Response { request_id }) {
                Ok(Ok(ReplyBody::Response { response: Some(r), .. })) => r.body,
                Ok(Ok(ReplyBody::Response { response: None, .. })) => {
                    return Err(CoreError(format!("request #{request_id} has no response yet")))
                }
                Ok(Ok(other)) => return Err(CoreError(format!("unexpected answer: {other:?}"))),
                Ok(Err(e)) => return Err(CoreError(e)),
                Err(e) => return Err(CoreError(e.to_string())),
            };
            let laid = Arc::new(lay_out(&body));
            *LAID_OUT.lock().unwrap() = Some((request_id, laid.clone()));
            laid
        }
    };
    let (json, lines) = (&laid.0, &laid.1);
    Ok(BodyView { request_id, json: *json, total: lines.len() as u32, ..visible(lines, &folded, limit) })
}

/// The lines `folded` leaves visible, at most `limit` of them, and how many
/// there are in all.
fn visible(lines: &[Line], folded: &[u32], limit: u32) -> BodyView {
    let mut out = Vec::new();
    let mut count = 0u32;
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let no = i as u32 + 1;
        let fold = line.close;
        let is_folded = fold.is_some() && folded.contains(&no);
        count += 1;
        if out.len() < limit as usize {
            let (value, note) = match (is_folded, fold) {
                (true, Some((close, n))) => {
                    let comma = if lines[close].value.ends_with(',') { "," } else { "" };
                    let (open, shut, what) = if line.value.starts_with('{') { ("{", "}", "key") } else { ("[", "]", "item") };
                    let plural = if n == 1 { "" } else { "s" };
                    (format!("{open} … {shut}{comma}"), format!("{n} {what}{plural}"))
                }
                _ => (line.value.clone(), String::new()),
            };
            out.push(BodyLine {
                no,
                indent: line.indent,
                key: line.key.clone(),
                value,
                kind: line.kind.to_string(),
                folds: fold.map_or(0, |(close, _)| (close - i) as u32),
                folded: is_folded,
                note,
            });
        }
        i = match (is_folded, fold) {
            (true, Some((close, _))) => close + 1,
            _ => i + 1,
        };
    }
    BodyView { request_id: 0, json: false, lines: out, visible: count, total: 0 }
}

/// The body as lines: pretty-printed JSON if it is JSON, else its own lines.
fn lay_out(body: &str) -> (bool, Vec<Line>) {
    let mut p = Parser { s: body.as_bytes(), at: 0, lines: Vec::new() };
    p.space();
    let parsed = p.value(0, String::new(), true).and_then(|_| {
        p.space();
        if p.at == p.s.len() { Ok(()) } else { Err(()) }
    });
    if parsed.is_ok() && !p.lines.is_empty() {
        return (true, p.lines);
    }
    let lines = body
        .lines()
        .map(|l| Line { indent: 0, key: String::new(), value: l.to_string(), kind: "plain", close: None })
        .collect();
    (false, lines)
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    lines: Vec<Line>,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.at < self.s.len() && self.s[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }

    fn text(&self, from: usize) -> String {
        String::from_utf8_lossy(&self.s[from..self.at]).into_owned()
    }

    /// A string's text, quotes and escapes as written.
    fn string(&mut self) -> Result<String, ()> {
        let from = self.at;
        self.at += 1;
        loop {
            match self.peek().ok_or(())? {
                b'"' => break,
                b'\\' => self.at += 2,
                b'\n' => return Err(()),
                _ => self.at += 1,
            }
        }
        self.at += 1;
        Ok(self.text(from))
    }

    /// One value, keyed or not, as lines; `last` leaves off its comma.
    fn value(&mut self, indent: u32, key: String, last: bool) -> Result<(), ()> {
        let comma = if last { "" } else { "," };
        let (open, shut) = match self.peek().ok_or(())? {
            b'{' => (b'{', b'}'),
            b'[' => (b'[', b']'),
            b'"' => {
                let s = self.string()?;
                self.lines.push(Line { indent, key, value: format!("{s}{comma}"), kind: "string", close: None });
                return Ok(());
            }
            _ => {
                let from = self.at;
                while self.peek().is_some_and(|c| !b",]}: \t\r\n".contains(&c)) {
                    self.at += 1;
                }
                let word = self.text(from);
                let kind = match word.as_str() {
                    "true" | "false" => "bool",
                    "null" => "null",
                    w if w.parse::<f64>().is_ok() && !w.starts_with('+') && !w.starts_with('.') => "number",
                    _ => return Err(()),
                };
                self.lines.push(Line { indent, key, value: format!("{word}{comma}"), kind, close: None });
                return Ok(());
            }
        };
        self.at += 1;
        self.space();
        let (open_c, shut_c) = (open as char, shut as char);
        if self.peek() == Some(shut) {
            self.at += 1;
            self.lines.push(Line { indent, key, value: format!("{open_c}{shut_c}{comma}"), kind: "punct", close: None });
            return Ok(());
        }
        let at_open = self.lines.len();
        self.lines.push(Line { indent, key, value: open_c.to_string(), kind: "punct", close: None });
        let mut entries = 0;
        loop {
            self.space();
            let key = if open == b'{' {
                if self.peek() != Some(b'"') {
                    return Err(());
                }
                let k = self.string()?;
                self.space();
                if self.peek() != Some(b':') {
                    return Err(());
                }
                self.at += 1;
                self.space();
                format!("{k}: ")
            } else {
                String::new()
            };
            // Whether this is the last entry is known only after it, so its
            // comma goes on afterwards.
            self.value(indent + 1, key, true)?;
            entries += 1;
            self.space();
            match self.peek().ok_or(())? {
                b',' => {
                    self.at += 1;
                    // On the entry's last line: its value, or its closing
                    // bracket.
                    self.lines.last_mut().expect("an entry has a line").value.push(',');
                }
                c if c == shut => {
                    self.at += 1;
                    break;
                }
                _ => return Err(()),
            }
        }
        let close = self.lines.len();
        self.lines.push(Line { indent, key: String::new(), value: format!("{shut_c}{comma}"), kind: "punct", close: None });
        self.lines[at_open].close = Some((close, entries));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(body: &str, folded: &[u32]) -> Vec<String> {
        let (_, lines) = lay_out(body);
        visible(&lines, folded, 100)
            .lines
            .into_iter()
            .map(|l| format!("{}{}{}{}", "  ".repeat(l.indent as usize), l.key, l.value, if l.note.is_empty() { String::new() } else { format!(" ({})", l.note) }))
            .collect()
    }

    #[test]
    fn json_is_laid_out_in_its_own_order_and_words() {
        let body = r#"{"z":1.50,"a":[true,null,"x\"y"],"e":{},"n":-2e3}"#;
        assert_eq!(
            shown(body, &[]),
            ["{", "  \"z\": 1.50,", "  \"a\": [", "    true,", "    null,", "    \"x\\\"y\"", "  ],", "  \"e\": {},", "  \"n\": -2e3", "}"]
        );
    }

    #[test]
    fn a_fold_hides_what_it_holds_and_says_how_much() {
        let body = r#"{"a":[1,2,3],"b":{"c":{"d":1}}}"#;
        assert_eq!(shown(body, &[2]), ["{", "  \"a\": [ … ], (3 items)", "  \"b\": {", "    \"c\": {", "      \"d\": 1", "    }", "  }", "}"]);
        assert_eq!(shown(body, &[1]), ["{ … } (2 keys)"]);
        // Numbers are the unfolded ones, so an inner fold stays put.
        assert_eq!(shown(body, &[8, 9]), ["{", "  \"a\": [", "    1,", "    2,", "    3", "  ],", "  \"b\": {", "    \"c\": { … } (1 key)", "  }", "}"]);
    }

    #[test]
    fn a_limit_counts_what_is_visible() {
        let (_, lines) = lay_out("[1,2,3,4,5]");
        let v = visible(&lines, &[], 3);
        assert_eq!((v.lines.len(), v.visible), (3, 7));
        assert_eq!(v.lines[0].folds, 6);
    }

    #[test]
    fn what_isnt_json_is_shown_as_its_lines() {
        for body in ["<html>\n<body>", "{\"a\":1", "{\"a\":1} trailing", "nope", "{a:1}"] {
            let (json, _) = lay_out(body);
            assert!(!json, "{body}");
        }
        assert_eq!(shown("one\ntwo", &[]), ["one", "two"]);
    }
}
