//! A request as code that sends it: a shell's `curl`, JavaScript's
//! `fetch`, Python's `requests`, or a Rux app's native export.
//!
//! Variables are filled from the environment given, if any; a secret, or a
//! variable it doesn't define, stays `{{name}}` for whoever runs the code
//! to fill, so no secret value leaves the core this way. A GraphQL request
//! is exported as the JSON POST it's sent as.

use tracer_protocol::{Browser, Environment, ExportLang, HttpRequest};

use crate::{graphql, http};

/// `request` (as written) in `lang`, its variables filled from `env`. A
/// host `browser` trusts without checking its certificate is trusted so in
/// the code too, where the language lets it be.
pub fn export(request: &HttpRequest, env: Option<&Environment>, lang: ExportLang, browser: &Browser) -> Result<String, String> {
    let request = if graphql::is_graphql(request) { graphql::to_http(request)? } else { request.clone() };
    let fill = |text: &str| fill_known(text, env);
    let method = request.method.trim().to_ascii_uppercase();
    let url = fill(request.url.trim());
    let headers: Vec<(String, String)> = request.headers.iter().map(|(n, v)| (fill(n), fill(v))).collect();
    let body = request.body.as_deref().map(fill).filter(|b| !b.is_empty());
    let unchecked_tls = http::tls_unchecked(browser, &url);
    let request = Plain { method, url, headers, body, unchecked_tls };
    Ok(match lang {
        ExportLang::Curl => curl(&request),
        ExportLang::Fetch => fetch(&request),
        ExportLang::Python => python(&request),
        ExportLang::Rux => rux(&request),
    })
}

/// A request with its variables filled: what the code sends.
struct Plain {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    /// The host's certificate isn't to be checked.
    unchecked_tls: bool,
}

/// `text` with each `{{name}}` that `env` defines as a variable filled; a
/// secret's, or one it doesn't define, is left as written.
fn fill_known(text: &str, env: Option<&Environment>) -> String {
    let Some(env) = env else { return text.to_string() };
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else { break };
        let whole = &rest[start..start + 4 + len];
        let name = rest[start + 2..start + 2 + len].trim();
        out.push_str(&rest[..start]);
        let value = (!env.secrets.iter().any(|s| s == name)).then(|| env.vars.iter().find(|(k, _)| k == name)).flatten();
        out.push_str(value.map_or(whole, |(_, v)| v.as_str()));
        rest = &rest[start + 4 + len..];
    }
    out.push_str(rest);
    out
}

/// In single quotes for a POSIX shell: each `'` becomes `'"'"'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

fn curl(r: &Plain) -> String {
    let mut parts = vec!["curl".to_string()];
    if r.unchecked_tls {
        parts.push("--insecure".into());
    }
    if r.method != "GET" {
        parts.push(format!("-X {}", r.method));
    }
    parts.push(shell_quote(&r.url));
    for (name, value) in &r.headers {
        parts.push(format!("-H {}", shell_quote(&format!("{name}: {value}"))));
    }
    if let Some(body) = &r.body {
        parts.push(format!("--data-raw {}", shell_quote(body)));
    }
    parts.join(" ")
}

