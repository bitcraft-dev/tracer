//! `tracer mcp`: an MCP server on stdio, through which an AI agent drives
//! Tracer.
//!
//! It is a client of the workspace's core like the window and the CLI, as
//! [`ClientKind::Agent`] under the name the agent gives in `initialize`, so
//! everything it does shows in the window as that agent's, live, and goes
//! through the core's guardrails: the workspace's `agents.policy` says where
//! its requests may go, how many and how big, and which wait for a person to
//! allow them (by default, any that changes something or uses a production
//! environment); secrets stay `{{name}}`; a person can switch agents off.
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
    BenchPlan, ClientKind, Command, Direction, Ended, BENCH_BUCKETS_MS, GRAPHQL_INTROSPECTION, Event, EventKind, HistoryEntry, HistoryFilter, HttpRequest, HttpResponse, ReplyBody,
    StatusClass, StreamItem, StreamKind, StreamMessage, UntilPlan, WEBSOCKET,
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
/// How long the stream tools listen for messages unless told otherwise, and
/// how long `read_stream` may wait for new ones.
const LISTEN_DEFAULT: u64 = 2;
const LISTEN_MAX: u64 = 120;

const INSTRUCTIONS: &str = "\
Tracer is an API workbench. These tools send HTTP requests through the \
workspace's Tracer core, read its saved requests, environments and history, \
and save requests. A person may be watching in Tracer's window, where \
everything you do appears under your name.

`{{name}}` in a URL, header or body is filled from the environment named by \
`env` when the request is sent. Secrets are environment variables whose \
values you never see: use `{{name}}`, and wherever a secret's value comes \
back in a response it reads `{{name}}` again.

By default, any request other than GET, HEAD or OPTIONS, and any request \
sent with an environment whose name starts with `prod`, waits for a person \
to allow it in the window. Such a send returns `awaiting_approval`; call \
get_response later for the outcome. A refused request fails with `refused by \
<person>`. The workspace's policy may say otherwise for you, and may limit \
where your requests go, how many you start and how big their bodies are: \
read_policy says what applies to you.

