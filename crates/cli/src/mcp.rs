//! `tracer mcp`: an MCP server on stdio, through which an AI agent drives
//! Tracer.
//!
//! It is a client of the workspace's core like the window and the CLI, as
//! [`ClientKind::Agent`] under the name the agent gives in `initialize`, so
//! everything it does shows in the window as that agent's, live, and goes
//! through the core's guardrails: a request that changes something, or uses
//! a production environment, waits for a person to allow it; secrets stay
//! `{{name}}`; a person can switch agents off.
//!
//! JSON-RPC 2.0, one message per line, as MCP's stdio transport has it.
//! Nothing but messages goes to stdout; anything else goes to stderr.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tracer_protocol::client::{Client, Incoming, Shared};
use tracer_protocol::{
    ClientKind, Command, Ended, Event, EventKind, HistoryEntry, HistoryFilter, HttpRequest, HttpResponse, ReplyBody,
    StatusClass,
};

/// MCP versions this server speaks, newest first. A client asking for
/// another is answered with the newest.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The most of a response body handed back unless more is asked for.
const BODY_SHOWN: usize = 32 * 1024;
/// The most `get_response` hands back however much is asked for.
const BODY_MAX: usize = 1024 * 1024;
/// How long a send waits for its answer unless told otherwise, and at most.
const WAIT_DEFAULT: u64 = 60;
const WAIT_MAX: u64 = 600;
/// How long a core started here gets to answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);

const INSTRUCTIONS: &str = "\
Tracer is an API workbench. These tools send HTTP requests through the \
workspace's Tracer core, read its saved requests, environments and history, \
and save requests. A person may be watching in Tracer's window, where \
everything you do appears under your name.

`{{name}}` in a URL, header or body is filled from the environment named by \
`env` when the request is sent. Secrets are environment variables whose \
values you never see: use `{{name}}`, and wherever a secret's value comes \
back in a response it reads `{{name}}` again.

Any request other than GET, HEAD or OPTIONS, and any request sent with an \
environment whose name starts with `prod`, waits for a person to allow it \
in the window. Such a send returns `awaiting_approval`; call get_response \
later for the outcome. A refused request fails with `refused by <person>`.";

pub fn serve(workspace: PathBuf, name_override: Option<String>) -> io::Result<()> {
    let mut server = Server { workspace, name_override, core: None };
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        // Some shells start a pipe with a byte-order mark.
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(line) {
            Ok(message) => server.handle(message),
            Err(e) => Some(error_message(Value::Null, -32700, &format!("not JSON: {e}"))),
        };
        if let Some(answer) = answer {
            writeln!(stdout, "{answer}")?;
            stdout.flush()?;
        }
    }
    if let Some(core) = server.core.take() {
        core.client.close();
    }
    Ok(())
}

struct Server {
    workspace: PathBuf,
    name_override: Option<String>,
    core: Option<Core>,
}

/// The connection to the core, and every event it has sent since, in order.
struct Core {
    client: Shared,
    events: Receiver<Event>,
}

impl Server {
    /// The answer to one message; none for a notification.
    fn handle(&mut self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // A notification, or a response to something we never ask: nothing to say.
        let id = id?;
        let result = match method.as_str() {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(self.call_tool(&params)),
            "" => Err((-32600, "no method".to_string())),
            other => Err((-32601, format!("no method `{other}`"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, why)) => error_message(id, code, &why),
        })
    }

