//! OpenAPI: a spec in the workspace, the saved requests made from it, whether
//! answers match it, where the API and the spec have drifted apart, and a
//! mock answering from it.
//!
//! ```text
//! specs/petstore.json        the document as it was fetched (JSON or YAML)
//! specs/petstore.from        where it came from, to import it again
//! requests/petstore/pets/List all pets.http
//!   # @expect status 200
//!   # @expect spec petstore      ← checked against the spec on each answer
//!   GET {{base}}/pets?limit=20
//! ```
//!
//! OpenAPI 3.0 and 3.1, and Swagger 2.0. Only `$ref`s within the document
//! are followed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};
use tracer_protocol::{
    CheckResult, Ended, HistoryEntry, HttpRequest, HttpResponse, SavedRequest, SpecDrift, SpecEndpoint, SpecImported, SpecInfo, SpecOperation, SpecReport, SPECS_DIR,
};
use ureq::Agent;

use crate::files;
use crate::hub::Hub;
use crate::schema::Schemas;

/// The variable an imported request's URL starts with.
pub const BASE_VAR: &str = "base";

/// A spec, read.
#[derive(Debug)]
pub struct Spec {
    pub name: String,
    pub doc: Value,
    pub title: String,
    pub version: String,
    /// `3.0.3`, `3.1.0` or `2.0`.
    pub openapi: String,
    /// Its servers' URLs, variables filled with their defaults.
    pub servers: Vec<String>,
    pub ops: Vec<Op>,
    pub from: String,
}

/// One operation: a method on a path.
#[derive(Clone, Debug)]
pub struct Op {
    pub method: String,
    /// As the spec writes it: `/pets/{petId}`.
    pub path: String,
    pub id: String,
    pub summary: String,
    pub tag: String,
    /// The path's and the operation's parameters, `$ref`s followed; the
    /// operation's win over the path's of the same name and place.
    pub params: Vec<Value>,
    /// What the body is: media type and its object (3.x), or for Swagger a
    /// made-up one holding the body parameter's schema.
    pub body: Vec<(String, Value)>,
    pub body_required: bool,
    /// By code as written: `200`, `4XX`, `default`.
    pub responses: Vec<(String, Value)>,
    /// The security scheme names it takes, the first alternative's.
    pub security: Vec<String>,
    pub deprecated: bool,
}

impl Op {
    pub fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }

    /// The response for `status`: the exact code, then its class (`4XX`),
    /// then `default`.
    pub fn response_for(&self, status: u16) -> Option<(&str, &Value)> {
        let exact = status.to_string();
        let class = format!("{}XX", status / 100);
        let by = |want: &str| self.responses.iter().find(|(c, _)| c.eq_ignore_ascii_case(want)).map(|(c, r)| (c.as_str(), r));
        by(&exact).or_else(|| by(&class)).or_else(|| by("default"))
    }

    fn codes(&self) -> String {
        self.responses.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join(", ")
    }
}

// ---- what clients ask for ----

/// Import the spec at `from` as `name` (see [`plan`]), saving its new
/// requests through the hub so each is published, and with `env`, give
/// that environment a `base` if it has none.
pub fn import(hub: &Hub, actor: &tracer_protocol::Actor, from: &str, name: Option<&str>, env: Option<&str>) -> Result<SpecImported, String> {
    let root = hub.root();
    let (saved, _, _) = hub.list_requests();
    let guess = name.map(String::from);
    let old = guess.as_deref().and_then(|n| load(root, n).ok());
    let mut plan = plan(root, from, name, &saved, old.as_deref())?;
    // Named from its title: what was there before under that name.
    if old.is_none() {
        if let Ok(before) = load(root, &plan.name) {
            plan.reply.changes = changes(&before, &plan.spec);
        }
    }
    write_spec(root, &plan)?;
    for (path, request) in &plan.save {
        hub.save_request(actor.clone(), path, request.clone())?;
    }
    if let (Some(env), Some(server)) = (env.filter(|e| !e.is_empty()), plan.spec.servers.first()) {
        match hub.environment_shown(env) {
            Ok(mut e) if !e.vars.iter().any(|(k, _)| k == BASE_VAR) => {
                e.vars.push((BASE_VAR.into(), server.clone()));
                hub.save_environment(actor.clone(), e)?;
                plan.reply.notes.insert(0, format!("environment {env} now has {BASE_VAR} = {server}"));
            }
            Ok(_) => {}
            Err(e) => plan.reply.notes.push(e),
        }
    }
    hub.publish(actor.clone(), tracer_protocol::EventKind::SpecImported { spec: plan.reply.spec.clone(), created: plan.reply.created.len() as u32 });
    Ok(plan.reply)
}

/// How finished request `request_id`'s answer measures up to spec `name`,
/// or, if not given, the spec its `spec` check names, or the first that
/// has its operation.
pub fn check_request(hub: &Hub, request_id: u64, name: Option<&str>) -> Result<SpecReport, String> {
    let (entry, response, _) = hub.response(request_id).ok_or_else(|| format!("there is no request #{request_id} in the history"))?;
    let response = response.ok_or_else(|| format!("request #{request_id} has no answer to check"))?;
    let root = hub.root();
    let named = name.map(String::from).or_else(|| entry.request.expect.iter().find_map(|e| spec_line(e).map(String::from)));
    let spec = match named {
        Some(n) => load(root, &n)?,
        None => {
            let (specs, _) = list(root);
            specs
                .iter()
                .filter_map(|s| load(root, &s.name).ok())
                .find(|s| find_op(s, &entry.request.method, &entry.request.url).is_some())
                .ok_or_else(|| format!("no spec in {SPECS_DIR}/ has {} {}", entry.request.method, path_of(&entry.request.url)))?
        }
    };
    Ok(report(&spec, &entry.request, &response))
}

/// [`drift`] for spec `name`, over the whole history.
pub fn drift_of(hub: &Hub, name: &str) -> Result<SpecDrift, String> {
    let spec = load(hub.root(), name)?;
    let (saved, _, _) = hub.list_requests();
    Ok(drift(&spec, &saved, &hub.answered()))
}

// ---- reading a spec ----

/// A document's text as JSON, whether it's JSON or YAML.
pub fn parse(text: &str) -> Result<Value, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).map_err(|e| format!("not JSON: {e}"));
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(trimmed).map_err(|e| format!("neither JSON nor YAML: {e}"))?;
    Ok(from_yaml(yaml))
}

fn from_yaml(v: serde_yaml::Value) -> Value {
    use serde_yaml::Value as Y;
    match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                n.as_f64().and_then(serde_json::Number::from_f64).map_or(Value::Null, Value::Number)
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(items) => Value::Array(items.into_iter().map(from_yaml).collect()),
        // Keys such as `200:` are numbers in YAML and text in JSON.
        Y::Mapping(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let key = match k {
                        Y::String(s) => s,
                        Y::Number(n) => n.to_string(),
                        Y::Bool(b) => b.to_string(),
                        other => serde_yaml::to_string(&other).unwrap_or_default().trim().to_string(),
                    };
                    (key, from_yaml(v))
                })
                .collect(),
        ),
        Y::Tagged(t) => from_yaml(t.value),
    }
}

