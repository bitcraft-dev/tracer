//! What Tracer's clients and its core say to each other.
//!
//! The core is a separate process; the Rux window, the CLI and AI agents are
//! all clients of it, on equal terms. A client sends [`Request`]s; the core
//! answers each with a [`Reply`] carrying the same `id`, and pushes every
//! [`Event`] to every subscribed client, numbered, in one order, with no
//! gaps. That last property is the point: the window shows what an agent did
//! by replaying the same events the agent caused, so none may be dropped or
//! reordered.
//!
//! On the wire every message is one line of JSON (newline-delimited), over
//! TCP on `127.0.0.1`. A client finds the core through [`Discovery`], written
//! to `<workspace>/.tracer/core.json`, and proves it may talk to it with the
//! token there in its [`Command::Hello`].

use serde::{Deserialize, Serialize};

pub mod client;

/// The protocol's version. A client and a core that disagree refuse each
/// other at `Hello` rather than misread each other later.
pub const PROTOCOL_VERSION: u32 = 1;

/// The file, relative to the workspace, through which a client finds the core.
pub const DISCOVERY_PATH: &str = ".tracer/core.json";

/// The file, relative to the workspace, that holds every event, one per line.
pub const JOURNAL_PATH: &str = ".tracer/journal.ndjson";

/// The folder, relative to the workspace, that holds saved requests: one
/// `.http` file each, folders being collections.
pub const REQUESTS_DIR: &str = "requests";

/// The folder, relative to the workspace, that holds environments: one
/// `<name>.env` file each, `KEY=value` a line.
pub const ENVIRONMENTS_DIR: &str = "environments";

/// The folder, relative to the workspace, that holds secret values: one
/// `<env>.env` file each, beside the journal under `.tracer/`, which the core
/// keeps out of git. Their values never leave the core.
pub const SECRETS_DIR: &str = ".tracer/secrets";

/// The file, relative to the workspace, that says what AI agents may do in
/// it: [`AgentRules`] for every agent and for agents by name. Plain text,
/// meant to be committed with the workspace.
pub const POLICY_PATH: &str = "agents.policy";

/// Where the core is listening and what it wants to hear first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub protocol: u32,
}

/// Who is talking. Every event names the actor that caused it, so the window
/// can show `🤖 claude-code` beside `👤 you`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// The Rux window: a person.
    Window,
    /// The `tracer` command line, by hand or in CI.
    Cli,
    /// An AI agent, through MCP or the local API.
    Agent,
    /// Not a client: a file under `requests/` or `environments/` that changed
    /// on disk behind the core, by an editor, an agent writing files, or git.
    Disk,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ClientKind,
    /// What the client called itself in `Hello`, such as `claude-code`.
    pub name: String,
}