    fn initialize(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
        let client_name = params.pointer("/clientInfo/name").and_then(Value::as_str).unwrap_or("agent");
        let name = self.name_override.clone().unwrap_or_else(|| client_name.to_string());
        // Connected now, so the window shows the agent as soon as it's here.
        if let Err(e) = self.connect(&name) {
            eprintln!("tracer mcp: {e}");
        }
        Ok(json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "tracer", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }))
    }

    fn connect(&mut self, name: &str) -> io::Result<()> {
        if self.core.is_some() {
            return Ok(());
        }
        let client = match Client::connect(&self.workspace, ClientKind::Agent, name) {
            Ok(client) => client,
            Err(_) => start_core(&self.workspace, name)?,
        };
        let (tx, events) = mpsc::channel();
        let client = client.into_shared(move |incoming| {
            if let Incoming::Event(event) = incoming {
                let _ = tx.send(event);
            }
        })?;
        client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
        self.core = Some(Core { client, events });
        Ok(())
    }

    fn call_tool(&mut self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if self.core.is_none() {
            let who = self.name_override.clone().unwrap_or_else(|| "agent".into());
            if let Err(e) = self.connect(&who) {
                return tool_error(&format!("can't reach Tracer's core: {e}"));
            }
        }
        let core = self.core.as_ref().expect("connected above");
        match run_tool(core, name, &args) {
            Ok(value) => json!({
                "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                "structuredContent": value,
                "isError": false,
            }),
            Err(why) => tool_error(&why),
        }
    }
}

fn run_tool(core: &Core, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "list_requests" => {
            let ReplyBody::Requests { requests, skipped, .. } = call(core, Command::ListRequests)? else {
                return Err(unexpected());
            };
            let list: Vec<Value> = requests
                .into_iter()
                .map(|r| {
                    let mut v = request_json(&r.request);
                    v["path"] = json!(r.path);
                    v
                })
                .collect();
            Ok(json!({ "requests": list, "skipped": skipped }))
        }
        "list_environments" => {
            let ReplyBody::Environments { environments, skipped, .. } = call(core, Command::ListEnvironments)? else {
                return Err(unexpected());
            };
            let list: Vec<Value> = environments
                .into_iter()
                .map(|e| {
                    let vars: Map<String, Value> = e
                        .vars
                        .into_iter()
                        .filter(|(k, _)| !e.secrets.contains(k))
                        .map(|(k, v)| (k, json!(v)))
                        .collect();
                    json!({ "name": e.name, "variables": vars, "secrets": e.secrets })
                })
                .collect();
            Ok(json!({ "environments": list, "skipped": skipped }))
        }
        "send_request" => {
            let request = HttpRequest {
                method: str_arg(args, "method").unwrap_or("GET").to_ascii_uppercase(),
                url: str_arg(args, "url").ok_or("`url` is required")?.to_string(),
                headers: headers_arg(args)?,
                body: str_arg(args, "body").map(String::from),
            };
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            send_and_wait(core, Command::Send { request, env }, args)
        }
        "run_saved_request" => {
            let path = str_arg(args, "path").ok_or("`path` is required")?;
            let wanted = path.trim().trim_start_matches("requests/").trim_end_matches(".http");
            let ReplyBody::Requests { requests, .. } = call(core, Command::ListRequests)? else {
                return Err(unexpected());
            };
            let found = requests
                .into_iter()
                .find(|r| r.path == wanted)
                .ok_or_else(|| format!("there is no saved request `{wanted}`; list_requests lists them"))?;
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            send_and_wait(core, Command::Send { request: found.request, env }, args)
        }
        "rerun_request" => {
            let request_id = id_arg(args)?;
            let env = str_arg(args, "env").map(String::from);
            send_and_wait(core, Command::Rerun { request_id, env }, args)
        }
        "read_history" => {
            let actor = match str_arg(args, "actor") {
                None => None,
                Some("window") => Some(ClientKind::Window),
                Some("cli") => Some(ClientKind::Cli),
                Some("agent") => Some(ClientKind::Agent),
                Some(other) => return Err(format!("`actor` is window, cli or agent, not `{other}`")),
            };
            let status = match str_arg(args, "status") {
                None => None,
                Some(s) => Some(StatusClass::parse(s).ok_or_else(|| format!("`status` is {}", StatusClass::NAMES))?),
            };
            let limit = args.get("limit").and_then(Value::as_u64).map(|n| n as usize);
            let filter = HistoryFilter { text: str_arg(args, "text").map(String::from), actor, status, limit };
            let ReplyBody::History { entries, matched, .. } = call(core, Command::History { filter })? else {
                return Err(unexpected());
            };
            let list: Vec<Value> = entries.iter().map(entry_json).collect();
            Ok(json!({ "requests": list, "matched": matched }))
        }
        "get_response" => {
            let request_id = id_arg(args)?;
            let max = args.get("max_bytes").and_then(Value::as_u64).map_or(BODY_SHOWN, |n| (n as usize).min(BODY_MAX));
            let ReplyBody::Response { entry, response } = call(core, Command::Response { request_id })? else {
                return Err(unexpected());
            };
            let mut out = entry_json(&entry);
            if let Some(response) = response {
                add_response(&mut out, &response, max);
            }
            Ok(out)
        }
        "save_request" => {
            let request = HttpRequest {
                method: str_arg(args, "method").unwrap_or("GET").to_ascii_uppercase(),
                url: str_arg(args, "url").ok_or("`url` is required")?.to_string(),
                headers: headers_arg(args)?,
                body: str_arg(args, "body").map(String::from),
            };
            let path = str_arg(args, "path").ok_or("`path` is required")?.to_string();
            let ReplyBody::Saved { path } = call(core, Command::SaveRequest { path, request })? else {
                return Err(unexpected());
            };
            Ok(json!({ "path": path, "file": format!("requests/{path}.http") }))
        }
        "cancel_request" => {
            let request_id = id_arg(args)?;
            call(core, Command::Cancel { request_id })?;
            Ok(json!({ "request_id": request_id, "state": "cancelled" }))
        }
        other => Err(format!("no tool `{other}`")),
    }
}

