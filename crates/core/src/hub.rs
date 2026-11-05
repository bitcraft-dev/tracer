//! The event hub: one numbered order of events, journalled and fanned out.
//!
//! Numbering, journalling and fan-out happen under one lock, so every
//! subscriber receives events in exactly the order of their numbers, and a
//! subscriber that joins with `since` is replayed and attached in the same
//! critical section, so no event falls between its replay and its first live
//! one.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracer_protocol::{
    Actor, AgentRules, BenchPlan, BenchStats, CheckResult, ClientKind, Command, Ended, Environment, Event, EventKind, HistoryEntry, HistoryFilter, HttpRequest,
    HttpResponse, SavedRequest, ServerMessage, StreamItem, StreamKind, StreamMessage, UntilPlan, HISTORY_LIMIT, JOURNAL_PATH,
    POLICY_PATH, Browser, CookieSeen,
};

use crate::cookies::Jars;

/// The workspace's browser behaviour and its cookie jar, beside the journal.
const BROWSER_PATH: &str = ".tracer/browser.json";
const COOKIES_PATH: &str = ".tracer/cookies.json";

use crate::policy::Policy;
use crate::baselines::{self, Side};
use crate::{envs, files, guard};

pub struct Hub {
    inner: Mutex<Inner>,
    workspace: String,
    root: PathBuf,
}

struct Inner {
    next_seq: u64,
    next_client: u64,
    next_request: u64,
    next_flow: u64,
    next_bench: u64,
    next_until: u64,
    next_mock: u64,
    /// Mocks serving, each with the flag its listener checks and its port.
    mocks: HashMap<u64, (Arc<AtomicBool>, u16)>,
    journal: File,
    /// Every event so far, for replay. Kept whole for now; a long-lived
    /// workspace will want this paged from the file instead.
    events: Vec<Event>,
    clients: BTreeMap<u64, Actor>,
    subscribers: Vec<(u64, Sender<ServerMessage>)>,
    /// Requests still running, each with the flag its worker checks to stop
    /// early. Whoever takes a request out of here publishes how it ended, so
    /// it ends exactly once.
    running: HashMap<u64, Flight>,
    /// Benches still running: the flag their workers check, and who stopped
    /// it once someone has.
    benches: HashMap<u64, Bench>,
    /// Run-untils still running.
    untils: HashMap<u64, Until>,
    /// Requests waiting for a person to allow them, each with where the
    /// answer goes.
    awaiting: HashMap<u64, mpsc::Sender<(bool, Actor)>>,
    /// The kill switch: false while agents are stopped.
    agents_allowed: bool,
    /// What agents may do, from the policy file, and why the file as it
    /// stands was not taken, if it wasn't.
    policy: Policy,
    policy_error: Option<String>,
    /// The policy file's text when last read (`None`: no file), so a rescan
    /// acts only on a change.
    policy_text: Option<String>,
    /// When each agent, by lowercased name, started its recent requests,
    /// for the policy's rate.
    starts: HashMap<String, VecDeque<Instant>>,
    /// What the core last knew of the files, so a rescan publishes only what
    /// changed behind it. Its own saves and deletes update this under the
    /// same lock, so they aren't echoed back as disk changes.
    disk: Disk,
    /// The variables a flow's step was sent with beyond its environment (the
    /// captures of the steps before it), by request id, so sending it again
    /// fills them as the flow did. Kept in memory only: a capture is often a
    /// token, and resolved values stay out of the journal.
    step_vars: HashMap<u64, Vec<(String, String)>>,
    /// How requests behave like a browser's, and the cookies kept for it.
    browser: Browser,
    jars: Jars,
}

/// A request in flight: the flag its worker checks to stop early, and what
/// kind of client sent it, for the kill switch.
struct Flight {
    cancelled: Arc<AtomicBool>,
    kind: ClientKind,
    /// The bench it's the first request of, which stopping cancels.
    bench: Option<u64>,
    /// Its stream, once its connection has stayed open as one.
    stream: Option<Live>,
}

/// An open stream. Cancelling it closes it rather than dropping it: its
/// worker sees the flag, closes the connection and ends it as finished,
/// naming `closed_by`.
struct Live {
    /// Where messages to send on a WebSocket go, with who sends them.
    outbox: Sender<(Actor, String)>,
    kind: StreamKind,
    /// Messages so far, both ways.
    messages: u64,
    closed_by: Option<Actor>,
}

/// A bench running, for stopping it.
struct Bench {
    stop: Arc<AtomicBool>,
    kind: ClientKind,
    /// Why it was stopped, once it has been.
    stopped: Option<String>,
}

/// A run-until running, for stopping it.
struct Until {
    stop: Arc<AtomicBool>,
    kind: ClientKind,
    /// Why it was stopped, once it has been.
    stopped: Option<String>,
    /// The request it's waiting on, which stopping it cancels.
    current: Option<u64>,
}

#[derive(Default)]
struct Disk {
    requests: BTreeMap<String, HttpRequest>,
    environments: BTreeMap<String, Vec<(String, String)>>,
}

impl Disk {
    fn read(root: &Path) -> Disk {
        Disk {
            requests: files::list(root).0.into_iter().map(|r| (r.path, r.request)).collect(),
            environments: envs::list(root).0.into_iter().map(|e| (e.name, e.vars)).collect(),
        }
    }
}

