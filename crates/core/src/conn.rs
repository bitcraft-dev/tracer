//! One client's connection: a reader on this thread, a writer on its own.
//!
//! Replies and events for a client go through one channel to one writer, so
//! what the client reads is in the order the core decided, whether it is a
//! reply or an event. A connection is lines of JSON in and out, whatever
//! carries them: TCP here, or a WebSocket or one HTTP call ([`crate::api`]).

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;

use tracer_protocol::{
    Access, Actor, Command, EventKind, HttpRequest, Outcome, Reply, ReplyBody, Request, ServerMessage, PROTOCOL_VERSION,
};

use crate::{envs, guard, http};
use crate::hub::Hub;
use crate::Stop;

/// A client on the core's TCP protocol: a line of JSON each way.
pub fn serve_tcp(stream: TcpStream, hub: Arc<Hub>, token: &str, stop: &Stop) {
    let Ok(write_half) = stream.try_clone() else { return };
    let mut w = BufWriter::new(write_half);
    let write_line = move |line: &str| writeln!(w, "{line}").and_then(|_| w.flush());
    serve(BufReader::new(stream).lines(), write_line, hub, token, stop);
}

/// A client whose messages arrive as `lines`, and whose replies and events
/// go to `write_line`, one message each, until either side stops.
pub fn serve(
    lines: impl IntoIterator<Item = io::Result<String>>,
    mut write_line: impl FnMut(&str) -> io::Result<()> + Send + 'static,
    hub: Arc<Hub>,
    token: &str,
    stop: &Stop,
) {
    let (out, rx) = mpsc::channel::<ServerMessage>();
    let writer = thread::spawn(move || {
        for msg in rx {
            let Ok(line) = serde_json::to_string(&msg) else { continue };
            if write_line(&line).is_err() {
                break;
            }
        }
    });

    let mut session: Option<(u64, Actor)> = None;
    let mut shutting_down = false;
    for line in lines {
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
        if let Some((_, actor)) = &session {
            if let Some(why) = hub.refused(actor, &request.command) {
                if out.send(reply(id, Err(why))).is_err() {
                    break;
                }
                continue;
            }
        }
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
                send(&hub, actor, request, env, None, |request_id| {
                    let _ = out.send(reply(id, Ok(ReplyBody::Accepted { request_id })));
                });
                continue;
            }
            (Some((_, actor)), Command::Rerun { request_id, env }) => match hub.written(request_id) {
                Some((request, was)) => {
                    let env = match env {
                        Some(name) if name.is_empty() => None,
                        Some(name) => Some(name),
                        None => was,
                    };
                    send(&hub, actor, request, env, Some(request_id), |request_id| {
                        let _ = out.send(reply(id, Ok(ReplyBody::Accepted { request_id })));
                    });
                    continue;
                }
                None => Err(format!("there is no request #{request_id} in the history")),
            },
            (Some(_), Command::History { filter }) => {
                let (entries, matched, as_of) = hub.history(&filter);
                Ok(ReplyBody::History { entries, matched, as_of })
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
            (Some(_), Command::Response { request_id }) => match hub.response(request_id) {
                Some((entry, response)) => Ok(ReplyBody::Response { entry, response }),
                None => Err(format!("there is no request #{request_id} in the history")),
            },
            (Some((_, actor)), Command::Decide { request_id, allow }) => {
                hub.decide(actor.clone(), request_id, allow).map(|_| ReplyBody::Decided)
            }
            (Some((_, actor)), Command::SetAgents { allowed }) => {
                hub.set_agents(actor.clone(), allowed).map(|_| ReplyBody::AgentsSwitched)
            }
            (Some((_, actor)), Command::Policy) => {
                let (rules, error) = hub.policy(actor);
                Ok(ReplyBody::Policy { rules, error })
            }
            (Some(_), Command::Status) => {
                let (clients, next_seq, agents_allowed) = hub.status();
                Ok(ReplyBody::Status { clients, next_seq, workspace: hub.workspace().to_string(), agents_allowed })
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

/// Publish that `request` started, have `accepted` answer the sender with
/// its id, then send it on a thread of its own and publish how it ended.
fn send(
    hub: &Arc<Hub>,
    actor: &Actor,
    request: HttpRequest,
    env: Option<String>,
    rerun_of: Option<u64>,
    accepted: impl FnOnce(u64),
) {
    // The rules as they stand when it's sent, for the whole of its run.
    let rules = hub.rules_for(actor);
    let (request_id, cancelled) = hub.start_request(actor.clone(), request.clone(), env.clone(), rerun_of);
    // Before the thread starts, so the sender hears its id before the end.
    accepted(request_id);
    let (hub, actor) = (hub.clone(), actor.clone());
    thread::spawn(move || {
        // Resolved here, not before it started, so a missing variable is a
        // failed request every client sees; and before any approval, so a
        // person is only asked about what the policy lets through.
        let resolved = env.as_deref().map(|name| hub.environment_for_sending(name)).transpose();
        let (resolved, secrets) = match resolved {
            Ok(Some((env, secrets))) => (envs::resolve(&request, Some(&env)), secrets),
            Ok(None) => (envs::resolve(&request, None), Vec::new()),
            Err(e) => (Err(e), Vec::new()),
        };
        if let (Some(rules), Ok(sending)) = (&rules, &resolved) {
            if let Some(why) = guard::outside_rules(rules, sending) {
                let error = envs::redact(&format!("refused by the workspace's policy: {why}"), &secrets);
                hub.end_request(actor, request_id, EventKind::RequestFailed { request_id, error });
                return;
            }
            if let Some(reason) = guard::needs_approval(rules, &request, env.as_deref()) {
                if !approved(&hub, &actor, request_id, reason) {
                    return;
                }
            }
        }
        // A local agent's request mustn't be redirected off the machine.
        let redirects = rules.is_none_or(|r| r.access != Access::Local);
        // Secret values that come back are replaced by their names before
        // anything is published or journalled.
        let kind = match resolved.and_then(|request| http::execute(&request, redirects, &cancelled)) {
            Ok(response) => EventKind::RequestFinished { request_id, response: envs::redact_response(response, &secrets) },
            Err(error) => EventKind::RequestFailed { request_id, error: envs::redact(&error, &secrets) },
        };
        // Dropped if the request was cancelled meanwhile.
        hub.end_request(actor, request_id, kind);
    });
}

/// Ask a person to allow request `request_id` and wait for them. True if
/// they did; otherwise the request has been ended, refused, timed out or
/// cancelled, and nothing more is to be done with it.
fn approved(hub: &Hub, actor: &Actor, request_id: u64, reason: String) -> bool {
    let Some(answer) = hub.ask_approval(actor.clone(), request_id, reason) else { return false };
    let decision = match answer.recv_timeout(guard::APPROVAL_TIMEOUT) {
        Ok(decision) => decision,
        // A cancel dropped the sender, and has published the end.
        Err(RecvTimeoutError::Disconnected) => return false,
        Err(RecvTimeoutError::Timeout) => {
            if hub.give_up_approval(request_id) {
                let minutes = guard::APPROVAL_TIMEOUT.as_secs() / 60;
                let error = format!("nobody allowed it within {minutes} minutes");
                hub.end_request(actor.clone(), request_id, EventKind::RequestFailed { request_id, error });
                return false;
            }
            // An answer came as the time ran out; it's in the channel.
            match answer.recv() {
                Ok(decision) => decision,
                Err(_) => return false,
            }
        }
    };
    match decision {
        (true, _) => true,
        (false, person) => {
            let error = format!("refused by {}", person.name);
            hub.end_request(actor.clone(), request_id, EventKind::RequestFailed { request_id, error });
            false
        }
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