/// Send (`Send` or `Rerun`) and wait for the end, up to `wait_seconds`.
fn send_and_wait(core: &Core, sending: Command, args: &Value) -> Result<Value, String> {
    let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT).min(WAIT_MAX);
    // Events of earlier requests, left from a wait that gave up, aren't ours.
    while core.events.try_recv().is_ok() {}
    let ReplyBody::Accepted { request_id } = call(core, sending)? else {
        return Err(unexpected());
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let mut state = json!({ "request_id": request_id, "state": "running" });
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = match core.events.recv_timeout(left) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                state["note"] = json!(format!(
                    "not finished after {wait} s; it carries on. get_response with request_id {request_id} gives the outcome"
                ));
                return Ok(state);
            }
            Err(RecvTimeoutError::Disconnected) => return Err("the core hung up".into()),
        };
        match event.kind {
            EventKind::RequestStarted { request_id: id, request, env, rerun_of } if id == request_id => {
                state["request"] = request_json(&request);
                if let Some(env) = env {
                    state["env"] = json!(env);
                }
                if let Some(of) = rerun_of {
                    state["rerun_of"] = json!(of);
                }
            }
            EventKind::ApprovalAsked { request_id: id, reason } if id == request_id => {
                // Not waited out here: a person may take minutes.
                state["state"] = json!("awaiting_approval");
                state["reason"] = json!(reason);
                state["note"] = json!(format!(
                    "a person has to allow this in Tracer's window; get_response with request_id {request_id} gives the outcome"
                ));
                return Ok(state);
            }
            EventKind::RequestFinished { request_id: id, response } if id == request_id => {
                state["state"] = json!("done");
                add_response(&mut state, &response, BODY_SHOWN);
                return Ok(state);
            }
            EventKind::RequestFailed { request_id: id, error } if id == request_id => {
                state["state"] = json!("failed");
                state["error"] = json!(error);
                return Ok(state);
            }
            EventKind::RequestCancelled { request_id: id } if id == request_id => {
                state["state"] = json!("cancelled");
                state["cancelled_by"] = json!(event.actor.name);
                return Ok(state);
            }
            _ => {}
        }
    }
}

fn call(core: &Core, command: Command) -> Result<ReplyBody, String> {
    match core.client.call(command) {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(refused)) => Err(refused),
        Err(e) => Err(format!("the core: {e}")),
    }
}

