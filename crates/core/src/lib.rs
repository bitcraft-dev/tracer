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

mod conn;
mod http;
mod hub;

use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use tracer_protocol::{Discovery, DISCOVERY_PATH, PROTOCOL_VERSION};

use crate::hub::Hub;

/// A core that is listening. Dropping it does not stop it; call
/// [`Running::stop`].
pub struct Running {
    pub port: u16,
    pub token: String,
    workspace: PathBuf,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
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

    /// Block until the core is stopped from elsewhere.
    pub fn wait(mut self) {
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }

    /// Stop accepting clients and remove the discovery file. Clients already
    /// connected keep their threads until they hang up.
    pub fn stop(mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // The accept loop is blocked in `accept`; a connection wakes it.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let _ = fs::remove_file(self.workspace.join(DISCOVERY_PATH));
    }
}

/// Open `workspace` (creating `.tracer/` in it), listen on an ephemeral port
/// on `127.0.0.1`, and write the discovery file.
pub fn start(workspace: &Path) -> io::Result<Running> {
    fs::create_dir_all(workspace.join(".tracer"))?;
    let hub = Arc::new(Hub::open(workspace)?);
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = new_token();

    let running = Running {
        port,
        token: token.clone(),
        workspace: workspace.to_path_buf(),
        stopping: Arc::new(AtomicBool::new(false)),
        accept: None,
    };
    let discovery = serde_json::to_string_pretty(&running.discovery()).map_err(io::Error::other)?;
    fs::write(workspace.join(DISCOVERY_PATH), discovery)?;

    let stopping = running.stopping.clone();
    let accept = thread::Builder::new().name("tracer-accept".into()).spawn(move || {
        for stream in listener.incoming() {
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let hub = hub.clone();
            let token = token.clone();
            let _ = thread::Builder::new()
                .name("tracer-client".into())
                .spawn(move || conn::serve(stream, hub, &token));
        }
    })?;
    Ok(Running { accept: Some(accept), ..running })
}

/// 128 bits from the OS's randomness, as hex. `RandomState` is seeded from
/// the OS once per process and per instance, which is enough for a token that
/// only has to keep other local processes out.
fn new_token() -> String {
    (0..2)
        .map(|_| {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(std::process::id() as u64);
            format!("{:016x}", h.finish())
        })
        .collect()
}