/// One message from a client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Chosen by the client; the reply carries it back.
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Must come first. Anything else before it is refused.
    Hello {
        protocol: u32,
        token: String,
        kind: ClientKind,
        name: String,
    },
    /// Start receiving events. With `since`, every journalled event after
    /// that sequence number is replayed first, then live ones follow with no
    /// gap between the two.
    Subscribe { since: Option<u64> },
    /// Send an HTTP request. The reply gives its id at once; what happens to
    /// it arrives as events, which every client sees. `{{name}}` in it is
    /// filled from `env` when it is sent; one `env` doesn't define fails it.
    Send {
        request: HttpRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
    },
    /// Cancel a request still running, whoever sent it. The core publishes
    /// `request_cancelled` and nothing else follows for that request; a
    /// request that has already finished or failed is refused.
    Cancel { request_id: u64 },
    /// Every saved request, and the event number the list is true at:
    /// `request_saved` and `request_deleted` events numbered from `as_of` on
    /// came after it.
    ListRequests,
    /// Save a request at `path` (such as `albums/get album`), over any
    /// request already there. Published as `request_saved`.
    SaveRequest { path: String, request: HttpRequest },
    /// Delete the saved request at `path`. Published as `request_deleted`.
    DeleteRequest { path: String },
    /// Every environment, and the event number the list is true at.
    ListEnvironments,
    /// Save an environment, over any of that name. Published as
    /// `environment_saved`.
    SaveEnvironment { env: Environment },
    /// Published as `environment_deleted`. Its secrets go with it.
    DeleteEnvironment { name: String },
    /// Set the secret `name` of environment `env`, or remove it with no
    /// `value`. Published as `secret_set`, without the value.
    SetSecret {
        env: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// Past requests, newest first, each as it was written and with how it
    /// ended. Only those `filter` matches, and at most its `limit`.
    History {
        #[serde(default)]
        filter: HistoryFilter,
    },
    /// Send a past request again, exactly as it was written, with the
    /// environment it was sent with, or with `env` in its place (`""` for
    /// none). Published as `request_started` with `rerun_of`; otherwise it
    /// goes as `Send` does.
    Rerun {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
    },
    /// Request `request_id` as `History` describes it, with its whole
    /// response once it has finished.
    Response { request_id: u64 },
    /// Allow or refuse a request waiting for approval (`approval_asked`).
    /// Only a person may: a window or the CLI, never an agent.
    Decide { request_id: u64, allow: bool },
    /// The kill switch. Off, every agent is refused anything but reading,
    /// and their running requests are cancelled. Only a person may switch it.
    SetAgents { allowed: bool },
    /// Run the saved requests under folder `path` (or the one at `path`),
    /// in the order of their paths, as one flow: each step's captures fill
    /// `{{name}}` in the steps after it, over `env`'s variables. A step
    /// fails when it gets no answer, a check fails, or (with no checks) its
    /// status is 400 or more; the steps after a failed one are skipped
    /// unless `keep_going`. The reply names the steps; the run is events.
    RunFlow {
        path: String,
        #[serde(default)]
        env: Option<String>,
        #[serde(default)]
        keep_going: bool,
    },
    /// Send `request` over and over: `plan.concurrency` at a time until
    /// `plan.total` have gone or `plan.duration_secs` has passed, whichever
    /// comes first, no faster than `plan.rate` a second if given. The first
    /// is an ordinary request, in history, held to the policy and asked
    /// about as any send; the rest are counted, not kept. The reply gives
    /// the bench's id; the run is `bench_progress` events, then
    /// `bench_finished`.
    RunBench {
        request: HttpRequest,
        #[serde(default)]
        env: Option<String>,
        plan: BenchPlan,
    },
    /// Stop a running bench, whoever started it. It ends with
    /// `bench_finished`, naming who stopped it.
    StopBench { bench_id: u64 },
    /// Compare two finished requests' responses: status, headers, body.
    /// `ignore` adds patterns to the default noisy ones (see
    /// [`ResponseDiff::patterns`]), or replaces them with `no_defaults`.
    Diff {
        a: u64,
        b: u64,
        #[serde(default)]
        ignore: Vec<String>,
        #[serde(default)]
        no_defaults: bool,
    },
    /// The rules the workspace's policy gives agents, for `tracer policy`.
    /// An agent asking is told its own.
    Policy,
    /// The core's state, for `tracer status`.
    Status,
    /// Stop the core: it publishes `core_stopping`, replies, stops taking
    /// clients and exits. The window sends it on close when it started the
    /// core itself.
    Shutdown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
    /// What its answer should be, one expectation a line (`status 2xx`,
    /// `$.id == 1`; see the core's `expect.rs`), checked when it arrives
    /// and published as `request_checked`. Saved in its `.http` file as
    /// `# @expect …` lines above the request line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect: Vec<String>,
    /// What a flow takes from its answer for the requests after it, one a
    /// line: `token = $.access_token`, `next = header Location`, `code =
    /// status`. Saved as `# @capture` lines above the request line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capture: Vec<String>,
}

/// One expectation, checked: whether it held, and what was found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub expect: String,
    pub pass: bool,
    /// The value found, as JSON, `missing`, or why it couldn't be checked.
    pub actual: String,
}

/// A named set of variables, filling `{{name}}` in requests sent with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// One word, such as `local`: the file is `environments/<name>.env`.
    pub name: String,
    pub vars: Vec<(String, String)>,
    /// The names of its secrets, never their values. A secret fills
    /// `{{name}}` like a variable, wins over a variable of the same name, and
    /// is replaced by `{{name}}` wherever its value comes back in a response.
    /// Ignored in `SaveEnvironment`: secrets are set with `SetSecret`.
    #[serde(default)]
    pub secrets: Vec<String>,
}

/// A request saved in the workspace, by its path: relative to
/// [`REQUESTS_DIR`], `/` between folders, no extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRequest {
    pub path: String,
    pub request: HttpRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The body as text. A body that is not UTF-8 is replaced lossily; Tracer
    /// shows text and JSON first.
    pub body: String,
    /// The body's size in bytes, as received.
    pub size: u64,
    pub timing: Timing,
}

