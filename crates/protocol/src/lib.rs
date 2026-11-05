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

/// Where saved benches live, one `<name>.json` each ([`Baseline`]), beside
/// `requests/` so they can be checked in and a CI run compared against them.
pub const BASELINES_DIR: &str = "baselines";

/// Where OpenAPI documents live, one `<name>.json` or `<name>.yaml` each as
/// fetched, beside `<name>.from` saying where from. Their requests are
/// saved under `requests/<name>/`.
pub const SPECS_DIR: &str = "specs";

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
    /// How the workspace's requests behave like a browser's, from now on.
    /// A person's to change. Published as `browser_changed`.
    SetBrowser { browser: Browser },
    /// The browser behaviour and the cookies in the jar. Values are given
    /// to a person, never to an agent.
    Cookies,
    /// `request` (as written) as code that sends it, its variables filled
    /// from `env`; secrets stay `{{name}}`, so none leaves this way.
    Export {
        request: HttpRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        lang: ExportLang,
    },
    /// A request read from a pasted `curl` command or `fetch(…)` call.
    /// Nothing is saved or sent.
    Import { text: String },
    /// Past request `request_id` as Markdown that explains how it went:
    /// the request as written, its environment (secrets named only), the
    /// answer and its timing, the checks, the workspace's browser
    /// behaviour, and the last time the same request worked.
    Explain { request_id: u64 },
    /// The servers answering HTTP on this machine: every TCP port the
    /// system says is listening (or the usual dev ports, where it can't
    /// say), and `ports` too, each tried for HTTP then HTTPS and looked
    /// over for an OpenAPI document. Takes a few seconds.
    LocalServers {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ports: Vec<u16>,
    },
    /// Empty the jar, or only `domain`'s cookies (and its subdomains').
    /// A person's to do. Published as `cookies_changed`.
    ClearCookies {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        /// Only that environment's jar ("" for the shared one); every jar
        /// if not given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
    },
    /// Run the saved requests under folder `path` (or the one at `path`),
    /// in the order of their paths, as one flow: each step's captures fill
    /// `{{name}}` in the steps after it, over `env`'s variables. A step
    /// fails when it gets no answer, a check fails, or (with no checks) its
    /// status is 400 or more; the steps after a failed one are skipped
    /// unless `keep_going`. The reply names the steps; the run is events.
    ///
    /// With `data`, a CSV (a header line naming the columns) or a JSON
    /// array of objects in the workspace, the flow runs once per row, in
    /// order, each row's values filling `{{column}}` (over `env`, under
    /// captures); a failed step skips the rest of its own row only.
    RunFlow {
        path: String,
        #[serde(default)]
        env: Option<String>,
        #[serde(default)]
        keep_going: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    /// Send `request` over and over: `plan.concurrency` at a time until
    /// `plan.total` have gone or `plan.duration_secs` has passed, whichever
    /// comes first, no faster than `plan.rate` a second if given. The first
    /// is an ordinary request, in history, held to the policy and asked
    /// about as any send; the rest are counted, not kept. The reply gives
    /// the bench's id; the run is `bench_progress` events, then
    /// `bench_finished`.
    ///
    /// With `vars_of`, a past request's number, the variables it was sent
    /// with beyond its environment (a flow step's captures, such as a
    /// token) fill the bench's too, so a flow's later steps can be benched.
    RunBench {
        request: HttpRequest,
        #[serde(default)]
        env: Option<String>,
        plan: BenchPlan,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vars_of: Option<u64>,
    },
    /// Stop a running bench, whoever started it. It ends with
    /// `bench_finished`, naming who stopped it.
    StopBench { bench_id: u64 },
    /// Send `request` again and again, one at a time (the next once the
    /// last has answered, `plan.gap_ms` after it), until `plan.streak`
    /// answers in a row count: ones `plan.matching` passes on, or, without
    /// it, failures (no answer, a failed check, or with no checks a status
    /// of 400 or more). Any other answer starts the count again. At most
    /// `plan.max` sends ([`UNTIL_MAX`] if not given). Each is an ordinary
    /// request, in history, held to the policy and asked about as any
    /// send. The reply gives the run's id; the run is `until_progress`
    /// events, one an answer, then `until_finished`.
    RunUntil {
        request: HttpRequest,
        #[serde(default)]
        env: Option<String>,
        plan: UntilPlan,
    },
    /// Stop a running run-until, whoever started it. It ends with
    /// `until_finished`, naming who stopped it.
    StopUntil { until_id: u64 },
    /// Serve the saved requests under folder `path` as a mock server on
    /// 127.0.0.1:`port` (any free port if not given): each answers its
    /// method and path (a `{{variable}}` segment matching any) with its
    /// own last real answer in history. Hits are `mock_hit` events.
    StartMock {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    /// Stop mock `mock_id`, whoever started it.
    StopMock { mock_id: u64 },
    /// Keep finished bench `bench_id` as baseline `name`
    /// (`baselines/<name>.json`, replacing one of that name).
    SaveBaseline { bench_id: u64, name: String },
    /// The workspace's baselines, by name.
    ListBaselines,
    /// How finished bench `bench_id` compares with `baseline`, or with
    /// another finished bench, `other_bench` (one of the two): each latency,
    /// the throughput, the error and rate-limited shares, each flagged worse
    /// past `tolerance_pct` ([`BENCH_TOLERANCE_PCT`] if not given).
    CompareBench {
        bench_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        baseline: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other_bench: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tolerance_pct: Option<f64>,
    },
    /// Read the OpenAPI (3.x or Swagger 2) document at `from`, a URL or a
    /// file, keep it as spec `name` (from its title if not given) under
    /// [`SPECS_DIR`], and save a request for each operation that hasn't one
    /// under `requests/<name>/`, each checking its answer against the spec.
    /// Requests already there are kept as they are. With `env`, that
    /// environment gets `base` set to the spec's server if it has no `base`.
    /// Published as `spec_imported`, and each new request as saved.
    ImportSpec {
        from: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
    },
    /// The workspace's specs.
    ListSpecs,
    /// How finished request `request_id`'s answer measures up to spec
    /// `spec`: the one its `spec` check names, or the first that has its
    /// operation, if not given.
    CheckSpec {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spec: Option<String>,
    },
    /// Where the API, as history has seen it, and spec `name` part ways.
    SpecDrift { name: String },
    /// Serve spec `name` as a mock on 127.0.0.1:`port` (any free port if not
    /// given): each operation answers with its example (or one made from
    /// its schema), after checking the request's parameters and body
    /// against it. `Prefer: code=404` picks another answer.
    StartSpecMock {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    /// `query`'s problems against the GraphQL schema in finished request
    /// `request_id`'s answer (as [`Command::GraphqlSchema`] reads it), each
    /// at its line and column with what would fit there.
    CheckGraphql { query: String, request_id: u64 },
    /// The GraphQL schema in finished request `request_id`'s answer: one
    /// sent with the introspection query ([`GRAPHQL_INTROSPECTION`]).
    GraphqlSchema { request_id: u64 },
    /// Send `text` on open WebSocket `request_id` (a `WEBSOCKET` request
    /// that has published `stream_opened` and not yet closed). It goes out
    /// as a text frame and is published as an outgoing `stream_message` in
    /// the asker's name. A stream is closed with [`Command::Cancel`].
    StreamSend { request_id: u64, text: String },
    /// A stream's messages after number `after` (0: from the first), up to
    /// `limit` ([`STREAM_LIMIT`] if not given), and whether it is still open.
    StreamMessages {
        request_id: u64,
        #[serde(default)]
        after: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    /// Stream `request_id`'s messages so far, both ways, as Markdown: each
    /// with its time, direction, sender and text.
    Transcript { request_id: u64 },
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

impl HttpRequest {
    /// Whether `request_checked` follows its end: it has expectations, or
    /// it's GraphQL, whose answer is always checked for errors.
    pub fn is_checked(&self) -> bool {
        !self.expect.is_empty() || self.method.eq_ignore_ascii_case("GRAPHQL")
    }
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
    /// The server's certificate wasn't checked: its host is one the
    /// workspace trusts without checking ([`Browser::unchecked_tls`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls_unchecked: bool,
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
    /// In a data-driven flow, the row (from 1) it ran for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<u32>,
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

/// A server answering on this machine ([`Command::LocalServers`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalServer {
    pub port: u16,
    /// `http://127.0.0.1:<port>/` (or `[::1]`, the address that answered),
    /// `https://` when only TLS answered.
    pub url: String,
    /// What `GET /` answered.
    pub status: u16,
    /// Its `Server` (or `X-Powered-By`) header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// An HTML page's title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The path of an OpenAPI (or Swagger) document it serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openapi: Option<String>,
    /// The process listening, where the system says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// What [`Command::Export`] writes a request as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportLang {
    /// A POSIX shell's `curl` command.
    Curl,
    /// JavaScript's `fetch`.
    Fetch,
    /// Python with `requests`.
    Python,
    /// A Rux app's native export (Rust, with `ureq`) and the page's call:
    /// Rux script has no HTTP of its own.
    Rux,
}

impl ExportLang {
    pub const ALL: [ExportLang; 4] = [ExportLang::Curl, ExportLang::Fetch, ExportLang::Python, ExportLang::Rux];

    pub fn name(self) -> &'static str {
        match self {
            ExportLang::Curl => "curl",
            ExportLang::Fetch => "fetch",
            ExportLang::Python => "python",
            ExportLang::Rux => "rux",
        }
    }

    pub fn parse(name: &str) -> Option<ExportLang> {
        Self::ALL.into_iter().find(|l| l.name().eq_ignore_ascii_case(name.trim()) || (l == &ExportLang::Fetch && name.eq_ignore_ascii_case("js")))
    }
}

/// What of a browser's behaviour the workspace's requests take on. Off by
/// default: Tracer sends what it's given and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Browser {
    /// Keep `Set-Cookie` answers in the workspace's jar and send them back
    /// as a browser would: by domain and path, `Secure` ones only over
    /// https, until they expire.
    #[serde(default)]
    pub cookies: bool,
    /// The page's origin (`https://app.example.com`) a browser would send
    /// from. Given, a request to another origin carries it as `Origin`, a
    /// request a browser would preflight is preceded by its `OPTIONS`
    /// (a request of its own in history), and an answer a browser would
    /// keep from the page fails a `CORS` check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Hosts whose TLS certificates aren't checked: a self-signed or
    /// expired one is accepted. `localhost`, `*.dev.example.com` (the
    /// domain and anything under it) or `*` (every host). Not a browser's
    /// way but kept with them: a person sets it, never an agent. Each
    /// redirect's host is judged on its own, and an answer got this way
    /// says so ([`HttpResponse::tls_unchecked`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unchecked_tls: Vec<String>,
    /// Each environment keeps cookies in a jar of its own (requests sent
    /// with none share one), so a login against one doesn't follow you to
    /// another. Off, every request shares one jar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub jar_per_env: bool,
}

impl Browser {
    /// Whether `host` (a URL's host, no port) is one whose certificate
    /// isn't checked.
    pub fn tls_unchecked_for(&self, host: &str) -> bool {
        let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        self.unchecked_tls.iter().any(|pattern| {
            let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
            match pattern.strip_prefix("*.") {
                _ if pattern == "*" => true,
                Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
                None => host == pattern,
            }
        })
    }
}

/// A cookie in the jar, as clients see it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookieSeen {
    pub name: String,
    /// Given to a person; never to an agent or in an event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub domain: String,
    pub path: String,
    /// When it expires, in milliseconds since the epoch; none for a
    /// session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    pub secure: bool,
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    /// The environment whose jar holds it, with a jar per environment;
    /// none for the shared jar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// The most a run-until sends when its plan doesn't say.
pub const UNTIL_MAX: u64 = 1000;

/// When a run-until stops ([`Command::RunUntil`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UntilPlan {
    /// How many counted answers in a row end it.
    pub streak: u32,
    /// What counts: an expectation an answer passes (`status == 401`).
    /// Without one, a failure counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matching: Option<String>,
    /// The most to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
    /// How long to wait after an answer before the next send, in
    /// milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_ms: Option<u64>,
}

impl UntilPlan {
    /// `4 failures in a row, at most 1000`,
    /// `3 in a row of status == 401, at most 50, 500 ms apart`.
    pub fn describe(&self) -> String {
        let mut out = match &self.matching {
            Some(m) => format!("{} in a row of {m}", self.streak),
            None => format!("{} failure{} in a row", self.streak, if self.streak == 1 { "" } else { "s" }),
        };
        out.push_str(&format!(", at most {}", self.max.unwrap_or(UNTIL_MAX)));
        if let Some(gap) = self.gap_ms.filter(|g| *g > 0) {
            out.push_str(&format!(", {gap} ms apart"));
        }
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
    /// What it saw of the server limiting its rate, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitSeen>,
}

/// A bench's view of a server's rate limiting: answers that said "slow
/// down" (429, or 503 with `Retry-After`), when the first came, and what
/// the server said of its limit in headers (`X-RateLimit-*`, `RateLimit-*`,
/// `Retry-After`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSeen {
    pub limited: u64,
    /// Requests answered before the first limited one, when that came (ms
    /// into the bench), and the rate they came at: about what the server
    /// lets through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_at_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_sec_before: Option<f64>,
    /// The limit as the server last stated it (`X-RateLimit-Limit: 100`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// The least it said remained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_min: Option<u64>,
    /// Its last `Retry-After`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<String>,
}

impl RateLimitSeen {
    /// `limited 37 times, first after 120 requests (4.1 s in, 29.3 a
    /// second); limit X-RateLimit-Limit: 100; retry after 30`
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.limited > 0 {
            let mut first = format!("limited {} time{}", self.limited, if self.limited == 1 { "" } else { "s" });
            if let (Some(n), Some(ms)) = (self.first_after, self.first_at_ms) {
                first.push_str(&format!(", first after {n} requests ({:.1} s in", ms / 1000.0));
                if let Some(rate) = self.per_sec_before {
                    first.push_str(&format!(", {rate:.1} a second"));
                }
                first.push(')');
            }
            parts.push(first);
        } else {
            parts.push("not limited".to_string());
        }
        if let Some(limit) = &self.limit {
            parts.push(format!("limit {limit}"));
        }
        if let Some(left) = self.remaining_min {
            parts.push(format!("{left} left at the least"));
        }
        if let Some(after) = &self.retry_after {
            parts.push(format!("retry after {after}"));
        }
        parts.join("; ")
    }
}