/// A JSON string, which is also a JavaScript and a Python string literal.
fn string(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

fn fetch(r: &Plain) -> String {
    let mut out = String::new();
    if r.unchecked_tls {
        out.push_str("// The workspace takes this host's certificate unchecked; in Node, so does\n// NODE_TLS_REJECT_UNAUTHORIZED=0 (for the whole process). A browser won't.\n");
    }
    out.push_str(&format!("const response = await fetch({}, {{\n  method: {},\n", string(&r.url), string(&r.method)));
    if !r.headers.is_empty() {
        out.push_str("  headers: {\n");
        for (name, value) in &r.headers {
            out.push_str(&format!("    {}: {},\n", string(name), string(value)));
        }
        out.push_str("  },\n");
    }
    if let Some(body) = &r.body {
        out.push_str(&format!("  body: {},\n", string(body)));
    }
    out.push_str("});\nconsole.log(response.status, await response.text());\n");
    out
}

fn python(r: &Plain) -> String {
    let mut out = format!("import requests\n\nresponse = requests.request(\n    {},\n    {},\n", string(&r.method), string(&r.url));
    if !r.headers.is_empty() {
        out.push_str("    headers={\n");
        for (name, value) in &r.headers {
            out.push_str(&format!("        {}: {},\n", string(name), string(value)));
        }
        out.push_str("    },\n");
    }
    if let Some(body) = &r.body {
        // Bytes, so a body that isn't Latin-1 is sent as UTF-8.
        out.push_str(&format!("    data={}.encode(\"utf-8\"),\n", string(body)));
    }
    if r.unchecked_tls {
        out.push_str("    verify=False,  # the workspace takes this host's certificate unchecked\n");
    }
    out.push_str(")\nprint(response.status_code, response.text)\n");
    out
}

/// Rux script has no HTTP of its own: an app sends from its native crate.
/// So the export is a Rust function the page awaits, and the page's call.
fn rux(r: &Plain) -> String {
    let mut builder = format!("    let request = ureq::http::Request::builder()\n        .method({:?})\n        .uri({:?})", r.method, r.url);
    for (name, value) in &r.headers {
        builder.push_str(&format!("\n        .header({name:?}, {value:?})"));
    }
    let body = match &r.body {
        Some(body) => format!("{body:?}.as_bytes().to_vec()"),
        None => "Vec::new()".into(),
    };
    builder.push_str(&format!("\n        .body({body})\n        .map_err(|e| SendError(e.to_string()))?;"));
    // The workspace takes this host's certificate unchecked, so the code does too.
    let tls = if r.unchecked_tls { "\n            .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())" } else { "" };
    format!(
        r#"// native/src/api.rs, with `pub mod api;` in native/src/lib.rs and
// `ureq = "3"` under [dependencies] in native/Cargo.toml.
use std::fmt;

#[rux::export]
#[derive(Clone)]
pub struct Answer {{
    pub status: u32,
    pub body: String,
}}

#[derive(Debug)]
pub struct SendError(String);

impl fmt::Display for SendError {{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {{
        f.write_str(&self.0)
    }}
}}

/// {method} {url}
#[rux::export]
pub async fn send_request() -> Result<Answer, SendError> {{
{builder}
    // An error status is an answer, not an error.
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false){tls}
            .build(),
    );
    let mut response = agent.run(request).map_err(|e| SendError(e.to_string()))?;
    let status = response.status().as_u16() as u32;
    let body = response.body_mut().read_to_string().map_err(|e| SendError(e.to_string()))?;
    Ok(Answer {{ status, body }})
}}

// app.rux, in the page's script:
//
//   let status = signal(0);
//   let answer = signal("");
//
//   async fn send() {{
//     try {{
//       let a = await api.sendRequest();
//       status = a.status;
//       answer = a.body;
//     }} catch e {{
//       answer = e.message;
//     }}
//   }}
"#,
        method = r.method,
        url = r.url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post() -> HttpRequest {
        HttpRequest {
            method: "post".into(),
            url: "{{base}}/users?q=it's".into(),
            headers: vec![("Content-Type".into(), "application/json".into()), ("Authorization".into(), "Bearer {{token}}".into())],
            body: Some(r#"{"name":"Ada","note":"it's \"quoted\""}"#.into()),
            expect: vec![],
            capture: vec![],
        }
    }

    fn env() -> Environment {
        Environment {
            name: "local".into(),
            vars: vec![("base".into(), "http://localhost:8080".into()), ("token".into(), "shadowed".into())],
            secrets: vec!["token".into()],
        }
    }

    #[test]
    fn variables_are_filled_and_secrets_left_named() {
        let out = export(&post(), Some(&env()), ExportLang::Curl, &Browser::default()).unwrap();
        assert_eq!(
            out,
            r#"curl -X POST 'http://localhost:8080/users?q=it'"'"'s' -H 'Content-Type: application/json' -H 'Authorization: Bearer {{token}}' --data-raw '{"name":"Ada","note":"it'"'"'s \"quoted\""}'"#
        );
        // Without an environment, everything stays as written.
        assert!(export(&post(), None, ExportLang::Curl, &Browser::default()).unwrap().contains("'{{base}}/users"));
    }

    #[test]
    fn fetch_and_python_quote_as_json_strings() {
        let js = export(&post(), Some(&env()), ExportLang::Fetch, &Browser::default()).unwrap();
        assert!(js.starts_with("const response = await fetch(\"http://localhost:8080/users?q=it's\", {\n  method: \"POST\",\n"), "{js}");
        assert!(js.contains(r#"  body: "{\"name\":\"Ada\",\"note\":\"it's \\\"quoted\\\"\"}","#), "{js}");
        let py = export(&post(), Some(&env()), ExportLang::Python, &Browser::default()).unwrap();
        assert!(py.contains("        \"Authorization\": \"Bearer {{token}}\",\n"), "{py}");
        assert!(py.contains(".encode(\"utf-8\"),\n)\nprint(response.status_code"), "{py}");
    }

    #[test]
    fn rux_is_a_native_export_and_its_call() {
        let rux = export(&post(), Some(&env()), ExportLang::Rux, &Browser::default()).unwrap();
        assert!(rux.contains("        .method(\"POST\")\n        .uri(\"http://localhost:8080/users?q=it's\")\n"), "{rux}");
        assert!(rux.contains(r#".body("{\"name\":\"Ada\",\"note\":\"it's \\\"quoted\\\"\"}".as_bytes().to_vec())"#), "{rux}");
        assert!(rux.contains("pub async fn send_request() -> Result<Answer, SendError>"));
        assert!(rux.contains("await api.sendRequest()"));
        let get = HttpRequest { method: "GET".into(), url: "https://example.com".into(), headers: vec![], body: None, expect: vec![], capture: vec![] };
        assert!(export(&get, None, ExportLang::Rux, &Browser::default()).unwrap().contains(".body(Vec::new())"));
    }

    #[test]
    fn a_host_trusted_unchecked_is_so_in_the_code() {
        let trusting = Browser { unchecked_tls: vec!["localhost".into()], ..Browser::default() };
        let local = HttpRequest { method: "GET".into(), url: "https://localhost:8443/x".into(), ..HttpRequest::default() };
        let curl = export(&local, None, ExportLang::Curl, &trusting).unwrap();
        assert_eq!(curl, "curl --insecure 'https://localhost:8443/x'");
        assert!(export(&local, None, ExportLang::Python, &trusting).unwrap().contains("    verify=False,"));
        assert!(export(&local, None, ExportLang::Rux, &trusting).unwrap().contains("disable_verification(true)"));
        assert!(export(&local, None, ExportLang::Fetch, &trusting).unwrap().starts_with("// The workspace takes"));
        // Another host, or plain http, is checked as ever.
        let other = HttpRequest { url: "https://example.com/".into(), ..local.clone() };
        assert_eq!(export(&other, None, ExportLang::Curl, &trusting).unwrap(), "curl 'https://example.com/'");
        let plain = HttpRequest { url: "http://localhost:8080/".into(), ..local };
        assert!(!export(&plain, None, ExportLang::Curl, &trusting).unwrap().contains("insecure"));
    }

    #[test]
    fn graphql_goes_as_its_json_post() {
        let gql = HttpRequest {
            method: "GRAPHQL".into(),
            url: "https://countries.example/".into(),
            headers: vec![],
            body: Some("query { a }\n\n{\"x\": 1}".into()),
            expect: vec![],
            capture: vec![],
        };
        let out = export(&gql, None, ExportLang::Curl, &Browser::default()).unwrap();
        assert!(out.starts_with("curl -X POST 'https://countries.example/'"), "{out}");
        assert!(out.contains("application/json"), "{out}");
        assert!(out.contains(r#""query":"query { a }""#), "{out}");
    }
}
