//! Saved requests: one `.http` file each under the workspace's `requests/`,
//! folders being collections.
//!
//! The format is the plain one VS Code's REST Client and JetBrains read, so
//! the files diff well in git and an agent can write them by hand:
//!
//! ```text
//! GET https://jsonplaceholder.typicode.com/albums?id=1
//! Accept: application/json
//!
//! {"optional": "body"}
//! ```
//!
//! A request's path is what clients use to name it: relative to `requests/`,
//! with `/` between folders and no extension, such as `albums/get album`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracer_protocol::{HttpRequest, SavedRequest, REQUESTS_DIR};

/// `path` as the core names it, or why it can't be one. Accepts `\` for `/`,
/// and a leading `requests/` or trailing `.http`, which people type.
pub fn normalize(path: &str) -> Result<String, String> {
    let mut p = path.trim().replace('\\', "/");
    if let Some(rest) = p.strip_prefix(&format!("{REQUESTS_DIR}/")) {
        p = rest.to_string();
    }
    if let Some(rest) = p.strip_suffix(".http") {
        p = rest.to_string();
    }
    let mut parts = Vec::new();
    for part in p.split('/') {
        let bad = part.is_empty()
            || part == "."
            || part == ".."
            || part != part.trim()
            || part.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c));
        if bad {
            return Err(format!("`{path}` is not a request path: write folders and a name, such as `albums/get album`"));
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

pub fn file_of(workspace: &Path, path: &str) -> PathBuf {
    workspace.join(REQUESTS_DIR).join(format!("{path}.http"))
}

pub fn write(workspace: &Path, path: &str, request: &HttpRequest) -> io::Result<()> {
    let file = file_of(workspace, path);
    fs::create_dir_all(file.parent().expect("a request file is inside requests/"))?;
    // Written beside it and renamed over it, so a reader never sees half.
    let partial = file.with_extension("http.partial");
    fs::write(&partial, to_text(request))?;
    fs::rename(&partial, &file)
}

/// Remove a saved request, and any folders it leaves empty.
pub fn remove(workspace: &Path, path: &str) -> io::Result<()> {
    let file = file_of(workspace, path);
    fs::remove_file(&file)?;
    let root = workspace.join(REQUESTS_DIR);
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Every saved request, by path. A file that doesn't parse is left out and
/// named in `skipped`, with why.
pub fn list(workspace: &Path) -> (Vec<SavedRequest>, Vec<String>) {
    let root = workspace.join(REQUESTS_DIR);
    let (mut found, mut skipped) = (Vec::new(), Vec::new());
    walk(&root, &root, &mut found, &mut skipped);
    found.sort_by(|a: &SavedRequest, b| a.path.cmp(&b.path));
    (found, skipped)
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<SavedRequest>, skipped: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let file = entry.path();
        if file.is_dir() {
            walk(root, &file, found, skipped);
            continue;
        }
        if file.extension().and_then(|e| e.to_str()) != Some("http") {
            continue;
        }
        let rel = file.strip_prefix(root).unwrap_or(&file).with_extension("");
        let path = rel.to_string_lossy().replace('\\', "/");
        match fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|text| from_text(&text)) {
            Ok(request) => found.push(SavedRequest { path, request }),
            Err(why) => skipped.push(format!("{path}: {why}")),
        }
    }
}

pub fn to_text(request: &HttpRequest) -> String {
    let mut out = format!("{} {}\n", request.method, request.url);
    for (name, value) in &request.headers {
        out.push_str(&format!("{name}: {value}\n"));
    }
    if let Some(body) = &request.body {
        out.push('\n');
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// One request from a `.http` file. Blank lines and `#` or `//` comments may
/// come first; a `###` line ends the request, since this format allows
/// several to a file and Tracer keeps the first.
pub fn from_text(text: &str) -> Result<HttpRequest, String> {
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.peek() {
        let t = line.trim();
        if t.is_empty() || (t.starts_with('#') && !t.starts_with("###")) || t.starts_with("//") {
            lines.next();
        } else {
            break;
        }
    }
    let first = lines.next().ok_or("the file holds no request")?;
    // `METHOD URL`, or `METHOD URL HTTP/1.1` as the format allows.
    let words: Vec<&str> = first.split_whitespace().collect();
    let (method, url) = match words.as_slice() {
        [m, u] => (*m, *u),
        [m, u, v] if v.starts_with("HTTP/") => (*m, *u),
        _ => return Err(format!("`{first}` is not `METHOD URL`")),
    };
    const METHODS: [&str; 9] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT"];
    if !METHODS.contains(&method.to_uppercase().as_str()) {
        return Err(format!("`{method}` is not an HTTP method"));
    }

    let mut headers = Vec::new();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        if line.trim_start().starts_with("###") {
            return Ok(HttpRequest { method: method.to_uppercase(), url: url.to_string(), headers, body: None });
        }
        let (name, value) = line.split_once(':').ok_or_else(|| format!("`{line}` is not a `Name: value` header"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let body: Vec<&str> = lines.take_while(|l| !l.trim_start().starts_with("###")).collect();
    let body = body.join("\n");
    let body = body.trim_end_matches(['\n', '\r']);
    Ok(HttpRequest {
        method: method.to_uppercase(),
        url: url.to_string(),
        headers,
        body: if body.is_empty() { None } else { Some(body.to_string()) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_through_its_file() {
        let request = HttpRequest {
            method: "POST".into(),
            url: "https://x.test/albums?id=1".into(),
            headers: vec![("Accept".into(), "application/json".into()), ("X-Note".into(), "a: b".into())],
            body: Some("{\n  \"title\": \"t\"\n}".into()),
        };
        let text = to_text(&request);
        assert_eq!(text, "POST https://x.test/albums?id=1\nAccept: application/json\nX-Note: a: b\n\n{\n  \"title\": \"t\"\n}\n");
        assert_eq!(from_text(&text).unwrap(), request);
    }

    #[test]
    fn comments_lead_and_a_separator_ends_it() {
        let text = "# Get one\n// by id\n\nget https://x.test/a\n\n###\nPOST https://x.test/b\n";
        let request = from_text(text).unwrap();
        assert_eq!((request.method.as_str(), request.url.as_str(), request.body), ("GET", "https://x.test/a", None));
    }

    #[test]
    fn paths_are_checked_and_tidied() {
        assert_eq!(normalize("requests\\albums\\get one.http").unwrap(), "albums/get one");
        for bad in ["", "../etc", "a//b", "a/./b", "a /b", "a?b"] {
            assert!(normalize(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