/// The tolerance a bench comparison flags changes past, in percent.
pub const BENCH_TOLERANCE_PCT: f64 = 20.0;
/// A latency must also change by this much to count: 1 ms against 1.3 ms
/// is noise, not a 30 % regression.
pub const BENCH_LATENCY_FLOOR_MS: f64 = 2.0;
/// The error and limited shares are flagged when they move by more than
/// this many percentage points.
pub const BENCH_SHARE_FLOOR_PCT: f64 = 1.0;

/// A finished bench kept to compare later ones with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub name: String,
    pub bench_id: u64,
    /// Milliseconds since the Unix epoch.
    pub saved_at_ms: u64,
    /// The request as written, with the environment it was sent with.
    pub request: HttpRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    pub plan: BenchPlan,
    pub stats: BenchStats,
}

/// One measure of two benches, before and after.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchDelta {
    /// `p95_ms`, `per_sec`, `error_pct`, …
    pub metric: String,
    pub before: f64,
    pub after: f64,
    /// After against before, in percent (+25: a quarter more); for the
    /// shares, the change in percentage points.
    pub change: f64,
    pub worse: bool,
    pub better: bool,
}

/// Two benches compared: `after` against `before`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchComparison {
    /// `baseline main`, `bench 3 (env staging)`.
    pub before: String,
    pub after: String,
    pub tolerance_pct: f64,
    pub deltas: Vec<BenchDelta>,
    /// Whether any measure got worse past the tolerance.
    pub regressed: bool,
    /// What makes them less comparable: different plans, requests or
    /// environments, a bench stopped early.
    pub notes: Vec<String>,
}
impl BenchStats {
    /// The share of requests that went wrong, in percent.
    pub fn error_pct(&self) -> f64 {
        if self.done == 0 { 0.0 } else { self.bad() as f64 * 100.0 / self.done as f64 }
    }

