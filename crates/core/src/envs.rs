//! Environments: one `.env` file each under the workspace's `environments/`,
//! `KEY=value` a line, and `{{KEY}}` in a request resolved from one when it
//! is sent.
//!
//! ```text
//! # local
//! baseUrl=http://localhost:8080
//! token=dev-token
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracer_protocol::{Environment, HttpRequest, ENVIRONMENTS_DIR};

/// `name` as the core names an environment, or why it can't be one: one
/// word of letters, digits, `-`, `_` and `.`, such as `local` or `staging-eu`.
pub fn normalize(name: &str) -> Result<String, String> {
    let name = name.trim();
    let name = name.strip_suffix(".env").unwrap_or(name);
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok {
        Ok(name.to_string())
    } else {
        Err(format!("`{name}` is not an environment name: one word, such as `local` or `staging-eu`"))
    }
}

pub fn file_of(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(ENVIRONMENTS_DIR).join(format!("{name}.env"))
}

pub fn write(workspace: &Path, env: &Environment) -> io::Result<()> {
    let file = file_of(workspace, &env.name);
    fs::create_dir_all(file.parent().expect("an environment file is inside environments/"))?;
    let partial = file.with_extension("env.partial");
    fs::write(&partial, to_text(env))?;
    fs::rename(&partial, &file)
}

pub fn remove(workspace: &Path, name: &str) -> io::Result<()> {
    fs::remove_file(file_of(workspace, name))
}

/// Every environment, by name, and the files that don't parse, with why.
pub fn list(workspace: &Path) -> (Vec<Environment>, Vec<String>) {
    let (mut found, mut skipped) = (Vec::new(), Vec::new());
    let Ok(entries) = fs::read_dir(workspace.join(ENVIRONMENTS_DIR)) else { return (found, skipped) };
    for entry in entries.flatten() {
        let file = entry.path();
        if file.extension().and_then(|e| e.to_str()) != Some("env") {
            continue;
        }
        let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = normalize(&name)
            .and_then(|name| fs::read_to_string(&file).map_err(|e| e.to_string()).map(|text| (name, text)))
            .and_then(|(name, text)| from_text(&text).map(|vars| Environment { name, vars }));
        match parsed {
            Ok(env) => found.push(env),
            Err(why) => skipped.push(format!("{name}: {why}")),
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    (found, skipped)
}

pub fn read(workspace: &Path, name: &str) -> Result<Environment, String> {
    let name = normalize(name)?;
    let text = fs::read_to_string(file_of(workspace, &name)).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("there is no environment `{name}`"),
        _ => format!("could not read environment `{name}`: {e}"),
    })?;
    Ok(Environment { vars: from_text(&text)?, name })
}

pub fn to_text(env: &Environment) -> String {
    env.vars.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

/// `KEY=value` lines; blank lines and `#` comments are skipped. The value is
/// everything after the first `=`, trimmed, so it may hold `=` itself.
pub fn from_text(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut vars = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (key, value) = t.split_once('=').ok_or_else(|| format!("`{t}` is not `KEY=value`"))?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
            return Err(format!("`{key}` is not a variable name"));
        }
        vars.push((key.to_string(), value.trim().to_string()));
    }
    Ok(vars)
}

/// `request` with every `{{name}}` filled from `env`. Any it uses that `env`
/// doesn't define, or any at all with no environment, is an error naming
/// them, rather than a request sent with `{{baseUrl}}` in it.
pub fn resolve(request: &HttpRequest, env: Option<&Environment>) -> Result<HttpRequest, String> {
    let mut missing: Vec<String> = Vec::new();
    let mut f = |text: &str| fill(text, env, &mut missing);
    let resolved = HttpRequest {
        method: request.method.clone(),
        url: f(&request.url),
        headers: request.headers.iter().map(|(k, v)| (k.clone(), f(v))).collect(),
        body: request.body.as_deref().map(&mut f),
    };
    if missing.is_empty() {
        return Ok(resolved);
    }
    // Each named once, in the order they first appear.
    let mut seen = Vec::new();
    missing.retain(|m| if seen.contains(m) { false } else { seen.push(m.clone()); true });
    let names = missing.iter().map(|m| format!("{{{{{m}}}}}")).collect::<Vec<_>>().join(", ");
    Err(match env {
        Some(env) => format!("{names} not defined in environment `{}`", env.name),
        None => format!("uses {names}; choose an environment"),
    })
}

fn fill(text: &str, env: Option<&Environment>, missing: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else { break };
        let name = rest[start + 2..start + 2 + len].trim();
        out.push_str(&rest[..start]);
        match env.and_then(|e| e.vars.iter().find(|(k, _)| k == name)) {
            Some((_, value)) => out.push_str(value),
            None => {
                missing.push(name.to_string());
                out.push_str(&rest[start..start + 4 + len]);
            }
        }
        rest = &rest[start + 4 + len..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Environment {
        Environment {
            name: "local".into(),
            vars: vec![("baseUrl".into(), "http://localhost:8080".into()), ("token".into(), "a=b".into())],
        }
    }

    #[test]
    fn variables_fill_the_url_headers_and_body() {
        let request = HttpRequest {
            method: "POST".into(),
            url: "{{baseUrl}}/albums?t={{ token }}".into(),
            headers: vec![("Authorization".into(), "Bearer {{token}}".into())],
            body: Some("{\"url\": \"{{baseUrl}}\"}".into()),
        };
        let r = resolve(&request, Some(&local())).unwrap();
        assert_eq!(r.url, "http://localhost:8080/albums?t=a=b");
        assert_eq!(r.headers[0].1, "Bearer a=b");
        assert_eq!(r.body.as_deref(), Some("{\"url\": \"http://localhost:8080\"}"));
    }

    #[test]
    fn a_missing_variable_is_named_not_sent() {
        let request = HttpRequest { method: "GET".into(), url: "{{host}}/{{baseUrl}}/{{host}}".into(), headers: vec![], body: None };
        assert_eq!(resolve(&request, Some(&local())).unwrap_err(), "{{host}} not defined in environment `local`");
        assert_eq!(resolve(&request, None).unwrap_err(), "uses {{host}}, {{baseUrl}}; choose an environment");
        let plain = HttpRequest { method: "GET".into(), url: "http://x/{{".into(), headers: vec![], body: None };
        assert_eq!(resolve(&plain, None).unwrap().url, "http://x/{{");
    }

    #[test]
    fn the_file_is_key_value_lines() {
        let vars = from_text("# local\n\nbaseUrl = http://localhost:8080\ntoken=a=b\n").unwrap();
        assert_eq!(vars, local().vars);
        assert_eq!(to_text(&local()), "baseUrl=http://localhost:8080\ntoken=a=b\n");
        assert!(from_text("no equals here").is_err());
        assert_eq!(normalize(" staging-eu.env ").unwrap(), "staging-eu");
        for bad in ["", "a/b", "../x", ".hidden", "a b"] {
            assert!(normalize(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