impl Hub {
    /// Open the workspace's journal, carrying on its numbering.
    pub fn open(workspace: &Path) -> io::Result<Hub> {
        let path = workspace.join(JOURNAL_PATH);
        let mut events = Vec::new();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                // A line that does not parse is a journal written by a newer
                // or broken core; refusing to start is better than renumbering.
                let event: Event = serde_json::from_str(&line)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
                events.push(event);
            }
        }
        let next_seq = events.last().map_or(1, |e| e.seq + 1);
        let next_request = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::RequestStarted { request_id, .. } => Some(request_id),
                _ => None,
            })
            .max()
            .map_or(1, |id| id + 1);
        let next_flow = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::FlowStarted { flow_id, .. } => Some(flow_id),
                _ => None,
            })
            .max()
            .map_or(1, |id| id + 1);
        let next_bench = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::BenchStarted { bench_id, .. } => Some(bench_id),
                _ => None,
            })
            .max()
            .map_or(1, |id| id + 1);
        let next_until = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::UntilStarted { until_id, .. } => Some(until_id),
                _ => None,
            })
            .max()
            .map_or(1, |id| id + 1);
        let next_mock = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::MockStarted { mock_id, .. } => Some(mock_id),
                _ => None,
            })
            .max()
            .map_or(1, |id| id + 1);
        // Run-untils a previous core never finished, with how far they got.
        let mut open_untils: Vec<(u64, Actor, u64, u32)> = Vec::new();
        for e in &events {
            match &e.kind {
                EventKind::UntilStarted { until_id, .. } => open_untils.push((*until_id, e.actor.clone(), 0, 0)),
                EventKind::UntilProgress { until_id, sent, streak, .. } => {
                    if let Some(open) = open_untils.iter_mut().find(|(id, ..)| id == until_id) {
                        (open.2, open.3) = (*sent, *streak);
                    }
                }
                EventKind::UntilFinished { until_id, .. } => open_untils.retain(|(id, ..)| id != until_id),
                _ => {}
            }
        }
        // Benches a previous core never finished, with how far they got.
        let mut open_benches: Vec<(u64, Actor, BenchStats)> = Vec::new();
        for e in &events {
            match &e.kind {
                EventKind::BenchStarted { bench_id, .. } => open_benches.push((*bench_id, e.actor.clone(), BenchStats::default())),
                EventKind::BenchProgress { bench_id, stats } => {
                    if let Some(open) = open_benches.iter_mut().find(|(id, _, _)| id == bench_id) {
                        open.2 = stats.clone();
                    }
                }
                EventKind::BenchFinished { bench_id, .. } => open_benches.retain(|(id, _, _)| id != bench_id),
                _ => {}
            }
        }
        // The kill switch outlives the core: as a person last left it.
        let agents_allowed = events
            .iter()
            .rev()
            .find_map(|e| match e.kind {
                EventKind::AgentsSwitched { allowed } => Some(allowed),
                _ => None,
            })
            .unwrap_or(true);
        // Requests a previous core started and never ended: it stopped, or
        // was killed, with them in flight.
        let mut unfinished: Vec<(u64, Actor)> = Vec::new();
        for e in &events {
            match &e.kind {
                EventKind::RequestStarted { request_id, .. } => unfinished.push((*request_id, e.actor.clone())),
                EventKind::RequestFinished { request_id, .. }
                | EventKind::RequestFailed { request_id, .. }
                | EventKind::RequestCancelled { request_id } => unfinished.retain(|(id, _)| id != request_id),
                _ => {}
            }
        }
        fs::create_dir_all(path.parent().expect("the journal is inside .tracer/"))?;
        let journal = OpenOptions::new().create(true).append(true).open(&path)?;
        // A policy that doesn't parse leaves agents only reading until a
        // person fixes it, rather than free.
        let (policy, policy_error, policy_text) = match read_policy(workspace) {
            Ok(None) => (Policy::default(), None, None),
            Ok(Some(text)) => match Policy::parse(&text) {
                Ok(policy) => (policy, None, Some(text)),
                Err(e) => (Policy::read_only(), Some(format!("{POLICY_PATH}: {e}")), Some(text)),
            },
            // Text no file has, so the next scan reads it again as a change.
            Err(e) => (Policy::read_only(), Some(e), Some("\0unread".into())),
        };
        let hub = Hub {
            inner: Mutex::new(Inner {
                next_seq,
                next_client: 1,
                next_request,
                next_flow,
                next_bench,
                next_until,
                next_mock,
                mocks: HashMap::new(),
                journal,
                events,
                clients: BTreeMap::new(),
                subscribers: Vec::new(),
                running: HashMap::new(),
                benches: HashMap::new(),
                untils: HashMap::new(),
                awaiting: HashMap::new(),
                agents_allowed,
                policy,
                policy_error,
                policy_text,
                starts: HashMap::new(),
                disk: Disk::read(workspace),
                step_vars: HashMap::new(),
                browser: read_json(&workspace.join(BROWSER_PATH)),
                jars: Jars::read(&fs::read_to_string(workspace.join(COOKIES_PATH)).unwrap_or_default()),
            }),
            workspace: workspace.display().to_string(),
            root: workspace.to_path_buf(),
        };
        // Each is closed in its sender's name, as any failure is, so every
        // client stops showing it as running.
        {
            let mut inner = hub.inner.lock().unwrap();
            for (request_id, actor) in unfinished {
                let error = "the core stopped before it finished".to_string();
                publish_locked(&mut inner, actor, EventKind::RequestFailed { request_id, error });
            }
            for (bench_id, actor, stats) in open_benches {
                let stopped = Some("the core stopped before it finished".to_string());
                publish_locked(&mut inner, actor, EventKind::BenchFinished { bench_id, stats, stopped });
            }
            for (until_id, actor, sent, streak) in open_untils {
                let stopped = Some("the core stopped before it finished".to_string());
                publish_locked(&mut inner, actor, EventKind::UntilFinished { until_id, sent, streak, reached: false, stopped });
            }
        }
        Ok(hub)
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn join(&self, actor: Actor) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let client = inner.next_client;
        inner.next_client += 1;
        inner.clients.insert(client, actor.clone());
        publish_locked(&mut inner, actor, EventKind::ClientJoined { client });
        client
    }

    pub fn leave(&self, client: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribers.retain(|(c, _)| *c != client);
        if let Some(actor) = inner.clients.remove(&client) {
            publish_locked(&mut inner, actor, EventKind::ClientLeft { client });
        }
    }

    /// Send `out` what `first` makes of the replay's size and the next number
    /// to come (the reply to `Subscribe`), then every event after `since`,
    /// then attach `out` for live events, all under one lock.
    pub fn subscribe(
        &self,
        client: u64,
        since: Option<u64>,
        out: Sender<ServerMessage>,
        first: impl FnOnce(u64, u64) -> ServerMessage,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let replay: Vec<&Event> = match since {
            Some(since) => inner.events.iter().filter(|e| e.seq > since).collect(),
            None => Vec::new(),
        };
        let _ = out.send(first(replay.len() as u64, inner.next_seq));
        for event in replay {
            let _ = out.send(ServerMessage::Event(event.clone()));
        }
        inner.subscribers.retain(|(c, _)| *c != client);
        inner.subscribers.push((client, out));
    }

    /// Number a new request and publish that it started. Gives its id and
    /// the flag that says it was cancelled.
    pub fn start_request(
        &self,
        actor: Actor,
        request: HttpRequest,
        env: Option<String>,
        rerun_of: Option<u64>,
        flow: Option<u64>,
        bench: Option<u64>,
    ) -> (u64, Arc<AtomicBool>) {
        let mut inner = self.inner.lock().unwrap();
        let request_id = inner.next_request;
        inner.next_request += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        inner.running.insert(request_id, Flight { cancelled: cancelled.clone(), kind: actor.kind.clone(), bench, stream: None });
        publish_locked(&mut inner, actor, EventKind::RequestStarted { request_id, request, env, rerun_of, flow, bench });
        (request_id, cancelled)
    }

    /// Number a new flow and publish that it started.
    pub fn start_flow(&self, actor: Actor, path: String, env: Option<String>, steps: Vec<String>, data: Option<(String, u32)>) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let flow_id = inner.next_flow;
        inner.next_flow += 1;
        let (data, rows) = data.map_or((None, None), |(file, rows)| (Some(file), Some(rows)));
        publish_locked(&mut inner, actor, EventKind::FlowStarted { flow_id, path, env, steps, data, rows });
        flow_id
    }

    /// The workspace's folder.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Number a new bench and publish that it started. Gives its id and the
    /// flag that says to stop.
    pub fn start_bench(&self, actor: Actor, request: HttpRequest, env: Option<String>, plan: BenchPlan) -> (u64, Arc<AtomicBool>) {
        let mut inner = self.inner.lock().unwrap();
        let bench_id = inner.next_bench;
        inner.next_bench += 1;
        let stop = Arc::new(AtomicBool::new(false));
        inner.benches.insert(bench_id, Bench { stop: stop.clone(), kind: actor.kind.clone(), stopped: None });
        publish_locked(&mut inner, actor, EventKind::BenchStarted { bench_id, request, env, plan });
        (bench_id, stop)
    }

    /// Stop bench `bench_id` on `actor`'s behalf. False when it isn't
    /// running. Its runner publishes the end, naming who stopped it.
    pub fn stop_bench(&self, actor: &Actor, bench_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        stop_bench_locked(&mut inner, actor, bench_id)
    }

    /// Publish how bench `bench_id` stands, unless it was stopped meanwhile.
    pub fn bench_progress(&self, actor: Actor, bench_id: u64, stats: BenchStats) {
        let mut inner = self.inner.lock().unwrap();
        if inner.benches.get(&bench_id).is_some_and(|b| b.stopped.is_none()) {
            publish_locked(&mut inner, actor, EventKind::BenchProgress { bench_id, stats });
        }
    }

    /// Publish how bench `bench_id` ended: `stopped` says why it ended early,
    /// unless someone stopped it, which says so instead.
    pub fn end_bench(&self, actor: Actor, bench_id: u64, stats: BenchStats, stopped: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        let by = inner.benches.remove(&bench_id).and_then(|b| b.stopped);
        publish_locked(&mut inner, actor, EventKind::BenchFinished { bench_id, stats, stopped: by.or(stopped) });
    }

    /// Number a new run-until and publish that it started. Gives its id and
    /// the flag that says to stop.
    pub fn start_until(&self, actor: Actor, request: HttpRequest, env: Option<String>, plan: UntilPlan) -> (u64, Arc<AtomicBool>) {
        let mut inner = self.inner.lock().unwrap();
        let until_id = inner.next_until;
        inner.next_until += 1;
        let stop = Arc::new(AtomicBool::new(false));
        inner.untils.insert(until_id, Until { stop: stop.clone(), kind: actor.kind.clone(), stopped: None, current: None });
        publish_locked(&mut inner, actor, EventKind::UntilStarted { until_id, request, env, plan });
        (until_id, stop)
    }

    /// Note that run-until `until_id` now waits on request `request_id`.
    pub fn until_sending(&self, until_id: u64, request_id: u64) {
        if let Some(until) = self.inner.lock().unwrap().untils.get_mut(&until_id) {
            until.current = Some(request_id);
        }
    }

    /// Stop run-until `until_id` on `actor`'s behalf, cancelling the request
    /// it waits on. False when it isn't running.
    /// Number a mock serving the saved requests under `path` on `port`, and
    /// publish that it started.
    pub fn start_mock(&self, actor: Actor, path: String, port: u16, routes: Vec<String>, stop: Arc<AtomicBool>) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let mock_id = inner.next_mock;
        inner.next_mock += 1;
        inner.mocks.insert(mock_id, (stop, port));
        publish_locked(&mut inner, actor, EventKind::MockStarted { mock_id, path, port, routes });
        mock_id
    }

    /// Stop mock `mock_id`; false if it isn't serving.
    pub fn stop_mock(&self, actor: &Actor, mock_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some((stop, port)) = inner.mocks.remove(&mock_id) else { return false };
        stop.store(true, Ordering::SeqCst);
        // The listener waits in accept: a connection wakes it to see the flag.
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        publish_locked(&mut inner, actor.clone(), EventKind::MockStopped { mock_id });
        true
    }

    /// What `request` (as written: method and URL) last answered: the last
    /// answer below 400 if there is one, else the last of any status.
    pub fn last_answer(&self, request: &HttpRequest) -> Option<HttpResponse> {
        let inner = self.inner.lock().unwrap();
        let mut answers = inner.events.iter().rev().filter_map(|e| match &e.kind {
            EventKind::RequestFinished { request_id, response } => Some((*request_id, response)),
            _ => None,
        });
        let ours: Vec<u64> = entries(&inner.events)
            .into_iter()
            .filter(|e| crate::mock::same(&e.request, request) && e.bench.is_none())
            .map(|e| e.request_id)
            .collect();
        let mut fallback = None;
        for (id, response) in answers.by_ref() {
            if !ours.contains(&id) {
                continue;
            }
            if response.status < 400 {
                return Some(response.clone());
            }
            fallback.get_or_insert_with(|| response.clone());
        }
        fallback
    }

    pub fn stop_until(&self, actor: &Actor, until_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        stop_until_locked(&mut inner, actor, until_id)
    }

    /// Publish how run-until `until_id` stands after request `request_id`.
    pub fn until_progress(&self, actor: Actor, until_id: u64, request_id: u64, counted: bool, sent: u64, streak: u32) {
        let mut inner = self.inner.lock().unwrap();
        publish_locked(&mut inner, actor, EventKind::UntilProgress { until_id, request_id, counted, sent, streak });
    }

    /// Publish how run-until `until_id` ended: `stopped` says why it ended
    /// without its streak, unless someone stopped it, which says so instead.
    pub fn end_until(&self, actor: Actor, until_id: u64, sent: u64, streak: u32, reached: bool, stopped: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        let by = inner.untils.remove(&until_id).and_then(|u| u.stopped);
        let stopped = if reached { None } else { by.or(stopped) };
        publish_locked(&mut inner, actor, EventKind::UntilFinished { until_id, sent, streak, reached, stopped });
    }

    /// Bench `bench_id` as it was started and, once finished, how it went;
    /// `None` if there is no such bench.
    pub fn bench_side(&self, bench_id: u64) -> Option<(Side, bool)> {
        let inner = self.inner.lock().unwrap();
        let mut side: Option<Side> = None;
        let mut finished = false;
        for e in &inner.events {
            match &e.kind {
                EventKind::BenchStarted { bench_id: id, request, env, plan } if *id == bench_id => {
                    let label = match env {
                        Some(env) => format!("bench {bench_id} (env {env})"),
                        None => format!("bench {bench_id}"),
                    };
                    side = Some(Side {
                        label,
                        request: request.clone(),
                        env: env.clone(),
                        plan: plan.clone(),
                        stats: BenchStats::default(),
                        stopped: None,
                    });
                }
                EventKind::BenchFinished { bench_id: id, stats, stopped } if *id == bench_id => {
                    if let Some(side) = side.as_mut() {
                        side.stats = stats.clone();
                        side.stopped = stopped.clone();
                        finished = true;
                    }
                }
                _ => {}
            }
        }
        side.map(|s| (s, finished))
    }

    /// Keep finished bench `bench_id` as baseline `name`, and publish it.
    /// Gives its name and its file, relative to the workspace.
    pub fn save_baseline(&self, actor: Actor, bench_id: u64, name: &str) -> Result<(String, String), String> {
        let name = baselines::normalize(name)?;
        let (side, finished) = self.bench_side(bench_id).ok_or_else(|| format!("there is no bench {bench_id}"))?;
        if !finished {
            return Err(format!("bench {bench_id} is still running; save it once it has finished"));
        }
        let baseline = tracer_protocol::Baseline {
            name: name.clone(),
            bench_id,
            saved_at_ms: now_ms(),
            request: side.request,
            env: side.env,
            plan: side.plan,
            stats: side.stats,
        };
        baselines::write(&self.root, &baseline).map_err(|e| format!("could not save baseline {name}: {e}"))?;
        let mut inner = self.inner.lock().unwrap();
        publish_locked(&mut inner, actor, EventKind::BaselineSaved { name: name.clone(), bench_id });
        Ok((name.clone(), format!("{}/{name}.json", tracer_protocol::BASELINES_DIR)))
    }

    pub fn list_baselines(&self) -> (Vec<tracer_protocol::Baseline>, Vec<String>) {
        baselines::list(&self.root)
    }

    pub fn baseline(&self, name: &str) -> Result<tracer_protocol::Baseline, String> {
        baselines::read(&self.root, name)
    }

    /// Request `request_id` as it was written, and the environment it was
    /// sent with.
    /// How the workspace's requests behave like a browser's.
    pub fn browser(&self) -> Browser {
        self.inner.lock().unwrap().browser.clone()
    }

    /// Change the browser behaviour on `actor`'s word, keep it, and tell
    /// every client.
    pub fn set_browser(&self, actor: Actor, browser: Browser) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        write_json(&self.root.join(BROWSER_PATH), &browser)?;
        inner.browser = browser.clone();
        publish_locked(&mut inner, actor, EventKind::BrowserChanged { browser });
        Ok(())
    }

    /// The `Cookie` header the jar for a request sent with `env` gives
    /// `url`, if any.
    pub fn cookie_header(&self, url: &str, env: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let key = jar_key(&inner.browser, env);
        inner.jars.jar(&key).header_for(url, now_ms())
    }

    /// Keep the `Set-Cookie` headers of an answer from `url`, to a request
    /// sent with `env`, in `actor`'s name if that changes the jar.
    pub fn keep_cookies(&self, actor: Actor, url: &str, headers: &[(String, String)], env: Option<&str>) {
        let mut inner = self.inner.lock().unwrap();
        let key = jar_key(&inner.browser, env);
        let before = inner.jars.clone();
        let now = now_ms();
        for (_, value) in headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie")) {
            inner.jars.jar(&key).store(url, value, now);
        }
        if inner.jars != before {
            self.jar_changed(&mut inner, actor);
        }
    }

    /// The browser behaviour and every jar's cookies, with values if
    /// `values`.
    pub fn cookies(&self, values: bool) -> (Browser, Vec<CookieSeen>) {
        let inner = self.inner.lock().unwrap();
        (inner.browser.clone(), inner.jars.seen(now_ms(), values))
    }

    /// Empty the jars (or `env`'s: "" for the shared one), or `domain`'s
    /// part of them. Gives how many went.
    pub fn clear_cookies(&self, actor: Actor, domain: Option<&str>, env: Option<&str>) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let removed = inner
            .jars
            .by_env
            .iter_mut()
            .filter(|(key, _)| env.is_none_or(|e| e == key.as_str()))
            .map(|(_, jar)| jar.clear(domain))
            .sum::<usize>();
        if removed > 0 {
            self.jar_changed(&mut inner, actor);
        }
        removed
    }

    fn jar_changed(&self, inner: &mut Inner, actor: Actor) {
        inner.jars.by_env.retain(|key, jar| key.is_empty() || !jar.cookies.is_empty());
        // A jar that can't be written still serves this core's life.
        let _ = write_json(&self.root.join(COOKIES_PATH), &inner.jars);
        let cookies = inner.jars.seen(now_ms(), false);
        publish_locked(inner, actor, EventKind::CookiesChanged { cookies });
    }

    /// Keep `vars`, the captures request `request_id` is sent with, for
    /// sending it again. Nothing is kept for none.
    pub fn remember_vars(&self, request_id: u64, vars: &[(String, String)]) {
        if !vars.is_empty() {
            self.inner.lock().unwrap().step_vars.insert(request_id, vars.to_vec());
        }
    }

    /// The captures request `request_id` was sent with, if it was a flow's
    /// step (or sent again from one) since the core started.
    pub fn vars_of(&self, request_id: u64) -> Vec<(String, String)> {
        self.inner.lock().unwrap().step_vars.get(&request_id).cloned().unwrap_or_default()
    }

    pub fn written(&self, request_id: u64) -> Option<(HttpRequest, Option<String>)> {
        let inner = self.inner.lock().unwrap();
        inner.events.iter().find_map(|e| match &e.kind {
            EventKind::RequestStarted { request_id: id, request, env, .. } if *id == request_id => {
                Some((request.clone(), env.clone()))
            }
            _ => None,
        })
    }

    /// The past requests `filter` lets through, newest first and at most its
    /// limit; how many it let through in all; and the event number that's
    /// true at.
    pub fn history(&self, filter: &HistoryFilter) -> (Vec<HistoryEntry>, u64, u64) {
        let inner = self.inner.lock().unwrap();
        let limit = filter.limit.unwrap_or(HISTORY_LIMIT);
        let mut matched = 0;
        let mut out = Vec::new();
        for entry in entries(&inner.events).into_iter().rev().filter(|x| x.matches(filter)) {
            matched += 1;
            if out.len() < limit {
                out.push(entry);
            }
        }
        (out, matched, inner.next_seq)
    }

    /// Request `request_id` as history has it, and its whole response once
    /// it has finished.
    pub fn response(&self, request_id: u64) -> Option<(HistoryEntry, Option<HttpResponse>, Vec<CheckResult>)> {
        let inner = self.inner.lock().unwrap();
        let entry = entries(&inner.events).into_iter().find(|e| e.request_id == request_id)?;
        let response = inner.events.iter().rev().find_map(|e| match &e.kind {
            EventKind::RequestFinished { request_id: id, response } if *id == request_id => Some(response.clone()),
            _ => None,
        });
        let checks = inner.events.iter().rev().find_map(|e| match &e.kind {
            EventKind::RequestChecked { request_id: id, results } if *id == request_id => Some(results.clone()),
            _ => None,
        });
        Some((entry, response, checks.unwrap_or_default()))
    }

    /// The last request before `request_id` with the same method and URL
    /// (as written) that answered below 400 with no check failed.
    pub fn last_good_before(&self, request_id: u64, method: &str, url: &str) -> Option<HistoryEntry> {
        let inner = self.inner.lock().unwrap();
        entries(&inner.events).into_iter().rev().find(|e| {
            e.request_id < request_id
                && e.request.method.eq_ignore_ascii_case(method)
                && e.request.url == url
                && matches!(e.ended, Ended::Done { status, .. } if status < 400)
                && e.checks.is_none_or(|(_, failed)| failed == 0)
        })
    }

    /// Publish that running request `request_id` stays open as a stream of
    /// `kind`, its head `response`. Gives where messages to send on it
    /// arrive; `None` if it was cancelled meanwhile.
    pub fn open_stream(&self, actor: Actor, request_id: u64, kind: StreamKind, response: HttpResponse) -> Option<Receiver<(Actor, String)>> {
        let mut inner = self.inner.lock().unwrap();
        let flight = inner.running.get_mut(&request_id)?;
        let (outbox, messages) = mpsc::channel();
        flight.stream = Some(Live { outbox, kind, messages: 0, closed_by: None });
        publish_locked(&mut inner, actor, EventKind::StreamOpened { request_id, kind, response });
        Some(messages)
    }

    /// Number `message` and publish it on open stream `request_id`, in
    /// `actor`'s name.
    pub fn stream_message(&self, actor: Actor, request_id: u64, mut message: StreamMessage) {
        let mut inner = self.inner.lock().unwrap();
        let Some(live) = inner.running.get_mut(&request_id).and_then(|f| f.stream.as_mut()) else { return };
        live.messages += 1;
        message.n = live.messages;
        publish_locked(&mut inner, actor, EventKind::StreamMessage { request_id, message });
    }

    /// Hand `text` to open WebSocket `request_id`'s worker to send.
    pub fn stream_send(&self, actor: Actor, request_id: u64, text: String) -> Result<(), String> {
        let inner = self.inner.lock().unwrap();
        match inner.running.get(&request_id).map(|f| f.stream.as_ref()) {
            Some(Some(live)) if live.kind == StreamKind::Websocket && live.closed_by.is_none() => {
                live.outbox.send((actor, text)).map_err(|_| format!("stream #{request_id} is closing"))
            }
            Some(Some(live)) if live.kind == StreamKind::Sse => {
                Err(format!("#{request_id} is server-sent events, which only the server sends on"))
            }
            Some(Some(_)) => Err(format!("stream #{request_id} is closing")),
            Some(None) => Err(format!("request #{request_id} is not open as a stream (yet)")),
            None => Err(format!("request #{request_id} is not running")),
        }
    }

    /// Who closed stream `request_id`, once someone has.
    pub fn closed_by(&self, request_id: u64) -> Option<Actor> {
        let inner = self.inner.lock().unwrap();
        inner.running.get(&request_id)?.stream.as_ref()?.closed_by.clone()
    }

    /// Publish why stream `request_id` ended. Its `request_finished`
    /// follows from its worker.
    pub fn stream_closed(&self, actor: Actor, request_id: u64, reason: String, received: u64, sent: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.running.contains_key(&request_id) {
            publish_locked(&mut inner, actor, EventKind::StreamClosed { request_id, reason, received, sent });
        }
    }

    /// Stream `request_id`'s messages numbered after `after`, up to
    /// `limit`; whether it's still open; how many more there are. `None` if
    /// the request was never a stream.
    pub fn stream_messages(&self, request_id: u64, after: u64, limit: u64) -> Option<(Vec<StreamItem>, bool, u64)> {
        let inner = self.inner.lock().unwrap();
        let mut opened = false;
        let mut closed = false;
        let mut messages = Vec::new();
        let mut more = 0;
        for e in &inner.events {
            match &e.kind {
                EventKind::StreamOpened { request_id: id, .. } if *id == request_id => opened = true,
                EventKind::StreamClosed { request_id: id, .. } if *id == request_id => closed = true,
                EventKind::StreamMessage { request_id: id, message } if *id == request_id && message.n > after => {
                    if (messages.len() as u64) < limit {
                        messages.push(StreamItem { at_ms: e.at_ms, by: e.actor.clone(), message: message.clone() });
                    } else {
                        more += 1;
                    }
                }
                _ => {}
            }
        }
        // A core that stopped mid-stream closed it as a dangling request.
        let open = opened && !closed && inner.running.contains_key(&request_id);
        opened.then_some((messages, open, more))
    }

    /// Publish how a request ended, unless it was cancelled first.
    pub fn end_request(&self, actor: Actor, request_id: u64, kind: EventKind) {
        self.end_checked(actor, request_id, kind, Vec::new());
    }


    /// Publish how a request ended and, right after it under the same lock,
    /// how its expectations came out (if it had any), unless it was
    /// cancelled first. False if it was.
    pub fn end_checked(&self, actor: Actor, request_id: u64, kind: EventKind, results: Vec<CheckResult>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.running.remove(&request_id).is_none() {
            return false;
        }
        inner.awaiting.remove(&request_id);
        publish_locked(&mut inner, actor.clone(), kind);
        if !results.is_empty() {
            publish_locked(&mut inner, actor, EventKind::RequestChecked { request_id, results });
        }
        true
    }

    /// Cancel a running request on `actor`'s behalf. False when it is not
    /// running: never sent, or already ended.
    pub fn cancel_request(&self, actor: Actor, request_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        cancel_locked(&mut inner, actor, request_id)
    }

    /// Publish that request `request_id` waits for a person, and give what
    /// their answer arrives on: whether they allowed it, and who they are.
    /// `None` if it was cancelled before it got here. A cancel drops the
    /// sending side, which wakes the receiver with an error.
    pub fn ask_approval(&self, actor: Actor, request_id: u64, reason: String) -> Option<Receiver<(bool, Actor)>> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.running.contains_key(&request_id) {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        inner.awaiting.insert(request_id, tx);
        publish_locked(&mut inner, actor, EventKind::ApprovalAsked { request_id, reason });
        Some(rx)
    }

    /// A person's answer to an approval asked for.
    pub fn decide(&self, actor: Actor, request_id: u64, allow: bool) -> Result<(), String> {
        if actor.kind == ClientKind::Agent {
            return Err("an agent can't approve requests; a person does, in the window or with `tracer approve`".into());
        }
        let mut inner = self.inner.lock().unwrap();
        let Some(answer) = inner.awaiting.remove(&request_id) else {
            return Err(format!("request #{request_id} is not waiting for approval"));
        };
        publish_locked(&mut inner, actor.clone(), EventKind::ApprovalGiven { request_id, allow });
        let _ = answer.send((allow, actor));
        Ok(())
    }

    /// Stop waiting for an answer nobody gave. False if one came meanwhile,
    /// which the receiver then holds.
    pub fn give_up_approval(&self, request_id: u64) -> bool {
        self.inner.lock().unwrap().awaiting.remove(&request_id).is_some()
    }

    /// The kill switch. Switched off, agents' running requests are cancelled
    /// in `actor`'s name.
    pub fn set_agents(&self, actor: Actor, allowed: bool) -> Result<(), String> {
        if actor.kind == ClientKind::Agent {
            return Err("an agent can't switch agents on or off".into());
        }
        let mut inner = self.inner.lock().unwrap();
        inner.agents_allowed = allowed;
        publish_locked(&mut inner, actor.clone(), EventKind::AgentsSwitched { allowed });
        if !allowed {
            let mut theirs: Vec<u64> =
                inner.running.iter().filter(|(_, f)| f.kind == ClientKind::Agent).map(|(id, _)| *id).collect();
            theirs.sort();
            for request_id in theirs {
                cancel_locked(&mut inner, actor.clone(), request_id);
            }
            let mut benches: Vec<u64> =
                inner.benches.iter().filter(|(_, b)| b.kind == ClientKind::Agent).map(|(id, _)| *id).collect();
            benches.sort();
            for bench_id in benches {
                stop_bench_locked(&mut inner, &actor, bench_id);
            }
            let mut untils: Vec<u64> =
                inner.untils.iter().filter(|(_, u)| u.kind == ClientKind::Agent).map(|(id, _)| *id).collect();
            untils.sort();
            for until_id in untils {
                stop_until_locked(&mut inner, &actor, until_id);
            }
        }
        Ok(())
    }

    /// Why `actor` may not send `command`, if it may not: the guard's
    /// rules, then, for a send, the policy's rate. A send let through here
    /// counts towards the rate.
    pub fn refused(&self, actor: &Actor, command: &Command) -> Option<String> {
        if actor.kind != ClientKind::Agent {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        let rules = inner.policy.for_agent(&actor.name);
        if let Some(why) = guard::refused(actor, command, inner.agents_allowed, rules.access) {
            return Some(why);
        }
        match command {
            Command::Send { .. } | Command::Rerun { .. } | Command::RunBench { .. } | Command::StreamSend { .. } => {
                take_rate_locked(&mut inner, actor, &rules).err()
            }
            _ => None,
        }
    }

    /// Count a request `actor` starts towards its rate, or say why it may
    /// not start one now. For a flow's steps, which are one command.
    pub fn take_rate(&self, actor: &Actor) -> Result<(), String> {
        if actor.kind != ClientKind::Agent {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        let rules = inner.policy.for_agent(&actor.name);
        take_rate_locked(&mut inner, actor, &rules)
    }

    /// The rules `actor` is held to, if it is an agent.
    pub fn rules_for(&self, actor: &Actor) -> Option<AgentRules> {
        (actor.kind == ClientKind::Agent).then(|| self.inner.lock().unwrap().policy.for_agent(&actor.name))
    }

    /// The policy's rules, every agent's first, and why the file was not
    /// taken, if it wasn't. An agent is given only its own rules.
    pub fn policy(&self, actor: &Actor) -> (Vec<AgentRules>, Option<String>) {
        let inner = self.inner.lock().unwrap();
        let rules = match actor.kind {
            ClientKind::Agent => vec![inner.policy.for_agent(&actor.name)],
            _ => inner.policy.rules().to_vec(),
        };
        (rules, inner.policy_error.clone())
    }

    pub fn publish(&self, actor: Actor, kind: EventKind) -> u64 {
        publish_locked(&mut self.inner.lock().unwrap(), actor, kind)
    }

    /// Every saved request, the event number that's true at, and the files
    /// that didn't parse. Read under the lock that saves and deletes take, so
    /// no change falls between the list and its number.
    pub fn list_requests(&self) -> (Vec<SavedRequest>, u64, Vec<String>) {
        let inner = self.inner.lock().unwrap();
        let (requests, skipped) = files::list(&self.root);
        (requests, inner.next_seq, skipped)
    }

    /// Write `request` at `path` and publish it. Gives the path as written.
    pub fn save_request(&self, actor: Actor, path: &str, request: HttpRequest) -> Result<String, String> {
        let path = files::normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        files::write(&self.root, &path, &request).map_err(|e| format!("could not save `{path}`: {e}"))?;
        inner.disk.requests.insert(path.clone(), request.clone());
        publish_locked(&mut inner, actor, EventKind::RequestSaved { path: path.clone(), request });
        Ok(path)
    }

    pub fn delete_request(&self, actor: Actor, path: &str) -> Result<(), String> {
        let path = files::normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        files::remove(&self.root, &path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => format!("there is no saved request `{path}`"),
            _ => format!("could not delete `{path}`: {e}"),
        })?;
        inner.disk.requests.remove(&path);
        publish_locked(&mut inner, actor, EventKind::RequestDeleted { path });
        Ok(())
    }

    pub fn list_environments(&self) -> (Vec<Environment>, u64, Vec<String>) {
        let inner = self.inner.lock().unwrap();
        let (environments, skipped) = envs::list(&self.root);
        (environments, inner.next_seq, skipped)
    }

    /// The environment `name` as clients see it: its variables, and its
    /// secrets by name only.
    pub fn environment_shown(&self, name: &str) -> Result<Environment, String> {
        let _inner = self.inner.lock().unwrap();
        envs::read(&self.root, name)
    }

    /// The environment `name` with its secrets filled in, for sending, and
    /// the secrets alone, for redacting what comes back. Never for a client.
    pub fn environment_for_sending(&self, name: &str) -> Result<(Environment, Vec<(String, String)>), String> {
        let _inner = self.inner.lock().unwrap();
        let env = envs::read(&self.root, name)?;
        let secrets = envs::secret_values(&self.root, &env.name);
        Ok((envs::with_secrets(env, &secrets), secrets))
    }

    pub fn set_secret(&self, actor: Actor, env: &str, name: &str, value: Option<String>) -> Result<(), String> {
        let env = envs::normalize(env)?;
        let mut inner = self.inner.lock().unwrap();
        if !envs::file_of(&self.root, &env).exists() {
            return Err(format!("there is no environment `{env}`"));
        }
        let present = value.is_some();
        let had = envs::set_secret(&self.root, &env, name, value.as_deref())?;
        if !present && !had {
            return Err(format!("`{env}` has no secret `{name}`"));
        }
        publish_locked(&mut inner, actor, EventKind::SecretSet { env, name: name.to_string(), present });
        Ok(())
    }

    pub fn save_environment(&self, actor: Actor, mut env: Environment) -> Result<String, String> {
        env.name = envs::normalize(&env.name)?;
        // What's written must read back the same.
        envs::from_text(&envs::to_text(&env))?;
        let mut inner = self.inner.lock().unwrap();
        envs::write(&self.root, &env).map_err(|e| format!("could not save environment `{}`: {e}", env.name))?;
        inner.disk.environments.insert(env.name.clone(), env.vars.clone());
        let name = env.name.clone();
        publish_locked(&mut inner, actor, EventKind::EnvironmentSaved { env });
        Ok(name)
    }

    pub fn delete_environment(&self, actor: Actor, name: &str) -> Result<(), String> {
        let name = envs::normalize(name)?;
        let mut inner = self.inner.lock().unwrap();
        envs::remove(&self.root, &name).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => format!("there is no environment `{name}`"),
            _ => format!("could not delete environment `{name}`: {e}"),
        })?;
        inner.disk.environments.remove(&name);
        envs::remove_secrets(&self.root, &name);
        publish_locked(&mut inner, actor, EventKind::EnvironmentDeleted { name });
        Ok(())
    }

    /// Read `requests/` and `environments/` again and publish what changed
    /// behind the core, in the name of [`ClientKind::Disk`]. A file that
    /// doesn't parse (an editor halfway through writing it, say) is left as
    /// it was last known until it does.
    pub fn rescan(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Disk::read(&self.root);
        let disk = Actor { kind: ClientKind::Disk, name: "disk".into() };
        let mut changes = Vec::new();
        for (path, request) in &now.requests {
            if inner.disk.requests.get(path) != Some(request) {
                changes.push(EventKind::RequestSaved { path: path.clone(), request: request.clone() });
            }
        }
        for path in inner.disk.requests.keys() {
            if !now.requests.contains_key(path) && !files::file_of(&self.root, path).exists() {
                changes.push(EventKind::RequestDeleted { path: path.clone() });
            }
        }
        for (name, vars) in &now.environments {
            if inner.disk.environments.get(name) != Some(vars) {
                let secrets = envs::read(&self.root, name).map(|e| e.secrets).unwrap_or_default();
                changes.push(EventKind::EnvironmentSaved { env: Environment { name: name.clone(), vars: vars.clone(), secrets } });
            }
        }
        for name in inner.disk.environments.keys() {
            if !now.environments.contains_key(name) && !envs::file_of(&self.root, name).exists() {
                changes.push(EventKind::EnvironmentDeleted { name: name.clone() });
            }
        }
        for change in changes {
            match &change {
                EventKind::RequestSaved { path, request } => {
                    inner.disk.requests.insert(path.clone(), request.clone());
                }
                EventKind::RequestDeleted { path } => {
                    inner.disk.requests.remove(path);
                }
                EventKind::EnvironmentSaved { env } => {
                    inner.disk.environments.insert(env.name.clone(), env.vars.clone());
                }
                EventKind::EnvironmentDeleted { name } => {
                    inner.disk.environments.remove(name);
                }
                _ => {}
            }
            publish_locked(&mut inner, disk.clone(), change);
        }

        // A policy that stops parsing keeps the rules it had: agents aren't
        // freed or locked out halfway through someone's edit.
        let Ok(text) = read_policy(&self.root) else { return };
        if text != inner.policy_text {
            let parsed = text.as_deref().map(Policy::parse).unwrap_or_else(|| Ok(Policy::default()));
            inner.policy_text = text;
            match parsed {
                Ok(policy) => {
                    inner.policy = policy;
                    inner.policy_error = None;
                }
                Err(e) => inner.policy_error = Some(format!("{POLICY_PATH}: {e}")),
            }
            let (rules, error) = (inner.policy.rules().to_vec(), inner.policy_error.clone());
            publish_locked(&mut inner, disk, EventKind::PolicyChanged { rules, error });
        }
    }

    pub fn status(&self) -> (Vec<Actor>, u64, bool) {
        let inner = self.inner.lock().unwrap();
        (inner.clients.values().cloned().collect(), inner.next_seq, inner.agents_allowed)
    }
}

