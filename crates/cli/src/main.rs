//! `tracer`: runs a workspace's core, and talks to it as a client.
//!
//! ```text
//! tracer core                       run the core for this workspace
//! tracer status                     who is connected, how far the journal goes
//! tracer send GET https://…         send a request and print its answer
//! tracer ls                         list the saved requests
//! tracer save <PATH> GET https://…  save a request as requests/<PATH>.http
//! tracer run <PATH>                 send a saved request
//! tracer env                        list the environments
//! tracer secret set <ENV> <NAME>    set a secret, its value read from stdin
//! tracer secret rm <ENV> <NAME>     remove a secret
//! tracer rm <PATH>                  delete a saved request
//! tracer watch [--all | --since N]  print events as they happen
//! tracer history [TEXT]             past requests, newest first, filtered
//! tracer rerun <ID>                 send a past request again, as written
//! tracer cancel <ID>                cancel a running request, whoever sent it
//! tracer stop                       stop the workspace's core
//! ```
//!
//! Every command takes `-w <dir>` for the workspace (default: here).

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use tracer_protocol::client::Client;
use tracer_protocol::{
    ClientKind, Command, Ended, Event, EventKind, HistoryFilter, HttpRequest, ReplyBody, SavedRequest, StatusClass,
};

/// `{{name}}` values are shown cut to this, so a long token doesn't fill the
/// terminal.
const VALUE_SHOWN: usize = 48;

const USAGE: &str = "\
usage:
  tracer core                         run the core for the workspace
  tracer status                       who is connected, how far the journal goes
  tracer send <METHOD> <URL> [-H 'Name: value']... [-d <body>] [--env <NAME>]
                                      send a request and print its answer
  tracer ls                           list the saved requests
  tracer save <PATH> <METHOD> <URL> [-H 'Name: value']... [-d <body>]
                                      save a request as requests/<PATH>.http
  tracer run <PATH> [--env <NAME>]    send a saved request and print its answer
  tracer env                          list the environments and their variables
  tracer secret set <ENV> <NAME>      set a secret; its value is read from stdin,
                                      so it stays out of shell history
  tracer secret rm <ENV> <NAME>       remove a secret
  tracer rm <PATH>                    delete a saved request
  tracer watch [--all | --since <N>]  print events as they happen
  tracer history [TEXT] [--actor <window|cli|agent>] [--status <2xx|3xx|4xx|5xx|failed|cancelled|running>] [-n <N>]
                                      past requests, newest first; TEXT is looked
                                      for in the method, URL, env, sender, status, #id
  tracer rerun <ID> [--env <NAME> | --no-env]
                                      send a past request again as it was written,
                                      with its environment unless told otherwise
  tracer cancel <ID>                  cancel a running request, whoever sent it
  tracer stop                         stop the workspace's core

options:
  -w <dir>                            the workspace (default: the current directory)";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let workspace = match take_value(&mut args, "-w") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let result = match args.first().map(String::as_str) {
        Some("core") => core(workspace),
        Some("status") => status(workspace),
        Some("send") => send(workspace, args[1..].to_vec()),
        Some("ls") => ls(workspace),
        Some("save") => save(workspace, args[1..].to_vec()),
        Some("run") => run(workspace, args[1..].to_vec()),
        Some("rm") => rm(workspace, args[1..].to_vec()),
        Some("env") => env_list(workspace),
        Some("secret") => secret(workspace, args[1..].to_vec()),
        Some("watch") => watch(workspace, args[1..].to_vec()),
        Some("history") => history(workspace, args[1..].to_vec()),
        Some("rerun") => rerun(workspace, args[1..].to_vec()),
        Some("cancel") => cancel(workspace, args[1..].to_vec()),
        Some("stop") => stop(workspace),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tracer: {e}");
            ExitCode::FAILURE
        }
    }
}

fn core(workspace: PathBuf) -> io::Result<ExitCode> {
    if let Ok(mut existing) = Client::connect(&workspace, ClientKind::Cli, "tracer core") {
        let _ = existing.call(Command::Status);
        eprintln!("tracer: a core is already running for {}", workspace.display());
        return Ok(ExitCode::FAILURE);
    }
    let running = tracer_core::start(&workspace)?;
    println!("tracer core: {} on 127.0.0.1:{}", workspace.display(), running.port);
    running.wait();
    Ok(ExitCode::SUCCESS)
}