/// Where a request's time went, in milliseconds.
///
/// `ttfb_ms` and `total_ms` count from when it was sent. The phases before
/// the answer are each its own length: looking the host up, opening the
/// connection, and the TLS handshake (none for `http:`). What's left of
/// TTFB after them is the server's time, and `total_ms - ttfb_ms` the
/// body's download. Requests journalled before the phases were measured
/// have none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    /// Until the status line and headers had arrived.
    pub ttfb_ms: f64,
    /// Until the whole body had arrived.
    pub total_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ms: Option<f64>,
}

impl Timing {
    /// TTFB less the phases before it: the server's time, and the request's
    /// way there and back.
    pub fn wait_ms(&self) -> f64 {
        let before = self.dns_ms.unwrap_or(0.0) + self.connect_ms.unwrap_or(0.0) + self.tls_ms.unwrap_or(0.0);
        (self.ttfb_ms - before).max(0.0)
    }
}

/// One step of a flow, as it went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowStep {
    /// The saved request's path.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    pub outcome: StepOutcome,
    /// Why it failed or was skipped, or "".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The names it captured for the steps after it (not their values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captured: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Passed,
    Failed,
    Skipped,
}

/// How much a bench sends. At least one of `total` and `duration_secs`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchPlan {
    /// The most requests to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// How many are in flight at once, each on a connection it keeps.
    pub concurrency: u32,
    /// The longest to run, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// The most to start a second, across all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
}

impl BenchPlan {
    /// `500 requests, 10 at a time`, `30 s at 20 a second, 4 at a time`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(total) = self.total {
            parts.push(format!("{total} request{}", if total == 1 { "" } else { "s" }));
        }
        if let Some(secs) = self.duration_secs {
            parts.push(format!("{secs} s"));
        }
        let mut out = parts.join(" or ");
        if let Some(rate) = self.rate {
            out.push_str(&format!(" at {rate} a second"));
        }
        out.push_str(&format!(", {} at a time", self.concurrency));
        out
    }
}

/// Upper bounds of the latency histogram's buckets, in milliseconds; the
/// last bucket holds everything slower than the last bound.
pub const BENCH_BUCKETS_MS: [f64; 15] =
    [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 300.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 30000.0];

/// A bench so far, or at its end. Latencies are each request's whole time,
/// sent to the last byte of its body, of the requests that got an answer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchStats {
    /// How long it has run, in milliseconds.
    pub elapsed_ms: f64,
    /// Requests that ended, answered or not.
    pub done: u64,
    /// Ended with no answer at all.
    pub failed: u64,
    /// Answered with a status of 400 or more.
    pub bad_status: u64,
    /// Answered, but an expectation of the request's failed.
    pub failed_checks: u64,
    /// Answers by status, lowest first.
    pub statuses: Vec<(u16, u64)>,
    /// Why requests got no answer, the commonest first, with how many.
    pub errors: Vec<(String, u64)>,
    pub min_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Requests ended a second, over the whole run.
    pub per_sec: f64,
    /// Answers in each bucket of [`BENCH_BUCKETS_MS`], one more for slower.
    pub histogram: Vec<u64>,
}

impl BenchStats {
    /// Requests that went wrong in any way: no answer, a status of 400 or
    /// more, or a failed expectation.
    pub fn bad(&self) -> u64 {
        self.failed + self.bad_status + self.failed_checks
    }
}

/// How two responses differ, `a` being the earlier one asked about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseDiff {
    pub a: DiffSide,
    pub b: DiffSide,
    /// By header name, lowercased.
    pub headers: Vec<Change>,
    pub body: BodyDiff,
    /// Where the two differed but a pattern said to ignore it: nothing is
    /// left out without saying so.
    pub ignored: Vec<String>,
    /// The patterns in force. `$.meta.time` (a path, `*` for any one key or
    /// index) ignores what's under it; a bare key such as `updatedAt`, that
    /// key at any depth; `header:x-amzn-*`, headers (a trailing `*` matches
    /// any ending).
    pub patterns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSide {
    pub request_id: u64,
    pub method: String,
    pub url: String,
    pub status: u16,
}