    /// The share answered "slow down", in percent.
    pub fn limited_pct(&self) -> f64 {
        let limited = self.rate_limit.as_ref().map_or(0, |r| r.limited);
        if self.done == 0 { 0.0 } else { limited as f64 * 100.0 / self.done as f64 }
    }

    /// `after` measured against `self`: each latency (worse when slower by
    /// more than `tolerance_pct` and [`BENCH_LATENCY_FLOOR_MS`]), the
    /// throughput (worse when lower by more than `tolerance_pct`), and the
    /// error and limited shares (worse when up by more than
    /// [`BENCH_SHARE_FLOOR_PCT`] points).
    pub fn deltas(&self, after: &BenchStats, tolerance_pct: f64) -> Vec<BenchDelta> {
        let tol = tolerance_pct / 100.0;
        let pct = |b: f64, a: f64| if b == 0.0 { if a == 0.0 { 0.0 } else { 100.0 } } else { (a - b) * 100.0 / b };
        let mut out = Vec::new();
        for (metric, b, a) in [
            ("mean_ms", self.mean_ms, after.mean_ms),
            ("p50_ms", self.p50_ms, after.p50_ms),
            ("p95_ms", self.p95_ms, after.p95_ms),
            ("p99_ms", self.p99_ms, after.p99_ms),
        ] {
            let moved = (a - b).abs() > BENCH_LATENCY_FLOOR_MS && (a - b).abs() > b * tol;
            out.push(BenchDelta { metric: metric.into(), before: b, after: a, change: pct(b, a), worse: moved && a > b, better: moved && a < b });
        }
        let (b, a) = (self.per_sec, after.per_sec);
        let moved = (a - b).abs() > b * tol;
        out.push(BenchDelta { metric: "per_sec".into(), before: b, after: a, change: pct(b, a), worse: moved && a < b, better: moved && a > b });
        for (metric, b, a) in [("error_pct", self.error_pct(), after.error_pct()), ("limited_pct", self.limited_pct(), after.limited_pct())] {
            let moved = (a - b).abs() > BENCH_SHARE_FLOOR_PCT;
            out.push(BenchDelta { metric: metric.into(), before: b, after: a, change: a - b, worse: moved && a > b, better: moved && a < b });
        }
        out
    }

