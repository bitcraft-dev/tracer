//! Probe 2: a large response body, as lines the page can colour.
//!
//! A real response viewer shows tens of thousands of lines. This builds a
//! JSON array of `records` objects, pretty-printed, already split into
//! tokens, so the page's cost is only rendering it.

#[rux::export]
pub struct JsonLine {
    pub no: u32,
    pub indent: u32,
    /// The key with its quotes, or "" for a line with no key.
    pub key: String,
    /// What follows the key: a value, a bracket, or both.
    pub value: String,
    /// "string", "number", "bool", "null" or "punct": the value's colour.
    pub kind: String,
}

fn line(no: &mut u32, indent: u32, key: &str, value: &str, kind: &str) -> JsonLine {
    *no += 1;
    JsonLine {
        no: *no,
        indent,
        key: key.to_string(),
        value: value.to_string(),
        kind: kind.to_string(),
    }
}

/// The same lines as [`sample`], returned at once rather than awaited, to
/// tell an `async` export's cost from a native value's.
#[rux::export]
pub fn sample_now(records: u32) -> Vec<JsonLine> {
    build(records)
}

/// `records` objects of eight fields each: ten lines per record.
#[rux::export]
pub async fn sample(records: u32) -> Vec<JsonLine> {
    build(records)
}

fn build(records: u32) -> Vec<JsonLine> {
    let mut no = 0;
    let mut out = Vec::with_capacity(records as usize * 10 + 2);
    out.push(line(&mut no, 0, "", "[", "punct"));
    for i in 0..records {
        out.push(line(&mut no, 1, "", "{", "punct"));
        out.push(line(&mut no, 2, "\"id\": ", &format!("{i},"), "number"));
        out.push(line(&mut no, 2, "\"albumId\": ", &format!("{},", i / 50 + 1), "number"));
        out.push(line(&mut no, 2, "\"title\": ", &format!("\"record number {i} of {records}\","), "string"));
        out.push(line(&mut no, 2, "\"url\": ", &format!("\"https://example.test/600/{i:06x}\","), "string"));
        out.push(line(&mut no, 2, "\"score\": ", &format!("{:.3},", (i as f64 * 0.7).sin()), "number"));
        out.push(line(&mut no, 2, "\"active\": ", if i % 3 == 0 { "true," } else { "false," }, "bool"));
        out.push(line(&mut no, 2, "\"parent\": ", "null,", "null"));
        out.push(line(&mut no, 2, "\"tag\": ", &format!("\"t{}\"", i % 7), "string"));
        let close = if i + 1 == records { "}" } else { "}," };
        out.push(line(&mut no, 1, "", close, "punct"));
    }
    out.push(line(&mut no, 0, "", "]", "punct"));
    out
}
