//! A blocking client, for the CLI and for the Rux window's native crate.
//!
//! Replies and events arrive on one connection. [`Client::call`] reads until
//! the reply it is waiting for and keeps any event that came first, so
//! [`Client::next_event`] still hands every event over, in order.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::Path;

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
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the core hung up"));
        }
        serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}