    /// Requests that went wrong in any way: no answer, a status of 400 or
    /// more, or a failed expectation.
    pub fn bad(&self) -> u64 {
        self.failed + self.bad_status + self.failed_checks
    }
}

/// The method of a request that opens a WebSocket, as JetBrains' HTTP
/// client writes it: `WEBSOCKET wss://host/path`. Its body, if any, is the
/// messages to send once it's open, separated by lines of `===`; a line
/// `=== wait-for-server` before one sends it only after the server has sent
/// something since the last. Any request whose answer is
/// `text/event-stream` is a stream too (server-sent events).
pub const WEBSOCKET: &str = "WEBSOCKET";

/// The most of a stream message's text an event carries; `size` tells the
/// whole.
pub const STREAM_TEXT_MAX: usize = 64 * 1024;

/// The most messages [`Command::StreamMessages`] gives at once.
pub const STREAM_LIMIT: u64 = 500;

/// What kind of live connection a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Websocket,
    Sse,
}

/// Which way a stream message went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From the server.
    #[default]
    In,
    /// To the server.
    Out,
}

/// One message on a stream, either way.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMessage {
    /// 1, 2, 3, … within its stream, both ways counted together.
    pub n: u64,
    pub dir: Direction,
    /// Up to [`STREAM_TEXT_MAX`] of it. A binary message is told as its
    /// size and its first bytes in hex.
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// Its whole size in bytes.
    pub size: u64,
    /// A server-sent event's `event:` name, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// A server-sent event's `id:`, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A stream message as [`Command::StreamMessages`] gives it: when, and who
