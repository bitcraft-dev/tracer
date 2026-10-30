//! `tracer`: runs a workspace's core, and talks to it as a client.
//!
//! ```text
//! tracer core                       run the core for this workspace
//! tracer status                     who is connected, how far the journal goes
//! tracer send GET https://…         send a request and print its answer
//! tracer watch [--all | --since N]  print events as they happen
//! tracer cancel <ID>                 cancel a running request, whoever sent it
//! tracer stop                       stop the workspace's core
//! ```
//!
//! Every command takes `-w <dir>` for the workspace (default: here).

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use tracer_protocol::client::Client;
use tracer_protocol::{ClientKind, Command, Event, EventKind, HttpRequest, ReplyBody};

const USAGE: &str = "\
usage:
  tracer core                         run the core for the workspace
  tracer status                       who is connected, how far the journal goes
  tracer send <METHOD> <URL> [-H 'Name: value']... [-d <body>]
                                      send a request and print its answer
  tracer watch [--all | --since <N>]  print events as they happen
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
        Some("watch") => watch(workspace, args[1..].to_vec()),
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
    let mut headers = Vec::new();
    while let Some(h) = take_value(&mut args, "-H") {
        let Some((name, value)) = h.split_once(':') else {
            return Err(io::Error::other(format!("a header is `Name: value`, not `{h}`")));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let body = take_value(&mut args, "-d");
    let [method, url] = <[String; 2]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let request = HttpRequest { method: method.to_uppercase(), url, headers, body };

    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer send")?;
    // Subscribed before sending, so the answer cannot go past unseen.
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let request_id = match client.call(Command::Send { request })? {
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
        EventKind::RequestStarted { request_id, request } => {
            format!("#{request_id} {} {}", request.method, request.url)
        }
        EventKind::RequestFinished { request_id, response } => format!(
            "#{request_id} → {} in {:.0} ms ({} B)",
            response.status, response.timing.total_ms, response.size
        ),
        EventKind::RequestFailed { request_id, error } => format!("#{request_id} failed: {error}"),
        EventKind::RequestCancelled { request_id } => format!("#{request_id} cancelled"),
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
