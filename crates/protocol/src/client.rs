//! Blocking clients, for the CLI and for the Rux window's native crate.
//!
//! Replies and events arrive on one connection. [`Client::call`] reads until
//! the reply it is waiting for and keeps any event that came first, so
//! [`Client::next_event`] still hands every event over, in order.
//!
//! A window cannot wait on events and make calls from one thread, so
//! [`Client::into_shared`] turns a client into a [`Shared`] one: a reader
//! thread of its own hands each event to a callback, in order, and routes each
//! reply to whichever thread is waiting for it.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::{
    ClientKind, Command, Discovery, Event, Outcome, ReplyBody, Request, ServerMessage, DISCOVERY_PATH,
    PROTOCOL_VERSION,
};

pub struct Client {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    next_id: u64,
    pending: VecDeque<Event>,
    /// The number the core gave this client in `Welcome`.
    pub client_id: u64,
}

impl Client {
    /// Find the workspace's core through its discovery file and say hello.
    pub fn connect(workspace: &Path, kind: ClientKind, name: &str) -> io::Result<Client> {
        let path = workspace.join(DISCOVERY_PATH);
        let text = fs::read_to_string(&path).map_err(|e| {
            io::Error::new(e.kind(), format!("no core for {} ({}): start one with `tracer core`", workspace.display(), e))
        })?;
        let found: Discovery = serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Client::connect_to(&found, kind, name)
    }

    pub fn connect_to(found: &Discovery, kind: ClientKind, name: &str) -> io::Result<Client> {
        let stream = TcpStream::connect(("127.0.0.1", found.port))?;
        stream.set_nodelay(true)?;
        let mut client = Client {
            reader: BufReader::new(stream.try_clone()?),
            writer: BufWriter::new(stream),
            next_id: 1,
            pending: VecDeque::new(),
            client_id: 0,
        };
        let hello = Command::Hello {
            protocol: PROTOCOL_VERSION,
            token: found.token.clone(),
            kind,
            name: name.to_string(),
        };
        match client.call(hello)? {
            Ok(ReplyBody::Welcome { client: id, .. }) => client.client_id = id,
            Ok(other) => return Err(io::Error::other(format!("unexpected answer to hello: {other:?}"))),
            Err(e) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, e)),
        }
        Ok(client)
    }

    /// Send `command` and wait for its reply. The outer error is the
    /// connection's; the inner one is the core refusing the command.
    pub fn call(&mut self, command: Command) -> io::Result<Result<ReplyBody, String>> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&Request { id, command }).map_err(io::Error::other)?;
        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;
        loop {
            match self.read()? {
                ServerMessage::Reply(reply) if reply.id == id => {
                    return Ok(match reply.outcome {
                        Outcome::Ok(body) => Ok(body),
                        Outcome::Error(e) => Err(e),
                    })
                }
                ServerMessage::Reply(_) => continue,
                ServerMessage::Event(event) => self.pending.push_back(event),
            }
        }
    }

    /// Hand the connection to a reader thread of its own, which gives `on`
    /// every event in the core's order (any that arrived during earlier calls
    /// first) and then [`Incoming::Closed`] once, when the connection ends.
    /// `on` runs on that thread, so it must not wait on a [`Shared::call`]:
    /// the reply it would wait for is read by the thread it is blocking.
    pub fn into_shared(self, mut on: impl FnMut(Incoming) + Send + 'static) -> io::Result<Shared> {
        let Client { mut reader, writer, next_id, pending, client_id } = self;
        let waiting = Arc::new(Mutex::new(Waiting { open: true, calls: HashMap::new() }));
        let routes = waiting.clone();
        thread::Builder::new().name("tracer-client".into()).spawn(move || {
            for event in pending {
                on(Incoming::Event(event));
            }
            let why = loop {
                match read_message(&mut reader) {
                    Ok(ServerMessage::Reply(reply)) => {
                        let caller = routes.lock().unwrap().calls.remove(&reply.id);
                        if let Some(caller) = caller {
                            let _ = caller.send(match reply.outcome {
                                Outcome::Ok(body) => Ok(body),
                                Outcome::Error(e) => Err(e),
                            });
                        }
                    }
                    Ok(ServerMessage::Event(event)) => on(Incoming::Event(event)),
                    Err(e) => break e.to_string(),
                }
            };
            // Dropping the senders wakes every caller still waiting, with an
            // error, rather than leaving it blocked on a reply that will not come.
            {
                let mut waiting = routes.lock().unwrap();
                waiting.open = false;
                waiting.calls.clear();
            }
            on(Incoming::Closed(why));
        })?;
        Ok(Shared { writer: Mutex::new(writer), next_id: AtomicU64::new(next_id), waiting, client_id })
    }

    /// The next event, in the core's order.
    pub fn next_event(&mut self) -> io::Result<Event> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        loop {
            if let ServerMessage::Event(event) = self.read()? {
                return Ok(event);
            }
        }
    }

    fn read(&mut self) -> io::Result<ServerMessage> {
        read_message(&mut self.reader)
    }
}

/// What a [`Shared`] client's reader thread hands over.
#[derive(Debug)]
pub enum Incoming {
    Event(Event),
    /// The connection ended, and why. Nothing follows it.
    Closed(String),
}

/// A client any thread may call through; see [`Client::into_shared`].
pub struct Shared {
    writer: Mutex<BufWriter<TcpStream>>,
    next_id: AtomicU64,
    waiting: Arc<Mutex<Waiting>>,
    /// The number the core gave this client in `Welcome`.
    pub client_id: u64,
}

struct Waiting {
    /// False once the reader thread has seen the connection end.
    open: bool,
    calls: HashMap<u64, mpsc::Sender<Result<ReplyBody, String>>>,
}

impl Shared {
    /// Send `command` and wait for its reply, as [`Client::call`] does. Any
    /// number of threads may call at once.
    pub fn call(&self, command: Command) -> io::Result<Result<ReplyBody, String>> {
        let hung_up = || io::Error::new(io::ErrorKind::NotConnected, "the core hung up");
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            // Registered before the request is written, so a reply that comes
            // back at once still finds its caller.
            let mut waiting = self.waiting.lock().unwrap();
            if !waiting.open {
                return Err(hung_up());
            }
            waiting.calls.insert(id, tx);
        }
        let line = serde_json::to_string(&Request { id, command }).map_err(io::Error::other)?;
        let written = {
            let mut writer = self.writer.lock().unwrap();
            writeln!(writer, "{line}").and_then(|_| writer.flush())
        };
        if let Err(e) = written {
            self.waiting.lock().unwrap().calls.remove(&id);
            return Err(e);
        }
        rx.recv().map_err(|_| hung_up())
    }

    /// Hang up. The reader thread sees the connection end and hands over
    /// [`Incoming::Closed`]; the core publishes this client's `client_left`.
    pub fn close(&self) {
        let writer = self.writer.lock().unwrap();
        let _ = writer.get_ref().shutdown(std::net::Shutdown::Both);
    }
}

fn read_message(reader: &mut BufReader<TcpStream>) -> io::Result<ServerMessage> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the core hung up"));
    }
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
