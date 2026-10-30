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
    /// The core's state, for `tracer status`.
    Status,
    /// Stop the core: it publishes `core_stopping`, replies, stops taking
    /// clients and exits. The window sends it on close when it started the
    /// core itself.
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
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

/// Where a request's time went, in milliseconds since it was sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    /// Until the status line and headers had arrived.
    pub ttfb_ms: f64,
    /// Until the whole body had arrived.
    pub total_ms: f64,
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
    Status { clients: Vec<Actor>, next_seq: u64, workspace: String },
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
    /// it was sent with. Resolved values stay out of the journal.
    RequestStarted {
        request_id: u64,
        request: HttpRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
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
}