/// One difference: at a JSON path such as `$.data[3].name`, or a header.
/// Values are compact JSON, or a header's text, cut to a few hundred bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub at: String,
    pub kind: ChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BodyDiff {
    /// No difference, ignored ones aside.
    Same,
    /// Both bodies are JSON: what changed, by path, and how many more past
    /// the ones listed. Objects compare by key, arrays by position.
    Json { changes: Vec<Change>, more: u64 },
    /// Line by line, the changes with a little context, `gap` between hunks.
    Text { lines: Vec<DiffLine>, more: u64 },
    /// Too long to compare line by line.
    TooLarge { a_lines: u64, b_lines: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: LineOp,
    /// Line numbers in each body, from 1, where the line is in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<u64>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineOp {
    Same,
    Removed,
    Added,
    Gap,
}

/// What an agent may do, from [`POLICY_PATH`]: the rules for every agent
/// (`agent` is `*`) or for agents connecting under one name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRules {
    pub agent: String,
    pub access: Access,
    /// What waits for a person to allow it. Empty: nothing does.
    pub approve: Vec<ApproveWhen>,
    /// The most requests the agent may start in a window of time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
    /// The largest request body it may send, in bytes, once `{{name}}` is
    /// filled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body: Option<u64>,
    /// The most requests one bench of its may send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bench: Option<u64>,
    /// The most a bench of its may have in flight at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
}

/// Where an agent's requests may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    /// Reading the workspace and its history; no sends, saves or cancels.
    ReadOnly,
    /// Sends only to this machine (`localhost`, `127.x.x.x`, `::1`), checked
    /// once variables are filled in, without following redirects.
    Local,
    Anywhere,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproveWhen {
    /// Any method but `GET`, `HEAD` and `OPTIONS`.
    Writes,
    /// An environment whose name starts with `prod`.
    Prod,
    /// Every request.
    All,
    /// Every bench, before its first request.
    Bench,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    pub count: u32,
    pub per_secs: u64,
}

/// Which past requests [`Command::History`] gives. Every filter given must
/// match.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFilter {
    /// Found, ignoring case, in the method, the URL as written, the
    /// environment, the sender's name, the status, or `#<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Sent by this kind of client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusClass>,
    /// The most to give, newest first; [`HISTORY_LIMIT`] when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// What [`Command::History`] gives when no limit is asked for.
pub const HISTORY_LIMIT: usize = 50;

fn yes() -> bool {
    true
}

/// How a request ended, broadly, for filtering history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusClass {
    #[serde(rename = "2xx")]
    Success,
    #[serde(rename = "3xx")]
    Redirect,
    #[serde(rename = "4xx")]
    ClientError,
    #[serde(rename = "5xx")]
    ServerError,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "running")]
    Running,
}

impl StatusClass {
    pub const NAMES: &'static str = "2xx, 3xx, 4xx, 5xx, failed, cancelled or running";

    /// From its name on the wire, such as `4xx` or `failed`.
    pub fn parse(name: &str) -> Option<StatusClass> {
        serde_json::from_value(serde_json::Value::String(name.to_ascii_lowercase())).ok()
    }

    pub fn of(ended: &Ended) -> StatusClass {
        match ended {
            Ended::Running | Ended::AwaitingApproval { .. } => StatusClass::Running,
            Ended::Failed { .. } => StatusClass::Failed,
            Ended::Cancelled { .. } => StatusClass::Cancelled,
            Ended::Done { status, .. } => match status {
                500.. => StatusClass::ServerError,
                400..=499 => StatusClass::ClientError,
                300..=399 => StatusClass::Redirect,
                _ => StatusClass::Success,
            },
        }
    }
}

/// One past request, as [`Command::History`] gives it: as written, with how
/// it ended, but without the response's body or headers, which the journal's
/// `request_finished` has.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub request_id: u64,
    /// When it was sent, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// Who sent it.
    pub actor: Actor,
    pub request: HttpRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<u64>,
    /// The flow it was a step of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<u64>,
    /// The bench it was the first request of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<u64>,
    pub ended: Ended,
    /// How its expectations came out, once checked: passed and failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<(u32, u32)>,
}

