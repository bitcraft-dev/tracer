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
use tracer_protocol::{ClientKind, Command, Environment, Event, EventKind, HttpRequest, ReplyBody, SavedRequest};

/// The most of a response body handed to the page. The core and its journal
/// keep the whole body; a viewer for large ones is Phase 4.
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
    /// `request_deleted` or `core_stopping`.
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
    pub error: String,
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
pub struct CoreError(String);

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
pub fn send(method: String, url: String, headers: Vec<Header>, body: String, env: String) -> Result<u64, CoreError> {
    let request = request_of(method, url, headers, body);
    let env = if env.is_empty() { None } else { Some(env) };
    match shared()?.call(Command::Send { request, env }) {
        Ok(Ok(ReplyBody::Accepted { request_id })) => Ok(request_id),
        Ok(Ok(other)) => Err(CoreError(format!("unexpected answer: {other:?}"))),
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
pub fn save_request(path: String, method: String, url: String, headers: Vec<Header>, body: String) -> Result<String, CoreError> {
    let request = request_of(method, url, headers, body);
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

fn shared() -> Result<Arc<Shared>, CoreError> {
    match SESSION.lock().unwrap().as_ref() {
        Some(session) => Ok(session.shared.clone()),
        None => Err(CoreError("not connected to a core".into())),
    }
}

fn request_of(method: String, url: String, headers: Vec<Header>, body: String) -> HttpRequest {
    HttpRequest {
        method: method.to_uppercase(),
        url: url.trim().to_string(),
        headers: headers.into_iter().map(|h| (h.name, h.value)).collect(),
        body: if body.is_empty() { None } else { Some(body) },
    }
}

fn headers_of(list: Vec<(String, String)>) -> Vec<Header> {
    list.into_iter().map(|(name, value)| Header { name, value }).collect()
}

fn saved_of(saved: SavedRequest) -> Saved {
    let r = saved.request;
    Saved { path: saved.path, method: r.method, url: r.url, headers: headers_of(r.headers), body: r.body.unwrap_or_default() }
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
        error: String::new(),
    };
    let headers = headers_of;
    match event.kind {
        EventKind::ClientJoined { client } => (out.kind, out.client) = ("client_joined".into(), client),
        EventKind::ClientLeft { client } => (out.kind, out.client) = ("client_left".into(), client),
        EventKind::RequestStarted { request_id, request, env, rerun_of } => {
            out.kind = "request_started".into();
            out.env = env.unwrap_or_default();
            out.request_id = request_id;
            out.rerun_of = rerun_of.unwrap_or(0);
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
            out.method = request.method;
            out.url = request.url;
            out.headers = headers(request.headers);
            out.body = request.body.unwrap_or_default();
        }
        EventKind::RequestDeleted { path } => {
            out.kind = "request_deleted".into();
            out.path = path;
        }
        // The window lists them again: they're few and small.
        EventKind::EnvironmentSaved { .. } | EventKind::EnvironmentDeleted { .. } | EventKind::SecretSet { .. } => {
            out.kind = "environment_changed".into()
        }
        EventKind::CoreStopping => out.kind = "core_stopping".into(),
    }
    out
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
