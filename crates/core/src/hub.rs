//! The event hub: one numbered order of events, journalled and fanned out.
//!
//! Numbering, journalling and fan-out happen under one lock, so every
//! subscriber receives events in exactly the order of their numbers, and a
//! subscriber that joins with `since` is replayed and attached in the same
//! critical section, so no event falls between its replay and its first live
//! one.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tracer_protocol::{
    Actor, ClientKind, Environment, Event, EventKind, HttpRequest, SavedRequest, ServerMessage, JOURNAL_PATH,
};

use crate::{envs, files};

pub struct Hub {
    inner: Mutex<Inner>,
    workspace: String,
    root: PathBuf,
}

struct Inner {
    next_seq: u64,
    next_client: u64,
    next_request: u64,
    journal: File,
    /// Every event so far, for replay. Kept whole for now; a long-lived
    /// workspace will want this paged from the file instead.
    events: Vec<Event>,
    clients: BTreeMap<u64, Actor>,
    subscribers: Vec<(u64, Sender<ServerMessage>)>,
    /// Requests still running, each with the flag its worker checks to stop
    /// early. Whoever takes a request out of here publishes how it ended, so
    /// it ends exactly once.
    running: HashMap<u64, Arc<AtomicBool>>,
    /// What the core last knew of the files, so a rescan publishes only what
    /// changed behind it. Its own saves and deletes update this under the
    /// same lock, so they aren't echoed back as disk changes.
    disk: Disk,
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
        let hub = Hub {
            inner: Mutex::new(Inner {
                next_seq,
                next_client: 1,
                next_request,
                journal,
                events,
                clients: BTreeMap::new(),
                subscribers: Vec::new(),
                running: HashMap::new(),
                disk: Disk::read(workspace),
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
    pub fn start_request(&self, actor: Actor, request: HttpRequest, env: Option<String>) -> (u64, Arc<AtomicBool>) {
        let mut inner = self.inner.lock().unwrap();
        let request_id = inner.next_request;
        inner.next_request += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        inner.running.insert(request_id, cancelled.clone());
        publish_locked(&mut inner, actor, EventKind::RequestStarted { request_id, request, env });
        (request_id, cancelled)
    }

    /// Publish how a request ended, unless it was cancelled first.
    pub fn end_request(&self, actor: Actor, request_id: u64, kind: EventKind) {
        let mut inner = self.inner.lock().unwrap();
        if inner.running.remove(&request_id).is_some() {
            publish_locked(&mut inner, actor, kind);
        }
    }

    /// Cancel a running request on `actor`'s behalf. False when it is not
    /// running: never sent, or already ended.
    pub fn cancel_request(&self, actor: Actor, request_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(cancelled) = inner.running.remove(&request_id) else { return false };
        cancelled.store(true, Ordering::SeqCst);
        publish_locked(&mut inner, actor, EventKind::RequestCancelled { request_id });
        true
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
    }

    pub fn status(&self) -> (Vec<Actor>, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.clients.values().cloned().collect(), inner.next_seq)
    }
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

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
