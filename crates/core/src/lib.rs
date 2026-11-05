//! Tracer's core: the one process that owns a workspace.
//!
//! Every client, whether the Rux window, the `tracer` CLI or an AI agent,
//! connects to it over TCP on `127.0.0.1`, says [`Command::Hello`] with the
//! token from the discovery file, and then sends commands. What those
//! commands cause is published as [`Event`]s: numbered, journalled, and
//! pushed to every subscriber in the same order. A window that opens late
//! subscribes with `since` and is replayed the journal, then follows live,
//! with no gap between the two.
//!
//! [`Command::Hello`]: tracer_protocol::Command::Hello

mod api;
mod baselines;
mod bench;
mod conn;
mod cookies;
mod cors;
mod data;
mod diff;
mod discover;
mod envs;
mod expect;
mod explain;
mod export;
mod files;
mod flows;
mod gqlcheck;
mod graphql;
mod guard;
mod http;
mod mock;
mod openapi;
mod import;
mod hub;
mod policy;
mod realtime;
mod schema;
mod transcript;
mod until;
pub mod ws;

use std::fs;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use tracer_protocol::{Discovery, DISCOVERY_PATH, PROTOCOL_VERSION};

use crate::hub::Hub;

/// A core that is listening. Dropping it does not stop it; call
/// [`Running::stop`], or send it [`Command::Shutdown`].
///
/// [`Command::Shutdown`]: tracer_protocol::Command::Shutdown
pub struct Running {
    pub port: u16,
    pub token: String,
    stop: Arc<Stop>,
    accept: Option<JoinHandle<()>>,
}

/// What stopping takes, shared with the connections so a client's `Shutdown`
/// can do it.
pub(crate) struct Stop {
    port: u16,
    workspace: PathBuf,
    stopping: AtomicBool,
}

impl Stop {
    /// Stop accepting clients and remove the discovery file. Clients already
    /// connected keep their threads until they hang up.
    pub(crate) fn stop(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // The accept loop is blocked in `accept`; a connection wakes it.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        let _ = fs::remove_file(self.workspace.join(DISCOVERY_PATH));
    }
}

impl Running {
    pub fn discovery(&self) -> Discovery {
        Discovery {
            port: self.port,
            token: self.token.clone(),
            pid: std::process::id(),
            protocol: PROTOCOL_VERSION,
        }
    }

    /// Block until the core is stopped from elsewhere: by a client's
    /// `Shutdown`, or by [`Running::stop`] on another thread.
    pub fn wait(mut self) {
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }

    /// Stop accepting clients and remove the discovery file. Clients already
    /// connected keep their threads until they hang up.
    pub fn stop(mut self) {
        self.stop.stop();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// Open `workspace` (creating `.tracer/` in it), listen on an ephemeral port
/// on `127.0.0.1`, and write the discovery file.
/// How often the core looks for files changed behind it.
const DISK_SCAN: std::time::Duration = std::time::Duration::from_secs(1);

pub fn start(workspace: &Path) -> io::Result<Running> {
    fs::create_dir_all(workspace.join(".tracer"))?;
    // The journal, the discovery file's token and the secrets stay out of
    // git, without touching the workspace's own .gitignore.
    let ignore = workspace.join(".tracer").join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "# Written by Tracer: nothing in here belongs in git.\n*\n")?;
    }
    let hub = Arc::new(Hub::open(workspace)?);
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = new_token();

    let running = Running {
        port,
        token: token.clone(),
        stop: Arc::new(Stop { port, workspace: workspace.to_path_buf(), stopping: AtomicBool::new(false) }),
        accept: None,
    };
    let discovery = serde_json::to_string_pretty(&running.discovery()).map_err(io::Error::other)?;
    fs::write(workspace.join(DISCOVERY_PATH), discovery)?;

    // Files changed behind the core, by an editor, an agent or git, are
    // published as they're found. Small workspaces read in well under the
    // second between scans.
    let (scan_hub, scan_stop) = (hub.clone(), running.stop.clone());
    thread::Builder::new().name("tracer-disk".into()).spawn(move || {
        while !scan_stop.stopping.load(Ordering::SeqCst) {
            thread::sleep(DISK_SCAN);
            scan_hub.rescan();
        }
    })?;

    let stop = running.stop.clone();
    let accept = thread::Builder::new().name("tracer-accept".into()).spawn(move || {
        for stream in listener.incoming() {
            if stop.stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let (hub, token, stop) = (hub.clone(), token.clone(), stop.clone());
            let _ = thread::Builder::new()
                .name("tracer-client".into())
                .spawn(move || api::serve(stream, hub, &token, &stop));
        }
    })?;
    Ok(Running { accept: Some(accept), ..running })
}

/// 128 bits from the OS's randomness, as hex.
fn new_token() -> String {
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `out` filled from the OS's randomness (`BCryptGenRandom` on Windows,
/// `getrandom` elsewhere). Without it there is nothing safe to fall back
/// to, so the core stops rather than hand out a guessable token.
pub(crate) fn random_bytes(out: &mut [u8]) {
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new().fill(out).expect("the OS's randomness could not be read");
}
