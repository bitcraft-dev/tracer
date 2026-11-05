//! The window's connection to the workspace's core.
//!
//! [`connect`] finds the core through the workspace's discovery file, or
//! starts `tracer core` for it when there is none, and subscribes from the
//! start of the journal, so the window shows everything that happened while
//! it was closed. From then on every event the core publishes is pushed onto
//! [`EVENTS`], in the core's order: a `rux::Channel`, because each one must
//! arrive (FINDINGS.md AW-011). The state of the connection itself is
//! [`LINK`], a signal, where only the latest value matters.
//!
//! [`close`] stops the core if this window started it, as `docs/PLAN.md` asks,
//! and otherwise only hangs up.

use std::env;
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracer_protocol::client::{Client, Incoming, Shared};
use tracer_protocol::{
    BenchPlan, BenchStats, ClientKind, Command, Direction, Environment, Event, EventKind, ExportLang, HttpRequest, ReplyBody, SavedRequest,
    StepOutcome, StreamKind, UntilPlan, Browser, BENCH_BUCKETS_MS, GRAPHQL_INTROSPECTION,
};

/// The most of a response body an event hands the page. The core and its
/// journal keep the whole body, which the viewer fetches (`view.rs`).
const BODY_SHOWN: usize = 64 * 1024;

/// How long a core that this window started gets to answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Set on a core this window starts. A process that has it never starts a
/// core of its own, so a wrong `tracer` binary cannot multiply.
const STARTED_BY_WINDOW: &str = "TRACER_STARTED_BY_WINDOW";

#[rux::export]
#[derive(Clone)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// One event from the core, flattened for the page: fields that the event's
/// `kind` does not have are empty.
#[rux::export]
#[derive(Clone)]
pub struct CoreEvent {
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// `window`, `cli` or `agent`.
    pub actor_kind: String,
    pub actor: String,
    /// `client_joined`, `client_left`, `request_started`, `request_finished`,
    /// `request_failed`, `request_cancelled`, `request_saved`,
    /// `request_deleted`, `environment_changed`, `approval_asked`,
    /// `approval_given`, `agents_switched`, `policy_changed`,
    /// `stream_opened`, `stream_message`, `stream_closed` or
    /// `core_stopping`.
    pub kind: String,
    /// A saved request's path, for `request_saved` and `request_deleted`.
    pub path: String,
    /// The environment a started request was sent with, or "".
    pub env: String,
    pub client: u64,
    pub request_id: u64,
    /// The request a started one sends again, or 0.
    pub rerun_of: u64,
    pub method: String,
    pub url: String,
    /// What was sent with a started request, or received with a finished one.
    pub headers: Vec<Header>,
    pub body: String,
    /// The body's size in bytes as received, which may be more than `body`.
    pub size: u64,
    pub status: u32,
    pub ttfb_ms: f64,
    pub total_ms: f64,
    /// How long looking the host up, connecting and the TLS handshake took,
    /// for `request_finished`; -1 where it wasn't measured (no TLS for
    /// `http:`, or a request journalled before phases were timed).
    pub dns_ms: f64,
    pub connect_ms: f64,
    pub tls_ms: f64,
    /// The server's certificate wasn't checked (a host the workspace
    /// trusts without checking), for `request_finished` and `stream_opened`.
    pub tls_unchecked: bool,
    /// Why a request failed; for `policy_changed`, why the policy file was
    /// not taken, or "" if it was.
    pub error: String,
    /// Why a request waits for approval, for `approval_asked`.
    pub reason: String,
    /// Allowed, for `approval_given`; agents on, for `agents_switched`.
    pub allow: bool,
    /// A started or saved request's expectations, one a line.
    pub checks: String,
    /// How they came out, for `request_checked`.
    pub results: Vec<Check>,
    /// The flow a started request is a step of, or the flow of
    /// `flow_started` and `flow_finished`; 0 for none.
    pub flow: u64,
    /// The bench a started request is the first of, or the bench of
    /// `bench_started`, `bench_progress` and `bench_finished`; 0 for none.
    pub bench: u64,
    /// How the bench stands, for those three; for `bench_finished`, `reason`
    /// says why it stopped early, or is "".
    pub stats: BenchNow,
    /// A stream's kind (`websocket` or `sse`), for `stream_opened`; for
    /// `stream_closed`, `reason` says why it closed.
    pub stream: String,
    /// A `stream_message`'s number in its stream, its way (`in` or `out`),
    /// its server-sent event's name, and whether it's binary (then `body`
    /// tells its size and first bytes). Its text is `body`, its whole size
    /// `size`.
    pub n: u64,
    pub dir: String,
    pub sse_event: String,
    pub binary: bool,
    /// Messages each way, for `stream_closed`; `sent` is also how many a
    /// run-until has sent.
    pub received: u64,
    pub sent: u64,
    /// The run-until of `until_started`, `until_progress` and
    /// `until_finished`; 0 for none. For those, `n` is the count in a row,
    /// `allow` whether the answer counted (`until_progress`) or the streak
    /// came (`until_finished`), and `reason` the plan (`until_started`) or
    /// why it ended without the streak (`until_finished`).
    pub until: u64,
    /// The mock, for `mock_started` (its port in `n`, its folder in `path`),
    /// `mock_hit` (`method`, `url` the path asked, `status`, `path` the
    /// saved request that answered) and `mock_stopped`.
    pub mock: u64,
}

/// A GraphQL server's schema: its root types by name ("" where it has
/// none), and its types, the root ones first and scalars left out.
#[rux::export]
#[derive(Clone)]
pub struct GSchema {
    pub query: String,
    pub mutation: String,
    pub types: Vec<GType>,
}

#[rux::export]
#[derive(Clone)]
pub struct GType {
    pub name: String,
    /// `type`, `input`, `enum`, `interface` or `union`.
    pub kind: String,
    pub description: String,
    pub fields: Vec<GField>,
    /// An enum's values, or a union's types, as one line.
    pub values: String,
}

#[rux::export]
#[derive(Clone)]
pub struct GField {
    pub name: String,
    /// `(id: ID!)`, or "".
    pub args: String,
    pub ty: String,
    pub description: String,
}

/// A bench so far, for the page. Times are in milliseconds.
#[rux::export]
#[derive(Clone)]
pub struct BenchNow {
    /// `500 requests, 10 at a time`.
    pub plan: String,
    /// The most it sends; 0 when only its time limits it.
    pub total: u64,
    /// The longest it runs; 0 for no limit.
    pub duration_ms: f64,
    pub elapsed_ms: f64,
    pub done: u64,
    /// Went wrong in any way: no answer, a status of 400 or more, or a
    /// failed expectation.
    pub wrong: u64,
    pub per_sec: f64,
    pub min_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Answers per bucket of `buckets`, one more for slower.
    pub histogram: Vec<u64>,
    /// Each bucket's upper bound.
    pub buckets: Vec<f64>,
    /// `200 × 498, 503 × 2`.
    pub statuses: String,
    /// `3 × connection refused`, the commonest first.
    pub errors: Vec<String>,
    /// What it saw of the server limiting its rate, or "".
    pub rate: String,
}