/// `doc` read as a spec, named `name`; `from` is where it came from, which
/// a relative server URL is taken against.
pub fn read(name: &str, doc: Value, from: &str) -> Result<Spec, String> {
    let openapi = doc.get("openapi").or_else(|| doc.get("swagger")).and_then(Value::as_str).unwrap_or("").to_string();
    if openapi.is_empty() {
        return Err("this isn't an OpenAPI document: it has no `openapi` (or `swagger`) version at the top".into());
    }
    if !(openapi.starts_with("3.") || openapi.starts_with("2.")) {
        return Err(format!("OpenAPI {openapi} isn't one Tracer reads: 3.0, 3.1 and Swagger 2.0 are"));
    }
    let swagger = openapi.starts_with("2.");
    let info = doc.get("info");
    let text = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    let title = text(info, "title");
    let version = text(info, "version");
    let servers = if swagger { swagger_server(&doc, from) } else { servers(&doc, from) };
    let ops = operations(&doc, swagger);
    Ok(Spec { name: name.to_string(), title, version, openapi, servers, ops, from: from.to_string(), doc })
}

fn origin_of(url: &str) -> Option<String> {
    let at = url.find("://")?;
    let rest = &url[at + 3..];
    let end = rest.find('/').unwrap_or(rest.len());
    Some(format!("{}{}", &url[..at + 3], &rest[..end]))
}

fn servers(doc: &Value, from: &str) -> Vec<String> {
    let list = doc.get("servers").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for s in &list {
        let Some(mut url) = s.get("url").and_then(Value::as_str).map(String::from) else { continue };
        if let Some(vars) = s.get("variables").and_then(Value::as_object) {
            for (name, v) in vars {
                let default = v.get("default").and_then(Value::as_str).unwrap_or("");
                url = url.replace(&format!("{{{name}}}"), default);
            }
        }
        if url.starts_with('/') || !url.contains("://") {
            if let Some(origin) = origin_of(from) {
                url = format!("{origin}/{}", url.trim_start_matches('/'));
            }
        }
        out.push(url.trim_end_matches('/').to_string());
    }
    if out.is_empty() {
        // 3.x: no servers means the document's own origin.
        if let Some(origin) = origin_of(from) {
            out.push(origin);
        }
    }
    out
}

fn swagger_server(doc: &Value, from: &str) -> Vec<String> {
    let base = doc.get("basePath").and_then(Value::as_str).unwrap_or("").trim_end_matches('/');
    let host = doc.get("host").and_then(Value::as_str).map(String::from).or_else(|| origin_of(from).map(|o| o.split("://").nth(1).unwrap_or("").to_string()));
    let scheme = doc
        .get("schemes")
        .and_then(Value::as_array)
        .and_then(|s| s.iter().filter_map(Value::as_str).find(|s| *s == "https").or_else(|| s.first().and_then(Value::as_str)))
        .map(String::from)
        .or_else(|| from.split("://").next().filter(|s| s.starts_with("http")).map(String::from))
        .unwrap_or_else(|| "https".into());
    match host {
        Some(h) if !h.is_empty() => vec![format!("{scheme}://{h}{base}")],
        _ => vec![],
    }
}

const METHODS: &[&str] = &["get", "put", "post", "delete", "options", "head", "patch", "trace"];