Streams: open_stream opens a WebSocket (ws:// or wss://) or reads server-sent \
events (an http(s) URL answered with text/event-stream). It stays open, its \
messages numbered both ways, until close_stream (or a person) closes it: \
read_stream gives messages after a number, send_stream_message sends on a \
WebSocket. Opening a WebSocket counts as a write for approval.";

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
                expect: list_arg(args, "expect"),
                capture: Vec::new(),
            };
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            send_and_wait(core, Command::Send { request, env }, args)
        }
        "send_graphql" => {
            let query = str_arg(args, "query").ok_or("`query` is required")?.trim().to_string();
            let body = match args.get("variables") {
                None | Some(Value::Null) => query,
                Some(Value::String(text)) if text.trim().is_empty() => query,
                Some(Value::String(text)) => {
                    let vars: Value = serde_json::from_str(text).map_err(|e| format!("`variables` is not JSON: {e}"))?;
                    format!("{query}\n\n{vars}")
                }
                Some(vars) => format!("{query}\n\n{vars}"),
            };
            let request = HttpRequest {
                method: "GRAPHQL".into(),
                url: str_arg(args, "url").ok_or("`url` is required")?.to_string(),
                headers: headers_arg(args)?,
                body: Some(body),
                expect: list_arg(args, "expect"),
                capture: Vec::new(),
            };
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            send_and_wait(core, Command::Send { request, env }, args)
        }
        "graphql_schema" => {
            let request = HttpRequest {
                method: "GRAPHQL".into(),
                url: str_arg(args, "url").ok_or("`url` is required")?.to_string(),
                headers: headers_arg(args)?,
                body: Some(GRAPHQL_INTROSPECTION.into()),
                ..Default::default()
            };
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            let sent = send_and_wait(core, Command::Send { request, env }, args)?;
            let request_id = sent["request_id"].as_u64().ok_or_else(unexpected)?;
            if sent["state"] != "done" {
                return Ok(sent);
            }
            let ReplyBody::GraphqlSchema { schema } = call(core, Command::GraphqlSchema { request_id })? else {
                return Err(unexpected());
            };
            Ok(json!({ "request_id": request_id, "types": schema.types.len(), "sdl": schema.to_sdl() }))
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
            let ReplyBody::Response { entry, response, checks } = call(core, Command::Response { request_id })? else {
                return Err(unexpected());
            };
            let mut out = entry_json(&entry);
            if let Some(response) = response {
                add_response(&mut out, &response, max);
            }
            if !checks.is_empty() {
                out["checks"] = serde_json::to_value(&checks).unwrap_or_default();
            }
            Ok(out)
        }
        "save_request" => {
            let request = HttpRequest {
                method: str_arg(args, "method").unwrap_or("GET").to_ascii_uppercase(),
                url: str_arg(args, "url").ok_or("`url` is required")?.to_string(),
                headers: headers_arg(args)?,
                body: str_arg(args, "body").map(String::from),
                expect: list_arg(args, "expect"),
                capture: list_arg(args, "capture"),
            };
            let path = str_arg(args, "path").ok_or("`path` is required")?.to_string();
            let ReplyBody::Saved { path } = call(core, Command::SaveRequest { path, request })? else {
                return Err(unexpected());
            };
            Ok(json!({ "path": path, "file": format!("requests/{path}.http") }))
        }
        "diff_responses" => {
            let id = |name: &str| {
                args.get(name).and_then(Value::as_u64).ok_or_else(|| format!("`{name}` (a request number) is required"))
            };
            let ignore = list_arg(args, "ignore");
            let no_defaults = args.get("all").and_then(Value::as_bool).unwrap_or(false);
            let ReplyBody::Diff { diff } = call(core, Command::Diff { a: id("a")?, b: id("b")?, ignore, no_defaults })? else {
                return Err(unexpected());
            };
            serde_json::to_value(diff).map_err(|e| e.to_string())
        }
        "run_flow" => {
            let path = str_arg(args, "path").ok_or("`path` is required")?.to_string();
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            let keep_going = args.get("keep_going").and_then(Value::as_bool).unwrap_or(false);
            let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT * 2).min(WAIT_MAX);
            while core.events.try_recv().is_ok() {}
            let ReplyBody::FlowStarted { flow_id, steps } = call(core, Command::RunFlow { path, env, keep_going })? else {
                return Err(unexpected());
            };
            let deadline = Instant::now() + Duration::from_secs(wait);
            let mut answers: Vec<(u64, u16, f64)> = Vec::new();
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let event = match core.events.recv_timeout(left) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        return Ok(json!({
                            "flow_id": flow_id,
                            "steps": steps,
                            "state": "running",
                            "note": format!("not finished after {wait} s; it carries on, and read_history shows its steps"),
                        }));
                    }
                    Err(RecvTimeoutError::Disconnected) => return Err("the core hung up".into()),
                };
                match event.kind {
                    EventKind::RequestFinished { request_id, response } => {
                        answers.push((request_id, response.status, response.timing.total_ms))
                    }
                    EventKind::FlowFinished { flow_id: f, passed, steps } if f == flow_id => {
                        let steps: Vec<Value> = steps
                            .iter()
                            .map(|s| {
                                let mut v = serde_json::to_value(s).unwrap_or_default();
                                if let Some((_, status, ms)) = s.request_id.and_then(|id| answers.iter().find(|(r, _, _)| *r == id)) {
                                    v["status"] = json!(status);
                                    v["total_ms"] = json!(ms.round());
                                }
                                v
                            })
                            .collect();
                        return Ok(json!({ "flow_id": flow_id, "passed": passed, "steps": steps }));
                    }
                    _ => {}
                }
            }
        }
        "run_bench" => {
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
            let total = args.get("total").and_then(Value::as_u64);
            let duration_secs = args.get("duration_seconds").and_then(Value::as_u64);
            let plan = BenchPlan {
                total: if total.is_none() && duration_secs.is_none() { Some(100) } else { total },
                concurrency: args.get("concurrency").and_then(Value::as_u64).unwrap_or(10) as u32,
                duration_secs,
                rate: args.get("rate").and_then(Value::as_f64),
            };
            let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT * 2).min(WAIT_MAX);
            while core.events.try_recv().is_ok() {}
            let ReplyBody::BenchStarted { bench_id } = call(core, Command::RunBench { request: found.request, env, plan })? else {
                return Err(unexpected());
            };
            let deadline = Instant::now() + Duration::from_secs(wait);
            let mut last = None;
            // Its first request, and why it waits for a person, if it does.
            let mut first = None;
            let mut asked: Option<String> = None;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let event = match core.events.recv_timeout(left) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(reason) = asked {
                            return Ok(json!({
                                "bench_id": bench_id,
                                "state": "awaiting_approval",
                                "note": format!("waiting for a person to allow it ({reason}); nothing has been sent yet. It runs once allowed; stop_bench withdraws it"),
                            }));
                        }
                        return Ok(json!({
                            "bench_id": bench_id,
                            "state": "running",
                            "so_far": last,
                            "note": format!("not finished after {wait} s; it carries on, and stop_bench stops it"),
                        }));
                    }
                    Err(RecvTimeoutError::Disconnected) => return Err("the core hung up".into()),
                };
                match event.kind {
                    EventKind::RequestStarted { request_id, bench: Some(b), .. } if b == bench_id => first = Some(request_id),
                    EventKind::ApprovalAsked { request_id, reason } if Some(request_id) == first => asked = Some(reason),
                    EventKind::ApprovalGiven { request_id, .. } if Some(request_id) == first => asked = None,
                    EventKind::BenchProgress { bench_id: b, stats } if b == bench_id => last = Some(stats),
                    EventKind::BenchFinished { bench_id: b, stats, stopped } if b == bench_id => {
                        let mut out = json!({
                            "bench_id": bench_id,
                            "state": if stopped.is_some() { "stopped" } else { "finished" },
                            "stats": stats,
                            "histogram_bounds_ms": BENCH_BUCKETS_MS,
                        });
                        if let Some(why) = stopped {
                            out["stopped"] = json!(why);
                        }
                        if let Some(name) = str_arg(args, "baseline") {
                            let tolerance_pct = args.get("tolerance_pct").and_then(Value::as_f64);
                            let command = Command::CompareBench { bench_id, baseline: Some(name.into()), other_bench: None, tolerance_pct };
                            out["comparison"] = match call(core, command)? {
                                ReplyBody::BenchCompared { comparison } => serde_json::to_value(comparison).unwrap_or_default(),
                                _ => return Err(unexpected()),
                            };
                        }
                        if let Some(name) = str_arg(args, "save_baseline") {
                            out["baseline_saved"] = match call(core, Command::SaveBaseline { bench_id, name: name.into() })? {
                                ReplyBody::BaselineSaved { path, .. } => json!(path),
                                _ => return Err(unexpected()),
                            };
                        }
                        return Ok(out);
                    }
                    _ => {}
                }
            }
        }
        "compare_bench" => {
            let bench_id = args.get("bench_id").and_then(Value::as_u64).ok_or("`bench_id` is required")?;
            let command = Command::CompareBench {
                bench_id,
                baseline: str_arg(args, "baseline").map(String::from),
                other_bench: args.get("other_bench").and_then(Value::as_u64),
                tolerance_pct: args.get("tolerance_pct").and_then(Value::as_f64),
            };
            match call(core, command)? {
                ReplyBody::BenchCompared { comparison } => Ok(serde_json::to_value(comparison).unwrap_or_default()),
                _ => Err(unexpected()),
            }
        }
        "save_baseline" => {
            let bench_id = args.get("bench_id").and_then(Value::as_u64).ok_or("`bench_id` is required")?;
            let name = str_arg(args, "name").ok_or("`name` is required")?.to_string();
            match call(core, Command::SaveBaseline { bench_id, name })? {
                ReplyBody::BaselineSaved { name, path } => Ok(json!({ "name": name, "path": path })),
                _ => Err(unexpected()),
            }
        }
        "list_baselines" => match call(core, Command::ListBaselines)? {
            ReplyBody::Baselines { baselines, skipped } => {
                let list: Vec<Value> = baselines
                    .iter()
                    .map(|b| {
                        json!({
                            "name": b.name,
                            "bench_id": b.bench_id,
                            "request": request_json(&b.request),
                            "env": b.env,
                            "plan": b.plan,
                            "p50_ms": b.stats.p50_ms,
                            "p95_ms": b.stats.p95_ms,
                            "p99_ms": b.stats.p99_ms,
                            "per_sec": b.stats.per_sec,
                            "error_pct": b.stats.error_pct(),
                        })
                    })
                    .collect();
                Ok(json!({ "baselines": list, "skipped": skipped }))
            }
            _ => Err(unexpected()),
        },
        "stop_bench" => {
            let bench_id = args.get("bench_id").and_then(Value::as_u64).ok_or("`bench_id` is required")?;
            call(core, Command::StopBench { bench_id })?;
            Ok(json!({ "bench_id": bench_id, "state": "stopping" }))
        }
        "run_until" => {
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
            let plan = UntilPlan {
                streak: args.get("streak").and_then(Value::as_u64).unwrap_or(3) as u32,
                matching: str_arg(args, "matching").filter(|m| !m.trim().is_empty()).map(String::from),
                max: args.get("max").and_then(Value::as_u64),
                gap_ms: args.get("gap_ms").and_then(Value::as_u64),
            };
            let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT * 2).min(WAIT_MAX);
            while core.events.try_recv().is_ok() {}
            let ReplyBody::UntilStarted { until_id } = call(core, Command::RunUntil { request: found.request, env, plan })? else {
                return Err(unexpected());
            };
            let deadline = Instant::now() + Duration::from_secs(wait);
            // Each answer as it came: its request, status or error, and whether it counted.
            let mut ends: Vec<(u64, Value)> = Vec::new();
            let mut answers: Vec<Value> = Vec::new();
            let (mut sent, mut streak) = (0, 0);
            let mut asked: Option<(u64, String)> = None;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let event = match core.events.recv_timeout(left) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        let note = match asked {
                            Some((request_id, reason)) => format!("request #{request_id} waits for a person to allow it ({reason}); stop_until withdraws the run"),
                            None => format!("not finished after {wait} s; it carries on, and stop_until stops it"),
                        };
                        let last: Vec<&Value> = answers.iter().rev().take(20).rev().collect();
                        return Ok(json!({ "until_id": until_id, "state": "running", "sent": sent, "streak": streak, "last_answers": last, "note": note }));
                    }
                    Err(RecvTimeoutError::Disconnected) => return Err("the core hung up".into()),
                };
                match event.kind {
                    EventKind::RequestFinished { request_id, response } => ends.push((request_id, json!(response.status))),
                    EventKind::RequestFailed { request_id, error } => ends.push((request_id, json!(error))),
                    EventKind::ApprovalAsked { request_id, reason } => asked = Some((request_id, reason)),
                    EventKind::ApprovalGiven { .. } => asked = None,
                    EventKind::UntilProgress { until_id: u, request_id, counted, sent: s, streak: k } if u == until_id => {
                        (sent, streak) = (s, k);
                        let end = ends.iter().position(|(id, _)| *id == request_id).map(|at| ends.remove(at).1);
                        let mut answer = json!({ "request_id": request_id, "counted": counted, "streak": k });
                        match end {
                            Some(Value::Number(status)) => answer["status"] = Value::Number(status),
                            Some(error) => answer["error"] = error,
                            None => {}
                        }
                        answers.push(answer);
                    }
                    EventKind::UntilFinished { until_id: u, sent, streak, reached, stopped } if u == until_id => {
                        // The last answers, which hold the streak; history has every one.
                        let last: Vec<&Value> = answers.iter().rev().take(20).rev().collect();
                        let mut out = json!({
                            "until_id": until_id,
                            "state": if reached { "reached" } else { "ended" },
                            "sent": sent,
                            "streak": streak,
                            "last_answers": last,
                        });
                        if let Some(why) = stopped {
                            out["stopped"] = json!(why);
                        }
                        return Ok(out);
                    }
                    _ => {}
                }
            }
        }
        "list_cookies" => match call(core, Command::Cookies)? {
            ReplyBody::Cookies { browser, cookies } => Ok(json!({ "browser": browser, "cookies": cookies })),
            _ => Err(unexpected()),
        },
        "stop_until" => {
            let until_id = args.get("until_id").and_then(Value::as_u64).ok_or("`until_id` is required")?;
            call(core, Command::StopUntil { until_id })?;
            Ok(json!({ "until_id": until_id, "state": "stopping" }))
        }
        "read_policy" => {
            let ReplyBody::Policy { rules, error } = call(core, Command::Policy)? else {
                return Err(unexpected());
            };
            let mut out = json!({ "rules": rules.first() });
            if let Some(error) = error {
                out["note"] = json!(format!("the policy file has a mistake, so the last rules taken stand: {error}"));
            }
            Ok(out)
        }
        "open_stream" => {
            let url = str_arg(args, "url").ok_or("`url` is required")?.to_string();
            let websocket = url.starts_with("ws://") || url.starts_with("wss://");
            let mut headers = headers_arg(args)?;
            if !websocket && !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("accept")) {
                headers.push(("Accept".into(), "text/event-stream".into()));
            }
            let body = list_arg(args, "messages").iter().map(|m| format!("===\n{m}\n")).collect::<String>();
            let request = HttpRequest {
                method: if websocket { WEBSOCKET.into() } else { "GET".into() },
                url,
                headers,
                body: (!body.is_empty()).then_some(body),
                ..Default::default()
            };
            let env = str_arg(args, "env").filter(|e| !e.is_empty()).map(String::from);
            open_and_listen(core, Command::Send { request, env }, args)
        }
        "read_stream" => {
            let request_id = id_arg(args)?;
            let after = args.get("after").and_then(Value::as_u64).unwrap_or(0);
            let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(0).min(LISTEN_MAX);
            while core.events.try_recv().is_ok() {}
            let read = || match call(core, Command::StreamMessages { request_id, after, limit: None })? {
                ReplyBody::StreamMessages { messages, open, more } => Ok((messages, open, more)),
                _ => Err(unexpected()),
            };
            // Until there's something new, it has closed, or the wait is up;
            // one still connecting (just allowed, say) is waited for too.
            let deadline = Instant::now() + Duration::from_secs(wait);
            let (messages, open, more) = loop {
                let now = read();
                let waited = Instant::now() >= deadline;
                match now {
                    Ok((messages, open, more)) if !messages.is_empty() || !open || waited => break (messages, open, more),
                    Err(e) if waited => return Err(e),
                    _ => {}
                }
                while let Ok(event) = core.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    if concerns(&event.kind, request_id) {
                        break;
                    }
                }
            };
            let next = messages.last().map_or(after, |m| m.message.n);
            Ok(json!({
                "request_id": request_id,
                "open": open,
                "messages": messages.iter().map(item_json).collect::<Vec<_>>(),
                "more": more,
                "next_after": next,
            }))
        }
        "send_stream_message" => {
            let request_id = id_arg(args)?;
            let text = str_arg(args, "text").ok_or("`text` is required")?.to_string();
            while core.events.try_recv().is_ok() {}
            call(core, Command::StreamSend { request_id, text })?;
            let listen = args.get("listen_seconds").and_then(Value::as_u64).unwrap_or(LISTEN_DEFAULT).min(LISTEN_MAX);
            let mut state = json!({ "request_id": request_id, "open": true });
            let messages = listen_to(core, request_id, Duration::from_secs(listen), &mut state);
            state["messages"] = json!(messages);
            Ok(state)
        }
        "close_stream" => {
            let request_id = id_arg(args)?;
            while core.events.try_recv().is_ok() {}
            call(core, Command::Cancel { request_id })?;
            let mut state = json!({ "request_id": request_id, "open": true });
            listen_to(core, request_id, Duration::from_secs(10), &mut state);
            Ok(state)
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
    // Known from its start: whether a `request_checked` follows its end.
    let mut checked = false;
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
            EventKind::RequestStarted { request_id: id, request, env, rerun_of, .. } if id == request_id => {
                checked = request.is_checked();
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
                if !checked {
                    return Ok(state);
                }
            }
            EventKind::RequestFailed { request_id: id, error } if id == request_id => {
                state["state"] = json!("failed");
                state["error"] = json!(error);
                if !checked {
                    return Ok(state);
                }
            }
            // Published right after the end, under the same lock.
            EventKind::RequestChecked { request_id: id, results } if id == request_id => {
                let failed = results.iter().filter(|r| !r.pass).count();
                state["checks"] = serde_json::to_value(&results).unwrap_or_default();
                state["checks_failed"] = json!(failed);
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

/// Open a stream and listen to it for `listen_seconds`; or, if it never
/// opens as one, what [`send_and_wait`] would say.
fn open_and_listen(core: &Core, sending: Command, args: &Value) -> Result<Value, String> {
    let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT).min(WAIT_MAX);
    while core.events.try_recv().is_ok() {}
    let ReplyBody::Accepted { request_id } = call(core, sending)? else {
        return Err(unexpected());
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let mut state = json!({ "request_id": request_id, "state": "connecting" });
    loop {
        let event = match core.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                state["note"] = json!(format!("not open after {wait} s; get_response with request_id {request_id} gives the outcome"));
                return Ok(state);
            }
            Err(RecvTimeoutError::Disconnected) => return Err("the core hung up".into()),
        };
        match event.kind {
            EventKind::StreamOpened { request_id: id, kind, response } if id == request_id => {
                state["state"] = json!("open");
                state["open"] = json!(true);
                state["kind"] = json!(if kind == StreamKind::Websocket { "websocket" } else { "sse" });
                state["status"] = json!(response.status);
                state["ttfb_ms"] = json!(response.timing.ttfb_ms.round());
                let listen = args.get("listen_seconds").and_then(Value::as_u64).unwrap_or(LISTEN_DEFAULT).min(LISTEN_MAX);
                let messages = listen_to(core, request_id, Duration::from_secs(listen), &mut state);
                state["next_after"] = json!(messages.last().and_then(|m| m["n"].as_u64()).unwrap_or(0));
                state["messages"] = json!(messages);
                state["note"] = json!("open until close_stream; read_stream gives what comes after next_after");
                return Ok(state);
            }
            EventKind::ApprovalAsked { request_id: id, reason } if id == request_id => {
                state["state"] = json!("awaiting_approval");
                state["reason"] = json!(reason);
                state["note"] = json!(format!(
                    "a person has to allow this in Tracer's window; read_stream with request_id {request_id} follows it once open"
                ));
                return Ok(state);
            }
            EventKind::RequestFinished { request_id: id, response } if id == request_id => {
                state["state"] = json!("done");
                state["note"] = json!("answered without opening a stream");
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

/// Stream `request_id`'s messages for `listen`, or until it closes, which
/// `state` then says.
fn listen_to(core: &Core, request_id: u64, listen: Duration, state: &mut Value) -> Vec<Value> {
    let deadline = Instant::now() + listen;
    let mut messages = Vec::new();
    while let Ok(event) = core.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        match event.kind {
            EventKind::StreamMessage { request_id: id, message } if id == request_id => {
                messages.push(message_json(&message, &event.actor.name, event.at_ms));
            }
            EventKind::StreamClosed { request_id: id, reason, received, sent } if id == request_id => {
                state["open"] = json!(false);
                state["closed"] = json!({ "reason": reason, "received": received, "sent": sent });
                break;
            }
            // Ended before it opened: cancelled, refused, or failed.
            EventKind::RequestCancelled { request_id: id } if id == request_id => {
                state["open"] = json!(false);
                state["state"] = json!("cancelled");
                break;
            }
            EventKind::RequestFailed { request_id: id, error } if id == request_id => {
                state["open"] = json!(false);
                state["state"] = json!("failed");
                state["error"] = json!(error);
                break;
            }
            _ => {}
        }
    }
    messages
}

/// Whether `kind` is news of request `request_id`'s stream.
fn concerns(kind: &EventKind, request_id: u64) -> bool {
    match kind {
        EventKind::StreamOpened { request_id: id, .. }
        | EventKind::StreamMessage { request_id: id, .. }
        | EventKind::StreamClosed { request_id: id, .. }
        | EventKind::RequestFinished { request_id: id, .. }
        | EventKind::RequestFailed { request_id: id, .. }
        | EventKind::RequestCancelled { request_id: id } => *id == request_id,
        _ => false,
    }
}

fn message_json(m: &StreamMessage, by: &str, at_ms: u64) -> Value {
    let mut v = json!({
        "n": m.n,
        "dir": if m.dir == Direction::In { "in" } else { "out" },
        "text": m.text,
        "size": m.size,
        "by": by,
        "at_ms": at_ms,
    });
    if m.binary {
        v["binary"] = json!(true);
    }
    if let Some(event) = &m.event {
        v["event"] = json!(event);
    }
    if let Some(id) = &m.id {
        v["id"] = json!(id);
    }
    v
}

fn item_json(item: &StreamItem) -> Value {
    message_json(&item.message, &item.by.name, item.at_ms)
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
    if !r.expect.is_empty() {
        v["expect"] = json!(r.expect);
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
    if let Some((passed, failed)) = e.checks {
        v["checks_passed"] = json!(passed);
        v["checks_failed"] = json!(failed);
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
    // Where the time went, each phase its own length, when it was measured.
    if r.timing.dns_ms.is_some() {
        let ms = |x: Option<f64>| x.map(|x| (x * 10.0).round() / 10.0);
        v["phases_ms"] = json!({
            "dns": ms(r.timing.dns_ms),
            "connect": ms(r.timing.connect_ms),
            "tls": ms(r.timing.tls_ms),
            "wait": ms(Some(r.timing.wait_ms())),
            "download": ms(Some(r.timing.total_ms - r.timing.ttfb_ms)),
        });
    }
    if r.tls_unchecked {
        v["tls_unchecked"] = json!(true);
    }
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

/// A list of strings, or one string as a list of one.
fn list_arg(args: &Value, name: &str) -> Vec<String> {
    match args.get(name) {
        Some(Value::Array(list)) => list.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Some(Value::String(one)) if !one.trim().is_empty() => vec![one.clone()],
        _ => Vec::new(),
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
    let expect = json!({ "type": "array", "items": { "type": "string" }, "description": "What the answer should be, one each, checked when it arrives: `status 2xx`, `status == 201`, `time < 500` (ms), `header Content-Type contains json`, `$.user.id == 1`, `$.items[0].name contains Kam`, `len($.items) >= 3`, `$.token exists`, `body contains ok`. The result lists each with pass and what was found." });
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
            "description": "Send an HTTP request through Tracer and wait for its answer: status, timing, headers and body. By default anything but GET, HEAD or OPTIONS, or a prod environment, waits for a person to allow it; read_policy says what applies to you.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "description": "GET (the default), POST, PUT, PATCH, DELETE, HEAD or OPTIONS." },
                    "url": { "type": "string", "description": "May use `{{name}}` from `env`." },
                    "headers": headers,
                    "body": { "type": "string" },
                    "env": env,
                    "expect": expect,
                    "wait_seconds": wait,
                },
                "required": ["url"],
            },
        },
        {
            "name": "send_graphql",
            "description": "Send a GraphQL query or mutation (sent as a JSON POST) and wait for the answer. The answer is checked for GraphQL `errors`, which servers send with a 200: see `checks`. For the agent policy a query reads and a mutation writes. Saved, it's a `GRAPHQL <url>` request whose body is the query, then its variables JSON after a blank line.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "query": { "type": "string" },
                    "variables": { "type": "object", "description": "The query's variables, as JSON." },
                    "headers": headers,
                    "env": env,
                    "expect": expect,
                    "wait_seconds": wait,
                },
                "required": ["url", "query"],
            },
        },
        {
            "name": "graphql_schema",
            "description": "A GraphQL server's schema as SDL, asked for by introspection (an ordinary request, in history).",
            "inputSchema": {
                "type": "object",
                "properties": { "url": { "type": "string" }, "headers": headers, "env": env, "wait_seconds": wait },
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
                    "expect": expect,
                    "capture": { "type": "array", "items": { "type": "string" }, "description": "For a flow (run_flow): what to take from the answer for the requests after it, such as `token = $.access_token`, `next = header Location`, `code = status`; later requests use `{{token}}`." },
                },
                "required": ["path", "url"],
            },
        },
        {
            "name": "run_flow",
            "description": "Run the saved requests under a folder (list_requests shows the paths) in path order, as one flow: each step's captures (`# @capture token = $.access_token` in its file, or save_request's `capture`) fill `{{token}}` in the steps after it. A step fails with no answer, a failed check, or (without checks) a status of 400 or more; the steps after a failure are skipped unless keep_going. Each step is an ordinary request, so approvals and the policy apply to each. Gives each step's outcome, status and what it captured (names only).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "A folder under requests/, such as `signup`, or one request's path." },
                    "env": env,
                    "keep_going": { "type": "boolean" },
                    "wait_seconds": { "type": "integer", "description": "How long to wait for the end (default 120, at most 600)." },
                },
                "required": ["path"],
            },
        },
        {
            "name": "run_bench",
            "description": "Benchmark a saved request: send it `total` times (default 100), `concurrency` at a time (default 10), or for `duration_seconds`, no faster than `rate` a second. The first is an ordinary request in the history (with its waterfall); the rest are counted. By default a person approves an agent's bench before it starts, and the policy may cap max_bench and max_concurrency (read_policy). Gives latency min/mean/p50/p95/p99/max in ms, requests a second, statuses, errors and a histogram (counts per bucket of histogram_bounds_ms, one more for slower).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "A saved request's path, as list_requests gives it." },
                    "env": env,
                    "total": { "type": "integer" },
                    "concurrency": { "type": "integer" },
                    "duration_seconds": { "type": "integer" },
                    "rate": { "type": "number", "description": "The most requests to start a second." },
                    "baseline": { "type": "string", "description": "Compare the finished bench with this baseline (list_baselines); `comparison.regressed` says whether anything got worse past the tolerance." },
                    "tolerance_pct": { "type": "number", "description": "How much worse counts as a regression, in percent (default 20)." },
                    "save_baseline": { "type": "string", "description": "Keep the finished bench as a baseline of this name (baselines/<name>.json)." },
                    "wait_seconds": { "type": "integer", "description": "How long to wait for the end (default 120, at most 600)." },
                },
                "required": ["path"],
            },
        },
        {
            "name": "compare_bench",
            "description": "Compare a finished bench with a baseline or with another finished bench (the same request against two environments, say): mean, p50, p95, p99, requests a second, and the shares of errors and of rate-limited answers, each flagged worse or better past the tolerance. `notes` says what makes them less comparable (different plans, environments, a bench stopped early).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bench_id": { "type": "integer", "description": "The bench to judge." },
                    "baseline": { "type": "string", "description": "A baseline's name; or give other_bench." },
                    "other_bench": { "type": "integer", "description": "The bench to judge it against." },
                    "tolerance_pct": { "type": "number", "description": "In percent, default 20." },
                },
                "required": ["bench_id"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "save_baseline",
            "description": "Keep a finished bench as a baseline (baselines/<name>.json in the workspace, replacing one of that name), to compare later benches with.",
            "inputSchema": {
                "type": "object",
                "properties": { "bench_id": { "type": "integer" }, "name": { "type": "string", "description": "One word: main, before-cache." } },
                "required": ["bench_id", "name"],
            },
        },
        {
            "name": "list_baselines",
            "description": "The workspace's baselines: each bench kept, its request, environment, plan and headline numbers.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "stop_bench",
            "description": "Stop a running bench, whoever started it.",
            "inputSchema": { "type": "object", "properties": { "bench_id": { "type": "integer" } }, "required": ["bench_id"] },
        },
        {
            "name": "run_until",
            "description": "Send a saved request again and again, one at a time, until `streak` answers in a row count (default 3): answers `matching` passes on (an expectation such as `status == 401`), or without it, failures (no answer, a failed check, or with no checks a status of 400 or more). Any other answer starts the count again. At most `max` sends (default 1000), `gap_ms` apart. Every send is an ordinary request in the history, held to the policy and approvals like any send. Gives whether the streak came (`reached`), how many were sent, why it ended otherwise, and the last answers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "A saved request's path, as list_requests gives it." },
                    "env": env,
                    "streak": { "type": "integer", "description": "How many counted answers in a row end it (default 3)." },
                    "matching": { "type": "string", "description": "An expectation an answer must pass to count, as in a request's checks: status == 401, status 5xx, $.error == locked. Without it, failures count." },
                    "max": { "type": "integer", "description": "The most to send (default 1000)." },
                    "gap_ms": { "type": "integer", "description": "Milliseconds to wait after an answer before the next send." },
                    "wait_seconds": { "type": "integer", "description": "How long to wait for the end (default 120, at most 600)." },
                },
                "required": ["path"],
            },
        },
        {
            "name": "list_cookies",
            "description": "How the workspace's requests behave like a browser's (`cookies`: the jar is kept and sent back; `origin`: requests go as from a page there, with a CORS preflight where a browser sends one and a `CORS` check on the answer); `unchecked_tls`: hosts whose TLS certificates are accepted unchecked, an answer from one carrying `tls_unchecked`, and the cookies in the jar: names, domains, paths, expiry and flags, never values. A person switches these on (`tracer browser`).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "stop_until",
            "description": "Stop a running run-until, whoever started it.",
            "inputSchema": { "type": "object", "properties": { "until_id": { "type": "integer" } }, "required": ["until_id"] },
        },
        {
            "name": "diff_responses",
            "description": "How two past responses differ: status, headers, and the body (JSON compared as values and named by path, such as $.data[3].name; anything else line by line). Headers and keys that change on every call (Date, ETag, request ids, timestamp, …) are ignored by default, and every difference ignored is listed in `ignored`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": id,
                    "b": id,
                    "ignore": { "type": "array", "items": { "type": "string" }, "description": "More to ignore: a path ($.meta.time, $.items[*].id), a bare key anywhere (updatedAt), or header:name (a trailing * matches any ending)." },
                    "all": { "type": "boolean", "description": "Don't ignore the default noisy headers and keys." },
                },
                "required": ["a", "b"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "read_policy",
            "description": "What the workspace's policy lets you do: `access` (read-only, local: this machine only, or anywhere), what waits for a person's approval, how many requests you may start (`rate`: count per per_secs), the largest request body (`max_body`, bytes), and how big a bench may be (`max_bench` requests, `max_concurrency` at a time).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "open_stream",
            "description": "Open a WebSocket (a ws:// or wss:// URL) or read server-sent events (an http(s) URL answered with text/event-stream), and listen for a moment. It stays open until close_stream; every message, either way, is numbered and shows in Tracer's window. Opening a WebSocket counts as a write: by default a person must allow it (state awaiting_approval).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "ws:// or wss:// for a WebSocket, http(s):// for server-sent events. May use `{{name}}` from `env`." },
                    "headers": headers,
                    "messages": { "type": "array", "items": { "type": "string" }, "description": "For a WebSocket: messages to send, in order, once it's open." },
                    "env": env,
                    "listen_seconds": { "type": "integer", "description": "How long to collect messages once open before returning (default 2, at most 120)." },
                    "wait_seconds": wait,
                },
                "required": ["url"],
            },
        },
        {
            "name": "read_stream",
            "description": "A stream's messages numbered after `after`, up to 500 at a time, and whether it is still open. With wait_seconds, waits that long for a new one when there is none yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": id,
                    "after": { "type": "integer", "description": "The last message number already read (next_after from before); 0 for all." },
                    "wait_seconds": { "type": "integer", "description": "How long to wait for a new message if there is none (default 0, at most 120)." },
                },
                "required": ["request_id"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "send_stream_message",
            "description": "Send a text message on an open WebSocket, then return what comes in during listen_seconds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": id,
                    "text": { "type": "string" },
                    "listen_seconds": { "type": "integer", "description": "How long to collect what comes back (default 2, at most 120)." },
                },
                "required": ["request_id", "text"],
            },
        },
        {
            "name": "close_stream",
            "description": "Close an open WebSocket or event stream. It ends as a finished request; the result says why it closed and how many messages went each way.",
            "inputSchema": { "type": "object", "properties": { "request_id": id }, "required": ["request_id"] },
        },
        {
            "name": "cancel_request",
            "description": "Cancel a request still running or waiting for approval, whoever sent it.",
            "inputSchema": { "type": "object", "properties": { "request_id": id }, "required": ["request_id"] },
        },
    ])
}
