//! One client's connection: a reader on this thread, a writer on its own.
//!
//! Replies and events for a client go through one channel to one writer, so
//! what the client reads is in the order the core decided, whether it is a
//! reply or an event.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use tracer_protocol::{
    Actor, Command, EventKind, Outcome, Reply, ReplyBody, Request, ServerMessage, PROTOCOL_VERSION,
};

use crate::{envs, http};
use crate::hub::Hub;
use crate::Stop;

pub fn serve(stream: TcpStream, hub: Arc<Hub>, token: &str, stop: &Stop) {
    let Ok(write_half) = stream.try_clone() else { return };
    let (out, rx) = mpsc::channel::<ServerMessage>();
    let writer = thread::spawn(move || {
        let mut w = BufWriter::new(write_half);
        for msg in rx {
            let Ok(line) = serde_json::to_string(&msg) else { continue };
            if writeln!(w, "{line}").and_then(|_| w.flush()).is_err() {
                break;
            }
        }
    });

    let mut session: Option<(u64, Actor)> = None;
    let mut shutting_down = false;
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // No id to answer to; say so once with id 0 and carry on.
                let _ = out.send(reply(0, Err(format!("not a request: {e}"))));
                continue;
            }
        };
        let id = request.id;
        let outcome = match (&session, request.command) {
            (None, Command::Hello { protocol, token: given, kind, name }) => {
                if protocol != PROTOCOL_VERSION {
                    Err(format!("protocol {protocol} is not this core's {PROTOCOL_VERSION}"))
                } else if given != token {
                    Err("wrong token".to_string())
                } else {
                    let actor = Actor { kind, name };
                    let client = hub.join(actor.clone());
                    session = Some((client, actor));
                    Ok(ReplyBody::Welcome { client, protocol: PROTOCOL_VERSION })
                }
            }
            (None, _) => Err("say hello first".to_string()),
            (Some(_), Command::Hello { .. }) => Err("already said hello".to_string()),
            (Some((client, _)), Command::Subscribe { since }) => {
                // The reply goes out before the replay, under the hub's lock,
                // so the client reads "subscribed" and then the events.
                hub.subscribe(*client, since, out.clone(), |replayed, next_seq| {
                    reply(id, Ok(ReplyBody::Subscribed { replayed, next_seq }))
                });
                continue;
            }
            (Some((_, actor)), Command::Send { request, env }) => {
                let (request_id, cancelled) = hub.start_request(actor.clone(), request.clone(), env.clone());
                let _ = out.send(reply(id, Ok(ReplyBody::Accepted { request_id })));
                let (hub, actor) = (hub.clone(), actor.clone());
                thread::spawn(move || {
                    // Resolved here, not before it started, so a missing
                    // variable is a failed request every client sees.
                    let env = env.as_deref().map(|name| hub.environment_for_sending(name)).transpose();
                    let (env, secrets) = match env {
                        Ok(Some((env, secrets))) => (Ok(Some(env)), secrets),
                        Ok(None) => (Ok(None), Vec::new()),
                        Err(e) => (Err(e), Vec::new()),
                    };
                    let resolved = env.and_then(|env| envs::resolve(&request, env.as_ref()));
                    // Secret values that come back are replaced by their
                    // names before anything is published or journalled.
                    let kind = match resolved.and_then(|request| http::execute(&request, &cancelled)) {
                        Ok(response) => {
                            EventKind::RequestFinished { request_id, response: envs::redact_response(response, &secrets) }
                        }
                        Err(error) => EventKind::RequestFailed { request_id, error: envs::redact(&error, &secrets) },
                    };
                    // Dropped if the request was cancelled meanwhile.
                    hub.end_request(actor, request_id, kind);
                });
                continue;
            }
            (Some((_, actor)), Command::Cancel { request_id }) => {
                if hub.cancel_request(actor.clone(), request_id) {
                    Ok(ReplyBody::Cancelled)
                } else {
                    Err(format!("request #{request_id} is not running"))
                }
            }
            (Some(_), Command::ListRequests) => {
                let (requests, as_of, skipped) = hub.list_requests();
                Ok(ReplyBody::Requests { requests, as_of, skipped })
            }
            (Some((_, actor)), Command::SaveRequest { path, request }) => {
                hub.save_request(actor.clone(), &path, request).map(|path| ReplyBody::Saved { path })
            }
            (Some((_, actor)), Command::DeleteRequest { path }) => {
                hub.delete_request(actor.clone(), &path).map(|_| ReplyBody::Deleted)
            }
            (Some(_), Command::ListEnvironments) => {
                let (environments, as_of, skipped) = hub.list_environments();
                Ok(ReplyBody::Environments { environments, as_of, skipped })
            }
            (Some((_, actor)), Command::SaveEnvironment { env }) => {
                hub.save_environment(actor.clone(), env).map(|name| ReplyBody::EnvironmentSaved { name })
            }
            (Some((_, actor)), Command::DeleteEnvironment { name }) => {
                hub.delete_environment(actor.clone(), &name).map(|_| ReplyBody::EnvironmentDeleted)
            }
            (Some((_, actor)), Command::SetSecret { env, name, value }) => {
                hub.set_secret(actor.clone(), &env, &name, value).map(|_| ReplyBody::SecretSet)
            }
            (Some(_), Command::Status) => {
                let (clients, next_seq) = hub.status();
                Ok(ReplyBody::Status { clients, next_seq, workspace: hub.workspace().to_string() })
            }
            (Some((_, actor)), Command::Shutdown) => {
                hub.publish(actor.clone(), EventKind::CoreStopping);
                let _ = out.send(reply(id, Ok(ReplyBody::Stopping)));
                // Stopped below, once the writer has flushed the reply, so
                // the asker always hears that it worked.
                shutting_down = true;
                break;
            }
        };
        if out.send(reply(id, outcome)).is_err() {
            break;
        }
    }

    if let Some((client, _)) = session {
        hub.leave(client);
    }
    drop(out);
    let _ = writer.join();
    if shutting_down {
        stop.stop();
    }
}

fn reply(id: u64, outcome: Result<ReplyBody, String>) -> ServerMessage {
    ServerMessage::Reply(Reply {
        id,
        outcome: match outcome {
            Ok(body) => Outcome::Ok(body),
            Err(e) => Outcome::Error(e),
        },
    })
}