fn operations(doc: &Value, swagger: bool) -> Vec<Op> {
    let s = Schemas { root: doc };
    let global_security = doc.get("security");
    let mut out = Vec::new();
    let Some(paths) = doc.get("paths").and_then(Value::as_object) else { return out };
    for (path, item) in paths {
        let item = s.resolve(item);
        let shared: Vec<Value> = item.get("parameters").and_then(Value::as_array).into_iter().flatten().map(|p| s.resolve(p).clone()).collect();
        for method in METHODS {
            let Some(op) = item.get(*method) else { continue };
            let own: Vec<Value> = op.get("parameters").and_then(Value::as_array).into_iter().flatten().map(|p| s.resolve(p).clone()).collect();
            let key = |p: &Value| (p.get("name").and_then(Value::as_str).unwrap_or("").to_string(), p.get("in").and_then(Value::as_str).unwrap_or("").to_string());
            let mut params: Vec<Value> = shared.iter().filter(|p| !own.iter().any(|o| key(o) == key(p))).cloned().collect();
            params.extend(own);
            let (body, body_required) = if swagger {
                let consumes = op.get("consumes").or_else(|| doc.get("consumes")).and_then(Value::as_array);
                let ty = consumes.and_then(|c| c.first()).and_then(Value::as_str).unwrap_or("application/json").to_string();
                match params.iter().find(|p| p.get("in").and_then(Value::as_str) == Some("body")) {
                    Some(p) => (
                        vec![(ty, serde_json::json!({ "schema": p.get("schema").cloned().unwrap_or(Value::Bool(true)) }))],
                        p.get("required") == Some(&Value::Bool(true)),
                    ),
                    None => (vec![], false),
                }
            } else {
                match op.get("requestBody").map(|b| s.resolve(b)) {
                    Some(b) => (
                        b.get("content").and_then(Value::as_object).map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
                        b.get("required") == Some(&Value::Bool(true)),
                    ),
                    None => (vec![], false),
                }
            };
            let responses = op
                .get("responses")
                .and_then(Value::as_object)
                .map(|r| {
                    r.iter()
                        .filter(|(c, _)| !c.starts_with("x-"))
                        .map(|(c, v)| {
                            let v = s.resolve(v).clone();
                            // Swagger: a response's schema and the produces list,
                            // put in 3.x's shape.
                            let v = if swagger {
                                let produces = op.get("produces").or_else(|| doc.get("produces")).and_then(Value::as_array);
                                let ty = produces.and_then(|p| p.first()).and_then(Value::as_str).unwrap_or("application/json").to_string();
                                match v.get("schema") {
                                    Some(schema) => {
                                        let mut media = serde_json::json!({ "schema": schema });
                                        if let Some(ex) = v.get("examples").and_then(|e| e.get(&ty)) {
                                            media["example"] = ex.clone();
                                        }
                                        let mut content = Map::new();
                                        content.insert(ty, media);
                                        serde_json::json!({ "description": v.get("description").cloned().unwrap_or_default(), "content": content })
                                    }
                                    None => v,
                                }
                            } else {
                                v
                            };
                            (c.clone(), v)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let security = op
                .get("security")
                .or(global_security)
                .and_then(Value::as_array)
                .and_then(|alts| alts.first())
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            out.push(Op {
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                id: op.get("operationId").and_then(Value::as_str).unwrap_or("").to_string(),
                summary: op.get("summary").and_then(Value::as_str).unwrap_or("").trim().to_string(),
                tag: op.get("tags").and_then(Value::as_array).and_then(|t| t.first()).and_then(Value::as_str).unwrap_or("").to_string(),
                params,
                body,
                body_required,
                responses,
                security,
                deprecated: op.get("deprecated") == Some(&Value::Bool(true)),
            });
        }
    }
    out
}

// ---- the workspace's specs ----

pub fn dir(root: &Path) -> PathBuf {
    root.join(SPECS_DIR)
}

/// A spec's name: one word, as an environment's is.
pub fn normalize(name: &str) -> Result<String, String> {
    let name = name.trim();
    let ok = !name.is_empty() && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok {
        Ok(name.to_string())
    } else {
        Err(format!("`{name}` is not a spec name: one word, such as `petstore`"))
    }
}

/// A name from a title: `Swagger Petstore - OpenAPI 3.0` → `swagger-petstore-openapi-3.0`.
fn name_from(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches(|c| c == '-' || c == '.').chars().take(40).collect::<String>();
    if out.is_empty() {
        "api".into()
    } else {
        out.trim_end_matches('-').to_string()
    }
}

fn file_of(root: &Path, name: &str) -> Option<PathBuf> {
    ["json", "yaml", "yml"].iter().map(|ext| dir(root).join(format!("{name}.{ext}"))).find(|p| p.is_file())
}

type Cached = (SystemTime, Arc<Spec>);

fn cache() -> &'static Mutex<HashMap<PathBuf, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spec `name` in the workspace, read once and again only when its file
/// changes.
pub fn load(root: &Path, name: &str) -> Result<Arc<Spec>, String> {
    let name = normalize(name)?;
    let file = file_of(root, &name).ok_or_else(|| format!("there's no spec `{name}` in {SPECS_DIR}/: import one first"))?;
    let modified = fs::metadata(&file).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    if let Some((at, spec)) = cache().lock().unwrap().get(&file) {
        if *at == modified {
            return Ok(spec.clone());
        }
    }
    let text = fs::read_to_string(&file).map_err(|e| format!("could not read {}: {e}", file.display()))?;
    let from = fs::read_to_string(dir(root).join(format!("{name}.from"))).unwrap_or_default().trim().to_string();
    let spec = Arc::new(read(&name, parse(&text).map_err(|e| format!("{SPECS_DIR}/{name}: {e}"))?, &from)?);
    cache().lock().unwrap().insert(file, (modified, spec.clone()));
    Ok(spec)
}

/// The workspace's specs, by name, and those that don't read, with why.
pub fn list(root: &Path) -> (Vec<SpecInfo>, Vec<String>) {
    let mut names = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(dir(root)) {
        for e in entries.flatten() {
            let p = e.path();
            if matches!(p.extension().and_then(|x| x.to_str()), Some("json" | "yaml" | "yml")) {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    let (mut specs, mut skipped) = (Vec::new(), Vec::new());
    for name in names {
        match load(root, &name) {
            Ok(s) => specs.push(info(&s)),
            Err(e) => skipped.push(format!("{name}: {e}")),
        }
    }
    (specs, skipped)
}

pub fn info(s: &Spec) -> SpecInfo {
    SpecInfo {
        name: s.name.clone(),
        title: s.title.clone(),
        version: s.version.clone(),
        openapi: s.openapi.clone(),
        from: s.from.clone(),
        servers: s.servers.clone(),
        operations: s.ops.iter().map(|o| SpecOperation { method: o.method.clone(), path: o.path.clone(), summary: o.summary.clone(), tag: o.tag.clone(), deprecated: o.deprecated }).collect(),
    }
}

/// The text at `from`: a URL, or a file (relative to the workspace).
pub fn fetch(root: &Path, from: &str) -> Result<String, String> {
    let from = from.trim();
    if from.starts_with("http://") || from.starts_with("https://") {
        let agent = Agent::new_with_config(Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(20))).build());
        let mut res = agent.get(from).header("Accept", "application/json, application/yaml;q=0.9, */*;q=0.5").call().map_err(|e| format!("could not fetch {from}: {e}"))?;
        let status = res.status().as_u16();
        if status >= 400 {
            return Err(format!("{from} answered {status}"));
        }
        res.body_mut().with_config().limit(32 * 1024 * 1024).read_to_string().map_err(|e| format!("could not read {from}: {e}"))
    } else {
        let path = Path::new(from);
        let path = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
        fs::read_to_string(&path).map_err(|e| format!("could not read {}: {e}", path.display()))
    }
}

/// What importing did, before it's written: the spec's files and the
/// requests to save.
pub struct Plan {
    pub name: String,
    pub text: String,
    pub ext: &'static str,
    pub spec: Spec,
    pub reply: SpecImported,
    pub save: Vec<(String, HttpRequest)>,
}

/// Read the spec at `from` and work out what importing it does: which
/// operations get a new saved request under `requests/<name>/`, which
/// already have one (kept as they are, edits and all), and which saved
/// requests no longer match any operation. `saved` is the workspace's
/// saved requests now; `old` the spec of that name before, if any.
pub fn plan(root: &Path, from: &str, name: Option<&str>, saved: &[SavedRequest], old: Option<&Spec>) -> Result<Plan, String> {
    let text = fetch(root, from)?;
    let doc = parse(&text)?;
    let ext = if text.trim_start_matches('\u{feff}').trim_start().starts_with('{') { "json" } else { "yaml" };
    let title = doc.get("info").and_then(|i| i.get("title")).and_then(Value::as_str).unwrap_or("api").to_string();
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => normalize(n)?,
        None => name_from(&title),
    };
    let from_kept = if from.starts_with("http") || Path::new(from).is_absolute() { from.to_string() } else { from.replace('\\', "/") };
    let spec = read(&name, doc, &from_kept)?;
    let mut notes = Vec::new();
    let ours: Vec<&SavedRequest> = saved.iter().filter(|s| s.path.starts_with(&format!("{name}/"))).collect();
    let (mut created, mut kept, mut save) = (Vec::new(), Vec::new(), Vec::new());
    let mut taken: BTreeSet<String> = saved.iter().map(|s| s.path.to_lowercase()).collect();
    for op in &spec.ops {
        if let Some(s) = ours.iter().find(|s| s.request.method.eq_ignore_ascii_case(&op.method) && find_op(&spec, &s.request.method, &s.request.url).is_some_and(|o| o.path == op.path)) {
            kept.push(s.path.clone());
            continue;
        }
        let (request, mut why) = request_for(&spec, op);
        let mut path = format!("{name}/{}{}", if op.tag.is_empty() { String::new() } else { format!("{}/", clean(&op.tag)) }, clean(&leaf(op)));
        if taken.contains(&path.to_lowercase()) {
            let mut n = 2;
            while taken.contains(&format!("{path} {n}").to_lowercase()) {
                n += 1;
            }
            path = format!("{path} {n}");
        }
        let path = files::normalize(&path)?;
        taken.insert(path.to_lowercase());
        created.push(path.clone());
        notes.append(&mut why);
        save.push((path, request));
    }
    let gone: Vec<String> = ours
        .iter()
        .filter(|s| !s.request.method.eq_ignore_ascii_case("GRAPHQL") && !s.request.method.eq_ignore_ascii_case("WEBSOCKET"))
        .filter(|s| find_op(&spec, &s.request.method, &s.request.url).is_none())
        .map(|s| s.path.clone())
        .collect();
    let changes = old.map(|o| changes(o, &spec)).unwrap_or_default();
    notes.sort();
    notes.dedup();
    if let Some(server) = spec.servers.first() {
        notes.insert(0, format!("requests start with {{{{{BASE_VAR}}}}}; the spec's server is {server}"));
    } else {
        notes.insert(0, format!("requests start with {{{{{BASE_VAR}}}}}, and the spec names no server: set {BASE_VAR} in an environment"));
    }
    let reply = SpecImported { spec: info(&spec), created, kept, gone, changes, notes };
    Ok(Plan { name, text, ext, spec, reply, save })
}

/// Write what `plan` decided: the spec's files. The requests are saved by
/// the caller, through the hub, so each is published.
pub fn write_spec(root: &Path, plan: &Plan) -> Result<(), String> {
    let d = dir(root);
    fs::create_dir_all(&d).map_err(|e| format!("could not make {SPECS_DIR}/: {e}"))?;
    for ext in ["json", "yaml", "yml"] {
        if ext != plan.ext {
            let _ = fs::remove_file(d.join(format!("{}.{ext}", plan.name)));
        }
    }
    let file = d.join(format!("{}.{}", plan.name, plan.ext));
    fs::write(&file, &plan.text).map_err(|e| format!("could not write {}: {e}", file.display()))?;
    fs::write(d.join(format!("{}.from", plan.name)), format!("{}\n", plan.spec.from)).map_err(|e| format!("could not write {SPECS_DIR}/{}.from: {e}", plan.name))?;
    cache().lock().unwrap().remove(&file);
    Ok(())
}

/// What changed between two versions of a spec, in words.
fn changes(old: &Spec, new: &Spec) -> Vec<String> {
    let mut out = Vec::new();
    let labels = |s: &Spec| s.ops.iter().map(Op::label).collect::<BTreeSet<_>>();
    let (a, b) = (labels(old), labels(new));
    out.extend(b.difference(&a).map(|l| format!("added {l}")));
    out.extend(a.difference(&b).map(|l| format!("removed {l}")));
    for op in &new.ops {
        let Some(before) = old.ops.iter().find(|o| o.label() == op.label()) else { continue };
        let codes = |o: &Op| o.responses.iter().map(|(c, _)| c.clone()).collect::<BTreeSet<_>>();
        let (ca, cb) = (codes(before), codes(op));
        for c in cb.difference(&ca) {
            out.push(format!("{}: now answers {c}", op.label()));
        }
        for c in ca.difference(&cb) {
            out.push(format!("{}: no longer answers {c}", op.label()));
        }
        let required = |o: &Op| o.params.iter().filter(|p| p.get("required") == Some(&Value::Bool(true))).filter_map(|p| p.get("name").and_then(Value::as_str).map(String::from)).collect::<BTreeSet<_>>();
        for p in required(op).difference(&required(before)) {
            out.push(format!("{}: now requires {p}", op.label()));
        }
        if op.deprecated && !before.deprecated {
            out.push(format!("{}: now deprecated", op.label()));
        }
        // The success answer's schema, compared as written after `$ref`s.
        let shape = |s: &Spec, o: &Op| {
            let sch = Schemas { root: &s.doc };
            o.responses.iter().find(|(c, _)| c.starts_with('2')).and_then(|(_, r)| json_media(r)).and_then(|m| m.get("schema")).map(|x| flatten(&sch, x, 0))
        };
        if shape(old, before) != shape(new, op) {
            out.push(format!("{}: its answer's schema changed", op.label()));
        }
    }
    out
}

/// A schema with its `$ref`s put in place (to a depth), for comparing.
fn flatten(s: &Schemas, v: &Value, depth: usize) -> Value {
    if depth > 8 {
        return Value::Null;
    }
    match s.resolve(v) {
        Value::Object(m) => Value::Object(m.iter().filter(|(k, _)| !matches!(k.as_str(), "description" | "example" | "examples" | "title")).map(|(k, x)| (k.clone(), flatten(s, x, depth + 1))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|x| flatten(s, x, depth + 1)).collect()),
        other => other.clone(),
    }
}

fn leaf(op: &Op) -> String {
    if !op.summary.is_empty() {
        op.summary.chars().take(60).collect()
    } else if !op.id.is_empty() {
        op.id.clone()
    } else {
        format!("{} {}", op.method, op.path.replace(['{', '}'], ""))
    }
}

/// Text made fit for a file name.
fn clean(s: &str) -> String {
    let mapped: String = s.chars().map(|c| if c.is_control() || "<>:\"|?*/\\".contains(c) { ' ' } else { c }).collect();
    let words: Vec<&str> = mapped.split_whitespace().collect();
    let out = words.join(" ").trim_matches('.').to_string();
    if out.is_empty() {
        "request".into()
    } else {
        out
    }
}

/// A saved request for `op`, and notes on what couldn't be carried over.
pub fn request_for(spec: &Spec, op: &Op) -> (HttpRequest, Vec<String>) {
    let s = Schemas { root: &spec.doc };
    let mut notes = Vec::new();
    let mut path = op.path.clone();
    let mut query: Vec<(String, String)> = Vec::new();
    let mut headers: Vec<(String, String)> = Vec::new();
    for p in &op.params {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        let place = p.get("in").and_then(Value::as_str).unwrap_or("");
        let required = p.get("required") == Some(&Value::Bool(true)) || place == "path";
        let example = param_example(&s, p);
        let value = example.clone().unwrap_or_else(|| format!("{{{{{name}}}}}"));
        match place {
            "path" => path = path.replace(&format!("{{{name}}}"), &value),
            "query" if required => query.push((name.to_string(), value)),
            "header" if required => headers.push((name.to_string(), value)),
            _ => {}
        }
    }
    for scheme in &op.security {
        let Some(def) = spec.doc.pointer(&format!("/components/securitySchemes/{scheme}")).or_else(|| spec.doc.pointer(&format!("/securityDefinitions/{scheme}"))) else { continue };
        let def = s.resolve(def);
        let ty = def.get("type").and_then(Value::as_str).unwrap_or("");
        let how = def.get("scheme").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        match ty {
            "http" if how == "basic" => headers.push(("Authorization".into(), "Basic {{basic}}".into())),
            "http" | "oauth2" | "openIdConnect" => headers.push(("Authorization".into(), "Bearer {{token}}".into())),
            "basic" => headers.push(("Authorization".into(), "Basic {{basic}}".into())),
            "apiKey" => {
                let name = def.get("name").and_then(Value::as_str).unwrap_or("X-API-Key").to_string();
                match def.get("in").and_then(Value::as_str) {
                    Some("query") => query.push((name, "{{apiKey}}".into())),
                    Some("cookie") => headers.push(("Cookie".into(), format!("{name}={{{{apiKey}}}}"))),
                    _ => headers.push((name, "{{apiKey}}".into())),
                }
            }
            _ => notes.push(format!("{}: its `{scheme}` security isn't one Tracer fills in", op.label())),
        }
    }
    let mut body = None;
    if let Some((ty, media)) = pick_media(&op.body) {
        let example = media_example(&s, media).unwrap_or(Value::Null);
        if ty.contains("json") {
            body = Some(serde_json::to_string_pretty(&example).unwrap_or_default());
            headers.push(("Content-Type".into(), ty.to_string()));
        } else if ty == "application/x-www-form-urlencoded" {
            let pairs: Vec<String> = example.as_object().map(|m| m.iter().map(|(k, v)| format!("{k}={}", scalar(v))).collect()).unwrap_or_default();
            body = Some(pairs.join("&"));
            headers.push(("Content-Type".into(), ty.to_string()));
        } else {
            notes.push(format!("{}: its {ty} body isn't made; write it by hand", op.label()));
        }
    }
    let q = if query.is_empty() { String::new() } else { format!("?{}", query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")) };
    let mut expect = Vec::new();
    let success: Vec<&str> = op.responses.iter().map(|(c, _)| c.as_str()).filter(|c| c.starts_with('2')).collect();
    match success.as_slice() {
        [one] if one.len() == 3 && one.chars().all(|c| c.is_ascii_digit()) => expect.push(format!("status {one}")),
        _ => expect.push("status 2xx".into()),
    }
    expect.push(format!("spec {}", spec.name));
    let request = HttpRequest { method: op.method.clone(), url: format!("{{{{{BASE_VAR}}}}}{path}{q}"), headers, body, expect, capture: vec![] };
    (request, notes)
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn param_example(s: &Schemas, p: &Value) -> Option<String> {
    let v = p
        .get("example")
        .cloned()
        .or_else(|| p.get("examples").and_then(Value::as_object).and_then(|e| e.values().next()).and_then(|e| s.resolve(e).get("value").cloned()))
        .or_else(|| p.get("schema").map(|sch| s.resolve(sch)).and_then(|sch| sch.get("example").or_else(|| sch.get("default")).cloned()))
        .or_else(|| p.get("x-example").or_else(|| p.get("default")).cloned())?;
    Some(scalar(&v))
}

/// The media type to use: JSON if there is one.
fn pick_media(content: &[(String, Value)]) -> Option<(&str, &Value)> {
    content
        .iter()
        .find(|(t, _)| is_json(t))
        .or_else(|| content.first())
        .map(|(t, v)| (t.as_str(), v))
}

fn is_json(ty: &str) -> bool {
    let base = ty.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    base == "application/json" || base.ends_with("+json") || base == "*/*" || base == "application/*"
}

/// The JSON media object of a response, if it has one.
fn json_media(response: &Value) -> Option<&Value> {
    response.get("content")?.as_object()?.iter().find(|(t, _)| is_json(t)).map(|(_, m)| m)
}

/// An example of a media type: its own `example`, its first `examples`
/// value, or one made from its schema.
fn media_example(s: &Schemas, media: &Value) -> Option<Value> {
    if let Some(e) = media.get("example") {
        return Some(e.clone());
    }
    if let Some(first) = media.get("examples").and_then(Value::as_object).and_then(|e| e.values().next()) {
        if let Some(v) = s.resolve(first).get("value") {
            return Some(v.clone());
        }
    }
    media.get("schema").map(|sch| s.example(sch))
}

// ---- matching a request to an operation ----

/// A URL's path, as written: after `{{base}}`, or after the scheme and
/// host; the query left off.
fn path_of(url: &str) -> String {
    let url = url.trim();
    let rest = if let Some(at) = url.find("://") {
        let after = &url[at + 3..];
        after.find('/').map_or("", |i| &after[i..])
    } else if url.starts_with("{{") {
        url.find("}}").map_or("", |i| &url[i + 2..])
    } else {
        url
    };
    rest.split(['?', '#']).next().unwrap_or("").to_string()
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Whether a spec's path segment (`{id}.json`) takes a request's
/// (`42.json`, or `{{id}}` as written).
fn segment_fits(template: &str, actual: &str) -> bool {
    if actual.contains("{{") {
        return template.contains('{') || actual == template;
    }
    fn go(t: &str, a: &str) -> bool {
        match t.find('{') {
            None => t == a,
            Some(start) => {
                let Some(len) = t[start..].find('}') else { return t == a };
                let (lit, rest) = (&t[..start], &t[start + len + 1..]);
                let Some(after) = a.strip_prefix(lit) else { return false };
                (1..=after.len()).filter(|&i| after.is_char_boundary(i)).any(|i| go(rest, &after[i..]))
            }
        }
    }
    go(template, actual)
}

/// How much of a path is literal, outside `{…}`.
fn literal_len(path: &str) -> usize {
    let mut depth = 0;
    path.chars()
        .filter(|c| {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => return depth == 0,
            }
            false
        })
        .count()
}

/// The operation `method` `url` is, if the spec has it: the most literal
/// match, trying the path with the server's base path taken off too.
pub fn find_op<'a>(spec: &'a Spec, method: &str, url: &str) -> Option<&'a Op> {
    locate(spec, method, url).map(|(op, _)| op)
}

/// [`find_op`], and the path as it matched (the server's base path taken
/// off where it was there).
fn locate<'a>(spec: &'a Spec, method: &str, url: &str) -> Option<(&'a Op, String)> {
    let path = path_of(url);
    let mut tries = vec![path.clone()];
    for server in &spec.servers {
        let base = path_of(server);
        let base = base.trim_end_matches('/');
        if !base.is_empty() {
            if let Some(rest) = path.strip_prefix(base) {
                tries.push(rest.to_string());
            }
        }
    }
    for p in tries {
        let want = segments(&p);
        let best = spec
            .ops
            .iter()
            .filter(|o| o.method.eq_ignore_ascii_case(method))
            .filter(|o| {
                let have = segments(&o.path);
                have.len() == want.len() && have.iter().zip(&want).all(|(t, a)| segment_fits(t, a))
            })
            .max_by_key(|o| literal_len(&o.path));
        if let Some(op) = best {
            return Some((op, p));
        }
    }
    None
}

// ---- checking an answer ----

/// How `response` to `request` (as written) measures up to `spec`.
pub fn report(spec: &Spec, request: &HttpRequest, response: &HttpResponse) -> SpecReport {
    let mut r = SpecReport { spec: spec.name.clone(), operation: None, status: response.status, declared: false, schema: None, problems: vec![], undocumented: vec![], pass: false };
    let Some(op) = find_op(spec, &request.method, &request.url) else {
        r.problems.push(format!("the spec has no {} {}", request.method.to_ascii_uppercase(), path_of(&request.url)));
        return r;
    };
    r.operation = Some(op.label());
    let Some((_, answer)) = op.response_for(response.status) else {
        r.problems.push(format!("it answered {}, which the spec doesn't list for {} (it lists {})", response.status, op.label(), op.codes()));
        return r;
    };
    r.declared = true;
    let s = Schemas { root: &spec.doc };
    let content: Vec<(String, Value)> = answer.get("content").and_then(Value::as_object).map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    let got_type = response.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase()).unwrap_or_default();
    if content.is_empty() {
        if !response.body.trim().is_empty() && response.status != 204 {
            r.problems.push(format!("it has a body, and the spec says {} {} has none", op.label(), response.status));
        }
        r.pass = r.problems.is_empty();
        return r;
    }
    let media = content.iter().find(|(t, _)| {
        let t = t.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        t == got_type || t == "*/*" || (t.ends_with("/*") && got_type.starts_with(&t[..t.len() - 1]))
    });
    let media = match media {
        Some(m) => m,
        None => {
            let listed: Vec<&str> = content.iter().map(|(t, _)| t.as_str()).collect();
            let got = if got_type.is_empty() { "no Content-Type".to_string() } else { got_type.clone() };
            r.problems.push(format!("it answered {got}, and the spec says {}", listed.join(" or ")));
            return r;
        }
    };
    let Some(schema) = media.1.get("schema") else {
        r.pass = true;
        return r;
    };
    r.schema = Some(Schemas::name_of(schema).unwrap_or_else(|| "its schema".into()));
    let json = is_json(&got_type) || got_type.is_empty();
    if !json {
        r.pass = true;
        return r;
    }
    match serde_json::from_str::<Value>(&response.body) {
        Err(e) => r.problems.push(format!("the body isn't JSON: {e}")),
        Ok(v) => {
            let c = s.check(schema, &v);
            r.problems = c.problems.into_iter().map(|p| format!("{} {}", p.at, p.what)).collect();
            r.undocumented = c.undocumented;
        }
    }
    r.pass = r.problems.is_empty();
    r
}

/// `report` as one check line's result.
pub fn summary(r: &SpecReport) -> String {
    let op = r.operation.clone().unwrap_or_default();
    let extra = match r.undocumented.len() {
        0 => String::new(),
        n => format!("; {n} field{} it doesn't name: {}", if n == 1 { "" } else { "s" }, r.undocumented.iter().take(3).cloned().collect::<Vec<_>>().join(", ")),
    };
    if r.pass {
        match &r.schema {
            Some(name) => format!("{op} {} matches {name}{extra}", r.status),
            None => format!("{op} {} is as the spec says{extra}", r.status),
        }
    } else {
        let more = if r.problems.len() > 3 { format!(" (and {} more)", r.problems.len() - 3) } else { String::new() };
        format!("{}{more}", r.problems.iter().take(3).cloned().collect::<Vec<_>>().join("; "))
    }
}

/// `spec <name>` lines among `results` (as `expect` read them) checked
/// against the workspace's spec of that name.
pub fn fill_checks(root: &Path, request: &HttpRequest, response: Result<&HttpResponse, &str>, results: &mut [CheckResult]) {
    for r in results.iter_mut() {
        let Some(name) = spec_line(&r.expect) else { continue };
        let (pass, actual) = match (load(root, name), response) {
            (Err(e), _) => (false, e),
            (_, Err(why)) => (false, format!("no answer: {why}")),
            (Ok(spec), Ok(response)) => {
                let rep = report(&spec, request, response);
                (rep.pass, summary(&rep))
            }
        };
        r.pass = pass;
        r.actual = actual;
    }
}

/// The spec a `spec <name>` line names.
pub fn spec_line(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("spec")?;
    rest.starts_with(char::is_whitespace).then(|| rest.trim()).filter(|n| !n.is_empty() && !n.contains(' '))
}

// ---- drift ----

/// Where the API, as history saw it, and the spec part ways: answers that
/// don't match, statuses the spec doesn't list, fields it doesn't name,
/// endpoints it doesn't have, operations never tried, and saved requests
/// left behind by a change to the spec.
///
/// History counted is what was sent to the spec's API: requests whose URL
/// starts as one of the spec's saved requests' does (`{{base}}`), or with
/// one of its servers.
pub fn drift(spec: &Spec, saved: &[SavedRequest], history: &[(HistoryEntry, Option<HttpResponse>)]) -> SpecDrift {
    let ours: Vec<&SavedRequest> = saved.iter().filter(|s| s.path.starts_with(&format!("{}/", spec.name))).collect();
    let mut prefixes: BTreeSet<String> = spec.servers.iter().map(|s| s.trim_end_matches('/').to_string()).collect();
    prefixes.insert(format!("{{{{{BASE_VAR}}}}}"));
    for s in &ours {
        let url = s.request.url.trim();
        if url.starts_with("{{") {
            if let Some(end) = url.find("}}") {
                prefixes.insert(url[..end + 2].to_string());
            }
        } else if let Some(o) = origin_of(url) {
            prefixes.insert(o);
        }
    }
    let in_scope = |url: &str| {
        let url = url.trim();
        prefixes.iter().any(|p| url.starts_with(p.as_str()) && url[p.len()..].chars().next().is_none_or(|c| c == '/' || c == '?'))
    };

    #[derive(Default)]
    struct Tally {
        sent: u32,
        checked: u32,
        failing: u32,
        statuses: BTreeSet<u16>,
        undeclared: BTreeSet<u16>,
        problems: Vec<String>,
        undocumented: BTreeSet<String>,
        last: Option<u64>,
        last_failing: Option<u64>,
    }
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let mut unknown: BTreeMap<String, SpecEndpoint> = BTreeMap::new();
    let mut checked = 0;
    for (entry, response) in history {
        let req = &entry.request;
        let m = req.method.to_ascii_uppercase();
        if matches!(m.as_str(), "GRAPHQL" | "WEBSOCKET") || !in_scope(&req.url) {
            continue;
        }
        let Some(op) = find_op(spec, &m, &req.url) else {
            let status = match &entry.ended {
                Ended::Done { status, .. } => Some(*status),
                _ => None,
            };
            // A 404 or 405 says the API doesn't have it either.
            if matches!(status, Some(404 | 405 | 501)) {
                continue;
            }
            let key = format!("{m} {}", path_of(&req.url));
            let e = unknown.entry(key).or_insert_with(|| SpecEndpoint { method: m.clone(), path: path_of(&req.url), sent: 0, last_status: None, last_request: 0 });
            e.sent += 1;
            e.last_status = status.or(e.last_status);
            e.last_request = e.last_request.max(entry.request_id);
            continue;
        };
        let t = tallies.entry(op.label()).or_default();
        t.sent += 1;
        t.last = Some(t.last.unwrap_or(0).max(entry.request_id));
        let Some(response) = response else { continue };
        checked += 1;
        t.checked += 1;
        t.statuses.insert(response.status);
        let r = report(spec, req, response);
        if !r.declared {
            t.undeclared.insert(response.status);
        }
        if !r.pass {
            t.failing += 1;
            t.last_failing = Some(t.last_failing.unwrap_or(0).max(entry.request_id));
            // An unlisted status is said once, under `undeclared`.
            for p in r.problems.into_iter().filter(|_| r.declared) {
                // `$[3].id …` and `$[7].id …` are the same problem.
                let general = generalize(&p);
                if !t.problems.contains(&general) && t.problems.len() < 8 {
                    t.problems.push(general);
                }
            }
        }
        for u in r.undocumented {
            if t.undocumented.len() < 12 {
                t.undocumented.insert(generalize(&u));
            }
        }
    }
    let operations = spec
        .ops
        .iter()
        .map(|op| {
            let t = tallies.remove(&op.label()).unwrap_or_default();
            let saved: Vec<String> = ours.iter().filter(|s| find_op(spec, &s.request.method, &s.request.url).is_some_and(|o| o.label() == op.label())).map(|s| s.path.clone()).collect();
            tracer_protocol::SpecOpDrift {
                method: op.method.clone(),
                path: op.path.clone(),
                summary: op.summary.clone(),
                deprecated: op.deprecated,
                saved,
                sent: t.sent,
                checked: t.checked,
                failing: t.failing,
                statuses: t.statuses.into_iter().collect(),
                undeclared: t.undeclared.into_iter().collect(),
                problems: t.problems,
                undocumented: t.undocumented.into_iter().collect(),
                last_request: t.last.unwrap_or(0),
                last_failing: t.last_failing.unwrap_or(0),
            }
        })
        .collect();
    let stale = ours
        .iter()
        .filter(|s| !matches!(s.request.method.to_ascii_uppercase().as_str(), "GRAPHQL" | "WEBSOCKET"))
        .filter(|s| find_op(spec, &s.request.method, &s.request.url).is_none())
        .map(|s| s.path.clone())
        .collect();
    SpecDrift { spec: info(spec), operations, undocumented: unknown.into_values().collect(), stale, checked }
}

/// A problem or path with its array indexes made general: `$[3].id` →
/// `$[*].id`.
fn generalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '[' && chars.peek().is_some_and(|d| d.is_ascii_digit()) {
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
            }
            out.push('*');
        }
    }
    out
}