/// sent it (the stream's opener, for what came in).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamItem {
    pub at_ms: u64,
    pub by: Actor,
    pub message: StreamMessage,
}

/// What a `GRAPHQL` request sends to ask a server for its schema, which
/// [`Command::GraphqlSchema`] then reads from the answer. Field types nest
/// three deep, enough for `[User!]!`, and arguments' two, so the whole query
/// stays within the depth of 8 that servers commonly cap queries at; an
/// argument of type `[ID!]!` reads as `[?]!`.
pub const GRAPHQL_INTROSPECTION: &str = "query Schema { __schema { queryType { name } mutationType { name } subscriptionType { name } types { kind name description fields { name description args { name defaultValue type { ...A } } type { ...T } } inputFields { name type { ...T } } enumValues { name } possibleTypes { name } } } } fragment T on __Type { kind name ofType { kind name ofType { kind name ofType { kind name } } } } fragment A on __Type { kind name ofType { kind name ofType { kind name } } }";

/// A GraphQL server's schema, as its introspection describes it, its own
/// `__` types left out. The root types are named, or "" where it has none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GqlSchema {
    pub query: String,
    pub mutation: String,
    pub subscription: String,
    pub types: Vec<GqlType>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GqlType {
    pub name: String,
    /// `OBJECT`, `INPUT_OBJECT`, `ENUM`, `INTERFACE`, `UNION` or `SCALAR`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// An object's or interface's fields, or an input's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<GqlField>,
    /// An enum's values, or the types a union or interface may be.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GqlField {
    pub name: String,
    /// `id: ID!, first: Int`, or "".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// `[User!]!`.
    pub ty: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl GqlSchema {
    /// As SDL, the root types first and scalars left out: compact enough to
    /// read, or to hand an agent.
    pub fn to_sdl(&self) -> String {
        let roots = [&self.query, &self.mutation, &self.subscription];
        let mut ordered: Vec<&GqlType> = roots.iter().filter_map(|r| self.types.iter().find(|t| &t.name == *r)).collect();
        ordered.extend(self.types.iter().filter(|t| !roots.contains(&&t.name) && t.kind != "SCALAR"));
        let mut out = String::new();
        for t in ordered {
            if !t.description.is_empty() {
                out.push_str(&format!("\"{}\"\n", t.description.replace('\n', " ")));
            }
            match t.kind.as_str() {
                "ENUM" => out.push_str(&format!("enum {} {{ {} }}\n\n", t.name, t.values.join(" "))),
                "UNION" => out.push_str(&format!("union {} = {}\n\n", t.name, t.values.join(" | "))),
                kind => {
                    let word = match kind {
                        "INPUT_OBJECT" => "input",
                        "INTERFACE" => "interface",
                        _ => "type",
                    };
                    out.push_str(&format!("{word} {} {{\n", t.name));
                    for f in &t.fields {
                        if !f.description.is_empty() {
                            out.push_str(&format!("  \"{}\"\n", f.description.replace('\n', " ")));
                        }
                        let args = if f.args.is_empty() { String::new() } else { format!("({})", f.args) };
                        out.push_str(&format!("  {}{args}: {}\n", f.name, f.ty));
                    }
                    out.push_str("}\n\n");
                }
            }
        }
        out.trim_end().to_string()
    }
}

/// A problem in a GraphQL query, against its server's schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GqlProblem {
    /// 1-based; the column counts characters.
    pub line: u32,
    pub col: u32,
    /// The bytes of the query it's about, so a suggestion can replace them.
    pub start: u64,
    pub end: u64,
    /// `error` or `warning`.
    pub severity: String,
    pub message: String,
    /// What would fit there, nearest first: field names, values, types.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
    /// Whether a suggestion replaces `start..end` (a misspelt name), or
    /// only says what goes there.
    #[serde(default)]
    pub replaces: bool,
}