/// Two benches compared, for the page: `after` against `before`.
#[rux::export]
#[derive(Clone)]
pub struct Comparison {
    pub before: String,
    pub after: String,
    pub tolerance_pct: f64,
    pub rows: Vec<DeltaRow>,
    pub regressed: bool,
    pub notes: Vec<String>,
}

/// One measure of a comparison, ready to show: `p95`, `120.4 ms`,
/// `96.0 ms`, `+25.4 %`.
#[rux::export]
#[derive(Clone)]
pub struct DeltaRow {
    pub label: String,
    pub before: String,
    pub after: String,
    pub change: String,
    pub worse: bool,
    pub better: bool,
}

/// One expectation, checked.
#[rux::export]
#[derive(Clone)]
pub struct Check {
    pub expect: String,
    pub pass: bool,
    /// What was found, or why it couldn't be checked.
    pub actual: String,
}

/// A request saved in the workspace.
#[rux::export]
#[derive(Clone)]
pub struct Saved {
    /// Relative to `requests/`, `/` between folders, no extension.
    pub path: String,
    pub method: String,
    pub url: String,
    pub headers: Vec<Header>,
    pub body: String,
    /// Its expectations, one a line.
    pub checks: String,
}

/// Every saved request, as of event `as_of`: saves and deletes numbered from
/// `as_of` on came after it.
#[rux::export]
#[derive(Clone)]
pub struct Collection {
    pub requests: Vec<Saved>,
    pub as_of: u64,
    /// Files under `requests/` that don't parse, and why.
    pub skipped: Vec<String>,
}

/// One `KEY=value` of an environment.
#[rux::export]
#[derive(Clone)]
pub struct Var {
    pub name: String,
    pub value: String,
}

#[rux::export]
#[derive(Clone)]
pub struct Env {
    pub name: String,
    pub vars: Vec<Var>,
    /// The names of its secrets; their values never leave the core.
    pub secrets: Vec<String>,
}

#[rux::export]
#[derive(Clone)]
pub struct EnvList {
    pub environments: Vec<Env>,
    /// Files under `environments/` that don't parse, and why.
    pub skipped: Vec<String>,
}

/// The window's connection, for the status bar.
#[rux::export]
#[derive(Clone)]
pub struct Link {
    /// `connecting`, `connected`, `lost` (the core went away) or `closed`.
    pub state: String,
    /// Why, when it is not `connected`; the core's port when it is.
    pub detail: String,
    pub workspace: String,
    /// Whether this window started the core, and so will stop it on close.
    pub started_core: bool,
}

#[rux::export]
pub static EVENTS: rux::Channel<CoreEvent> = rux::Channel::new();

#[rux::export]
pub static LINK: rux::Signal<Link> = rux::Signal::new();

#[derive(Debug)]
pub struct CoreError(pub(crate) String);

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

struct Session {
    shared: Arc<Shared>,
    /// The core, when this window started it.
    child: Option<Child>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// Held for the whole of a `connect`, so two at once do not start two cores.
static CONNECTING: Mutex<()> = Mutex::new(());

/// The workspace the window opens: `TRACER_WORKSPACE`, or `Tracer` in the
/// home directory.
#[rux::export]
pub fn default_workspace() -> String {
    if let Some(dir) = env::var_os("TRACER_WORKSPACE") {
        return PathBuf::from(dir).display().to_string();
    }
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from);
    home.unwrap_or_else(|| PathBuf::from(".")).join("Tracer").display().to_string()
}

/// Connect to `workspace`'s core, starting one if none answers, and follow its
/// events from the beginning of its journal. Connecting again while connected
/// keeps the connection and has the journal sent again from the start: the
/// page asking is a fresh one (a dev build's hot reload keeps the Rust side
/// and resets the page), and has none of it.
#[rux::export]
pub async fn connect(workspace: String) -> Result<Link, CoreError> {
    let _one_at_a_time = CONNECTING.lock().unwrap();
    let live = SESSION.lock().unwrap().as_ref().map(|s| s.shared.clone());
    if let (Some(link), Some(shared)) = (LINK.get(), live) {
        if link.state == "connected" {
            return match shared.call(Command::Subscribe { since: Some(0) }) {
                Ok(Ok(_)) => Ok(link),
                Ok(Err(refused)) => Err(CoreError(refused)),
                Err(e) => Err(CoreError(e.to_string())),
            };
        }
    }
    let ws = PathBuf::from(&workspace);
    set_link("connecting", "", &workspace, false);

    let result = open(&ws);
    let (client, child) = match result {
        Ok(found) => found,
        Err(why) => {
            set_link("closed", &why, &workspace, false);
            return Err(CoreError(why));
        }
    };
    let started_core = child.is_some();
    let ws_name = workspace.clone();
    let shared = client
        .into_shared(move |incoming| match incoming {
            Incoming::Event(event) => EVENTS.push(to_page(event)),
            Incoming::Closed(why) => {
                // A close this window asked for has already said so.
                if LINK.get().is_some_and(|l| l.state == "connected") {
                    set_link("lost", &why, &ws_name, started_core);
                }
            }
        })
        .map_err(|e| CoreError(e.to_string()))?;

    let old = SESSION.lock().unwrap().replace(Session { shared: Arc::new(shared), child });
    if let Some(mut old) = old {
        if let Some(child) = old.child.as_mut() {
            let _ = child.kill();
        }
    }
    let link = set_link("connected", "", &workspace, started_core);
    Ok(link)
}

