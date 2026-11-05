//! One client's connection: a reader on this thread, a writer on its own.
//!
//! Replies and events for a client go through one channel to one writer, so
//! what the client reads is in the order the core decided, whether it is a
//! reply or an event. A connection is lines of JSON in and out, whatever
//! carries them: TCP here, or a WebSocket or one HTTP call ([`crate::api`]).

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;

use tracer_protocol::{
    Access, Actor, BenchPlan, Browser, CheckResult, ClientKind, Command, DiffSide, HttpResponse, EventKind, HttpRequest, Outcome, Reply, ReplyBody, Request, ServerMessage, BENCH_TOLERANCE_PCT, PROTOCOL_VERSION, STREAM_LIMIT,
};

use crate::baselines::{self, Side};
use crate::{explain, export, files, import};
use crate::{bench, cors, diff, envs, flows, graphql, guard, http, realtime, until};
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
                send(&hub, actor, request, env, None, Vec::new(), |request_id| {
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
                    // A flow's step is sent with the captures it had then.
                    let vars = hub.vars_of(request_id);
                    send(&hub, actor, request, env, Some(request_id), vars, |request_id| {
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
                Some((entry, response, checks)) => Ok(ReplyBody::Response { entry, response, checks }),
                None => Err(format!("there is no request #{request_id} in the history")),
            },
            (Some((_, actor)), Command::Decide { request_id, allow }) => {
                hub.decide(actor.clone(), request_id, allow).map(|_| ReplyBody::Decided)
            }
            (Some((_, actor)), Command::SetAgents { allowed }) => {
                hub.set_agents(actor.clone(), allowed).map(|_| ReplyBody::AgentsSwitched)
            }
            (Some((_, actor)), Command::RunFlow { path, env, keep_going }) => {
                match flows::start(&hub, actor, &path, env, keep_going) {
                    Ok((flow_id, steps)) => Ok(ReplyBody::FlowStarted { flow_id, steps }),
                    Err(e) => Err(e),
                }
            }
            (Some(_), Command::GraphqlSchema { request_id }) => match hub.response(request_id) {
                Some((_, Some(response), _)) => graphql::schema(&response.body).map(|schema| ReplyBody::GraphqlSchema { schema }),
                Some(_) => Err(format!("request #{request_id} has no answer to read a schema from")),
                None => Err(format!("there is no request #{request_id} in the history")),
            },
            (Some((_, actor)), Command::RunBench { request, env, plan }) => {
                bench::start(&hub, actor, request, env, plan).map(|bench_id| ReplyBody::BenchStarted { bench_id })
            }
            (Some((_, actor)), Command::SaveBaseline { bench_id, name }) => {
                hub.save_baseline(actor.clone(), bench_id, &name).map(|(name, path)| ReplyBody::BaselineSaved { name, path })
            }
            (Some(_), Command::ListBaselines) => {
                let (baselines, skipped) = hub.list_baselines();
                Ok(ReplyBody::Baselines { baselines, skipped })
            }
            (Some(_), Command::CompareBench { bench_id, baseline, other_bench, tolerance_pct }) => {
                compare_bench(&hub, bench_id, baseline, other_bench, tolerance_pct)
            }
            (Some((_, actor)), Command::StopBench { bench_id }) => {
                if hub.stop_bench(actor, bench_id) {
                    Ok(ReplyBody::BenchStopped)
                } else {
                    Err(format!("bench {bench_id} is not running"))
                }
            }
            (Some((_, actor)), Command::SetBrowser { mut browser }) => {
                // An origin as a browser writes it: scheme, host, a port that isn't the default.
                let origin = browser.origin.take().map(|o| o.trim().to_string()).filter(|o| !o.is_empty());
                match origin.map(|o| cors::origin_of(&o).ok_or(o)).transpose() {
                    Err(o) => Err(format!("`{o}` isn't an origin such as https://app.example.com")),
                    Ok(origin) => {
                        browser.origin = origin;
                        hub.set_browser(actor.clone(), browser).map(|_| ReplyBody::BrowserSet)
                    }
                }
            }
            (Some(_), Command::Export { request, env, lang }) => {
                let env = env.map(|name| hub.environment_shown(&name)).transpose();
                env.and_then(|env| export::export(&request, env.as_ref(), lang, &hub.browser())).map(|text| ReplyBody::Exported { text })
            }
            (Some(_), Command::Explain { request_id }) => match hub.response(request_id) {
                None => Err(format!("there is no request #{request_id}")),
                Some((entry, response, checks)) => {
                    let env = entry.env.as_deref().and_then(|name| hub.environment_shown(name).ok());
                    let last_good = hub.last_good_before(request_id, &entry.request.method, &entry.request.url);
                    let browser = hub.browser();
                    let case = explain::Case {
                        entry: &entry,
                        response: response.as_ref(),
                        checks: &checks,
                        env: env.as_ref(),
                        browser: &browser,
                        last_good: last_good.as_ref(),
                    };
                    let (failed, summary) = explain::verdict(&entry, &checks);
                    Ok(ReplyBody::Explained { text: explain::bundle(&case), failed, summary })
                }
            },
            (Some(_), Command::Import { text }) => import::import(&text).map(|(request, notes)| ReplyBody::Imported { http: files::to_text(&request), request, notes }),
            (Some((_, actor)), Command::Cookies) => {
                let (browser, cookies) = hub.cookies(actor.kind != ClientKind::Agent);
                Ok(ReplyBody::Cookies { browser, cookies })
            }
            (Some((_, actor)), Command::ClearCookies { domain }) => {
                let removed = hub.clear_cookies(actor.clone(), domain.as_deref());
                Ok(ReplyBody::CookiesCleared { removed: removed as u64 })
            }
            (Some((_, actor)), Command::RunUntil { request, env, plan }) => {
                until::start(&hub, actor, request, env, plan).map(|until_id| ReplyBody::UntilStarted { until_id })
            }
            (Some((_, actor)), Command::StopUntil { until_id }) => {
                if hub.stop_until(actor, until_id) {
                    Ok(ReplyBody::UntilStopped)
                } else {
                    Err(format!("run-until {until_id} is not running"))
                }
            }
            (Some((_, actor)), Command::StreamSend { request_id, text }) => {
                hub.stream_send(actor.clone(), request_id, text).map(|_| ReplyBody::StreamSent)
            }
            (Some(_), Command::StreamMessages { request_id, after, limit }) => {
                match hub.stream_messages(request_id, after, limit.unwrap_or(STREAM_LIMIT).min(STREAM_LIMIT)) {
                    Some((messages, open, more)) => Ok(ReplyBody::StreamMessages { messages, open, more }),
                    None => Err(format!("request #{request_id} has not been a stream")),
                }
            }
            (Some(_), Command::Diff { a, b, ignore, no_defaults }) => {
                let side = |id: u64| match hub.response(id) {
                    Some((entry, Some(response), _)) => Ok((
                        DiffSide {
                            request_id: id,
                            method: entry.request.method,
                            url: entry.request.url,
                            status: response.status,
                        },
                        response,
                    )),
                    Some(_) => Err(format!("request #{id} has no response to compare")),
                    None => Err(format!("there is no request #{id} in the history")),
                };
                side(a).and_then(|a| side(b).map(|b| (a, b))).map(|((sa, ra), (sb, rb))| ReplyBody::Diff {
                    diff: diff::compare((sa, &ra), (sb, &rb), &ignore, !no_defaults),
                })
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
/// its id, then send it on a thread of its own, with `vars` over its
/// environment, and publish how it ended.
fn send(
    hub: &Arc<Hub>,
    actor: &Actor,
    request: HttpRequest,
    env: Option<String>,
    rerun_of: Option<u64>,
    vars: Vec<(String, String)>,
    accepted: impl FnOnce(u64),
) {
    let (request_id, cancelled) = hub.start_request(actor.clone(), request.clone(), env.clone(), rerun_of, None, None);
    // Kept for this one too, so sending it again works the same.
    hub.remember_vars(request_id, &vars);
    // Before the thread starts, so the sender hears its id before the end.
    accepted(request_id);
    let (hub, actor) = (hub.clone(), actor.clone());
    thread::spawn(move || {
        run_one(&hub, &actor, request_id, &cancelled, &request, env.as_deref(), &vars, None);
    });
}

/// How a request run by [`run_one`] ended.
pub(crate) enum Ran {
    /// It ended with this answer, or this error, and these checks, all
    /// published.
    Ended(Result<HttpResponse, String>, Vec<CheckResult>),
    /// It ended some other way, already published: refused, cancelled, or
    /// not allowed in time. Why, for a flow's step.
    Stopped(String),
}

/// Send started request `request_id` and publish how it ends: resolved
/// with `env` and, over it, `extra` (a flow's captures), held to the
/// sender's policy, approved if it must be (as the first of `bench`, if it
/// is), checked. Blocks until then.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_one(
    hub: &Hub,
    actor: &Actor,
    request_id: u64,
    cancelled: &AtomicBool,
    request: &HttpRequest,
    env: Option<&str>,
    extra: &[(String, String)],
    bench: Option<&BenchPlan>,
) -> Ran {
    // The rules as they stand when it's sent, for the whole of its run.
    let rules = hub.rules_for(actor);
    let stopped = |why: String| {
        hub.end_request(actor.clone(), request_id, EventKind::RequestFailed { request_id, error: why.clone() });
        Ran::Stopped(why)
    };
    // Resolved here, not before it started, so a missing variable is a
    // failed request every client sees; and before any approval, so a
    // person is only asked about what the policy lets through.
    let resolved = env.map(|name| hub.environment_for_sending(name)).transpose();
    let (environment, secrets) = match resolved {
        Ok(Some((env, secrets))) => (Ok(Some(env)), secrets),
        Ok(None) => (Ok(None), Vec::new()),
        Err(e) => (Err(e), Vec::new()),
    };
    let resolved = environment
        .and_then(|e| envs::resolve(request, envs::with_extra(e, extra).as_ref()))
        .and_then(|r| if graphql::is_graphql(&r) { graphql::to_http(&r) } else { Ok(r) });
    if let (Some(rules), Ok(sending)) = (&rules, &resolved) {
        if let Some(why) = guard::outside_rules(rules, sending) {
            return stopped(envs::redact(&format!("refused by the workspace's policy: {why}"), &secrets));
        }
        if let Some(reason) = guard::needs_approval(rules, request, env, bench) {
            if !approved(hub, actor, request_id, reason) {
                return Ran::Stopped("not allowed by a person".into());
            }
        }
    }
    // A local agent's request mustn't be redirected off the machine.
    let redirects = rules.as_ref().is_none_or(|r| r.access != Access::Local);
    // A browser's ways, if the workspace takes them on: the jar's cookies,
    // and for a page's origin, `Origin` and a preflight where a browser
    // would send one. Added only now, so the journal keeps it as written.
    let browser = hub.browser();
    let mut credentials = false;
    let mut cors_origin: Option<String> = None;
    let resolved = resolved.and_then(|mut sending| {
        if browser.cookies {
            if let Some(jar) = hub.cookie_header(&sending.url) {
                add_cookies(&mut sending, &jar);
                credentials = true;
            }
        }
        let origin = browser.origin.as_deref().filter(|o| !realtime::is_websocket(&sending) && cors::cross_origin(o, &sending.url));
        if let Some(origin) = origin {
            cors_origin = Some(origin.to_string());
            if !sending.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("origin")) {
                sending.headers.push(("Origin".into(), origin.into()));
            }
            if let Some((method, headers)) = cors::needs_preflight(&sending) {
                let ask = Ask { written: request, env, origin, method: &method, headers: &headers, credentials };
                preflight(hub, actor, &ask, &sending, &secrets)?;
            }
        }
        Ok(sending)
    });
    // Secret values that come back are replaced by their names before
    // anything is published or journalled.
    // A WebSocket, or an answer of server-sent events, stays open as a
    // stream until it's closed.
    let link = realtime::Link { hub, actor, request_id, cancelled, secrets: &secrets };
    let read_events = |head, reader, started| realtime::sse(&link, head, reader, started);
    let outcome = resolved
        .and_then(|sending| {
            if realtime::is_websocket(&sending) {
                realtime::websocket(&link, &sending, http::tls_unchecked(&browser, &sending.url))
            } else if browser.cookies || !browser.unchecked_tls.is_empty() {
                send_hop_by_hop(hub, actor, &browser, sending, redirects, cancelled, &read_events)
            } else {
                http::execute(&sending, redirects, false, cancelled, Some(&read_events))
            }
        })
        .map(|response| envs::redact_response(response, &secrets))
        .map_err(|error| envs::redact(&error, &secrets));
    // Checked on what clients see, secrets already taken out.
    let mut results = graphql::checks(request, outcome.as_ref().map_err(String::as_str));
    if let (Some(origin), Ok(response)) = (&cors_origin, &outcome) {
        results.push(cors::answer_check(response, origin, credentials));
    }
    let kind = match &outcome {
        Ok(response) => EventKind::RequestFinished { request_id, response: response.clone() },
        Err(error) => EventKind::RequestFailed { request_id, error: error.clone() },
    };
    // Dropped if the request was cancelled meanwhile.
    if hub.end_checked(actor.clone(), request_id, kind, results.clone()) {
        Ran::Ended(outcome, results)
    } else {
        Ran::Stopped("cancelled".into())
    }
}

/// The most redirects followed one at a time, as a browser's limit is.
const MAX_REDIRECTS: usize = 20;

/// Send `sending` following redirects a hop at a time, as a browser does:
/// with cookies on, each answer's `Set-Cookie` is kept, so a cookie set on
/// the way (a login's 302) is kept and the next hop gets the jar's cookies
/// for its own URL; and each hop's host is trusted without checking its
/// certificate only if it's one of `browser.unchecked_tls`. A 303, or a 301
/// or 302 to a POST, goes on as a GET without the body. The last hop's
/// answer and timing are the answer.
fn send_hop_by_hop(
    hub: &Hub,
    actor: &Actor,
    browser: &Browser,
    mut sending: HttpRequest,
    redirects: bool,
    cancelled: &AtomicBool,
    read_events: http::EventsReader,
) -> Result<HttpResponse, String> {
    for hop in 0..=MAX_REDIRECTS {
        let unchecked = http::tls_unchecked(browser, &sending.url);
        let response = http::execute(&sending, false, unchecked, cancelled, Some(read_events))
            .map_err(|e| if hop == 0 { e } else { format!("redirected to {}: {e}", sending.url) })?;
        if browser.cookies {
            hub.keep_cookies(actor.clone(), &sending.url, &response.headers);
        }
        let location = response.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).map(|(_, v)| v.trim());
        let next = match location {
            Some(to) if redirects && (300..400).contains(&response.status) && response.status != 304 => {
                join(&sending.url, to).ok_or_else(|| format!("a redirect to `{to}`, which isn't a URL"))?
            }
            _ => return Ok(response),
        };
        let method = sending.method.to_ascii_uppercase();
        if response.status == 303 && method != "HEAD" || matches!(response.status, 301 | 302) && method == "POST" {
            sending.method = "GET".into();
            sending.body = None;
            sending.headers.retain(|(n, _)| !n.eq_ignore_ascii_case("content-type") && !n.eq_ignore_ascii_case("content-length"));
        }
        if browser.cookies {
            sending.headers.retain(|(n, _)| !n.eq_ignore_ascii_case("cookie"));
            if let Some(jar) = hub.cookie_header(&next) {
                sending.headers.push(("Cookie".into(), jar));
            }
        }
        sending.url = next;
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// `to`, a redirect's `Location`, as a whole URL against `from`.
fn join(from: &str, to: &str) -> Option<String> {
    if to.contains("://") {
        return Some(to.to_string());
    }
    let uri: ureq::http::Uri = from.parse().ok()?;
    let base = format!("{}://{}", uri.scheme_str()?, uri.authority()?);
    if let Some(rest) = to.strip_prefix("//") {
        return Some(format!("{}://{rest}", uri.scheme_str()?));
    }
    if to.starts_with('/') {
        return Some(format!("{base}{to}"));
    }
    let dir = uri.path().rsplit_once('/').map_or("", |(dir, _)| dir);
    Some(format!("{base}{dir}/{to}"))
}

/// Add the jar's cookies to `sending`, after any it carries itself.
fn add_cookies(sending: &mut HttpRequest, jar: &str) {
    match sending.headers.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case("cookie")) {
        Some((_, value)) => *value = format!("{value}; {jar}"),
        None => sending.headers.push(("Cookie".into(), jar.into())),
    }
}

/// What a preflight asks, for the request `written` (as written, sent with
/// `env`) from a page at `origin`.
struct Ask<'a> {
    written: &'a HttpRequest,
    env: Option<&'a str>,
    origin: &'a str,
    method: &'a str,
    headers: &'a [String],
    credentials: bool,
}

/// Send the `OPTIONS` a browser sends before `sending`, as a request of
/// its own in history, and say why the request may not go if it may not.
fn preflight(hub: &Hub, actor: &Actor, ask: &Ask, sending: &HttpRequest, secrets: &[(String, String)]) -> Result<(), String> {
    let as_written = cors::preflight_of(ask.written, ask.origin, ask.method, ask.headers);
    let (pre_id, cancelled) = hub.start_request(actor.clone(), as_written, ask.env.map(String::from), None, None, None);
    let to_send = cors::preflight_of(sending, ask.origin, ask.method, ask.headers);
    // A browser doesn't follow a preflight's redirect.
    let unchecked = http::tls_unchecked(&hub.browser(), &to_send.url);
    let outcome = http::execute(&to_send, false, unchecked, &cancelled, None)
        .map(|r| envs::redact_response(r, secrets))
        .map_err(|e| envs::redact(&e, secrets));
    let verdict = match &outcome {
        Ok(response) => cors::preflight_allows(response, ask.origin, ask.method, ask.headers, ask.credentials),
        Err(e) => Err(format!("no answer: {e}")),
    };
    let kind = match outcome {
        Ok(response) => EventKind::RequestFinished { request_id: pre_id, response },
        Err(error) => EventKind::RequestFailed { request_id: pre_id, error },
    };
    hub.end_request(actor.clone(), pre_id, kind);
    verdict.map_err(|why| format!("blocked by CORS: preflight #{pre_id}: {why}"))
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

/// Finished bench `bench_id` against a baseline or another finished bench.
fn compare_bench(
    hub: &Hub,
    bench_id: u64,
    baseline: Option<String>,
    other_bench: Option<u64>,
    tolerance_pct: Option<f64>,
) -> Result<ReplyBody, String> {
    let finished = |id: u64| match hub.bench_side(id) {
        Some((side, true)) => Ok(side),
        Some(_) => Err(format!("bench {id} is still running")),
        None => Err(format!("there is no bench {id}")),
    };
    let after = finished(bench_id)?;
    let before = match (baseline, other_bench) {
        (Some(name), None) => {
            let b = hub.baseline(&name)?;
            Side { label: format!("baseline {}", b.name), request: b.request, env: b.env, plan: b.plan, stats: b.stats, stopped: None }
        }
        (None, Some(id)) => finished(id)?,
        _ => return Err("compare with one of `baseline` and `other_bench`".into()),
    };
    let tolerance = tolerance_pct.unwrap_or(BENCH_TOLERANCE_PCT);
    if !(0.0..=1000.0).contains(&tolerance) {
        return Err(format!("a tolerance of {tolerance} % makes no sense; 20 is the default"));
    }
    Ok(ReplyBody::BenchCompared { comparison: baselines::compare(&before, &after, tolerance) })
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