/// An OpenAPI document in the workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecInfo {
    pub name: String,
    pub title: String,
    pub version: String,
    /// `3.0.3`, `3.1.0`, `2.0`.
    pub openapi: String,
    /// Where it was imported from: a URL or a file.
    pub from: String,
    pub servers: Vec<String>,
    pub operations: Vec<SpecOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecOperation {
    pub method: String,
    /// As the spec writes it: `/pets/{petId}`.
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
}

/// What importing a spec did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecImported {
    pub spec: SpecInfo,
    /// Requests saved for operations that had none.
    pub created: Vec<String>,
    /// Requests already there for an operation, left as they were.
    pub kept: Vec<String>,
    /// Requests under the spec's folder that match no operation now.
    pub gone: Vec<String>,
    /// What changed since the spec was last imported, in words.
    pub changes: Vec<String>,
    pub notes: Vec<String>,
}

/// How one answer measures up to a spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecReport {
    pub spec: String,
    /// `GET /pets/{petId}`, if the spec has the request's operation.
    pub operation: Option<String>,
    pub status: u16,
    /// Whether the spec lists the status (or its class, or a default).
    pub declared: bool,
    /// The body's schema, by name where it has one.
    pub schema: Option<String>,
    /// What's wrong, each at a JSON path: `$.id is a string ("7"), and the
    /// spec says integer`.
    pub problems: Vec<String>,
    /// Fields the answer has and the schema doesn't name: drift, not a
    /// failure.
    pub undocumented: Vec<String>,
    pub pass: bool,
}