/// The policy file's text, `None` if there is none, or why it couldn't be
/// read (locked by an editor saving it, say), which is not the same as none.
fn read_policy(root: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(root.join(POLICY_PATH)) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{POLICY_PATH}: {e}")),
    }
}

fn take_rate_locked(inner: &mut Inner, actor: &Actor, rules: &AgentRules) -> Result<(), String> {
    let Some(rate) = rules.rate else { return Ok(()) };
    let window = Duration::from_secs(rate.per_secs);
    let now = Instant::now();
    let starts = inner.starts.entry(actor.name.to_ascii_lowercase()).or_default();
    while starts.front().is_some_and(|at| now.duration_since(*at) >= window) {
        starts.pop_front();
    }
    if starts.len() >= rate.count as usize {
        let wait = window - now.duration_since(starts[0]);
        return Err(format!(
            "the workspace's policy lets {} start {} requests every {} s; the next in {} s",
            actor.name,
            rate.count,
            rate.per_secs,
            wait.as_secs() + 1
        ));
    }
    starts.push_back(now);
    Ok(())
}

fn stop_until_locked(inner: &mut Inner, actor: &Actor, until_id: u64) -> bool {
    match inner.untils.get_mut(&until_id) {
        Some(until) if until.stopped.is_none() => {
            until.stop.store(true, Ordering::SeqCst);
            until.stopped = Some(format!("stopped by {}", actor.name));
            if let Some(request_id) = until.current.filter(|id| inner.running.contains_key(id)) {
                cancel_locked(inner, actor.clone(), request_id);
            }
            true
        }
        _ => false,
    }
}

