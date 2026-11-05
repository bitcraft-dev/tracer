//! A data file for a data-driven flow: rows of named values, each row
//! filling `{{name}}` for one run of the flow.
//!
//! - **CSV**: the first line names the columns; fields may be quoted
//!   (`"a, b"`, `""` for a quote, line breaks inside quotes), and a line
//!   left blank is skipped.
//! - **JSON**: an array of objects. A string value is its text; a number,
//!   a `true` or a nested value is its JSON; `null` is "".

use std::fs;
use std::path::Path;

use serde_json::Value;

/// One row: its values by column name, in the file's order.
pub type Row = Vec<(String, String)>;

/// The rows of `file`, a path inside the workspace (`data/users.csv`).
pub fn read(workspace: &Path, file: &str) -> Result<Vec<Row>, String> {
    let rel = file.trim().replace('\\', "/");
    let rel = rel.trim_start_matches("./");
    if rel.is_empty() || rel.starts_with('/') || rel.contains(':') || rel.split('/').any(|p| p == "..") {
        return Err(format!("`{file}` isn't a path inside the workspace"));
    }
    let text = fs::read_to_string(workspace.join(rel)).map_err(|e| format!("could not read `{rel}`: {e}"))?;
    let rows = if rel.to_ascii_lowercase().ends_with(".json") { json(&text) } else { csv(&text) }.map_err(|e| format!("{rel}: {e}"))?;
    if rows.is_empty() {
        return Err(format!("{rel} has no rows"));
    }
    Ok(rows)
}

fn json(text: &str) -> Result<Vec<Row>, String> {
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Array(items) = value else { return Err("isn't an array of objects".into()) };
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| match item {
            Value::Object(map) => Ok(map
                .into_iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) => s,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    (k, v)
                })
                .collect()),
            _ => Err(format!("item {} isn't an object", i + 1)),
        })
        .collect()
}

fn csv(text: &str) -> Result<Vec<Row>, String> {
    let records = records(text.trim_start_matches('\u{feff}'))?;
    let mut records = records.into_iter();
    let header = records.next().ok_or("is empty")?;
    let names: Vec<String> = header.iter().map(|h| h.trim().to_string()).collect();
    if names.iter().any(String::is_empty) {
        return Err("a column in the first line has no name".into());
    }
    let mut rows = Vec::new();
    for (i, record) in records.enumerate() {
        if record.len() == 1 && record[0].trim().is_empty() {
            continue;
        }
        if record.len() != names.len() {
            return Err(format!("row {} has {} fields, the header {}", i + 1, record.len(), names.len()));
        }
        rows.push(names.iter().cloned().zip(record).collect());
    }
    Ok(rows)
}

/// RFC 4180's records: comma-separated fields, double quotes around a
/// field that holds commas, quotes (doubled) or line breaks.
fn records(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut out = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => record.push(std::mem::take(&mut field)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                record.push(std::mem::take(&mut field));
                out.push(std::mem::take(&mut record));
                any = false;
            }
            c => field.push(c),
        }
    }
    if quoted {
        return Err("a quoted field isn't closed".into());
    }
    if any {
        record.push(field);
        out.push(record);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_with_quotes_line_breaks_and_blank_lines() {
        let text = "\u{feff}name,email,note\r\nAda,ada@x.test,\"says \"\"hi\"\", then, leaves\"\r\n\r\nGrace,grace@x.test,\"two\nlines\"\n";
        let rows = csv(text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![("name".into(), "Ada".into()), ("email".into(), "ada@x.test".into()), ("note".into(), "says \"hi\", then, leaves".into())]);
        assert_eq!(rows[1][2].1, "two\nlines");
        assert!(csv("a,b\n1\n").unwrap_err().contains("row 1 has 1 fields"));
        assert!(csv("a,b\n\"1,2\n").unwrap_err().contains("isn't closed"));
    }

    #[test]
    fn json_rows() {
        let rows = json(r#"[{"id": 1, "name": "Ada", "admin": true, "tags": ["a"], "boss": null}]"#).unwrap();
        assert_eq!(
            rows[0],
            vec![
                ("admin".into(), "true".into()),
                ("boss".into(), "".into()),
                ("id".into(), "1".into()),
                ("name".into(), "Ada".into()),
                ("tags".into(), "[\"a\"]".into())
            ]
        );
        assert!(json("{}").is_err());
        assert!(json("[1]").unwrap_err().contains("item 1"));
    }

    #[test]
    fn only_inside_the_workspace() {
        let dir = std::env::temp_dir();
        assert!(read(&dir, "../x.csv").unwrap_err().contains("inside the workspace"));
        assert!(read(&dir, "C:/x.csv").unwrap_err().contains("inside the workspace"));
        assert!(read(&dir, "/etc/passwd").unwrap_err().contains("inside the workspace"));
    }
}
