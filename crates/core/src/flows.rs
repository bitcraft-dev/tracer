//! Flows: a folder of saved requests run in order, each step's captures
//! filling `{{name}}` in the steps after it.
//!
//! ```text
//! requests/signup/1 login.http      # @capture token = $.access_token
//!                                   POST {{base}}/login
//! requests/signup/2 me.http         # @expect $.email exists
//!                                   GET {{base}}/me
//!                                   Authorization: Bearer {{token}}
//! ```
//!
//! Steps run in the order of their paths, so a number at the front of each
//! name orders them. Each is an ordinary request, numbered and published as
//! any send is, held to the same policy and approvals, and tagged with its
//! flow. A capture reads `$.path` (a string as its text, anything else as
//! JSON), `header <name>` or `status`.

use std::sync::Arc;
use std::thread;

use serde_json::Value;
use tracer_protocol::{Actor, EventKind, FlowStep, HttpResponse, SavedRequest, StepOutcome};

use crate::conn::{run_one, Ran};
use crate::files;
use crate::hub::Hub;

/// The saved requests a flow at `path` runs: those under folder `path`, or
/// the one at `path`. Starts it on a thread of its own, and gives its id
/// and steps.
pub fn start(hub: &Arc<Hub>, actor: &Actor, path: &str, env: Option<String>, keep_going: bool) -> Result<(u64, Vec<String>), String> {
    let path = path.trim().trim_end_matches('/');
    let path = if path.is_empty() { String::new() } else { files::normalize(path)? };
    let (requests, _, _) = hub.list_requests();
    let prefix = format!("{path}/");
    let steps: Vec<SavedRequest> = requests.into_iter().filter(|r| path.is_empty() || r.path == path || r.path.starts_with(&prefix)).collect();
    if steps.is_empty() {
        return Err(format!("there are no saved requests under `{path}`"));
    }
    let names: Vec<String> = steps.iter().map(|s| s.path.clone()).collect();
    let flow_id = hub.start_flow(actor.clone(), path.clone(), env.clone(), names.clone());
    let (hub, actor) = (hub.clone(), actor.clone());
    thread::spawn(move || run(&hub, &actor, flow_id, steps, env, keep_going));
    Ok((flow_id, names))
}

fn run(hub: &Hub, actor: &Actor, flow_id: u64, steps: Vec<SavedRequest>, env: Option<String>, keep_going: bool) {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut done: Vec<FlowStep> = Vec::new();
    let mut failing = false;
    for saved in steps {
        let mut step = FlowStep { path: saved.path, request_id: None, outcome: StepOutcome::Passed, note: String::new(), captured: Vec::new() };
        if failing && !keep_going {
            step.outcome = StepOutcome::Skipped;
            step.note = "a step before it failed".into();
            done.push(step);
            continue;
        }
        if let Err(why) = hub.take_rate(actor) {
            (step.outcome, step.note) = (StepOutcome::Failed, why);
            failing = true;
            done.push(step);
            continue;
        }
        let request = saved.request;
        let (request_id, cancelled) = hub.start_request(actor.clone(), request.clone(), env.clone(), None, Some(flow_id), None);
        step.request_id = Some(request_id);
        hub.remember_vars(request_id, &vars);
        match run_one(hub, actor, request_id, &cancelled, &request, env.as_deref(), &vars, None) {
            Ran::Stopped(why) => (step.outcome, step.note) = (StepOutcome::Failed, why),
            Ran::Ended(Err(error), _) => (step.outcome, step.note) = (StepOutcome::Failed, error),
            Ran::Ended(Ok(response), checks) => {
                let failed: Vec<&str> = checks.iter().filter(|c| !c.pass).map(|c| c.expect.as_str()).collect();
                if !failed.is_empty() {
                    (step.outcome, step.note) = (StepOutcome::Failed, format!("check failed: {}", failed.join("; ")));
                } else if checks.is_empty() && response.status >= 400 {
                    (step.outcome, step.note) = (StepOutcome::Failed, format!("status {}", response.status));
                }
                // Captured even from a failed step, for the steps that run on.
                for line in &request.capture {
                    match capture(line, &response) {
                        Ok((name, value)) => {
                            vars.retain(|(k, _)| *k != name);
                            vars.push((name.clone(), value));
                            step.captured.push(name);
                        }
                        Err(why) => {
                            step.outcome = StepOutcome::Failed;
                            if !step.note.is_empty() {
                                step.note.push_str("; ");
                            }
                            step.note.push_str(&why);
                        }
                    }
                }
            }
        }
        failing |= step.outcome == StepOutcome::Failed;
        done.push(step);
    }
    let passed = done.iter().all(|s| s.outcome == StepOutcome::Passed);
    hub.publish(actor.clone(), EventKind::FlowFinished { flow_id, passed, steps: done });
}

/// `token = $.access_token`, `next = header Location`, `code = status`.
fn capture(line: &str, response: &HttpResponse) -> Result<(String, String), String> {
    let (name, source) = line.split_once('=').ok_or_else(|| format!("capture `{line}` is not `name = source`"))?;
    let (name, source) = (name.trim(), source.trim());
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c));
    if !ok {
        return Err(format!("capture `{line}`: `{name}` is not a variable name"));
    }
    let missing = || format!("capture {name}: {source} isn't in the answer");
    let value = if source == "status" {
        response.status.to_string()
    } else if let Some(header) = source.strip_prefix("header ") {
        let header = header.trim();
        let found = response.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(header));
        found.map(|(_, v)| v.clone()).ok_or_else(missing)?
    } else if source.starts_with('$') {
        let json: Value = serde_json::from_str(&response.body).map_err(|_| format!("capture {name}: the body is not JSON"))?;
        match lookup(&json, source).ok_or_else(missing)? {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    } else {
        return Err(format!("capture {name}: `{source}` is not $.path, header <name> or status"));
    };
    Ok((name.to_string(), value))
}

/// `$.a.b[0]` in `json`.
fn lookup<'a>(json: &'a Value, path: &str) -> Option<&'a Value> {
    let mut at = json;
    let rest = path.strip_prefix('$')?;
    for part in rest.split('.').filter(|p| !p.is_empty()) {
        let (key, indexes) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if !key.is_empty() {
            at = at.get(key)?;
        }
        for index in indexes.split('[').filter(|s| !s.is_empty()) {
            at = at.get(index.trim_end_matches(']').parse::<usize>().ok()?)?;
        }
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::Timing;

    fn response() -> HttpResponse {
        HttpResponse {
            status: 201,
            headers: vec![("Location".into(), "/users/7".into())],
            body: r#"{"access_token":"abc","user":{"id":7,"roles":["a","b"]}}"#.into(),
            size: 0,
            timing: Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None },
            tls_unchecked: false,
        }
    }

    #[test]
    fn a_capture_reads_a_path_a_header_or_the_status() {
        let r = response();
        assert_eq!(capture("token = $.access_token", &r).unwrap(), ("token".into(), "abc".into()));
        assert_eq!(capture("id=$.user.id", &r).unwrap(), ("id".into(), "7".into()));
        assert_eq!(capture("role = $.user.roles[1]", &r).unwrap().1, "b");
        assert_eq!(capture("roles = $.user.roles", &r).unwrap().1, r#"["a","b"]"#);
        assert_eq!(capture("next = header location", &r).unwrap().1, "/users/7");
        assert_eq!(capture("code = status", &r).unwrap().1, "201");
        assert!(capture("x = $.nope", &r).unwrap_err().contains("isn't in the answer"));
        assert!(capture("no source", &r).is_err());
        assert!(capture("bad name = status", &r).is_err());
    }
}