fn stop_bench_locked(inner: &mut Inner, actor: &Actor, bench_id: u64) -> bool {
    match inner.benches.get_mut(&bench_id) {
        Some(bench) if bench.stopped.is_none() => {
            bench.stop.store(true, Ordering::SeqCst);
            bench.stopped = Some(format!("stopped by {}", actor.name));
            // Its first request, if it's still out or waiting for a person.
            let first: Vec<u64> = inner.running.iter().filter(|(_, f)| f.bench == Some(bench_id)).map(|(id, _)| *id).collect();
            for request_id in first {
                cancel_locked(inner, actor.clone(), request_id);
            }
            true
        }
        _ => false,
    }
}

fn cancel_locked(inner: &mut Inner, actor: Actor, request_id: u64) -> bool {
    // An open stream is closed by its worker, which ends it as finished.
    if let Some(flight) = inner.running.get_mut(&request_id) {
        if let Some(live) = flight.stream.as_mut() {
            if live.closed_by.is_none() {
                live.closed_by = Some(actor);
                flight.cancelled.store(true, Ordering::SeqCst);
            }
            return true;
        }
    }
    let Some(flight) = inner.running.remove(&request_id) else { return false };
    flight.cancelled.store(true, Ordering::SeqCst);
    // Dropping the answer's sender wakes a worker waiting for approval.
    inner.awaiting.remove(&request_id);
    publish_locked(inner, actor, EventKind::RequestCancelled { request_id });
    true
}