fn status(workspace: PathBuf) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer status")?;
    match client.call(Command::Status)? {
        Ok(ReplyBody::Status { clients, next_seq, workspace }) => {
            println!("workspace  {workspace}");
            println!("events     {}", next_seq - 1);
            println!("clients    {}", clients.len());
            for actor in clients {
                println!("  {:?} {}", actor.kind, actor.name);
            }
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn stop(workspace: PathBuf) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer stop")?;
    match client.call(Command::Shutdown)? {
        Ok(ReplyBody::Stopping) => {
            println!("tracer: the core for {} is stopping", workspace.display());
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn cancel(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let [id] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let request_id = id.trim_start_matches('#').parse().map_err(|_| io::Error::other(format!("`{id}` is not a request id")))?;
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer cancel")?;
    match client.call(Command::Cancel { request_id })? {
        Ok(ReplyBody::Cancelled) => {
            println!("tracer: cancelled #{request_id}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn send(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let env = take_value(&mut args, "--env");
    let request = request_from(args)?;
    send_and_wait(&workspace, Command::Send { request, env }, "tracer send")
}

fn history(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let actor = match take_value(&mut args, "--actor").as_deref() {
        None => None,
        Some("window") => Some(ClientKind::Window),
        Some("cli") => Some(ClientKind::Cli),
        Some("agent") => Some(ClientKind::Agent),
        Some(other) => return Err(io::Error::other(format!("--actor is window, cli or agent, not `{other}`"))),
    };
    let status = take_value(&mut args, "--status")
        .map(|s| StatusClass::parse(&s).ok_or_else(|| io::Error::other(format!("--status is {}, not `{s}`", StatusClass::NAMES))))
        .transpose()?;
    let limit = take_value(&mut args, "-n")
        .map(|n| n.parse::<usize>().map_err(|_| io::Error::other(format!("-n takes a number, not `{n}`"))))
        .transpose()?;
    if let Some(flag) = args.iter().find(|a| a.starts_with('-')) {
        return Err(io::Error::other(format!("unknown option `{flag}`\n{USAGE}")));
    }
    let text = if args.is_empty() { None } else { Some(args.join(" ")) };
    let filter = HistoryFilter { text, actor, status, limit };

    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer history")?;
    let ReplyBody::History { entries, matched, .. } = client.call(Command::History { filter })?.map_err(io::Error::other)? else {
        return Err(io::Error::other("unexpected answer"));
    };
    for e in &entries {
        let outcome = match &e.ended {
            Ended::Running => "…".to_string(),
            Ended::Done { status, total_ms, .. } => format!("{status} {total_ms:>5.0}ms"),
            Ended::Failed { .. } => "failed".to_string(),
            Ended::Cancelled { .. } => "cancelled".to_string(),
        };
        let who = format!("{:?}:{}", e.actor.kind, e.actor.name).to_lowercase();
        let env = e.env.as_ref().map(|n| format!(" [{n}]")).unwrap_or_default();
        let again = e.rerun_of.map(|id| format!(" (re-run of #{id})")).unwrap_or_default();
        println!(
            "{:>6}  {outcome:<11}  {who:<20}  {:<7} {}{env}{again}",
            format!("#{}", e.request_id),
            e.request.method,
            e.request.url
        );
    }
    if entries.len() < matched as usize {
        eprintln!("tracer: showing the newest {} of {matched}; -n shows more", entries.len());
    } else if matched == 0 {
        eprintln!("tracer: no request in the history matches");
    }
    Ok(ExitCode::SUCCESS)
}

fn rerun(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let no_env = match args.iter().position(|a| a == "--no-env") {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    };
    let env = take_value(&mut args, "--env");
    if no_env && env.is_some() {
        return Err(io::Error::other("--env and --no-env together"));
    }
    let env = if no_env { Some(String::new()) } else { env };
    let [id] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let request_id = id.trim_start_matches('#').parse().map_err(|_| io::Error::other(format!("`{id}` is not a request id")))?;
    send_and_wait(&workspace, Command::Rerun { request_id, env }, "tracer rerun")
}

/// `<METHOD> <URL> [-H 'Name: value']... [-d <body>]`.
fn request_from(mut args: Vec<String>) -> io::Result<HttpRequest> {
    let mut headers = Vec::new();
    while let Some(h) = take_value(&mut args, "-H") {
        let Some((name, value)) = h.split_once(':') else {
            return Err(io::Error::other(format!("a header is `Name: value`, not `{h}`")));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let body = take_value(&mut args, "-d");
    let [method, url] = <[String; 2]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    Ok(HttpRequest { method: method.to_uppercase(), url, headers, body })
}

fn ls(workspace: PathBuf) -> io::Result<ExitCode> {
    let (requests, skipped) = saved(&workspace, "tracer ls")?;
    for r in &requests {
        println!("{:<7} {:<32} {}", r.request.method, r.path, r.request.url);
    }
    for why in &skipped {
        eprintln!("tracer: skipped {why}");
    }
    if requests.is_empty() && skipped.is_empty() {
        eprintln!("tracer: no saved requests; `tracer save <PATH> GET <URL>` makes one");
    }
    Ok(ExitCode::SUCCESS)
}

fn save(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    if args.is_empty() {
        return Err(io::Error::other(USAGE));
    }
    let path = args.remove(0);
    let request = request_from(args)?;
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer save")?;
    match client.call(Command::SaveRequest { path, request })? {
        Ok(ReplyBody::Saved { path }) => {
            println!("tracer: saved requests/{path}.http");
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn run(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let env = take_value(&mut args, "--env");
    let [path] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    // Matched as the core names paths, so `requests/a/b.http` finds `a/b`.
    let wanted = path.trim().replace('\\', "/");
    let wanted = wanted.strip_prefix("requests/").unwrap_or(&wanted);
    let wanted = wanted.strip_suffix(".http").unwrap_or(wanted);
    let (requests, _) = saved(&workspace, "tracer run")?;
    let Some(found) = requests.into_iter().find(|r| r.path == wanted) else {
        return Err(io::Error::other(format!("there is no saved request `{wanted}`; `tracer ls` lists them")));
    };
    send_and_wait(&workspace, Command::Send { request: found.request, env }, "tracer run")
}

fn env_list(workspace: PathBuf) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer env")?;
    let ReplyBody::Environments { environments, skipped, .. } = client.call(Command::ListEnvironments)?.map_err(io::Error::other)? else {
        return Err(io::Error::other("unexpected answer"));
    };
    for env in &environments {
        println!("{}", env.name);
        for (key, value) in &env.vars {
            if env.secrets.contains(key) {
                continue;
            }
            let shown: String = value.chars().take(VALUE_SHOWN).collect();
            let more = if shown.len() < value.len() { "…" } else { "" };
            println!("  {key:<20} {shown}{more}");
        }
        for key in &env.secrets {
            println!("  {key:<20} •••• (secret)");
        }
    }
    for why in &skipped {
        eprintln!("tracer: skipped {why}");
    }
    if environments.is_empty() && skipped.is_empty() {
        eprintln!("tracer: no environments; write environments/<name>.env with KEY=value lines");
    }
    Ok(ExitCode::SUCCESS)
}

fn secret(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let (action, env, name) = match args.as_slice() {
        [a, e, n] if a == "set" || a == "rm" => (a.as_str(), e.clone(), n.clone()),
        _ => return Err(io::Error::other(USAGE)),
    };
    let value = if action == "set" {
        let mut line = String::new();
        if std::io::IsTerminal::is_terminal(&io::stdin()) {
            eprint!("value for {name} (not echoed to history; Enter to finish): ");
        }
        io::stdin().read_line(&mut line)?;
        let line = line.trim().to_string();
        if line.is_empty() {
            return Err(io::Error::other("an empty secret; `tracer secret rm` removes one"));
        }
        Some(line)
    } else {
        None
    };
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer secret")?;
    client.call(Command::SetSecret { env: env.clone(), name: name.clone(), value })?.map_err(io::Error::other)?;
    let done = if action == "set" { "set" } else { "removed" };
    println!("tracer: {done} secret {name} of {env}");
    Ok(ExitCode::SUCCESS)
}

fn rm(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let [path] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer rm")?;
    match client.call(Command::DeleteRequest { path: path.clone() })? {
        Ok(ReplyBody::Deleted) => {
            println!("tracer: deleted {path}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn saved(workspace: &PathBuf, name: &str) -> io::Result<(Vec<SavedRequest>, Vec<String>)> {
    let mut client = Client::connect(workspace, ClientKind::Cli, name)?;
    match client.call(Command::ListRequests)? {
        Ok(ReplyBody::Requests { requests, skipped, .. }) => Ok((requests, skipped)),
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

/// Send a request (`Send` or `Rerun`) and print its answer when it comes.
fn send_and_wait(workspace: &PathBuf, sending: Command, name: &str) -> io::Result<ExitCode> {
    let mut client = Client::connect(workspace, ClientKind::Cli, name)?;
    // Subscribed before sending, so the answer cannot go past unseen.
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let request_id = match client.call(sending)? {
        Ok(ReplyBody::Accepted { request_id }) => request_id,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    eprintln!("#{request_id} sent; `tracer cancel {request_id}` stops it");
    loop {
        let event = client.next_event()?;
        if let EventKind::RequestCancelled { request_id: id } = event.kind {
            if id == request_id {
                return Err(io::Error::other(format!("cancelled by {}", event.actor.name)));
            }
            continue;
        }

        match event.kind {
            EventKind::RequestFinished { request_id: id, response } if id == request_id => {
                eprintln!(
                    "{} · TTFB {:.0} ms · total {:.0} ms · {} B",
                    response.status, response.timing.ttfb_ms, response.timing.total_ms, response.size
                );
                println!("{}", response.body);
                return Ok(if response.status < 400 { ExitCode::SUCCESS } else { ExitCode::FAILURE });
            }
            EventKind::RequestFailed { request_id: id, error } if id == request_id => {
                return Err(io::Error::other(error));
            }
            _ => {}
        }
    }
}

fn watch(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let since = if args.iter().any(|a| a == "--all") {
        Some(0)
    } else {
        take_value(&mut args, "--since")
            .map(|n| n.parse::<u64>().map_err(|_| io::Error::other(format!("--since takes a number, not `{n}`"))))
            .transpose()?
    };
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer watch")?;
    client.call(Command::Subscribe { since })?.map_err(io::Error::other)?;
    loop {
        println!("{}", describe(&client.next_event()?));
    }
}

/// One line per event, for a person reading a terminal.
fn describe(event: &Event) -> String {
    let who = format!("{:?}:{}", event.actor.kind, event.actor.name).to_lowercase();
    let what = match &event.kind {
        EventKind::ClientJoined { client } => format!("joined as client {client}"),
        EventKind::ClientLeft { client } => format!("left (client {client})"),
        EventKind::RequestStarted { request_id, request, env, rerun_of } => {
            let with = env.as_ref().map(|e| format!(" (env {e})")).unwrap_or_default();
            let again = rerun_of.map(|id| format!(", re-run of #{id}")).unwrap_or_default();
            format!("#{request_id} {} {}{with}{again}", request.method, request.url)
        }
        EventKind::RequestFinished { request_id, response } => format!(
            "#{request_id} → {} in {:.0} ms ({} B)",
            response.status, response.timing.total_ms, response.size
        ),
        EventKind::RequestFailed { request_id, error } => format!("#{request_id} failed: {error}"),
        EventKind::RequestCancelled { request_id } => format!("#{request_id} cancelled"),
        EventKind::RequestSaved { path, request } => format!("saved {path} ({} {})", request.method, request.url),
        EventKind::RequestDeleted { path } => format!("deleted {path}"),
        EventKind::EnvironmentSaved { env } => format!("saved environment {} ({} variables)", env.name, env.vars.len()),
        EventKind::EnvironmentDeleted { name } => format!("deleted environment {name}"),
        EventKind::SecretSet { env, name, present } => {
            format!("{} secret {name} of {env}", if *present { "set" } else { "removed" })
        }
        EventKind::CoreStopping => "stopped the core".to_string(),
    };
    format!("{:>6}  {who:<24}  {what}", event.seq)
}

/// Remove `flag <value>` from `args` and give the value.
fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    if at + 1 >= args.len() {
        return None;
    }
    let value = args.remove(at + 1);
    args.remove(at);
    Some(value)
}