// ---- answering as a mock ----

/// What the mock answers `method` `target` with: status, headers, body,
/// and the operation's label (none when no operation matched).
pub fn mock_answer(spec: &Spec, method: &str, target: &str, headers: &[(String, String)], body: &str) -> (u16, Vec<(String, String)>, String, Option<String>) {
    let json = || vec![("Content-Type".to_string(), "application/json".to_string())];
    let Some((op, matched)) = locate(spec, method, target) else {
        let routes: Vec<String> = spec.ops.iter().map(Op::label).collect();
        let body = serde_json::json!({ "error": format!("the spec `{}` has no {method} {}", spec.name, path_of(target)), "operations": routes }).to_string();
        return (404, json(), body, None);
    };
    let s = Schemas { root: &spec.doc };
    let header = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    let query: Vec<(String, String)> = target
        .split_once('?')
        .map(|(_, q)| q.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((p.to_string(), String::new()), |(k, v)| (k.to_string(), v.to_string()))).collect())
        .unwrap_or_default();

    // `Prefer: code=404, example=sold` (as Prism takes it), or `?__code=404`.
    let prefer = header("prefer").unwrap_or("");
    let pref = |key: &str| prefer.split([',', ';']).filter_map(|p| p.trim().strip_prefix(&format!("{key}="))).next().map(|v| v.trim_matches('"').to_string());
    let wanted_code = pref("code").or_else(|| query.iter().find(|(k, _)| k == "__code").map(|(_, v)| v.clone()));
    let wanted_example = pref("example");

    // The request, checked against the operation, unless a code was asked for.
    if wanted_code.is_none() {
        let mut problems = Vec::new();
        let actual_segments = segments(&matched);
        let template_segments = segments(&op.path);
        for p in &op.params {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let place = p.get("in").and_then(Value::as_str).unwrap_or("");
            let required = p.get("required") == Some(&Value::Bool(true));
            let value = match place {
                "query" => query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
                "header" => header(name).map(String::from),
                "path" => template_segments.iter().position(|t| *t == format!("{{{name}}}")).and_then(|i| actual_segments.get(i)).map(|v| v.to_string()),
                _ => continue,
            };
            match (value, p.get("schema").or(Some(p))) {
                (None, _) if required => problems.push(format!("{place} parameter `{name}` is missing, and the spec requires it")),
                (Some(v), Some(schema)) => {
                    let sch = s.resolve(schema);
                    let ty = sch.get("type").and_then(Value::as_str).unwrap_or("");
                    let as_json = match ty {
                        "integer" | "number" => v.parse::<f64>().ok().map(|_| serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v.clone()))),
                        "boolean" => v.parse::<bool>().ok().map(Value::Bool),
                        _ => Some(Value::String(v.clone())),
                    };
                    match as_json {
                        None => problems.push(format!("{place} parameter `{name}` is {v:?}, and the spec says {ty}")),
                        Some(j) => {
                            for p in s.check(schema, &j).problems {
                                problems.push(format!("{place} parameter `{name}` {}", p.what));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !op.body.is_empty() {
            if body.trim().is_empty() {
                if op.body_required {
                    problems.push("the body is missing, and the spec requires one".into());
                }
            } else if let Some((ty, media)) = pick_media(&op.body).filter(|(t, _)| is_json(t)) {
                match serde_json::from_str::<Value>(body) {
                    Err(e) => problems.push(format!("the body isn't JSON ({ty}): {e}")),
                    Ok(v) => {
                        if let Some(schema) = media.get("schema") {
                            problems.extend(s.check(schema, &v).problems.into_iter().map(|p| format!("body {} {}", p.at, p.what)));
                        }
                    }
                }
            }
        }
        if !problems.is_empty() {
            let status = if problems.iter().any(|p| p.starts_with("body")) { 422 } else { 400 };
            let body = serde_json::json!({ "error": format!("the request doesn't match {} in the spec", op.label()), "problems": problems }).to_string();
            return (status, json(), body, Some(op.label()));
        }
    }

    let chosen = match &wanted_code {
        Some(code) => op.responses.iter().find(|(c, _)| c.eq_ignore_ascii_case(code)),
        None => {
            let mut successes: Vec<&(String, Value)> = op.responses.iter().filter(|(c, _)| c.starts_with('2')).collect();
            successes.sort_by(|a, b| a.0.cmp(&b.0));
            successes.first().copied().or_else(|| op.responses.iter().find(|(c, _)| c == "default")).or_else(|| op.responses.first())
        }
    };
    let Some((code, answer)) = chosen else {
        let body = serde_json::json!({ "error": format!("{} has no {} answer in the spec (it has {})", op.label(), wanted_code.unwrap_or_default(), op.codes()) }).to_string();
        return (501, json(), body, Some(op.label()));
    };
    let status = code.parse::<u16>().unwrap_or_else(|_| match code.chars().next() {
        Some(d @ '1'..='5') => (d as u16 - b'0' as u16) * 100,
        _ => 200,
    });
    let content: Vec<(String, Value)> = answer.get("content").and_then(Value::as_object).map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    // An `Accept` that names a type the operation has picks it.
    let accept = header("accept").unwrap_or("");
    let picked = content.iter().find(|(t, _)| accept.contains(t.as_str())).map(|(t, v)| (t.as_str(), v)).or_else(|| pick_media(&content));
    let Some((ty, media)) = picked else {
        return (status, vec![], String::new(), Some(op.label()));
    };
    let example = wanted_example
        .as_deref()
        .and_then(|name| media.get("examples").and_then(|e| e.get(name)).map(|e| s.resolve(e)).and_then(|e| e.get("value")).cloned())
        .or_else(|| media_example(&s, media))
        .unwrap_or(Value::Null);
    let ty = if ty == "*/*" || ty == "application/*" { "application/json" } else { ty };
    let body = match &example {
        Value::String(text) if !is_json(ty) => text.clone(),
        v => serde_json::to_string_pretty(v).unwrap_or_default(),
    };
    (status, vec![("Content-Type".into(), ty.to_string())], body, Some(op.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PETSTORE: &str = r##"
openapi: 3.0.3
info: { title: Swagger Petstore, version: 1.0.0 }
servers:
  - url: "{scheme}://pets.test/v1"
    variables: { scheme: { default: https } }
security: [ { key: [] } ]
paths:
  /pets:
    get:
      summary: List all pets
      operationId: listPets
      tags: [pets]
      parameters:
        - { name: limit, in: query, required: true, schema: { type: integer, maximum: 100 }, example: 20 }
      responses:
        200:
          description: A page of pets
          content:
            application/json:
              schema: { type: array, items: { $ref: "#/components/schemas/Pet" } }
        default:
          description: error
          content: { application/json: { schema: { $ref: "#/components/schemas/Error" } } }
    post:
      summary: Create a pet
      tags: [pets]
      requestBody:
        required: true
        content: { application/json: { schema: { $ref: "#/components/schemas/Pet" } } }
      responses:
        "201": { description: made }
  /pets/{petId}:
    parameters:
      - { name: petId, in: path, required: true, schema: { type: integer } }
    get:
      operationId: showPetById
      responses:
        "200":
          description: one pet
          content:
            application/json:
              schema: { $ref: "#/components/schemas/Pet" }
              examples: { rex: { value: { id: 7, name: Rex } } }
        "404": { description: none }
  /pets/mine:
    get:
      summary: My pets
      responses: { "200": { description: mine } }
components:
  securitySchemes:
    key: { type: apiKey, in: header, name: X-Api-Key }
  schemas:
    Pet:
      type: object
      required: [id, name]
      properties:
        id: { type: integer, format: int64 }
        name: { type: string }
        tag: { type: string }
    Error:
      type: object
      properties: { message: { type: string } }
"##;

    fn spec() -> Spec {
        read("petstore", parse(PETSTORE).unwrap(), "https://pets.test/openapi.yaml").unwrap()
    }

    fn answer(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())],
            body: body.into(),
            size: body.len() as u64,
            timing: tracer_protocol::Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None },
            tls_unchecked: false,
        }
    }

    fn req(method: &str, url: &str) -> HttpRequest {
        HttpRequest { method: method.into(), url: url.into(), ..Default::default() }
    }

    #[test]
    fn reads_yaml_servers_and_operations() {
        let s = spec();
        assert_eq!(s.title, "Swagger Petstore");
        assert_eq!(s.servers, vec!["https://pets.test/v1".to_string()]);
        let labels: Vec<String> = s.ops.iter().map(Op::label).collect();
        assert_eq!(labels, vec!["GET /pets", "POST /pets", "GET /pets/mine", "GET /pets/{petId}"]);
        assert_eq!(s.ops[3].params.len(), 1, "the path's parameters are the operation's too");
        assert_eq!(s.ops[0].responses[0].0, "200", "a YAML number key reads as text");
        assert_eq!(name_from("Swagger Petstore - OpenAPI 3.0"), "swagger-petstore-openapi-3.0");
    }

    #[test]
    fn requests_are_made_from_operations() {
        let s = spec();
        let (list, _) = request_for(&s, &s.ops[0]);
        assert_eq!(list.url, "{{base}}/pets?limit=20");
        assert_eq!(list.headers, vec![("X-Api-Key".to_string(), "{{apiKey}}".to_string())]);
        assert_eq!(list.expect, vec!["status 200".to_string(), "spec petstore".to_string()]);
        let (one, _) = request_for(&s, &s.ops[3]);
        assert_eq!(one.url, "{{base}}/pets/{{petId}}");
        let (make, _) = request_for(&s, &s.ops[1]);
        assert_eq!(make.expect[0], "status 201");
        let body: Value = serde_json::from_str(make.body.as_deref().unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "id": 1, "name": "string", "tag": "string" }));
    }

    #[test]
    fn requests_find_their_operation() {
        let s = spec();
        assert_eq!(find_op(&s, "GET", "{{base}}/pets/{{petId}}").unwrap().path, "/pets/{petId}");
        assert_eq!(find_op(&s, "get", "https://pets.test/v1/pets/42?x=1").unwrap().path, "/pets/{petId}");
        assert_eq!(find_op(&s, "GET", "{{base}}/pets/mine").unwrap().path, "/pets/mine", "the literal path wins");
        assert!(find_op(&s, "DELETE", "{{base}}/pets/1").is_none());
    }

    #[test]
    fn answers_are_checked() {
        let s = spec();
        let ok = report(&s, &req("GET", "{{base}}/pets/7"), &answer(200, r#"{"id":7,"name":"Rex","etag":"x"}"#));
        assert!(ok.pass, "{:?}", ok.problems);
        assert_eq!(summary(&ok), "GET /pets/{petId} 200 matches Pet; 1 field it doesn't name: $.etag");
        let bad = report(&s, &req("GET", "{{base}}/pets/7"), &answer(200, r#"{"id":"7"}"#));
        assert!(!bad.pass);
        assert_eq!(summary(&bad), "$.id is a string (\"7\"), and the spec says integer; $.name is missing, and the spec requires it");
        let odd = report(&s, &req("GET", "{{base}}/pets/7"), &answer(418, "{}"));
        assert_eq!(summary(&odd), "it answered 418, which the spec doesn't list for GET /pets/{petId} (it lists 200, 404)");
        let err = report(&s, &req("GET", "{{base}}/pets?limit=1"), &answer(500, r#"{"message":"down"}"#));
        assert!(err.pass, "default covers it");
        let mut html = answer(200, "<p>");
        html.headers[0].1 = "text/html".into();
        assert_eq!(summary(&report(&s, &req("GET", "{{base}}/pets/1"), &html)), "it answered text/html, and the spec says application/json");
        assert_eq!(spec_line("spec petstore"), Some("petstore"));
        assert_eq!(spec_line("specs"), None);
    }

    #[test]
    fn the_mock_answers_from_examples_and_checks_requests() {
        let s = spec();
        let (status, _, body, op) = mock_answer(&s, "GET", "/pets/7", &[], "");
        assert_eq!((status, op.as_deref()), (200, Some("GET /pets/{petId}")));
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), serde_json::json!({ "id": 7, "name": "Rex" }));
        let (status, _, body, _) = mock_answer(&s, "GET", "/pets/abc", &[], "");
        assert_eq!(status, 400, "{body}");
        let (status, _, _, _) = mock_answer(&s, "GET", "/pets", &[], "");
        assert_eq!(status, 400, "limit is required");
        let (status, _, body, _) = mock_answer(&s, "GET", "/pets?limit=2", &[], "");
        assert_eq!(status, 200);
        assert!(body.starts_with('['));
        let (status, _, body, _) = mock_answer(&s, "POST", "/pets", &[], r#"{"id":"x"}"#);
        assert_eq!(status, 422, "{body}");
        let (status, _, _, _) = mock_answer(&s, "GET", "/pets/7", &[("Prefer".into(), "code=404".into())], "");
        assert_eq!(status, 404);
        let (status, _, _, _) = mock_answer(&s, "GET", "/nope", &[], "");
        assert_eq!(status, 404);
        // The server's base path is taken off, parameters read after it.
        let (status, _, body, _) = mock_answer(&s, "GET", "/v1/pets/7", &[], "");
        assert_eq!(status, 200, "{body}");
    }

    #[test]
    fn drift_is_counted_from_history() {
        let s = spec();
        let entry = |id: u64, method: &str, url: &str, status: u16| HistoryEntry {
            request_id: id,
            at_ms: 0,
            actor: tracer_protocol::Actor { kind: tracer_protocol::ClientKind::Cli, name: "t".into() },
            request: req(method, url),
            env: None,
            rerun_of: None,
            flow: None,
            bench: None,
            ended: Ended::Done { status, total_ms: 1.0, size: 1 },
            checks: None,
        };
        let history = vec![
            (entry(1, "GET", "{{base}}/pets/7", 200), Some(answer(200, r#"{"id":7,"name":"a","etag":"1"}"#))),
            (entry(2, "GET", "{{base}}/pets/8", 200), Some(answer(200, r#"{"id":"8","name":"b"}"#))),
            (entry(3, "GET", "{{base}}/pets/9", 410), Some(answer(410, "{}"))),
            (entry(4, "GET", "{{base}}/stats", 200), Some(answer(200, "{}"))),
            (entry(5, "GET", "https://elsewhere.test/x", 200), Some(answer(200, "{}"))),
        ];
        let saved = vec![SavedRequest { path: "petstore/old".into(), request: req("DELETE", "{{base}}/pets/{{petId}}") }];
        let d = drift(&s, &saved, &history);
        let one = d.operations.iter().find(|o| o.path == "/pets/{petId}").unwrap();
        assert_eq!((one.sent, one.checked, one.failing), (3, 3, 2));
        assert_eq!(one.undeclared, vec![410]);
        assert!(!one.problems.iter().any(|p| p.contains("410")), "said once: {:?}", one.problems);
        assert_eq!(one.undocumented, vec!["$.etag".to_string()]);
        assert!(one.problems.iter().any(|p| p.starts_with("$.id is a string")), "{:?}", one.problems);
        assert_eq!(d.undocumented.len(), 1);
        assert_eq!(d.undocumented[0].path, "/stats");
        assert_eq!(d.stale, vec!["petstore/old".to_string()]);
        assert_eq!(d.operations.iter().filter(|o| o.sent == 0).count(), 3);
        assert_eq!(generalize("$[12].a[3]"), "$[*].a[*]");
    }

    #[test]
    fn swagger_two_reads_as_three() {
        let doc = serde_json::json!({
            "swagger": "2.0", "info": { "title": "Old", "version": "1" }, "host": "old.test", "basePath": "/api", "schemes": ["https"],
            "paths": { "/things/{id}": { "get": {
                "parameters": [{ "name": "id", "in": "path", "required": true, "type": "integer" }],
                "responses": { "200": { "description": "a thing", "schema": { "$ref": "#/definitions/Thing" } } }
            } } },
            "definitions": { "Thing": { "type": "object", "required": ["id"], "properties": { "id": { "type": "integer" } } } }
        });
        let s = read("old", doc, "").unwrap();
        assert_eq!(s.servers, vec!["https://old.test/api".to_string()]);
        let r = report(&s, &req("GET", "https://old.test/api/things/3"), &answer(200, r#"{"id":"3"}"#));
        assert!(!r.pass);
        assert_eq!(r.schema.as_deref(), Some("Thing"));
    }

    #[test]
    fn a_new_version_says_what_changed() {
        let old = spec();
        let text = PETSTORE.replace("        \"404\": { description: none }\n", "").replace("tag: { type: string }", "tag: { type: integer }");
        let new = read("petstore", parse(&text).unwrap(), "").unwrap();
        let c = changes(&old, &new);
        assert!(c.contains(&"GET /pets/{petId}: no longer answers 404".to_string()), "{c:?}");
        assert!(c.contains(&"GET /pets/{petId}: its answer's schema changed".to_string()), "{c:?}");
    }
}