/// Every request in `events`, oldest first, each with how it stands.
fn entries(events: &[Event]) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = Vec::new();
    for e in events {
        let (request_id, ended) = match &e.kind {
            EventKind::RequestStarted { request_id, request, env, rerun_of, flow, bench } => {
                entries.push(HistoryEntry {
                    request_id: *request_id,
                    at_ms: e.at_ms,
                    actor: e.actor.clone(),
                    request: request.clone(),
                    env: env.clone(),
                    rerun_of: *rerun_of,
                    flow: *flow,
                    bench: *bench,
                    ended: Ended::Running,
                    checks: None,
                });
                continue;
            }
            EventKind::RequestChecked { request_id, results } => {
                let passed = results.iter().filter(|r| r.pass).count() as u32;
                if let Some(entry) = entries.iter_mut().rev().find(|x| x.request_id == *request_id) {
                    entry.checks = Some((passed, results.len() as u32 - passed));
                }
                continue;
            }
            EventKind::ApprovalAsked { request_id, reason } => {
                (*request_id, Ended::AwaitingApproval { reason: reason.clone() })
            }
            // Refused, it fails next; allowed, it runs.
            EventKind::ApprovalGiven { request_id, .. } => (*request_id, Ended::Running),
            EventKind::RequestFinished { request_id, response } => (
                *request_id,
                Ended::Done { status: response.status, total_ms: response.timing.total_ms, size: response.size },
            ),
            EventKind::RequestFailed { request_id, error } => (*request_id, Ended::Failed { error: error.clone() }),
            EventKind::RequestCancelled { request_id } => (*request_id, Ended::Cancelled { by: e.actor.clone() }),
            _ => continue,
        };
        // Ends come soon after their starts, so look from the newest.
        if let Some(entry) = entries.iter_mut().rev().find(|x| x.request_id == request_id) {
            entry.ended = ended;
        }
    }
    entries
}

fn publish_locked(inner: &mut Inner, actor: Actor, kind: EventKind) -> u64 {
    let seq = inner.next_seq;
    inner.next_seq += 1;
    let event = Event { seq, at_ms: now_ms(), actor, kind };
    // Journal first: an event a client has seen is an event the journal has.
    if let Ok(line) = serde_json::to_string(&event) {
        let _ = writeln!(inner.journal, "{line}");
    }
    // A subscriber whose connection has gone is dropped here rather than
    // failing the publish for everyone else.
    inner
        .subscribers
        .retain(|(_, out)| out.send(ServerMessage::Event(event.clone())).is_ok());
    inner.events.push(event);
    seq
}

/// A JSON file's value, or the default when it's missing or unreadable.
/// Which jar a request sent with `env` uses.
fn jar_key(browser: &Browser, env: Option<&str>) -> String {
    if browser.jar_per_env { env.unwrap_or_default().to_string() } else { String::new() }
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