/// Send a request through the core. Gives its id at once; what happens to it
/// arrives on `events`, to this window and to every other client alike.
/// `env` names the environment that fills its `{{name}}`s; "" for none.
#[rux::export]
pub fn send(method: String, url: String, headers: Vec<Header>, body: String, env: String, checks: String) -> Result<u64, CoreError> {
    let request = request_of(method, url, headers, body, checks);
    let env = if env.is_empty() { None } else { Some(env) };
    match shared()?.call(Command::Send { request, env }) {
        Ok(Ok(ReplyBody::Accepted { request_id })) => Ok(request_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// A request (as written) as code that sends it, in `lang` (`curl`,
/// `fetch`, `python` or `rux`), its variables filled from `env` ("" for
/// none); secrets stay `{{name}}`.
#[rux::export]
pub fn export_as(lang: String, method: String, url: String, headers: Vec<Header>, body: String, env: String) -> Result<String, CoreError> {
    let lang = ExportLang::parse(&lang).ok_or_else(|| CoreError(format!("`{lang}`: export as curl, fetch, python or rux")))?;
    let request = request_of(method, url, headers, body, String::new());
    let env = if env.is_empty() { None } else { Some(env) };
    match shared()?.call(Command::Export { request, env, lang }) {
        Ok(Ok(ReplyBody::Exported { text })) => Ok(text),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// A server answering on this machine, for the Servers panel.
#[rux::export]
#[derive(Clone)]
pub struct ServerRow {
    pub port: u32,
    pub url: String,
    pub status: u32,
    /// The process, its Server header and an HTML title, those known.
    pub about: String,
    /// The OpenAPI document's path, or "".
    pub openapi: String,
}

/// The servers answering HTTP on this machine. Takes a few seconds.
#[rux::export]
pub async fn local_servers() -> Result<Vec<ServerRow>, CoreError> {
    match shared()?.call(Command::LocalServers { ports: Vec::new() }) {
        Ok(Ok(ReplyBody::LocalServers { servers })) => Ok(servers
            .into_iter()
            .map(|s| ServerRow {
                port: u32::from(s.port),
                url: s.url,
                status: u32::from(s.status),
                about: [s.process, s.server, s.title.map(|t| format!("“{t}”"))].into_iter().flatten().collect::<Vec<_>>().join(" · "),
                openapi: s.openapi.unwrap_or_default(),
            })
            .collect()),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Stream `request_id`'s messages so far as Markdown.
#[rux::export]
pub fn transcript(request_id: u64) -> Result<String, CoreError> {
    match shared()?.call(Command::Transcript { request_id }) {
        Ok(Ok(ReplyBody::Transcript { text })) => Ok(text),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Serve the saved requests under folder `path` as a mock. Gives its id.
#[rux::export]
pub fn start_mock(path: String) -> Result<u64, CoreError> {
    match shared()?.call(Command::StartMock { path, port: None }) {
        Ok(Ok(ReplyBody::MockStarted { mock_id, .. })) => Ok(mock_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

#[rux::export]
pub fn stop_mock(mock_id: u64) -> Result<(), CoreError> {
    match shared()?.call(Command::StopMock { mock_id }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Past request `request_id` as Markdown explaining how it went.
#[rux::export]
pub fn explain(request_id: u64) -> Result<String, CoreError> {
    match shared()?.call(Command::Explain { request_id }) {
        Ok(Ok(ReplyBody::Explained { text, .. })) => Ok(text),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// A request read from a pasted `curl` command or `fetch(…)` call.
#[rux::export]
#[derive(Clone)]
pub struct Imported {
    pub method: String,
    pub url: String,
    pub headers: Vec<Header>,
    pub body: String,
    /// What of the command wasn't carried over.
    pub notes: Vec<String>,
}

#[rux::export]
pub fn import_text(text: String) -> Result<Imported, CoreError> {
    match shared()?.call(Command::Import { text }) {
        Ok(Ok(ReplyBody::Imported { request, notes, .. })) => Ok(Imported {
            method: request.method,
            url: request.url,
            headers: headers_of(request.headers),
            body: request.body.unwrap_or_default(),
            notes,
        }),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Run the saved requests under folder `path` as a flow, once per row of
/// `data` (a CSV or JSON file in the workspace) if it isn't "". Gives its id.
#[rux::export]
pub fn run_flow(path: String, env: String, data: String) -> Result<u64, CoreError> {
    let env = if env.is_empty() { None } else { Some(env) };
    let data = if data.trim().is_empty() { None } else { Some(data) };
    match shared()?.call(Command::RunFlow { path, env, keep_going: false, data }) {
        Ok(Ok(ReplyBody::FlowStarted { flow_id, .. })) => Ok(flow_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Bench the request being edited: `total` times (0: as many as `secs`
/// allows), `concurrency` at a time, for at most `secs` (0: no limit), no
/// faster than `rate` a second (0: as fast as it goes). Gives its id.
#[rux::export]
#[allow(clippy::too_many_arguments)]
pub fn run_bench(
    method: String,
    url: String,
    headers: Vec<Header>,
    body: String,
    env: String,
    checks: String,
    total: u64,
    concurrency: u32,
    secs: u64,
    rate: f64,
) -> Result<u64, CoreError> {
    let request = request_of(method, url, headers, body, checks);
    let env = if env.is_empty() { None } else { Some(env) };
    let plan = BenchPlan {
        total: (total > 0).then_some(total),
        concurrency,
        duration_secs: (secs > 0).then_some(secs),
        rate: (rate > 0.0).then_some(rate),
    };
    match shared()?.call(Command::RunBench { request, env, plan, vars_of: None }) {
        Ok(Ok(ReplyBody::BenchStarted { bench_id })) => Ok(bench_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// The query that asks a GraphQL server for its schema; sent as a
/// `GRAPHQL` request's body, its answer is read by [`graphql_schema`].
#[rux::export]
pub fn introspection() -> String {
    GRAPHQL_INTROSPECTION.to_string()
}

/// A problem in the GraphQL query being edited, for the page.
#[rux::export]
#[derive(Clone)]
pub struct GqlDiag {
    pub line: u32,
    pub col: u32,
    pub start: u64,
    pub end: u64,
    pub warning: bool,
    pub message: String,
    pub suggestions: Vec<String>,
    /// Whether taking a suggestion replaces `start..end`.
    pub replaces: bool,
}

/// `query`'s problems against the schema in request `request_id`'s answer.
#[rux::export]
pub fn check_graphql(query: String, request_id: u64) -> Result<Vec<GqlDiag>, CoreError> {
    match shared()?.call(Command::CheckGraphql { query, request_id }) {
        Ok(Ok(ReplyBody::GraphqlChecked { problems })) => Ok(problems
            .into_iter()
            .map(|p| GqlDiag { line: p.line, col: p.col, start: p.start, end: p.end, warning: p.severity == "warning", message: p.message, suggestions: p.suggestions, replaces: p.replaces })
            .collect()),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// `text` with bytes `start..end` replaced by `with`: a suggestion taken.
/// Out of range or off a character boundary, `text` as it was.
#[rux::export]
pub fn replace_range(text: String, start: u64, end: u64, with: String) -> String {
    let (start, end) = (start as usize, end as usize);
    if start > end || end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return text;
    }
    format!("{}{with}{}", &text[..start], &text[end..])
}

/// The schema in finished request `request_id`'s answer.
#[rux::export]
pub fn graphql_schema(request_id: u64) -> Result<GSchema, CoreError> {
    let schema = match shared()?.call(Command::GraphqlSchema { request_id }) {
        Ok(Ok(ReplyBody::GraphqlSchema { schema })) => schema,
        Ok(Ok(other)) => return Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => return Err(CoreError(refused)),
        Err(e) => return Err(CoreError(e.to_string())),
    };
    let roots = [schema.query.clone(), schema.mutation.clone(), schema.subscription.clone()];
    let mut ordered: Vec<&tracer_protocol::GqlType> = roots.iter().filter_map(|r| schema.types.iter().find(|t| &t.name == r)).collect();
    ordered.extend(schema.types.iter().filter(|t| !roots.contains(&t.name) && t.kind != "SCALAR"));
    let types = ordered
        .into_iter()
        .map(|t| GType {
            name: t.name.clone(),
            kind: match t.kind.as_str() {
                "INPUT_OBJECT" => "input",
                "ENUM" => "enum",
                "INTERFACE" => "interface",
                "UNION" => "union",
                _ => "type",
            }
            .into(),
            description: t.description.clone(),
            fields: t
                .fields
                .iter()
                .map(|f| GField {
                    name: f.name.clone(),
                    args: if f.args.is_empty() { String::new() } else { format!("({})", f.args) },
                    ty: f.ty.clone(),
                    description: f.description.clone(),
                })
                .collect(),
            values: t.values.join(if t.kind == "UNION" { " | " } else { " · " }),
        })
        .collect();
    Ok(GSchema { query: schema.query, mutation: schema.mutation, types })
}

/// Keep finished bench `bench_id` as baseline `name`. Gives its file.
#[rux::export]
pub fn save_baseline(bench_id: u64, name: String) -> Result<String, CoreError> {
    match shared()?.call(Command::SaveBaseline { bench_id, name }) {
        Ok(Ok(ReplyBody::BaselineSaved { path, .. })) => Ok(path),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// The baselines' names.
#[rux::export]
pub async fn baseline_names() -> Result<Vec<String>, CoreError> {
    match shared()?.call(Command::ListBaselines) {
        Ok(Ok(ReplyBody::Baselines { baselines, .. })) => Ok(baselines.into_iter().map(|b| b.name).collect()),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Finished bench `bench_id` against `against`: `bench 3` for another
/// bench, or a baseline's name.
#[rux::export]
pub async fn compare_bench(bench_id: u64, against: String) -> Result<Comparison, CoreError> {
    let (baseline, other_bench) = match against.strip_prefix("bench ").and_then(|n| n.trim().parse().ok()) {
        Some(id) => (None, Some(id)),
        None => (Some(against), None),
    };
    let comparison = match shared()?.call(Command::CompareBench { bench_id, baseline, other_bench, tolerance_pct: None }) {
        Ok(Ok(ReplyBody::BenchCompared { comparison })) => comparison,
        Ok(Ok(other)) => return Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => return Err(CoreError(refused)),
        Err(e) => return Err(CoreError(e.to_string())),
    };
    let rows = comparison
        .deltas
        .iter()
        .filter(|d| !(d.metric.ends_with("_pct") && d.before == 0.0 && d.after == 0.0))
        .map(|d| {
            let (label, unit, points) = match d.metric.as_str() {
                "mean_ms" => ("mean", " ms", false),
                "p50_ms" => ("p50", " ms", false),
                "p95_ms" => ("p95", " ms", false),
                "p99_ms" => ("p99", " ms", false),
                "per_sec" => ("a second", "", false),
                "error_pct" => ("errors", " %", true),
                "limited_pct" => ("limited", " %", true),
                other => (other, "", false),
            };
            DeltaRow {
                label: label.into(),
                before: format!("{:.1}{unit}", d.before),
                after: format!("{:.1}{unit}", d.after),
                change: if points { format!("{:+.1} pts", d.change) } else { format!("{:+.1} %", d.change) },
                worse: d.worse,
                better: d.better,
            }
        })
        .collect();
    Ok(Comparison {
        before: comparison.before,
        after: comparison.after,
        tolerance_pct: comparison.tolerance_pct,
        rows,
        regressed: comparison.regressed,
        notes: comparison.notes,
    })
}

/// Send the request being edited again and again, one at a time, until
/// `streak` answers in a row count: ones `matching` passes on (an
/// expectation such as `status == 401`), or with "" failures. At most `max`
/// sends (0: the core's default), `gap_ms` apart. Gives its id.
#[rux::export]
#[allow(clippy::too_many_arguments)]
pub fn run_until(
    method: String,
    url: String,
    headers: Vec<Header>,
    body: String,
    env: String,
    checks: String,
    streak: u32,
    matching: String,
    max: u64,
    gap_ms: u64,
) -> Result<u64, CoreError> {
    let request = request_of(method, url, headers, body, checks);
    let env = if env.is_empty() { None } else { Some(env) };
    let matching = matching.trim();
    let plan = UntilPlan {
        streak,
        matching: (!matching.is_empty()).then(|| matching.to_string()),
        max: (max > 0).then_some(max),
        gap_ms: (gap_ms > 0).then_some(gap_ms),
    };
    match shared()?.call(Command::RunUntil { request, env, plan }) {
        Ok(Ok(ReplyBody::UntilStarted { until_id })) => Ok(until_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// A cookie in the jar, as the page shows it.
#[rux::export]
#[derive(Clone)]
pub struct CookieRow {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// When it expires, in milliseconds since the epoch; 0 for a session cookie.
    pub expires_ms: u64,
    /// `Secure HttpOnly SameSite=Lax`, those it has.
    pub flags: String,
    /// The environment whose jar holds it, or "" for the shared jar.
    pub env: String,
}

/// How the workspace's requests behave like a browser's, and the jar.
#[rux::export]
#[derive(Clone)]
pub struct BrowserState {
    pub cookies: bool,
    /// The page's origin, or "".
    pub origin: String,
    /// Hosts whose TLS certificates aren't checked, `, ` between them.
    pub unchecked_tls: String,
    /// A jar per environment.
    pub jar_per_env: bool,
    pub jar: Vec<CookieRow>,
}

/// The browser behaviour and the cookies in the jar, values too.
#[rux::export]
pub fn browser_state() -> Result<BrowserState, CoreError> {
    match shared()?.call(Command::Cookies) {
        Ok(Ok(ReplyBody::Cookies { browser, cookies })) => Ok(BrowserState {
            cookies: browser.cookies,
            unchecked_tls: browser.unchecked_tls.join(", "),
            origin: browser.origin.unwrap_or_default(),
            jar_per_env: browser.jar_per_env,
            jar: cookies
                .into_iter()
                .map(|c| {
                    let mut flags = Vec::new();
                    if c.secure {
                        flags.push("Secure".to_string());
                    }
                    if c.http_only {
                        flags.push("HttpOnly".to_string());
                    }
                    if let Some(s) = c.same_site {
                        flags.push(format!("SameSite={s}"));
                    }
                    CookieRow {
                        name: c.name,
                        value: c.value.unwrap_or_default(),
                        domain: c.domain,
                        path: c.path,
                        expires_ms: c.expires_ms.unwrap_or(0),
                        flags: flags.join(" "),
                        env: c.env.unwrap_or_default(),
                    }
                })
                .collect(),
        }),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Keep cookies or not, send as a page at `origin` ("" for none), and
/// don't check the TLS certificates of `unchecked_tls`'s hosts (separated
/// by commas or spaces; "" for none).
#[rux::export]
pub fn set_browser(cookies: bool, origin: String, unchecked_tls: String, jar_per_env: bool) -> Result<(), CoreError> {
    let origin = (!origin.trim().is_empty()).then(|| origin.trim().to_string());
    let unchecked_tls = unchecked_tls.split([',', ' ']).map(str::trim).filter(|h| !h.is_empty()).map(String::from).collect();
    match shared()?.call(Command::SetBrowser { browser: Browser { cookies, origin, unchecked_tls, jar_per_env } }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Empty the jar, or only `domain`'s cookies ("" for all).
#[rux::export]
pub fn clear_cookies(domain: String) -> Result<u64, CoreError> {
    let domain = (!domain.trim().is_empty()).then(|| domain.trim().to_string());
    match shared()?.call(Command::ClearCookies { domain, env: None }) {
        Ok(Ok(ReplyBody::CookiesCleared { removed })) => Ok(removed),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Stop run-until `until_id`, whoever started it.
#[rux::export]
pub fn stop_until(until_id: u64) -> Result<(), CoreError> {
    match shared()?.call(Command::StopUntil { until_id }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Stop bench `bench_id`, whoever started it.
#[rux::export]
pub fn stop_bench(bench_id: u64) -> Result<(), CoreError> {
    match shared()?.call(Command::StopBench { bench_id }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Send past request `request_id` again, as it was written and with the
/// environment it was sent with, whoever sent it first. Gives the new id.
#[rux::export]
pub fn rerun(request_id: u64) -> Result<u64, CoreError> {
    match shared()?.call(Command::Rerun { request_id, env: None }) {
        Ok(Ok(ReplyBody::Accepted { request_id })) => Ok(request_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Allow or refuse a request waiting for approval. Its `approval_given`
/// arrives on `events`.
#[rux::export]
pub fn decide(request_id: u64, allow: bool) -> Result<(), CoreError> {
    match shared()?.call(Command::Decide { request_id, allow }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// The kill switch: let agents act, or stop them and cancel what they have
/// running. Its `agents_switched` arrives on `events`.
#[rux::export]
pub fn set_agents(allowed: bool) -> Result<(), CoreError> {
    match shared()?.call(Command::SetAgents { allowed }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Cancel a running request, whoever sent it. Its `request_cancelled`
/// arrives on `events` like everything else.
#[rux::export]
pub fn cancel(request_id: u64) -> Result<(), CoreError> {
    match shared()?.call(Command::Cancel { request_id }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Send `text` on open WebSocket `request_id`. It shows as an outgoing
/// `stream_message` once it has gone. A stream is closed with [`cancel`].
#[rux::export]
pub fn stream_send(request_id: u64, text: String) -> Result<(), CoreError> {
    match shared()?.call(Command::StreamSend { request_id, text }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// The workspace's saved requests, read from its files by the core.
#[rux::export]
pub async fn list_requests() -> Result<Collection, CoreError> {
    match shared()?.call(Command::ListRequests) {
        Ok(Ok(ReplyBody::Requests { requests, as_of, skipped })) => {
            Ok(Collection { requests: requests.into_iter().map(saved_of).collect(), as_of, skipped })
        }
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// The workspace's environments, read from their files by the core.
#[rux::export]
pub async fn list_environments() -> Result<EnvList, CoreError> {
    match shared()?.call(Command::ListEnvironments) {
        Ok(Ok(ReplyBody::Environments { environments, skipped, .. })) => Ok(EnvList {
            environments: environments
                .into_iter()
                .map(|e| Env {
                    name: e.name,
                    vars: e.vars.into_iter().map(|(name, value)| Var { name, value }).collect(),
                    secrets: e.secrets,
                })
                .collect(),
            skipped,
        }),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Save environment `name` with `vars`, over any of that name. Its secrets
/// are kept. Gives the name as the core wrote it.
#[rux::export]
pub fn save_environment(name: String, vars: Vec<Var>) -> Result<String, CoreError> {
    let env = Environment { name, vars: vars.into_iter().map(|v| (v.name, v.value)).collect(), secrets: Vec::new() };
    match shared()?.call(Command::SaveEnvironment { env }) {
        Ok(Ok(ReplyBody::EnvironmentSaved { name })) => Ok(name),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Delete environment `name`, and its secrets.
#[rux::export]
pub fn delete_environment(name: String) -> Result<(), CoreError> {
    match shared()?.call(Command::DeleteEnvironment { name }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Set secret `name` of environment `env` to `value`, or remove it when
/// `value` is "". The value goes to the core and is never read back.
#[rux::export]
pub fn set_secret(env: String, name: String, value: String) -> Result<(), CoreError> {
    let value = if value.is_empty() { None } else { Some(value) };
    match shared()?.call(Command::SetSecret { env, name, value }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Save a request at `path`, over any there. Gives the path as the core
/// wrote it; every client hears `request_saved`.
#[rux::export]
pub fn save_request(
    path: String,
    method: String,
    url: String,
    headers: Vec<Header>,
    body: String,
    checks: String,
) -> Result<String, CoreError> {
    let request = request_of(method, url, headers, body, checks);
    match shared()?.call(Command::SaveRequest { path, request }) {
        Ok(Ok(ReplyBody::Saved { path })) => Ok(path),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

#[rux::export]
pub fn delete_request(path: String) -> Result<(), CoreError> {
    match shared()?.call(Command::DeleteRequest { path }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// Leave the core: stop it if this window started it, otherwise hang up.
/// `why` becomes the link's detail.
#[rux::export]
pub fn close(why: String) {
    // A `connect` still under way finishes first, so a core it is starting
    // is stopped here rather than left running with no window.
    let _after_connect = CONNECTING.lock().unwrap();
    let Some(mut session) = SESSION.lock().unwrap().take() else { return };
    if let Some(link) = LINK.get() {
        set_link("closed", &why, &link.workspace, link.started_core);
    }
    match session.child.as_mut() {
        Some(child) => {
            let _ = session.shared.call(Command::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        None => session.shared.close(),
    }
}

/// A client of the workspace's core, and the core itself if it had to be
/// started.
fn open(ws: &Path) -> Result<(Client, Option<Child>), String> {
    let name = env::var("USERNAME").or_else(|_| env::var("USER")).unwrap_or_else(|_| "you".into());
    let subscribe = |mut client: Client| -> Result<Client, String> {
        match client.call(Command::Subscribe { since: Some(0) }) {
            Ok(Ok(_)) => Ok(client),
            Ok(Err(refused)) => Err(refused),
            Err(e) => Err(e.to_string()),
        }
    };
    if let Ok(client) = Client::connect(ws, ClientKind::Window, &name) {
        return Ok((subscribe(client)?, None));
    }

    if env::var_os(STARTED_BY_WINDOW).is_some() {
        return Err("this process was started as a core, yet it is a window; refusing to start another".into());
    }
    fs::create_dir_all(ws.join(".tracer")).map_err(|e| format!("{}: {e}", ws.display()))?;
    let log_path = ws.join(".tracer").join("core.log");
    let log = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let bin = tracer_bin();
    let mut process = Process::new(&bin);
    process
        .arg("core")
        .arg("-w")
        .arg(ws)
        .env(STARTED_BY_WINDOW, "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A console program started from a window opens a console of its own.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = process
        .spawn()
        .map_err(|e| format!("could not start {} ({e}); build it with `cargo build -p tracer-cli` or set TRACER_BIN", bin.display()))?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(client) = Client::connect(ws, ClientKind::Window, &name) {
            return Ok((subscribe(client)?, Some(child)));
        }
        if let Ok(Some(status)) = child.try_wait() {
            let said = fs::read_to_string(&log_path).unwrap_or_default();
            return Err(format!("the core exited ({status}): {}", said.trim()));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("the core did not answer within {}s", START_TIMEOUT.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// `TRACER_BIN`, else a `tracer` beside this program, else one in a cargo
/// `target/` above it or above the current directory, else `tracer` on PATH.
///
/// Never this program itself: the window is named after the app, so it is a
/// `tracer.exe` too, and starting it as the core starts another window that
/// does the same, without end.
fn tracer_bin() -> PathBuf {
    if let Some(bin) = env::var_os("TRACER_BIN") {
        return PathBuf::from(bin);
    }
    let name = if cfg!(windows) { "tracer.exe" } else { "tracer" };
    let me = env::current_exe().ok().and_then(|e| e.canonicalize().ok());
    let usable = |bin: &Path| bin.is_file() && bin.canonicalize().ok() != me;
    let starts = [env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)), env::current_dir().ok()];
    for start in starts.into_iter().flatten() {
        if usable(&start.join(name)) {
            return start.join(name);
        }
        for dir in start.ancestors() {
            for profile in ["debug", "release"] {
                let bin = dir.join("target").join(profile).join(name);
                if usable(&bin) {
                    return bin;
                }
            }
        }
    }
    PathBuf::from(name)
}

pub(crate) fn shared() -> Result<Arc<Shared>, CoreError> {
    match SESSION.lock().unwrap().as_ref() {
        Some(session) => Ok(session.shared.clone()),
        None => Err(CoreError("not connected to a core".into())),
    }
}

/// `checks` is the page's text: an expectation a line, and a flow's
/// captures as `capture name = source`; blank lines aside.
fn request_of(method: String, url: String, headers: Vec<Header>, body: String, checks: String) -> HttpRequest {
    let lines: Vec<&str> = checks.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let capture = |l: &&str| l.strip_prefix("capture ").is_some();
    HttpRequest {
        method: method.to_uppercase(),
        url: url.trim().to_string(),
        headers: headers.into_iter().map(|h| (h.name, h.value)).collect(),
        body: if body.is_empty() { None } else { Some(body) },
        expect: lines.iter().filter(|l| !capture(l)).map(|l| l.to_string()).collect(),
        capture: lines.iter().filter(|l| capture(l)).map(|l| l["capture ".len()..].trim().to_string()).collect(),
    }
}

/// A request's expectations and captures as the page's Checks text.
fn checks_text(r: &HttpRequest) -> String {
    let mut lines = r.expect.clone();
    lines.extend(r.capture.iter().map(|c| format!("capture {c}")));
    lines.join("\n")
}

fn headers_of(list: Vec<(String, String)>) -> Vec<Header> {
    list.into_iter().map(|(name, value)| Header { name, value }).collect()
}

fn saved_of(saved: SavedRequest) -> Saved {
    let r = saved.request;
    Saved {
        checks: checks_text(&r),
        path: saved.path,
        method: r.method,
        url: r.url,
        headers: headers_of(r.headers),
        body: r.body.unwrap_or_default(),
    }
}

fn set_link(state: &str, detail: &str, workspace: &str, started_core: bool) -> Link {
    let link = Link {
        state: state.into(),
        detail: detail.into(),
        workspace: workspace.into(),
        started_core,
    };
    LINK.set(link.clone());
    link
}

fn to_page(event: Event) -> CoreEvent {
    let mut out = CoreEvent {
        seq: event.seq,
        at_ms: event.at_ms,
        actor_kind: format!("{:?}", event.actor.kind).to_lowercase(),
        actor: event.actor.name,
        kind: String::new(),
        path: String::new(),
        env: String::new(),
        client: 0,
        request_id: 0,
        rerun_of: 0,
        method: String::new(),
        url: String::new(),
        headers: Vec::new(),
        body: String::new(),
        size: 0,
        status: 0,
        ttfb_ms: 0.0,
        total_ms: 0.0,
        dns_ms: -1.0,
        connect_ms: -1.0,
        tls_ms: -1.0,
        tls_unchecked: false,
        error: String::new(),
        reason: String::new(),
        allow: false,
        checks: String::new(),
        results: Vec::new(),
        flow: 0,
        bench: 0,
        stats: bench_now(None, &BenchStats::default()),
        stream: String::new(),
        n: 0,
        dir: String::new(),
        sse_event: String::new(),
        binary: false,
        received: 0,
        sent: 0,
        until: 0,
        mock: 0,
    };
    let headers = headers_of;
    match event.kind {
        EventKind::ClientJoined { client } => (out.kind, out.client) = ("client_joined".into(), client),
        EventKind::ClientLeft { client } => (out.kind, out.client) = ("client_left".into(), client),
        EventKind::RequestStarted { request_id, request, env, rerun_of, flow, bench } => {
            out.kind = "request_started".into();
            out.flow = flow.unwrap_or(0);
            out.bench = bench.unwrap_or(0);
            out.env = env.unwrap_or_default();
            out.request_id = request_id;
            out.rerun_of = rerun_of.unwrap_or(0);
            out.checks = checks_text(&request);
            out.method = request.method;
            out.url = request.url;
            out.headers = headers(request.headers);
            out.body = request.body.unwrap_or_default();
        }
        EventKind::RequestFinished { request_id, response } => {
            out.kind = "request_finished".into();
            out.request_id = request_id;
            out.status = response.status as u32;
            out.headers = headers(response.headers);
            out.size = response.size;
            out.ttfb_ms = response.timing.ttfb_ms;
            out.total_ms = response.timing.total_ms;
            out.dns_ms = response.timing.dns_ms.unwrap_or(-1.0);
            out.connect_ms = response.timing.connect_ms.unwrap_or(-1.0);
            out.tls_ms = response.timing.tls_ms.unwrap_or(-1.0);
            out.tls_unchecked = response.tls_unchecked;
            out.body = shown(response.body);
        }
        EventKind::RequestFailed { request_id, error } => {
            out.kind = "request_failed".into();
            out.request_id = request_id;
            out.error = error;
        }
        EventKind::RequestCancelled { request_id } => {
            out.kind = "request_cancelled".into();
            out.request_id = request_id;
        }
        EventKind::RequestSaved { path, request } => {
            out.kind = "request_saved".into();
            out.path = path;
            out.checks = checks_text(&request);
            out.method = request.method;
            out.url = request.url;
            out.headers = headers(request.headers);
            out.body = request.body.unwrap_or_default();
        }
        EventKind::RequestChecked { request_id, results } => {
            out.kind = "request_checked".into();
            out.request_id = request_id;
            out.results = results.into_iter().map(|r| Check { expect: r.expect, pass: r.pass, actual: r.actual }).collect();
        }
        EventKind::RequestDeleted { path } => {
            out.kind = "request_deleted".into();
            out.path = path;
        }
        // The window lists them again: they're few and small.
        EventKind::EnvironmentSaved { .. } | EventKind::EnvironmentDeleted { .. } | EventKind::SecretSet { .. } => {
            out.kind = "environment_changed".into()
        }
        EventKind::ApprovalAsked { request_id, reason } => {
            out.kind = "approval_asked".into();
            out.request_id = request_id;
            out.reason = reason;
        }
        EventKind::ApprovalGiven { request_id, allow } => {
            out.kind = "approval_given".into();
            out.request_id = request_id;
            out.allow = allow;
        }
        EventKind::FlowStarted { flow_id, path, steps, .. } => {
            (out.kind, out.flow, out.path) = ("flow_started".into(), flow_id, path);
            out.reason = format!("{} steps", steps.len());
        }
        EventKind::FlowFinished { flow_id, passed, steps } => {
            (out.kind, out.flow, out.allow) = ("flow_finished".into(), flow_id, passed);
            // Rows run, in a data-driven flow.
            out.n = steps.iter().filter_map(|s| s.row).max().unwrap_or(0).into();
            let failed: Vec<String> = steps
                .iter()
                .filter(|s| s.outcome == StepOutcome::Failed)
                .map(|s| format!("{}{}: {}", s.row.map(|r| format!("row {r}: ")).unwrap_or_default(), s.path, s.note))
                .collect();
            out.reason = failed.join("; ");
        }
        EventKind::BenchStarted { bench_id, request, env, plan } => {
            (out.kind, out.bench, out.env) = ("bench_started".into(), bench_id, env.unwrap_or_default());
            out.stats = bench_now(Some(&plan), &BenchStats::default());
            out.method = request.method;
            out.url = request.url;
        }
        EventKind::BenchProgress { bench_id, stats } => {
            (out.kind, out.bench, out.stats) = ("bench_progress".into(), bench_id, bench_now(None, &stats));
        }
        EventKind::BenchFinished { bench_id, stats, stopped } => {
            (out.kind, out.bench, out.stats) = ("bench_finished".into(), bench_id, bench_now(None, &stats));
            out.reason = stopped.unwrap_or_default();
        }
        EventKind::UntilStarted { until_id, request, env, plan } => {
            (out.kind, out.until, out.env) = ("until_started".into(), until_id, env.unwrap_or_default());
            (out.method, out.url, out.reason) = (request.method, request.url, plan.describe());
        }
        EventKind::UntilProgress { until_id, request_id, counted, sent, streak } => {
            (out.kind, out.until, out.request_id) = ("until_progress".into(), until_id, request_id);
            (out.allow, out.sent, out.n) = (counted, sent, streak as u64);
        }
        EventKind::UntilFinished { until_id, sent, streak, reached, stopped } => {
            (out.kind, out.until, out.allow) = ("until_finished".into(), until_id, reached);
            (out.sent, out.n, out.reason) = (sent, streak as u64, stopped.unwrap_or_default());
        }
        EventKind::BrowserChanged { browser } => {
            (out.kind, out.allow, out.url) = ("browser_changed".into(), browser.cookies, browser.origin.unwrap_or_default());
        }
        EventKind::CookiesChanged { cookies } => (out.kind, out.n) = ("cookies_changed".into(), cookies.len() as u64),
        EventKind::BaselineSaved { name, bench_id } => {
            (out.kind, out.path, out.bench) = ("baseline_saved".into(), name, bench_id);
        }
        EventKind::AgentsSwitched { allowed } => (out.kind, out.allow) = ("agents_switched".into(), allowed),
        EventKind::MockStarted { mock_id, path, port, .. } => {
            (out.kind, out.mock, out.path, out.n) = ("mock_started".into(), mock_id, path, port.into());
        }
        EventKind::MockHit { mock_id, method, path, status, route } => {
            (out.kind, out.mock, out.method, out.url) = ("mock_hit".into(), mock_id, method, path);
            (out.status, out.path) = (status.into(), route.unwrap_or_default());
        }
        EventKind::MockStopped { mock_id } => (out.kind, out.mock) = ("mock_stopped".into(), mock_id),
        EventKind::SpecImported { spec, created } => (out.kind, out.path, out.n) = ("spec_imported".into(), spec.name, created.into()),
        EventKind::PolicyChanged { error, .. } => {
            (out.kind, out.error) = ("policy_changed".into(), error.unwrap_or_default())
        }
        EventKind::StreamOpened { request_id, kind, response } => {
            (out.kind, out.request_id) = ("stream_opened".into(), request_id);
            out.stream = match kind {
                StreamKind::Websocket => "websocket",
                StreamKind::Sse => "sse",
            }
            .into();
            out.status = response.status as u32;
            out.headers = headers(response.headers);
            out.ttfb_ms = response.timing.ttfb_ms;
            out.dns_ms = response.timing.dns_ms.unwrap_or(-1.0);
            out.connect_ms = response.timing.connect_ms.unwrap_or(-1.0);
            out.tls_ms = response.timing.tls_ms.unwrap_or(-1.0);
            out.tls_unchecked = response.tls_unchecked;
        }
        EventKind::StreamMessage { request_id, message } => {
            (out.kind, out.request_id, out.n) = ("stream_message".into(), request_id, message.n);
            out.dir = if message.dir == Direction::In { "in" } else { "out" }.into();
            out.sse_event = message.event.unwrap_or_default();
            out.binary = message.binary;
            out.size = message.size;
            out.body = message.text;
        }
        EventKind::StreamClosed { request_id, reason, received, sent } => {
            (out.kind, out.request_id, out.reason) = ("stream_closed".into(), request_id, reason);
            (out.received, out.sent) = (received, sent);
        }
        EventKind::CoreStopping => out.kind = "core_stopping".into(),
    }
    out
}

/// `stats` for the page; `plan`'s part only where the event has one.
fn bench_now(plan: Option<&BenchPlan>, stats: &BenchStats) -> BenchNow {
    let statuses: Vec<String> = stats.statuses.iter().map(|(code, n)| format!("{code} × {n}")).collect();
    BenchNow {
        plan: plan.map(BenchPlan::describe).unwrap_or_default(),
        total: plan.and_then(|p| p.total).unwrap_or(0),
        duration_ms: plan.and_then(|p| p.duration_secs).map_or(0.0, |s| s as f64 * 1000.0),
        elapsed_ms: stats.elapsed_ms,
        done: stats.done,
        wrong: stats.bad(),
        per_sec: stats.per_sec,
        min_ms: stats.min_ms,
        mean_ms: stats.mean_ms,
        p50_ms: stats.p50_ms,
        p95_ms: stats.p95_ms,
        p99_ms: stats.p99_ms,
        max_ms: stats.max_ms,
        histogram: if stats.histogram.is_empty() { vec![0; BENCH_BUCKETS_MS.len() + 1] } else { stats.histogram.clone() },
        buckets: BENCH_BUCKETS_MS.to_vec(),
        statuses: statuses.join(", "),
        errors: stats.errors.iter().map(|(e, n)| format!("{n} × {e}")).collect(),
        rate: stats.rate_limit.as_ref().map(|r| r.describe()).unwrap_or_default(),
    }
}

/// At most [`BODY_SHOWN`] bytes of `body`, cut on a character boundary.
fn shown(mut body: String) -> String {
    if body.len() > BODY_SHOWN {
        let mut cut = BODY_SHOWN;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    body
}

// ---- OpenAPI specs ----

/// A spec in the workspace, for the API view.
#[rux::export]
#[derive(Clone)]
pub struct SpecRow {
    pub name: String,
    pub title: String,
    pub version: String,
    pub openapi: String,
    pub from: String,
    /// Its first server, or "".
    pub server: String,
    pub ops: Vec<SpecOpRow>,
}

#[rux::export]
#[derive(Clone)]
pub struct SpecOpRow {
    pub method: String,
    pub path: String,
    pub summary: String,
    pub tag: String,
    pub deprecated: bool,
}

fn spec_row(s: tracer_protocol::SpecInfo) -> SpecRow {
    SpecRow {
        name: s.name,
        title: s.title,
        version: s.version,
        openapi: s.openapi,
        from: s.from,
        server: s.servers.into_iter().next().unwrap_or_default(),
        ops: s
            .operations
            .into_iter()
            .map(|o| SpecOpRow { method: o.method, path: o.path, summary: o.summary, tag: o.tag, deprecated: o.deprecated })
            .collect(),
    }
}

/// The workspace's specs.
#[rux::export]
pub async fn list_specs() -> Result<Vec<SpecRow>, CoreError> {
    match shared()?.call(Command::ListSpecs) {
        Ok(Ok(ReplyBody::Specs { specs, .. })) => Ok(specs.into_iter().map(spec_row).collect()),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// What importing a spec did.
#[rux::export]
#[derive(Clone)]
pub struct SpecImport {
    pub spec: SpecRow,
    pub created: Vec<String>,
    pub kept: u64,
    pub gone: Vec<String>,
    pub changes: Vec<String>,
    pub notes: Vec<String>,
}

/// Import the OpenAPI document at `from` (a URL or a file) as spec `name`
/// ("": from its title), giving environment `env` ("": none) a `base`.
#[rux::export]
pub async fn import_spec(from: String, name: String, env: String) -> Result<SpecImport, CoreError> {
    let name = (!name.trim().is_empty()).then_some(name);
    let env = (!env.trim().is_empty()).then_some(env);
    match shared()?.call(Command::ImportSpec { from, name, env }) {
        Ok(Ok(ReplyBody::SpecImported { imported })) => Ok(SpecImport {
            spec: spec_row(imported.spec),
            created: imported.created,
            kept: imported.kept.len() as u64,
            gone: imported.gone,
            changes: imported.changes,
            notes: imported.notes,
        }),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// One operation in a drift report, put in words for the page.
#[rux::export]
#[derive(Clone)]
pub struct DriftOp {
    pub method: String,
    pub path: String,
    pub summary: String,
    /// `off` (answers that don't match, or an unlisted status), `drift`
    /// (only fields the spec doesn't name), `never` (not tried) or `ok`.
    pub mark: String,
    /// `3 sent · 2 off · saw 200 410`, or `never sent`.
    pub seen: String,
    /// What's wrong, a line each.
    pub lines: Vec<String>,
    /// Its first saved request, or "".
    pub saved: String,
    /// The newest request for it and the newest that didn't match (0: none).
    pub last: u64,
    pub last_failing: u64,
}

#[rux::export]
#[derive(Clone)]
pub struct DriftEndpoint {
    pub method: String,
    pub path: String,
    pub line: String,
    pub last: u64,
}

#[rux::export]
#[derive(Clone)]
pub struct DriftView {
    pub name: String,
    pub checked: u64,
    pub tried: u64,
    pub off: u64,
    pub ops: Vec<DriftOp>,
    pub unknown: Vec<DriftEndpoint>,
    pub stale: Vec<String>,
}

/// Where the API, as history saw it, and spec `name` part ways.
#[rux::export]
pub async fn spec_drift(name: String) -> Result<DriftView, CoreError> {
    let drift = match shared()?.call(Command::SpecDrift { name }) {
        Ok(Ok(ReplyBody::SpecDrift { drift })) => drift,
        Ok(Ok(other)) => return Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => return Err(CoreError(refused)),
        Err(e) => return Err(CoreError(e.to_string())),
    };
    let ops: Vec<DriftOp> = drift
        .operations
        .into_iter()
        .map(|o| {
            let mark = if o.failing > 0 || !o.undeclared.is_empty() {
                "off"
            } else if !o.undocumented.is_empty() {
                "drift"
            } else if o.sent == 0 {
                "never"
            } else {
                "ok"
            };
            let statuses: Vec<String> = o.statuses.iter().map(u16::to_string).collect();
            let seen = if o.sent == 0 {
                "never sent".to_string()
            } else {
                let off = if o.failing > 0 { format!(" · {} off", o.failing) } else { String::new() };
                format!("{} sent{off} · saw {}", o.sent, statuses.join(" "))
            };
            let mut lines: Vec<String> = o.undeclared.iter().map(|s| format!("answers {s}, which the spec doesn't list")).collect();
            lines.extend(o.problems);
            if !o.undocumented.is_empty() {
                lines.push(format!("fields the spec doesn't name: {}", o.undocumented.join(", ")));
            }
            DriftOp {
                method: o.method,
                path: o.path,
                summary: format!("{}{}", o.summary, if o.deprecated { " (deprecated)" } else { "" }),
                mark: mark.into(),
                seen,
                lines,
                saved: o.saved.into_iter().next().unwrap_or_default(),
                last: o.last_request,
                last_failing: o.last_failing,
            }
        })
        .collect();
    Ok(DriftView {
        name: drift.spec.name,
        checked: u64::from(drift.checked),
        tried: ops.iter().filter(|o| o.mark != "never").count() as u64,
        off: ops.iter().filter(|o| o.mark == "off").count() as u64,
        ops,
        unknown: drift
            .undocumented
            .into_iter()
            .map(|e| DriftEndpoint {
                line: format!("{} sent{}", e.sent, e.last_status.map(|s| format!(" · last answered {s}")).unwrap_or_default()),
                method: e.method,
                path: e.path,
                last: e.last_request,
            })
            .collect(),
        stale: drift.stale,
    })
}

/// Serve spec `name` as a mock. Gives its id.
#[rux::export]
pub fn start_spec_mock(name: String) -> Result<u64, CoreError> {
    match shared()?.call(Command::StartSpecMock { name, port: None }) {
        Ok(Ok(ReplyBody::MockStarted { mock_id, .. })) => Ok(mock_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => Err(CoreError(refused)),
        Err(e) => Err(CoreError(e.to_string())),
    }
}

/// How past request `request_id`'s answer measures up to a spec.
#[rux::export]
#[derive(Clone)]
pub struct SpecCheck {
    pub pass: bool,
    /// `spec pet-shop · GET /pets/{petId} · 200 · Pet`.
    pub head: String,
    pub lines: Vec<String>,
}

#[rux::export]
pub fn check_spec(request_id: u64) -> Result<SpecCheck, CoreError> {
    let r = match shared()?.call(Command::CheckSpec { request_id, spec: None }) {
        Ok(Ok(ReplyBody::SpecChecked { report })) => report,
        Ok(Ok(other)) => return Err(CoreError(format!("unexpected answer: {other:?}"))),
        Ok(Err(refused)) => return Err(CoreError(refused)),
        Err(e) => return Err(CoreError(e.to_string())),
    };
    let mut head = format!("spec {} · {} · {}", r.spec, r.operation.clone().unwrap_or_else(|| "no operation".into()), r.status);
    if let Some(schema) = &r.schema {
        head.push_str(&format!(" · {schema}"));
    }
    let mut lines = r.problems;
    lines.extend(r.undocumented.iter().map(|u| format!("{u} isn't in the spec")));
    Ok(SpecCheck { pass: r.pass, head, lines })
}