/// Start `tracer core` for `workspace` in a process of its own, so it
/// outlives this session (a window may be using it by then), and connect.
fn start_core(workspace: &Path, name: &str) -> io::Result<Client> {
    let me = std::env::current_exe()?;
    let mut process = Process::new(me);
    process.arg("core").arg("-w").arg(workspace).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Detached, and with no console window of its own.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    keep_pipes_to_ourselves();
    let mut child = process.spawn()?;
    eprintln!("tracer mcp: started a core for {}", workspace.display());
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(client) = Client::connect(workspace, ClientKind::Agent, name) {
            return Ok(client);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(io::Error::other(format!("the core exited ({status})")));
        }
        if Instant::now() > deadline {
            return Err(io::Error::other(format!("the core did not answer within {}s", START_TIMEOUT.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stop a process started from here inheriting this one's stdin, stdout and
/// stderr. On Windows a child inherits every inheritable handle, and the
/// pipes an agent gave this server are; a core holding them would keep the
/// agent's end open after this server exits, and the agent would wait on it.
#[cfg(windows)]
fn keep_pipes_to_ourselves() {
    use std::os::windows::io::AsRawHandle;
    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for handle in [io::stdin().as_raw_handle(), io::stdout().as_raw_handle(), io::stderr().as_raw_handle()] {
        // SAFETY: these are this process's own standard handles, and only
        // their inherit flag changes.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    }
}

/// Elsewhere the child's standard streams are replaced, and the rest are
/// closed on exec.
#[cfg(not(windows))]
fn keep_pipes_to_ourselves() {}

fn request_json(r: &HttpRequest) -> Value {
    let headers: Vec<String> = r.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    let mut v = json!({ "method": r.method, "url": r.url, "headers": headers });
    if let Some(body) = &r.body {
        v["body"] = json!(body);
    }
    v
}

fn entry_json(e: &HistoryEntry) -> Value {
    let mut v = json!({
        "request_id": e.request_id,
        "at_ms": e.at_ms,
        "sent_by": format!("{:?}:{}", e.actor.kind, e.actor.name).to_lowercase(),
        "request": request_json(&e.request),
    });
    if let Some(env) = &e.env {
        v["env"] = json!(env);
    }
    if let Some(of) = e.rerun_of {
        v["rerun_of"] = json!(of);
    }
    match &e.ended {
        Ended::Running => v["state"] = json!("running"),
        Ended::AwaitingApproval { reason } => {
            v["state"] = json!("awaiting_approval");
            v["reason"] = json!(reason);
        }
        Ended::Done { status, total_ms, size } => {
            v["state"] = json!("done");
            v["status"] = json!(status);
            v["total_ms"] = json!(total_ms.round());
            v["size"] = json!(size);
        }
        Ended::Failed { error } => {
            v["state"] = json!("failed");
            v["error"] = json!(error);
        }
        Ended::Cancelled { by } => {
            v["state"] = json!("cancelled");
            v["cancelled_by"] = json!(by.name);
        }
    }
    v
}

fn add_response(v: &mut Value, r: &HttpResponse, max: usize) {
    v["status"] = json!(r.status);
    v["ttfb_ms"] = json!(r.timing.ttfb_ms.round());
    v["total_ms"] = json!(r.timing.total_ms.round());
    v["size"] = json!(r.size);
    let headers: Vec<String> = r.headers.iter().map(|(k, val)| format!("{k}: {val}")).collect();
    v["headers"] = json!(headers);
    let mut cut = r.body.len().min(max);
    while !r.body.is_char_boundary(cut) {
        cut -= 1;
    }
    v["body"] = json!(&r.body[..cut]);
    if cut < r.body.len() {
        v["body_truncated"] =
            json!(format!("showing {cut} of {} bytes; get_response with a larger max_bytes shows more", r.body.len()));
    }
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}

fn id_arg(args: &Value) -> Result<u64, String> {
    args.get("request_id").and_then(Value::as_u64).ok_or_else(|| "`request_id` (a number) is required".to_string())
}

/// `headers` as an object of name to value, or a list of `Name: value`.
fn headers_arg(args: &Value) -> Result<Vec<(String, String)>, String> {
    match args.get("headers") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                other => Ok((k.clone(), other.to_string())),
            })
            .collect(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|line| {
                let line = line.as_str().ok_or("each header is a `Name: value` string")?;
                let (k, v) = line.split_once(':').ok_or_else(|| format!("a header is `Name: value`, not `{line}`"))?;
                Ok((k.trim().to_string(), v.trim().to_string()))
            })
            .collect(),
        Some(_) => Err("`headers` is an object of name to value".into()),
    }
}

fn unexpected() -> String {
    "the core gave an unexpected answer".into()
}

fn tool_error(why: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": why }], "isError": true })
}

fn error_message(id: Value, code: i64, why: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": why } })
}

fn tools() -> Value {
    let wait = json!({ "type": "integer", "description": "How long to wait for the answer, in seconds (default 60, at most 600). Past it the request carries on; get_response gives the outcome." });
    let env = json!({ "type": "string", "description": "The environment whose variables fill `{{name}}`, as list_environments names them." });
    let headers = json!({ "type": "object", "additionalProperties": { "type": "string" }, "description": "Header name to value." });
    let id = json!({ "type": "integer", "description": "The request's number, as history and send results give it." });
    json!([
        {
            "name": "list_requests",
            "description": "The workspace's saved requests: one .http file each under requests/, folders as collections.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "list_environments",
            "description": "The workspace's environments, their variables, and the names of their secrets (never the values).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "send_request",
            "description": "Send an HTTP request through Tracer and wait for its answer: status, timing, headers and body. Anything but GET, HEAD or OPTIONS, or a prod environment, waits for a person to allow it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "description": "GET (the default), POST, PUT, PATCH, DELETE, HEAD or OPTIONS." },
                    "url": { "type": "string", "description": "May use `{{name}}` from `env`." },
                    "headers": headers,
                    "body": { "type": "string" },
                    "env": env,
                    "wait_seconds": wait,
                },
                "required": ["url"],
            },
        },
        {
            "name": "run_saved_request",
            "description": "Send a saved request, by its path as list_requests gives it, and wait for its answer.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" }, "env": env, "wait_seconds": wait },
                "required": ["path"],
            },
        },
        {
            "name": "rerun_request",
            "description": "Send a past request again exactly as it was written, with the environment it had unless `env` names another (\"\" for none), and wait for its answer.",
            "inputSchema": {
                "type": "object",
                "properties": { "request_id": id, "env": env, "wait_seconds": wait },
                "required": ["request_id"],
            },
        },
        {
            "name": "read_history",
            "description": "Past requests, newest first, by anyone: the person in the window, the CLI or agents. Each as written, with how it ended, without its response body (get_response has it).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Found in the method, URL, environment, sender, status or #id." },
                    "actor": { "type": "string", "enum": ["window", "cli", "agent"] },
                    "status": { "type": "string", "enum": ["2xx", "3xx", "4xx", "5xx", "failed", "cancelled", "running"] },
                    "limit": { "type": "integer", "description": "At most this many (default 50)." },
                },
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "get_response",
            "description": "One request from the history, with its whole response once it has finished: status, timing, headers, body.",
            "inputSchema": {
                "type": "object",
                "properties": { "request_id": id, "max_bytes": { "type": "integer", "description": "The most of the body to give (default 32768, at most 1048576)." } },
                "required": ["request_id"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "save_request",
            "description": "Save a request as requests/<path>.http (folders are collections), over any request already there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Such as `albums/get album`." },
                    "method": { "type": "string" },
                    "url": { "type": "string" },
                    "headers": headers,
                    "body": { "type": "string" },
                },
                "required": ["path", "url"],
            },
        },
        {
            "name": "cancel_request",
            "description": "Cancel a request still running or waiting for approval, whoever sent it.",
            "inputSchema": { "type": "object", "properties": { "request_id": id }, "required": ["request_id"] },
        },
    ])
}