impl HistoryEntry {
    /// Whether `filter` lets this one through, its limit aside.
    pub fn matches(&self, filter: &HistoryFilter) -> bool {
        if filter.actor.as_ref().is_some_and(|kind| *kind != self.actor.kind) {
            return false;
        }
        if filter.status.is_some_and(|class| class != StatusClass::of(&self.ended)) {
            return false;
        }
        let Some(text) = filter.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            return true;
        };
        let text = text.to_lowercase();
        let status = match &self.ended {
            Ended::Done { status, .. } => status.to_string(),
            _ => String::new(),
        };
        let id = format!("#{}", self.request_id);
        [&self.request.method, &self.request.url, self.env.as_deref().unwrap_or(""), &self.actor.name, &status, &id]
            .iter()
            .any(|field| field.to_lowercase().contains(&text))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Ended {
    Running,
    /// Started, but nothing sent until a person allows it.
    AwaitingApproval { reason: String },
    Done { status: u16, total_ms: f64, size: u64 },
    Failed { error: String },
    Cancelled { by: Actor },
}

/// The core's answer to one [`Request`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(ReplyBody),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum ReplyBody {
    Welcome { client: u64, protocol: u32 },
    Subscribed { replayed: u64, next_seq: u64 },
    Accepted { request_id: u64 },
    Status {
        clients: Vec<Actor>,
        next_seq: u64,
        workspace: String,
        /// False while the kill switch has agents stopped.
        #[serde(default = "yes")]
        agents_allowed: bool,
    },
    /// A past request and, once it finished, its whole response.
    Response {
        entry: HistoryEntry,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<HttpResponse>,
        /// Its expectations, once checked.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        checks: Vec<CheckResult>,
    },
    Decided,
    AgentsSwitched,
    /// `matched` counts every entry the filter let through, `entries` the
    /// newest of them up to its limit; both as of event `as_of`.
    History { entries: Vec<HistoryEntry>, matched: u64, as_of: u64 },
    Cancelled,
    /// `skipped` names files under `requests/` that don't parse, and why.
    Requests { requests: Vec<SavedRequest>, as_of: u64, skipped: Vec<String> },
    /// The path as the core wrote it, which may be tidier than the one sent.
    Saved { path: String },
    Deleted,
    Environments { environments: Vec<Environment>, as_of: u64, skipped: Vec<String> },
    EnvironmentSaved { name: String },
    EnvironmentDeleted,
    SecretSet,
    Diff { diff: ResponseDiff },
    FlowStarted { flow_id: u64, steps: Vec<String> },
    BenchStarted { bench_id: u64 },
    BenchStopped,
    /// The rules for every agent first, then by name; for an agent, only
    /// the rules it gets. `error` says why the policy file was not taken,
    /// in which case the rules are the last ones that were.
    Policy {
        rules: Vec<AgentRules>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Stopping,
}

/// Something that happened, in the one order every client sees.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// 1, 2, 3, … for the life of the workspace's journal, never reused.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub actor: Actor,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    ClientJoined { client: u64 },
    ClientLeft { client: u64 },
    /// `request` as written, `{{name}}` and all; `env` names the environment
    /// it was sent with. Resolved values stay out of the journal. `rerun_of`
    /// is the request this one sent again, through [`Command::Rerun`].
    RequestStarted {
        request_id: u64,
        request: HttpRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rerun_of: Option<u64>,
        /// The flow it's a step of.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow: Option<u64>,
        /// The bench it's the first request of.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bench: Option<u64>,
    },
    RequestFinished { request_id: u64, response: HttpResponse },
    RequestFailed { request_id: u64, error: String },
    /// Cancelled by this event's actor. A request ends with exactly one of
    /// `request_finished`, `request_failed` or this.
    RequestCancelled { request_id: u64 },
    RequestSaved { path: String, request: HttpRequest },
    RequestDeleted { path: String },
    EnvironmentSaved { env: Environment },
    EnvironmentDeleted { name: String },
    /// A secret was set (`present`) or removed. Never carries the value.
    SecretSet { env: String, name: String, present: bool },
    /// A started request waits for a person to allow it before anything is
    /// sent; `reason` says why it needs one. Published in the sender's name.
    ApprovalAsked { request_id: u64, reason: String },
    /// A person allowed or refused it; this event's actor is who. A refused
    /// request then fails, naming them.
    ApprovalGiven { request_id: u64, allow: bool },
    /// A request's expectations, checked once it ended (after its
    /// `request_finished` or `request_failed`), in the sender's name.
    RequestChecked { request_id: u64, results: Vec<CheckResult> },
    /// A flow began: its saved requests, in order.
    FlowStarted {
        flow_id: u64,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        steps: Vec<String>,
    },
    /// A flow ended: how each step went.
    FlowFinished { flow_id: u64, passed: bool, steps: Vec<FlowStep> },
    /// A bench began: `request` as written, sent with `env`, to `plan`.
    BenchStarted {
        bench_id: u64,
        request: HttpRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        plan: BenchPlan,
    },
    /// A bench so far, a few times a second while it runs.
    BenchProgress { bench_id: u64, stats: BenchStats },
    /// A bench ended: all it sent, or stopped early, and why.
    BenchFinished {
        bench_id: u64,
        stats: BenchStats,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stopped: Option<String>,
    },
    /// Agents were let act again, or stopped by this event's actor.
    AgentsSwitched { allowed: bool },
    /// The policy file changed. `rules` are what agents get from now on;
    /// with an `error`, the file was not taken and they are the old rules.
    PolicyChanged {
        rules: Vec<AgentRules>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The core was asked to stop, by this event's actor. Only the asker's
    /// own `client_left` can follow it.
    CoreStopping,
}

/// Everything the core writes to a client, on one line each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Reply(Reply),
    Event(Event),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_reads_as_flat_json() {
        let line = r#"{"id":7,"cmd":"send","request":{"method":"GET","url":"http://x/"}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.id, 7);
        assert_eq!(
            req.command,
            Command::Send {
                request: HttpRequest {
                    method: "GET".into(),
                    url: "http://x/".into(),
                    headers: vec![],
                    body: None,
                    ..Default::default()
                },
                env: None,
            }
        );
    }