/// Where the API and a spec part ways, from what history has seen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecDrift {
    pub spec: SpecInfo,
    pub operations: Vec<SpecOpDrift>,
    /// Endpoints the API answered that the spec doesn't have.
    pub undocumented: Vec<SpecEndpoint>,
    /// Saved requests under the spec's folder that match no operation.
    pub stale: Vec<String>,
    /// How many answers were looked at.
    pub checked: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecOpDrift {
    pub method: String,
    pub path: String,
    pub summary: String,
    pub deprecated: bool,
    /// Its saved requests.
    pub saved: Vec<String>,
    /// Requests sent; of those answered, how many were checked and how
    /// many didn't match the spec.
    pub sent: u32,
    pub checked: u32,
    pub failing: u32,
    /// Statuses seen, and those of them the spec doesn't list.
    pub statuses: Vec<u16>,
    pub undeclared: Vec<u16>,
    /// What didn't match, array indexes made `[*]`, a few at most.
    pub problems: Vec<String>,
    /// Fields answers had that the spec doesn't name.
    pub undocumented: Vec<String>,
    /// The newest request for it, and the newest that didn't match (0: none).
    pub last_request: u64,
    pub last_failing: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecEndpoint {
    pub method: String,
    pub path: String,
    pub sent: u32,
    pub last_status: Option<u16>,
    pub last_request: u64,
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
    UntilStarted { until_id: u64 },
    UntilStopped,
    MockStarted { mock_id: u64, port: u16, url: String, routes: Vec<String> },
    MockStopped,
    Transcript { text: String },
    BrowserSet,
    Cookies { browser: Browser, cookies: Vec<CookieSeen> },
    CookiesCleared { removed: u64 },
    Exported { text: String },
    /// `failed`: it went wrong (no answer, an error status, a failed
    /// check, cancelled, or not allowed); `summary` says how in a line.
    Explained { text: String, failed: bool, summary: String },
    LocalServers { servers: Vec<LocalServer> },
    /// The request read, the same as a `.http` file's text, and what of
    /// the command wasn't carried over.
    Imported {
        request: HttpRequest,
        http: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notes: Vec<String>,
    },
    BaselineSaved { name: String, path: String },
    Baselines { baselines: Vec<Baseline>, skipped: Vec<String> },
    BenchCompared { comparison: BenchComparison },
    GraphqlSchema { schema: GqlSchema },
    SpecImported { imported: SpecImported },
    GraphqlChecked { problems: Vec<GqlProblem> },
    Specs { specs: Vec<SpecInfo>, skipped: Vec<String> },
    SpecChecked { report: SpecReport },
    SpecDrift { drift: SpecDrift },
    StreamSent,
    StreamMessages {
        messages: Vec<StreamItem>,
        /// Whether the stream is still open.
        open: bool,
        /// How many more there are after these.
        more: u64,
    },
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
    /// A flow began: its saved requests, in order (in a data-driven one,
    /// every row's, as `row 2: path`).
    FlowStarted {
        flow_id: u64,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        steps: Vec<String>,
        /// The data file it runs once per row of, and how many rows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u32>,
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
    /// Finished bench `bench_id` was kept as baseline `name`.
    BaselineSaved { name: String, bench_id: u64 },
    /// A run-until began: `request` as written, sent with `env`, to `plan`.
    UntilStarted {
        until_id: u64,
        request: HttpRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        plan: UntilPlan,
    },
    /// A run-until's request `request_id` was answered (or not): whether it
    /// `counted`, how many have been sent, and the count in a row.
    UntilProgress { until_id: u64, request_id: u64, counted: bool, sent: u64, streak: u32 },
    /// A run-until ended: `reached` when the streak came; otherwise
    /// `stopped` says why it ended.
    UntilFinished {
        until_id: u64,
        sent: u64,
        streak: u32,
        reached: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stopped: Option<String>,
    },
    /// A mock server began on 127.0.0.1:`port`, answering `routes`
    /// (`GET /items/{id}`) from the saved requests under `path`.
    MockStarted { mock_id: u64, path: String, port: u16, routes: Vec<String> },
    /// A mock answered `method` `path` with `status`, from the saved
    /// request `route` (none when no route matched).
    MockHit {
        mock_id: u64,
        method: String,
        path: String,
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route: Option<String>,
    },
    /// A mock stopped, by this event's actor.
    MockStopped { mock_id: u64 },
    /// Spec `spec.name` was imported (again): `created` requests were saved
    /// for operations that had none.
    SpecImported { spec: SpecInfo, created: u32 },
    /// Agents were let act again, or stopped by this event's actor.
    AgentsSwitched { allowed: bool },
    /// The workspace's browser behaviour changed; this event's actor is who.
    BrowserChanged { browser: Browser },
    /// The jar changed: what's in it now, without values.
    CookiesChanged { cookies: Vec<CookieSeen> },
    /// The policy file changed. `rules` are what agents get from now on;
    /// with an `error`, the file was not taken and they are the old rules.
    PolicyChanged {
        rules: Vec<AgentRules>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The core was asked to stop, by this event's actor. Only the asker's
    /// own `client_left` can follow it.
    /// A request's connection stayed open as a stream: a WebSocket after
    /// its handshake, or an answer of server-sent events. `response` is the
    /// handshake's or the answer's head (status, headers, timing; no body).
    /// Its messages follow, then `stream_closed`, then `request_finished`.
    StreamOpened { request_id: u64, kind: StreamKind, response: HttpResponse },
    /// A message on an open stream: one that came in, in the opener's name,
    /// or one sent, in the sender's.
    StreamMessage { request_id: u64, message: StreamMessage },
    /// A stream ended: why (closed by a person or agent, by the server, or
    /// the connection went), and how many messages went each way.
    StreamClosed { request_id: u64, reason: String, received: u64, sent: u64 },
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
    fn unchecked_tls_hosts_match_exactly_or_by_domain() {
        let browser = Browser { unchecked_tls: vec!["localhost".into(), "*.Dev.Test".into()], ..Browser::default() };
        assert!(browser.tls_unchecked_for("localhost"));
        assert!(browser.tls_unchecked_for("LOCALHOST."));
        assert!(browser.tls_unchecked_for("dev.test"));
        assert!(browser.tls_unchecked_for("api.dev.test"));
        assert!(!browser.tls_unchecked_for("127.0.0.1"));
        assert!(!browser.tls_unchecked_for("evildev.test"));
        assert!(!browser.tls_unchecked_for("localhost.example.com"));
        assert!(!Browser::default().tls_unchecked_for("localhost"));
        let every = Browser { unchecked_tls: vec!["*".into()], ..Browser::default() };
        assert!(every.tls_unchecked_for("example.com"));
        // An old browser.json, without the field, reads.
        let old: Browser = serde_json::from_str(r#"{"cookies":true}"#).unwrap();
        assert!(old.unchecked_tls.is_empty());
    }

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