    #[test]
    fn server_messages_round_trip() {
        let event = ServerMessage::Event(Event {
            seq: 3,
            at_ms: 1,
            actor: Actor { kind: ClientKind::Agent, name: "claude-code".into() },
            kind: EventKind::RequestFailed { request_id: 9, error: "refused".into() },
        });
        let reply = ServerMessage::Reply(Reply {
            id: 4,
            outcome: Outcome::Ok(ReplyBody::Accepted { request_id: 9 }),
        });
        for msg in [event, reply] {
            let line = serde_json::to_string(&msg).unwrap();
            assert!(!line.contains('\n'), "one message, one line");
            assert_eq!(serde_json::from_str::<ServerMessage>(&line).unwrap(), msg);
        }
    }

    #[test]
    fn a_journal_line_from_before_reruns_still_reads() {
        let line = r#"{"seq":1,"at_ms":0,"actor":{"kind":"cli","name":"x"},"event":"request_started","request_id":1,"request":{"method":"GET","url":"http://x/"}}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        assert!(matches!(event.kind, EventKind::RequestStarted { rerun_of: None, env: None, .. }));
    }

    #[test]
    fn a_response_journalled_before_its_phases_were_timed_still_reads() {
        let line = r#"{"status":200,"headers":[],"body":"","size":0,"timing":{"ttfb_ms":30.0,"total_ms":31.0}}"#;
        let response: HttpResponse = serde_json::from_str(line).unwrap();
        assert_eq!((response.timing.dns_ms, response.timing.wait_ms()), (None, 30.0));
    }

    #[test]
    fn a_history_filter_matches_on_every_field_it_names() {
        let entry = HistoryEntry {
            request_id: 12,
            at_ms: 0,
            actor: Actor { kind: ClientKind::Agent, name: "claude-code".into() },
            request: HttpRequest { method: "POST".into(), url: "{{base}}/albums".into(), headers: vec![], body: None, ..Default::default() },
            env: Some("staging".into()),
            rerun_of: None,
            flow: None,
            bench: None,
            ended: Ended::Done { status: 404, total_ms: 3.0, size: 2 },
            checks: None,
        };
        let with = |text: &str| HistoryFilter { text: Some(text.into()), ..HistoryFilter::default() };
        for hit in ["ALBUMS", "post", "{{base}}", "stag", "claude", "404", "#12", "  "] {
            assert!(entry.matches(&with(hit)), "{hit:?} should match");
        }
        for miss in ["photos", "#13", "200"] {
            assert!(!entry.matches(&with(miss)), "{miss:?} should not match");
        }
        let class = |status| HistoryFilter { status: Some(status), ..HistoryFilter::default() };
        assert!(entry.matches(&class(StatusClass::ClientError)));
        assert!(!entry.matches(&class(StatusClass::Success)));
        let kind = |actor| HistoryFilter { actor: Some(actor), ..HistoryFilter::default() };
        assert!(entry.matches(&kind(ClientKind::Agent)));
        assert!(!entry.matches(&kind(ClientKind::Window)));
        assert_eq!(StatusClass::parse("5XX"), Some(StatusClass::ServerError));
        assert_eq!(StatusClass::parse("cancelled"), Some(StatusClass::Cancelled));
        assert_eq!(StatusClass::parse("6xx"), None);
    }
}
