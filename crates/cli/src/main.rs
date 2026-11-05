//! `tracer`: runs a workspace's core, and talks to it as a client.
//!
//! ```text
//! tracer core                       run the core for this workspace
//! tracer status                     who is connected, how far the journal goes
//! tracer send GET https://…         send a request and print its answer
//! tracer ws wss://…                 open a WebSocket: type to send, EOF closes
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
//! tracer diff <A> <B>               how two responses differ, noise aside
//! tracer cancel <ID>                cancel a running request, whoever sent it
//! tracer approve <ID> / deny <ID>   answer an agent's request waiting for a person
//! tracer agents on|off              the kill switch for agents
//! tracer policy                     what agents may do, from agents.policy
//! tracer mcp                        an MCP server on stdio, for AI agents
//! tracer stop                       stop the workspace's core
//! ```
//!
//! Every command takes `-w <dir>` for the workspace (default: here).

mod mcp;

use std::collections::HashMap;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use tracer_protocol::client::Client;
use tracer_protocol::{
    Access, AgentRules, ApproveWhen, ExportLang, BodyDiff, Change, ChangeKind, ClientKind, LineOp, Command, Ended, Event, EventKind, HistoryFilter, HttpRequest, ReplyBody,
    BenchComparison, BenchPlan, BenchStats, Direction, Discovery, FlowStep, BENCH_BUCKETS_MS, GRAPHQL_INTROSPECTION, SavedRequest, StatusClass, StepOutcome, StreamKind,
    Timing, UntilPlan, DISCOVERY_PATH, WEBSOCKET,
};

/// `{{name}}` values are shown cut to this, so a long token doesn't fill the
/// terminal.
const VALUE_SHOWN: usize = 48;

const USAGE: &str = "\
usage:
  tracer core                         run the core for the workspace
  tracer status                       who is connected, how far the journal goes
  tracer send <METHOD> <URL> [-H 'Name: value']... [-d <body>] [--env <NAME>]
             [--expect '<EXPECTATION>']...
                                      send a request and print its answer; with
                                      --expect, check it (status 2xx, $.id == 1,
                                      header Content-Type contains json, time < 500)
                                      and fail if a check does
  tracer ws <URL> [-H 'Name: value']... [-m <MESSAGE>]... [--env <NAME>]
                                      open a WebSocket (ws:// or wss://), send each
                                      -m once open, then each line typed; what comes
                                      in goes to stdout; end of input closes it,
                                      unless --for or --count says when
  (send, run, ws) [--for <DURATION>] [--count <N>]
                                      a stream (a WebSocket, or an answer of
                                      text/event-stream) prints its messages as they
                                      come and stays open until closed: by
                                      `tracer cancel <ID>`, after DURATION (10s, 2m),
                                      or after N messages have come in
  tracer ls                           list the saved requests
  tracer save <PATH> <METHOD> <URL> [-H 'Name: value']... [-d <body>] [--expect '…']...
                                      save a request as requests/<PATH>.http, its
                                      expectations as `# @expect` lines
  tracer run <PATH> [--env <NAME>] [--keep-going] [--report json] [--data <FILE>] [--watch]
                                      send a saved request and print its answer;
                                      a folder runs as a flow, in path order, each
                                      step's `# @capture name = $.path` filling
                                      {{name}} after it; stops at a failed step
                                      unless --keep-going; exits 1 if it fails.
                                      --data: a CSV (header line) or JSON array
                                      of objects; the flow runs once per row,
                                      its columns filling {{name}}
                                      --watch: run it again whenever its file
                                      (or one under the folder) or its env
                                      changes, until Ctrl+C
  tracer bench <PATH> [-n <N>] [-c <C>] [-t <DURATION>] [--rate <R>] [--env <NAME>]
             [--report json]          send a saved request N times (default 100),
                                      C at a time (default 10), or for DURATION
                                      (30s, 2m), no faster than R a second; live
                                      p50/p95/p99, a histogram, statuses, errors;
                                      exits 1 if any went wrong
  tracer bench <PATH> --env <A> --env <B> ...
                                      the same bench against each environment in
                                      turn, then side by side
  tracer bench <PATH> --save-baseline <NAME>
                                      keep the bench as baselines/<NAME>.json
  tracer bench <PATH> --baseline <NAME> [--tolerance <PCT>]
                                      compare with a baseline: each latency, the
                                      throughput, errors, rate limiting; exits 1
                                      if any is worse past PCT (default 20)
  tracer bench compare <ID> <NAME | #ID> [--tolerance <PCT>]
                                      compare a past bench with a baseline or
                                      another bench
  tracer bench baselines              list the baselines
  tracer bench stop <ID>              stop a running bench, whoever started it
  tracer until <PATH> [-n <STREAK>] [--match '<EXPECTATION>'] [--max <N>]
             [--every <DURATION>] [--env <NAME>]
                                      send a saved request again and again, one at
                                      a time, until STREAK answers in a row (default
                                      3) fail, or pass EXPECTATION (status == 401);
                                      any other answer starts the count again; at
                                      most N (default 1000), DURATION apart (500ms,
                                      2s); a line an answer; exits 0 when the streak
                                      came, 1 when it ended without it
  tracer until stop <ID>              stop a running run-until, whoever started it
  tracer browser [cookies on|off] [origin <URL>|none] [tls <HOSTS>|none]
                                      how requests behave like a browser's (off
                                      unless set): keep cookies and send them back;
                                      as a page at URL, send Origin, preflight what
                                      a browser would, and check the answer's CORS;
                                      HOSTS (localhost,*.dev.test or *) have their
                                      TLS certificates accepted unchecked
  tracer cookies [clear [DOMAIN]]     the cookie jar, or empty it (a domain's part)
  tracer schema <URL|PATH> [-H 'Name: value']... [--env <NAME>]
                                      a GraphQL server's schema as SDL, asked by
                                      introspection (a request in history);
                                      `tracer send GRAPHQL <URL> -d '<query>'`
                                      sends a query, its variables JSON after a
                                      blank line
  tracer export <PATH|#ID> [--as curl|fetch|python|rux] [--env <NAME>]
                                      a saved or past request as code that sends
                                      it (curl by default), variables filled from
                                      the env (a past one's own unless given);
                                      secrets stay {{name}}
  tracer import [FILE] [--save <PATH>]
                                      a request from a curl command or a fetch(…)
                                      call (FILE, or stdin), printed as a .http
                                      file, or saved at PATH
  tracer explain [ID]                 a past request (the newest that went wrong
                                      if no ID) as Markdown to paste to a person
                                      or an AI: request, env (secrets named only),
                                      answer, timing, checks, settings, and the
                                      last time it worked
  tracer servers [PORT]...            the servers answering HTTP(S) on this
                                      machine: its listening ports (or the usual
                                      dev ports) and PORTs, with what answers /,
                                      the process, and any OpenAPI document
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
  tracer diff <A> <B> [--ignore <PATTERN>]... [--all]
                                      how two past responses differ: status,
                                      headers, body (JSON by path, else by line);
                                      noisy headers and keys (Date, timestamp, …)
                                      are ignored and listed unless --all.
                                      PATTERN: $.path[*].key, a bare key, header:name*
  tracer cancel <ID>                  cancel a running request, whoever sent it
  tracer approve <ID>                 allow an agent's request that waits for a person
  tracer deny <ID>                    refuse it
  tracer agents on|off                let agents act, or stop them (their running
                                      requests are cancelled); `tracer status` shows it
  tracer policy                       what agents may do: the rules agents.policy
                                      gives every agent, and agents by name
  tracer mcp [--name <AGENT>]         an MCP server on stdio: an AI agent drives
                                      Tracer through it, as AGENT or the name its
                                      client gives; starts a core if none runs
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
        Some("ws") => ws(workspace, args[1..].to_vec()),
        Some("ls") => ls(workspace),
        Some("save") => save(workspace, args[1..].to_vec()),
        Some("run") => run(workspace, args[1..].to_vec()),
        Some("bench") => bench(workspace, args[1..].to_vec()),
        Some("until") => until(workspace, args[1..].to_vec()),
        Some("browser") => browser(workspace, args[1..].to_vec()),
        Some("cookies") => cookies(workspace, args[1..].to_vec()),
        Some("schema") => schema(workspace, args[1..].to_vec()),
        Some("rm") => rm(workspace, args[1..].to_vec()),
        Some("export") => export(workspace, args[1..].to_vec()),
        Some("import") => import(workspace, args[1..].to_vec()),
        Some("explain") => explain(workspace, args[1..].to_vec()),
        Some("servers") => servers(workspace, args[1..].to_vec()),
        Some("env") => env_list(workspace),
        Some("secret") => secret(workspace, args[1..].to_vec()),
        Some("watch") => watch(workspace, args[1..].to_vec()),
        Some("history") => history(workspace, args[1..].to_vec()),
        Some("rerun") => rerun(workspace, args[1..].to_vec()),
        Some("diff") => diff(workspace, args[1..].to_vec()),
        Some("cancel") => cancel(workspace, args[1..].to_vec()),
        Some("approve") => decide(workspace, args[1..].to_vec(), true),
        Some("deny") => decide(workspace, args[1..].to_vec(), false),
        Some("agents") => agents(workspace, args[1..].to_vec()),
        Some("policy") => policy(workspace),
        Some("mcp") => {
            let name = take_value(&mut args, "--name");
            mcp::serve(workspace, name).map(|_| ExitCode::SUCCESS)
        }
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
        Ok(ReplyBody::Status { clients, next_seq, workspace, agents_allowed }) => {
            println!("workspace  {workspace}");
            println!("events     {}", next_seq - 1);
            println!("agents     {}", if agents_allowed { "on" } else { "off (tracer agents on)" });
            // The local API is on the core's own port, behind the same token.
            let found = std::fs::read_to_string(std::path::Path::new(&workspace).join(DISCOVERY_PATH)).ok();
            if let Some(d) = found.and_then(|text| serde_json::from_str::<Discovery>(&text).ok()) {
                println!("api        POST http://127.0.0.1:{}/v1/command, ws://127.0.0.1:{}/v1/ws", d.port, d.port);
            }
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

fn decide(workspace: PathBuf, args: Vec<String>, allow: bool) -> io::Result<ExitCode> {
    let [id] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let request_id = id.trim_start_matches('#').parse().map_err(|_| io::Error::other(format!("`{id}` is not a request id")))?;
    let mut client = Client::connect(&workspace, ClientKind::Cli, &person())?;
    client.call(Command::Decide { request_id, allow })?.map_err(io::Error::other)?;
    println!("tracer: {} #{request_id}", if allow { "allowed" } else { "refused" });
    Ok(ExitCode::SUCCESS)
}

fn agents(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let allowed = match args.as_slice() {
        [on] if on == "on" => true,
        [off] if off == "off" => false,
        _ => return Err(io::Error::other(USAGE)),
    };
    let mut client = Client::connect(&workspace, ClientKind::Cli, &person())?;
    client.call(Command::SetAgents { allowed })?.map_err(io::Error::other)?;
    let what = if allowed { "agents may act again" } else { "agents are stopped; their running requests were cancelled" };
    println!("tracer: {what}");
    Ok(ExitCode::SUCCESS)
}

fn policy(workspace: PathBuf) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer policy")?;
    let Ok(ReplyBody::Policy { rules, error }) = client.call(Command::Policy)? else {
        return Err(io::Error::other("unexpected answer to policy"));
    };
    if let Some(error) = &error {
        println!("not taken, so the rules below stand: {error}");
    }
    for r in &rules {
        println!("{}", describe_rules(r));
    }
    Ok(if error.is_some() { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// `[claude-code] local · approve writes, prod · 60 per 60 s · bodies up to 1048576 B`
fn describe_rules(r: &AgentRules) -> String {
    let access = match r.access {
        Access::ReadOnly => "read-only",
        Access::Local => "local",
        Access::Anywhere => "anywhere",
    };
    let approve = match r.approve.as_slice() {
        [] => "approve none".to_string(),
        whens => {
            let names: Vec<&str> = whens
                .iter()
                .map(|w| match w {
                    ApproveWhen::Writes => "writes",
                    ApproveWhen::Prod => "prod",
                    ApproveWhen::All => "all",
                    ApproveWhen::Bench => "bench",
                })
                .collect();
            format!("approve {}", names.join(", "))
        }
    };
    let mut parts = vec![access.to_string(), approve];
    if let Some(rate) = r.rate {
        parts.push(format!("{} per {} s", rate.count, rate.per_secs));
    }
    if let Some(max) = r.max_body {
        parts.push(format!("bodies up to {max} B"));
    }
    if let Some(max) = r.max_bench {
        parts.push(format!("benches up to {max} requests"));
    }
    if let Some(max) = r.max_concurrency {
        parts.push(format!("{max} at a time"));
    }
    format!("[{}] {}", r.agent, parts.join(" · "))
}

/// Who is at the terminal, for decisions that name the person who made them
/// ("refused by …"), as the window names its user.
fn person() -> String {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "tracer".into())
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
    let until = Until::take(&mut args, false)?;
    let request = request_from(args)?;
    send_and_wait(&workspace, Command::Send { request, env }, "tracer send", until)
}

fn ws(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let env = take_value(&mut args, "--env");
    let until = Until::take(&mut args, true)?;
    let mut messages = Vec::new();
    while let Some(m) = take_value(&mut args, "-m") {
        messages.push(m);
    }
    let Some(url) = args.pop() else { return Err(io::Error::other(USAGE)) };
    args.extend([WEBSOCKET.to_string(), url]);
    let mut request = request_from(args)?;
    if !messages.is_empty() {
        request.body = Some(messages.iter().map(|m| format!("===\n{m}\n")).collect());
    }
    send_and_wait(&workspace, Command::Send { request, env }, "tracer ws", until)
}

/// When a stream the CLI opened is to be closed, besides by `tracer
/// cancel`: after a time, after a number of messages in, or (for `tracer
/// ws`) at the end of what's typed, each line typed being sent.
#[derive(Clone, Copy, Default)]
struct Until {
    secs: Option<u64>,
    count: Option<u64>,
    stdin: bool,
}

impl Until {
    fn take(args: &mut Vec<String>, stdin: bool) -> io::Result<Until> {
        let secs = take_value(args, "--for").map(|t| seconds(&t)).transpose()?;
        let count = take_value(args, "--count")
            .map(|n| n.parse().map_err(|_| io::Error::other(format!("--count takes a number, not `{n}`"))))
            .transpose()?;
        Ok(Until { secs, count, stdin })
    }

    /// Once stream `request_id` is open: close it after the time, and send
    /// what's typed. Each on a connection of its own, so the main one keeps
    /// reading events.
    fn start(self, workspace: &PathBuf, request_id: u64, websocket: bool) {
        if let Some(secs) = self.secs {
            let workspace = workspace.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                close(&workspace, request_id);
            });
        }
        if self.stdin && websocket {
            let workspace = workspace.clone();
            std::thread::spawn(move || {
                let Ok(mut c) = Client::connect(&workspace, ClientKind::Cli, "tracer ws") else { return };
                for line in io::stdin().lines() {
                    let Ok(text) = line else { break };
                    if let Ok(Err(e)) = c.call(Command::StreamSend { request_id, text }) {
                        eprintln!("tracer: {e}");
                        break;
                    }
                }
                // Unless told when to close it, which piped input needs.
                if self.secs.is_none() && self.count.is_none() {
                    let _ = c.call(Command::Cancel { request_id });
                }
            });
        }
    }
}

/// Close stream `request_id`, on a connection of its own.
fn close(workspace: &PathBuf, request_id: u64) {
    if let Ok(mut c) = Client::connect(workspace, ClientKind::Cli, "tracer") {
        let _ = c.call(Command::Cancel { request_id });
    }
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
            Ended::AwaitingApproval { .. } => "approval?".to_string(),
            Ended::Done { status, total_ms, .. } => format!("{status} {total_ms:>5.0}ms"),
            Ended::Failed { .. } => "failed".to_string(),
            Ended::Cancelled { .. } => "cancelled".to_string(),
        };
        let who = format!("{:?}:{}", e.actor.kind, e.actor.name).to_lowercase();
        let env = e.env.as_ref().map(|n| format!(" [{n}]")).unwrap_or_default();
        let again = e.rerun_of.map(|id| format!(" (re-run of #{id})")).unwrap_or_default();
        let checks = match e.checks {
            Some((passed, 0)) => format!("✓{passed}"),
            Some((passed, failed)) => format!("✗{failed}/{}", passed + failed),
            None => String::new(),
        };
        println!(
            "{:>6}  {outcome:<11} {checks:<5}  {who:<20}  {:<7} {}{env}{again}",
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
    send_and_wait(&workspace, Command::Rerun { request_id, env }, "tracer rerun", Until::default())
}

fn diff(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let mut ignore = Vec::new();
    while let Some(p) = take_value(&mut args, "--ignore") {
        ignore.push(p);
    }
    let no_defaults = match args.iter().position(|a| a == "--all") {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    };
    let [a, b] = <[String; 2]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let id = |s: &str| s.trim_start_matches('#').parse::<u64>().map_err(|_| io::Error::other(format!("`{s}` is not a request id")));
    let (a, b) = (id(&a)?, id(&b)?);
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer diff")?;
    let d = match client.call(Command::Diff { a, b, ignore, no_defaults })? {
        Ok(ReplyBody::Diff { diff }) => diff,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    for s in [&d.a, &d.b] {
        println!("#{:<5} {} {} → {}", s.request_id, s.method, s.url, s.status);
    }
    let mut same = true;
    if d.a.status != d.b.status {
        same = false;
        println!("status   {} → {}", d.a.status, d.b.status);
    }
    for c in &d.headers {
        same = false;
        println!("header   {}", change_line(c));
    }
    match &d.body {
        BodyDiff::Same => {}
        BodyDiff::Json { changes, more } => {
            same = false;
            println!("body     JSON, {} change{}", changes.len() as u64 + more, if changes.len() as u64 + more == 1 { "" } else { "s" });
            for c in changes {
                println!("  {}", change_line(c));
            }
            if *more > 0 {
                println!("  … and {more} more");
            }
        }
        BodyDiff::Text { lines, more } => {
            same = false;
            println!("body     text, by line");
            for l in lines {
                match l.op {
                    LineOp::Same => println!("  {:>5} {:>5}   {}", num(l.a), num(l.b), l.text),
                    LineOp::Removed => println!("  {:>5} {:>5} - {}", num(l.a), "", l.text),
                    LineOp::Added => println!("  {:>5} {:>5} + {}", "", num(l.b), l.text),
                    LineOp::Gap => println!("  {:>5} {:>5}   …", "", ""),
                }
            }
            if *more > 0 {
                println!("  … and {more} more lines");
            }
        }
        BodyDiff::TooLarge { a_lines, b_lines } => {
            same = false;
            println!("body     differs; {a_lines} and {b_lines} lines are too many to compare by line");
        }
    }
    if same {
        println!("the same{}", if d.ignored.is_empty() { "" } else { ", noise aside" });
    }
    if !d.ignored.is_empty() {
        println!("ignored  {}", d.ignored.join(", "));
    }
    Ok(if same { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn num(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

/// `~ $.a: 1 → 2`, `+ $.b: true`, `- $.c: null`.
fn change_line(c: &Change) -> String {
    match c.kind {
        ChangeKind::Changed => format!(
            "~ {}: {} → {}",
            c.at,
            c.before.as_deref().unwrap_or_default(),
            c.after.as_deref().unwrap_or_default()
        ),
        ChangeKind::Added => format!("+ {}: {}", c.at, c.after.as_deref().unwrap_or_default()),
        ChangeKind::Removed => format!("- {}: {}", c.at, c.before.as_deref().unwrap_or_default()),
    }
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
    let mut expect = Vec::new();
    while let Some(e) = take_value(&mut args, "--expect") {
        expect.push(e);
    }
    let [method, url] = <[String; 2]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    Ok(HttpRequest { method: method.to_uppercase(), url, headers, body, expect, ..Default::default() })
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
    let until = Until::take(&mut args, false)?;
    let report = take_value(&mut args, "--report");
    let data = take_value(&mut args, "--data");
    if report.as_deref().is_some_and(|r| r != "json") {
        return Err(io::Error::other("--report takes json"));
    }
    let mut flag = |name: &str| match args.iter().position(|a| a == name) {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    };
    let keep_going = flag("--keep-going");
    let watch = flag("--watch");
    let [path] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    // Matched as the core names paths, so `requests/a/b.http` finds `a/b`.
    let wanted = path.trim().replace('\\', "/");
    let wanted = wanted.strip_prefix("requests/").unwrap_or(&wanted);
    let wanted = wanted.strip_suffix(".http").unwrap_or(wanted).trim_end_matches('/').to_string();
    // Read afresh each time, so a run after a change sends what's saved now.
    let once = || -> io::Result<ExitCode> {
        let (requests, _) = saved(&workspace, "tracer run")?;
        match requests.into_iter().find(|r| r.path == wanted) {
            Some(found) if report.is_none() && data.is_none() => {
                send_and_wait(&workspace, Command::Send { request: found.request, env: env.clone() }, "tracer run", until)
            }
            // A folder, or one request wanted as a report: a flow.
            _ => flow(&workspace, &wanted, env.clone(), keep_going, report.is_some(), data.clone()),
        }
    };
    if !watch {
        return once();
    }
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer run --watch")?;
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    // A failed run doesn't end the watch: the next change may fix it.
    let report_run = |run: io::Result<ExitCode>| {
        if let Err(e) = run {
            eprintln!("tracer: {e}");
        }
    };
    report_run(once());
    let under = format!("{wanted}/");
    let env_too = env.as_deref().map(|e| format!(" and environment {e}")).unwrap_or_default();
    eprintln!("watching requests/{wanted}{env_too}; Ctrl+C stops");
    loop {
        let why = match client.next_event()?.kind {
            EventKind::RequestSaved { path, .. } if path == wanted || path.starts_with(&under) => format!("requests/{path}.http changed"),
            EventKind::EnvironmentSaved { env: changed } if env.as_deref() == Some(changed.name.as_str()) => {
                format!("environment {} changed", changed.name)
            }
            _ => continue,
        };
        eprintln!("\n── {why}; running again");
        report_run(once());
    }
}

/// Run a flow, telling its steps as they go on stderr; with `json`, a
/// report on stdout at the end. Fails when the flow does.
fn flow(workspace: &PathBuf, path: &str, env: Option<String>, keep_going: bool, json: bool, data: Option<String>) -> io::Result<ExitCode> {
    let mut client = Client::connect(workspace, ClientKind::Cli, "tracer run")?;
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let started = std::time::Instant::now();
    let (flow_id, steps) = match client.call(Command::RunFlow { path: path.to_string(), env: env.clone(), keep_going, data })? {
        Ok(ReplyBody::FlowStarted { flow_id, steps }) => (flow_id, steps),
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    eprintln!("flow {flow_id}: {} step{}", steps.len(), if steps.len() == 1 { "" } else { "s" });
    // Each step's request, as it starts, by id: its path, status and time.
    let mut ours: Vec<(u64, String)> = Vec::new();
    let mut answers: Vec<(u64, u16, f64)> = Vec::new();
    loop {
        let event = client.next_event()?;
        match event.kind {
            EventKind::RequestStarted { request_id, flow: Some(f), request, .. } if f == flow_id => {
                // As written: a skipped step starts no request, so its name
                // can't be told from the order.
                eprintln!("→ #{request_id} {} {}", request.method, request.url);
                ours.push((request_id, String::new()));
            }
            EventKind::RequestFinished { request_id, response } if ours.iter().any(|(id, _)| *id == request_id) => {
                eprintln!("  {} · {:.0} ms · {} B", response.status, response.timing.total_ms, response.size);
                answers.push((request_id, response.status, response.timing.total_ms));
            }
            EventKind::RequestFailed { request_id, error } if ours.iter().any(|(id, _)| *id == request_id) => {
                eprintln!("  failed: {error}");
            }
            EventKind::RequestChecked { request_id, results } if ours.iter().any(|(id, _)| *id == request_id) => {
                for r in &results {
                    eprintln!("  {} {}   (found {})", if r.pass { "✓" } else { "✗" }, r.expect, r.actual);
                }
            }
            EventKind::FlowFinished { flow_id: f, passed, steps } if f == flow_id => {
                let failed = steps.iter().filter(|s| s.outcome == StepOutcome::Failed).count();
                let skipped = steps.iter().filter(|s| s.outcome == StepOutcome::Skipped).count();
                for s in steps.iter().filter(|s| s.outcome != StepOutcome::Passed) {
                    let row = s.row.map(|r| format!("row {r}: ")).unwrap_or_default();
                    eprintln!("{} {row}{}: {}", if s.outcome == StepOutcome::Failed { "✗" } else { "–" }, s.path, s.note);
                }
                eprintln!(
                    "flow {flow_id} {}: {} passed, {failed} failed, {skipped} skipped, in {:.1} s",
                    if passed { "passed" } else { "failed" },
                    steps.len() - failed - skipped,
                    started.elapsed().as_secs_f64()
                );
                if json {
                    println!("{}", report_json(flow_id, path, env.as_deref(), passed, &steps, &answers, started.elapsed()));
                }
                return Ok(if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE });
            }
            _ => {}
        }
    }
}

/// The run as JSON, for CI: each step with its outcome, status and time.
fn report_json(
    flow_id: u64,
    path: &str,
    env: Option<&str>,
    passed: bool,
    steps: &[FlowStep],
    answers: &[(u64, u16, f64)],
    took: std::time::Duration,
) -> String {
    let steps: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            let answer = s.request_id.and_then(|id| answers.iter().find(|(r, _, _)| *r == id));
            let mut v = serde_json::to_value(s).unwrap_or_default();
            if let Some((_, status, ms)) = answer {
                v["status"] = serde_json::json!(status);
                v["total_ms"] = serde_json::json!(ms.round());
            }
            v
        })
        .collect();
    let report = serde_json::json!({
        "flow_id": flow_id,
        "path": path,
        "env": env,
        "passed": passed,
        "took_ms": took.as_millis() as u64,
        "steps": steps,
    });
    serde_json::to_string_pretty(&report).unwrap_or_default()
}

fn bench(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("stop") => {
            let id = bench_id(args.get(1))?;
            let mut client = Client::connect(&workspace, ClientKind::Cli, &person())?;
            client.call(Command::StopBench { bench_id: id })?.map_err(io::Error::other)?;
            eprintln!("bench {id} stopping");
            return Ok(ExitCode::SUCCESS);
        }
        Some("baselines") => return baselines(&workspace),
        Some("compare") => {
            args.remove(0);
            let tolerance_pct = tolerance(&mut args)?;
            let [after, against] = <[String; 2]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
            let (baseline, other_bench) = match against.strip_prefix('#') {
                Some(id) => (None, Some(bench_id(Some(&id.to_string()))?)),
                None => (Some(against), None),
            };
            let comparison = compare(&workspace, Command::CompareBench { bench_id: bench_id(Some(&after))?, baseline, other_bench, tolerance_pct })?;
            eprint!("{}", comparison_table(&comparison));
            return Ok(if comparison.regressed { ExitCode::FAILURE } else { ExitCode::SUCCESS });
        }
        _ => {}
    }
    let number = |flag: &str, value: Option<String>| -> io::Result<Option<f64>> {
        value.map(|v| v.parse::<f64>().map_err(|_| io::Error::other(format!("{flag} takes a number, not `{v}`")))).transpose()
    };
    // Each `--env` a bench of its own, one after the other, side by side.
    let mut envs = Vec::new();
    while let Some(env) = take_value(&mut args, "--env") {
        envs.push(env);
    }
    let save_as = take_value(&mut args, "--save-baseline");
    let baseline = take_value(&mut args, "--baseline");
    let tolerance_pct = tolerance(&mut args)?;
    if save_as.is_some() && envs.len() > 1 {
        return Err(io::Error::other("--save-baseline keeps one bench; give one --env"));
    }
    let report = take_value(&mut args, "--report");
    if report.as_deref().is_some_and(|r| r != "json") {
        return Err(io::Error::other("--report takes json"));
    }
    let total = number("-n", take_value(&mut args, "-n"))?.map(|n| n as u64);
    let concurrency = number("-c", take_value(&mut args, "-c"))?.map_or(10, |c| c as u32);
    let rate = number("--rate", take_value(&mut args, "--rate"))?;
    let duration_secs = take_value(&mut args, "-t").map(|t| seconds(&t)).transpose()?;
    let [path] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let wanted = path.trim().replace('\\', "/");
    let wanted = wanted.strip_prefix("requests/").unwrap_or(&wanted);
    let wanted = wanted.strip_suffix(".http").unwrap_or(wanted);
    let (requests, _) = saved(&workspace, "tracer bench")?;
    let Some(found) = requests.into_iter().find(|r| r.path == wanted) else {
        return Err(io::Error::other(format!("there is no saved request `{wanted}`; `tracer ls` lists them")));
    };
    // Neither a count nor a time: a hundred.
    let total = if total.is_none() && duration_secs.is_none() { Some(100) } else { total };
    let plan = BenchPlan { total, concurrency, duration_secs, rate };

    let runs: Vec<Option<String>> = if envs.is_empty() { vec![None] } else { envs.into_iter().map(Some).collect() };
    let mut ok = true;
    let mut ran = Vec::new();
    for env in &runs {
        let (bench_id, stats, stopped) = bench_once(&workspace, &found.request, env.clone(), &plan, wanted)?;
        ok &= stopped.is_none() && stats.bad() == 0;
        ran.push(serde_json::json!({ "bench_id": bench_id, "env": env, "stopped": stopped, "stats": stats }));
    }
    let ids: Vec<u64> = ran.iter().filter_map(|r| r["bench_id"].as_u64()).collect();
    let mut comparisons = Vec::new();
    // Side by side: each after the first against the first.
    for &id in &ids[1..] {
        let c = compare(&workspace, Command::CompareBench { bench_id: id, baseline: None, other_bench: Some(ids[0]), tolerance_pct })?;
        eprint!("\n{}", comparison_table(&c));
        comparisons.push(c);
    }
    if let Some(name) = &baseline {
        for &id in &ids {
            let c = compare(&workspace, Command::CompareBench { bench_id: id, baseline: Some(name.clone()), other_bench: None, tolerance_pct })?;
            eprint!("\n{}", comparison_table(&c));
            ok &= !c.regressed;
            comparisons.push(c);
        }
    }
    if let Some(name) = &save_as {
        let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer bench")?;
        match client.call(Command::SaveBaseline { bench_id: ids[0], name: name.clone() })? {
            Ok(ReplyBody::BaselineSaved { name, path }) => eprintln!("saved bench {} as baseline {name} ({path})", ids[0]),
            Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
            Err(e) => return Err(io::Error::other(e)),
        }
    }
    if report.is_some() {
        let json = serde_json::json!({
            "path": wanted,
            "plan": plan,
            "benches": ran,
            "comparisons": comparisons,
            "histogram_bounds_ms": BENCH_BUCKETS_MS,
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap_or_default());
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn browser(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, &person())?;
    let Ok(ReplyBody::Cookies { mut browser, .. }) = client.call(Command::Cookies)? else {
        return Err(io::Error::other("unexpected answer"));
    };
    if !args.is_empty() {
        let mut words = args.iter();
        while let Some(word) = words.next() {
            match (word.as_str(), words.next().map(String::as_str)) {
                ("cookies", Some("on")) => browser.cookies = true,
                ("cookies", Some("off")) => browser.cookies = false,
                ("origin", Some("none")) => browser.origin = None,
                ("origin", Some(url)) => browser.origin = Some(url.to_string()),
                ("tls", Some("none")) => browser.unchecked_tls.clear(),
                ("tls", Some(hosts)) => {
                    browser.unchecked_tls = hosts.split(',').map(str::trim).filter(|h| !h.is_empty()).map(String::from).collect()
                }
                _ => return Err(io::Error::other(USAGE)),
            }
        }
        client.call(Command::SetBrowser { browser })?.map_err(io::Error::other)?;
        let Ok(ReplyBody::Cookies { browser: now, .. }) = client.call(Command::Cookies)? else {
            return Err(io::Error::other("unexpected answer"));
        };
        browser = now;
    }
    println!("cookies  {}", if browser.cookies { "kept and sent back" } else { "off" });
    match &browser.origin {
        Some(origin) => println!("origin   {origin}: Origin sent, preflights as a browser's, answers checked for CORS"),
        None => println!("origin   none: no CORS"),
    }
    match browser.unchecked_tls.as_slice() {
        [] => println!("tls      every certificate checked"),
        hosts => println!("tls      certificates NOT checked for {}", hosts.join(", ")),
    }
    Ok(ExitCode::SUCCESS)
}

fn cookies(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, &person())?;
    if args.first().map(String::as_str) == Some("clear") {
        let domain = args.get(1).cloned();
        let Ok(ReplyBody::CookiesCleared { removed }) = client.call(Command::ClearCookies { domain })?.map_err(io::Error::other) else {
            return Err(io::Error::other("unexpected answer"));
        };
        eprintln!("removed {removed} cookie{}", if removed == 1 { "" } else { "s" });
        return Ok(ExitCode::SUCCESS);
    }
    if !args.is_empty() {
        return Err(io::Error::other(USAGE));
    }
    let Ok(ReplyBody::Cookies { browser, cookies }) = client.call(Command::Cookies)? else {
        return Err(io::Error::other("unexpected answer"));
    };
    if !browser.cookies {
        eprintln!("cookies are off; `tracer browser cookies on` keeps them");
    }
    for c in &cookies {
        let mut flags = Vec::new();
        if c.secure {
            flags.push("Secure".to_string());
        }
        if c.http_only {
            flags.push("HttpOnly".to_string());
        }
        if let Some(s) = &c.same_site {
            flags.push(format!("SameSite={s}"));
        }
        let ends = match c.expires_ms {
            Some(ms) => format!("ends in {}", short_span(ms.saturating_sub(now_ms()))),
            None => "session".into(),
        };
        println!("{}{}  {}={}  {ends}  {}", c.domain, c.path, c.name, c.value.as_deref().unwrap_or("…"), flags.join(" "));
    }
    Ok(ExitCode::SUCCESS)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `45 s`, `12 min`, `3 h`, `2 d`.
fn short_span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", s / 60),
        5400..=172_799 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86_400),
    }
}

fn until(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    if args.first().map(String::as_str) == Some("stop") {
        let id = bench_id(args.get(1))?;
        let mut client = Client::connect(&workspace, ClientKind::Cli, &person())?;
        client.call(Command::StopUntil { until_id: id })?.map_err(io::Error::other)?;
        eprintln!("run-until {id} stopping");
        return Ok(ExitCode::SUCCESS);
    }
    let whole = |flag: &str, value: Option<String>| -> io::Result<Option<u64>> {
        value.map(|v| v.parse::<u64>().map_err(|_| io::Error::other(format!("{flag} takes a whole number, not `{v}`")))).transpose()
    };
    let env = take_value(&mut args, "--env");
    let matching = take_value(&mut args, "--match");
    let streak = whole("-n", take_value(&mut args, "-n"))?.map_or(3, |n| n as u32);
    let max = whole("--max", take_value(&mut args, "--max"))?;
    let gap_ms = take_value(&mut args, "--every").map(|t| millis(&t)).transpose()?;
    let [path] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let wanted = path.trim().replace('\\', "/");
    let wanted = wanted.strip_prefix("requests/").unwrap_or(&wanted);
    let wanted = wanted.strip_suffix(".http").unwrap_or(wanted);
    let (requests, _) = saved(&workspace, "tracer until")?;
    let Some(found) = requests.into_iter().find(|r| r.path == wanted) else {
        return Err(io::Error::other(format!("there is no saved request `{wanted}`; `tracer ls` lists them")));
    };
    let plan = UntilPlan { streak, matching, max, gap_ms };

    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer until")?;
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let until_id = match client.call(Command::RunUntil { request: found.request, env: env.clone(), plan: plan.clone() })? {
        Ok(ReplyBody::UntilStarted { until_id }) => until_id,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    let with = env.as_ref().map(|e| format!(" (env {e})")).unwrap_or_default();
    eprintln!("run-until {until_id}: {wanted}{with}, {}; `tracer until stop {until_id}` stops it", plan.describe());
    // How each request ended, for its line when the run tells whether it counted.
    let mut ends: HashMap<u64, String> = HashMap::new();
    loop {
        let event = client.next_event()?;
        match event.kind {
            EventKind::RequestFinished { request_id, response } => {
                ends.insert(request_id, format!("{} · {:.0} ms", response.status, response.timing.total_ms));
            }
            EventKind::RequestFailed { request_id, error } => {
                ends.insert(request_id, format!("no answer: {error}"));
            }
            EventKind::ApprovalAsked { reason, request_id } => {
                eprintln!("waits for a person: {reason}; `tracer approve {request_id}`");
            }
            EventKind::UntilProgress { until_id: id, request_id, counted, streak, .. } if id == until_id => {
                let end = ends.remove(&request_id).unwrap_or_default();
                let mark = if counted { "●" } else { "·" };
                println!("#{request_id:<5} {mark} {end}  ({streak} in a row)");
            }
            EventKind::UntilFinished { until_id: id, sent, streak, reached, stopped } if id == until_id => {
                if reached {
                    eprintln!("{streak} in a row after {sent} sent");
                } else {
                    eprintln!("ended without {} in a row after {sent} sent: {}", plan.streak, stopped.unwrap_or_default());
                }
                return Ok(if reached { ExitCode::SUCCESS } else { ExitCode::FAILURE });
            }
            _ => {}
        }
    }
}

/// `500ms`, `2s`, `1m` as milliseconds; a bare number is milliseconds.
fn millis(text: &str) -> io::Result<u64> {
    let t = text.trim().to_ascii_lowercase();
    let (n, unit) = match t.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => (&t[..at], &t[at..]),
        None => (t.as_str(), "ms"),
    };
    let per = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" | "min" => 60_000,
        _ => 0,
    };
    match n.parse::<u64>() {
        Ok(n) if per > 0 => Ok(n * per),
        _ => Err(io::Error::other(format!("--every takes a duration such as 500ms or 2s, not `{text}`"))),
    }
}

fn bench_id(arg: Option<&String>) -> io::Result<u64> {
    arg.and_then(|a| a.trim_start_matches('#').parse().ok()).ok_or_else(|| io::Error::other(USAGE))
}

fn tolerance(args: &mut Vec<String>) -> io::Result<Option<f64>> {
    take_value(args, "--tolerance")
        .map(|t| t.trim_end_matches('%').parse().map_err(|_| io::Error::other(format!("--tolerance takes a percentage, not `{t}`"))))
        .transpose()
}

/// Run one bench and tell it as it goes: its id, its stats, and why it
/// stopped early if it did.
fn bench_once(
    workspace: &PathBuf,
    request: &HttpRequest,
    env: Option<String>,
    plan: &BenchPlan,
    path: &str,
) -> io::Result<(u64, BenchStats, Option<String>)> {
    let mut client = Client::connect(workspace, ClientKind::Cli, "tracer bench")?;
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let bench_id = match client.call(Command::RunBench { request: request.clone(), env: env.clone(), plan: plan.clone() })? {
        Ok(ReplyBody::BenchStarted { bench_id }) => bench_id,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    let with = env.as_ref().map(|e| format!(" (env {e})")).unwrap_or_default();
    eprintln!("bench {bench_id}: {path}{with}, {}; `tracer bench stop {bench_id}` stops it", plan.describe());
    let live = io::stderr().is_terminal();
    loop {
        let event = client.next_event()?;
        match event.kind {
            EventKind::BenchProgress { bench_id: id, stats } if id == bench_id && live => {
                eprint!("\r\x1b[2K  {}", bench_line(&stats));
            }
            EventKind::ApprovalAsked { reason, request_id } => {
                eprintln!("waits for a person: {reason}; `tracer approve {request_id}`");
            }
            EventKind::BenchFinished { bench_id: id, stats, stopped } if id == bench_id => {
                if live {
                    eprint!("\r\x1b[2K");
                }
                eprint!("{}", bench_summary(&stats));
                if let Some(why) = &stopped {
                    eprintln!("stopped early: {why}");
                }
                return Ok((bench_id, stats, stopped));
            }
            _ => {}
        }
    }
}

fn compare(workspace: &PathBuf, command: Command) -> io::Result<BenchComparison> {
    let mut client = Client::connect(workspace, ClientKind::Cli, "tracer bench")?;
    match client.call(command)? {
        Ok(ReplyBody::BenchCompared { comparison }) => Ok(comparison),
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn baselines(workspace: &PathBuf) -> io::Result<ExitCode> {
    let mut client = Client::connect(workspace, ClientKind::Cli, "tracer bench")?;
    let (baselines, skipped) = match client.call(Command::ListBaselines)? {
        Ok(ReplyBody::Baselines { baselines, skipped }) => (baselines, skipped),
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    if baselines.is_empty() {
        eprintln!("no baselines; `tracer bench <PATH> --save-baseline <NAME>` keeps one");
    }
    for b in &baselines {
        let with = b.env.as_ref().map(|e| format!(" · env {e}")).unwrap_or_default();
        println!(
            "{:<16} {} {}{with} · {} · p50 {:.1} p95 {:.1} p99 {:.1} ms · {:.1} a second · bench {}",
            b.name, b.request.method, b.request.url, b.plan.describe(), b.stats.p50_ms, b.stats.p95_ms, b.stats.p99_ms, b.stats.per_sec, b.bench_id
        );
    }
    for s in skipped {
        eprintln!("tracer: skipped {s}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Two benches in columns, each change marked ✗ when worse past the
/// tolerance and ✓ when better.
fn comparison_table(c: &BenchComparison) -> String {
    let (wb, wa) = (c.before.chars().count().max(10), c.after.chars().count().max(10));
    let mut out = format!("  {:<10}  {:>wb$}  {:>wa$}  change\n", "", c.before, c.after);
    let mut worse = Vec::new();
    for d in &c.deltas {
        let (label, unit, points) = match d.metric.as_str() {
            "mean_ms" => ("mean", " ms", false),
            "p50_ms" => ("p50", " ms", false),
            "p95_ms" => ("p95", " ms", false),
            "p99_ms" => ("p99", " ms", false),
            "per_sec" => ("a second", "", false),
            "error_pct" => ("errors", " %", true),
            "limited_pct" => ("limited", " %", true),
            other => (other, "", false),
        };
        if points && d.before == 0.0 && d.after == 0.0 {
            continue;
        }
        let mark = if d.worse { "✗" } else if d.better { "✓" } else { " " };
        let change = if points { format!("{:+.1} pts", d.change) } else { format!("{:+.1} %", d.change) };
        let (b, a) = (format!("{:.1}{unit}", d.before), format!("{:.1}{unit}", d.after));
        out.push_str(&format!("  {label:<10}  {b:>wb$}  {a:>wa$}  {change} {mark}\n"));
        if d.worse {
            worse.push(label);
        }
    }
    for note in &c.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    if worse.is_empty() {
        out.push_str(&format!("  no regression past {} %\n", c.tolerance_pct));
    } else {
        out.push_str(&format!("  regressed past {} %: {}\n", c.tolerance_pct, worse.join(", ")));
    }
    out
}

fn schema(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let env = take_value(&mut args, "--env");
    let mut headers = Vec::new();
    while let Some(h) = take_value(&mut args, "-H") {
        let (name, value) = h.split_once(':').ok_or_else(|| io::Error::other(format!("`{h}` is not `Name: value`")))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let [target] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    // A saved request's URL and headers, or a URL.
    let (url, saved_headers) = if target.contains("://") || target.contains("{{") {
        (target, Vec::new())
    } else {
        let wanted = target.trim_start_matches("requests/").trim_end_matches(".http").to_string();
        let (requests, _) = saved(&workspace, "tracer schema")?;
        let found = requests
            .into_iter()
            .find(|r| r.path == wanted)
            .ok_or_else(|| io::Error::other(format!("`{wanted}` is neither a URL nor a saved request")))?;
        (found.request.url, found.request.headers)
    };
    let request = HttpRequest {
        method: "GRAPHQL".into(),
        url,
        headers: saved_headers.into_iter().chain(headers).collect(),
        body: Some(GRAPHQL_INTROSPECTION.into()),
        ..Default::default()
    };
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer schema")?;
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let request_id = match client.call(Command::Send { request, env })? {
        Ok(ReplyBody::Accepted { request_id }) => request_id,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    loop {
        match client.next_event()?.kind {
            EventKind::RequestFinished { request_id: id, .. } if id == request_id => break,
            EventKind::RequestFailed { request_id: id, error } if id == request_id => return Err(io::Error::other(error)),
            EventKind::RequestCancelled { request_id: id } if id == request_id => return Err(io::Error::other("cancelled")),
            EventKind::ApprovalAsked { request_id: id, reason } if id == request_id => eprintln!("waits for a person: {reason}"),
            _ => {}
        }
    }
    match client.call(Command::GraphqlSchema { request_id })? {
        Ok(ReplyBody::GraphqlSchema { schema }) => {
            println!("{}", schema.to_sdl());
            eprintln!("#{request_id}: {} types", schema.types.len());
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

/// `30s`, `2m`, `1h`, or a bare number of seconds.
fn seconds(text: &str) -> io::Result<u64> {
    let t = text.trim().to_ascii_lowercase();
    let (n, unit) = match t.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => (&t[..at], &t[at..]),
        None => (t.as_str(), "s"),
    };
    let per = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        _ => 0,
    };
    match n.parse::<u64>() {
        Ok(n) if per > 0 => Ok(n * per),
        _ => Err(io::Error::other(format!("-t takes a duration such as 30s or 2m, not `{text}`"))),
    }
}

/// `412 done · 98.2/s · p50 41 · p95 80 · p99 120 ms · 0 wrong`
fn bench_line(s: &BenchStats) -> String {
    format!(
        "{} done · {:.1}/s · p50 {:.0} · p95 {:.0} · p99 {:.0} ms · {} wrong",
        s.done,
        s.per_sec,
        s.p50_ms,
        s.p95_ms,
        s.p99_ms,
        s.bad()
    )
}

/// The whole of a finished bench, for a person at a terminal.
fn bench_summary(s: &BenchStats) -> String {
    let mut out = format!(
        "{} requests in {:.1} s, {:.1} a second\n\n  min {:.1}  mean {:.1}  p50 {:.1}  p95 {:.1}  p99 {:.1}  max {:.1}  (ms)\n\n",
        s.done,
        s.elapsed_ms / 1000.0,
        s.per_sec,
        s.min_ms,
        s.mean_ms,
        s.p50_ms,
        s.p95_ms,
        s.p99_ms,
        s.max_ms
    );
    // Only the buckets from the first to the last that hold any.
    let most = s.histogram.iter().copied().max().unwrap_or(0).max(1);
    let used: Vec<usize> = (0..s.histogram.len()).filter(|&i| s.histogram[i] > 0).collect();
    if let (Some(&first), Some(&last)) = (used.first(), used.last()) {
        for i in first..=last {
            let label = match BENCH_BUCKETS_MS.get(i) {
                Some(bound) => format!("≤ {bound} ms"),
                None => format!("> {} ms", BENCH_BUCKETS_MS[BENCH_BUCKETS_MS.len() - 1]),
            };
            let bar = "█".repeat((s.histogram[i] * 40).div_ceil(most) as usize);
            out.push_str(&format!("  {label:>11}  {bar} {}\n", s.histogram[i]));
        }
        out.push('\n');
    }
    let statuses: Vec<String> = s.statuses.iter().map(|(code, n)| format!("{code} × {n}")).collect();
    if !statuses.is_empty() {
        out.push_str(&format!("  status  {}\n", statuses.join(", ")));
    }
    if s.failed_checks > 0 {
        out.push_str(&format!("  {} answered but failed a check\n", s.failed_checks));
    }
    for (error, n) in &s.errors {
        out.push_str(&format!("  no answer × {n}: {error}\n"));
    }
    if let Some(limit) = &s.rate_limit {
        out.push_str(&format!("  rate    {}\n", limit.describe()));
    }
    out
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

fn export(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let lang = match take_value(&mut args, "--as") {
        Some(name) => ExportLang::parse(&name).ok_or_else(|| io::Error::other(format!("`{name}`: export as curl, fetch, python or rux")))?,
        None => ExportLang::Curl,
    };
    let env = take_value(&mut args, "--env");
    let [target] = <[String; 1]>::try_from(args).map_err(|_| io::Error::other(USAGE))?;
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer export")?;
    // A past request by its number, with its own env; or a saved one.
    let (request, env) = match target.trim_start_matches('#').parse::<u64>() {
        Ok(request_id) => match client.call(Command::Response { request_id })? {
            Ok(ReplyBody::Response { entry, .. }) => (entry.request, env.or(entry.env)),
            Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
            Err(e) => return Err(io::Error::other(e)),
        },
        Err(_) => {
            let wanted = target.trim_start_matches("requests/").trim_end_matches(".http").to_string();
            let (requests, _) = saved(&workspace, "tracer export")?;
            let found = requests
                .into_iter()
                .find(|r| r.path == wanted)
                .ok_or_else(|| io::Error::other(format!("`{wanted}` is neither a request's number nor a saved request")))?;
            (found.request, env)
        }
    };
    match client.call(Command::Export { request, env, lang })? {
        Ok(ReplyBody::Exported { text }) => {
            println!("{}", text.trim_end());
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

fn servers(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let ports = args
        .iter()
        .map(|p| p.parse::<u16>().map_err(|_| io::Error::other(format!("`{p}` isn't a port"))))
        .collect::<io::Result<Vec<u16>>>()?;
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer servers")?;
    eprintln!("looking…");
    let servers = match client.call(Command::LocalServers { ports })? {
        Ok(ReplyBody::LocalServers { servers }) => servers,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    if servers.is_empty() {
        println!("nothing answers HTTP on this machine's listening ports");
    }
    for s in servers {
        let mut line = format!("{:<28} {}", s.url, s.status);
        if let Some(process) = &s.process {
            line.push_str(&format!("  {process}"));
        }
        if let Some(server) = &s.server {
            line.push_str(&format!("  ({server})"));
        }
        if let Some(title) = &s.title {
            line.push_str(&format!("  \"{title}\""));
        }
        if let Some(openapi) = &s.openapi {
            line.push_str(&format!("  OpenAPI at {openapi}"));
        }
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn explain(workspace: PathBuf, args: Vec<String>) -> io::Result<ExitCode> {
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer explain")?;
    let explained = |client: &mut Client, request_id: u64| match client.call(Command::Explain { request_id })? {
        Ok(ReplyBody::Explained { text, failed, summary }) => Ok((text, failed, summary)),
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    };
    let text = match args.as_slice() {
        [id] => {
            let request_id = id.trim_start_matches('#').parse().map_err(|_| io::Error::other(format!("`{id}` isn't a request's number")))?;
            explained(&mut client, request_id)?.0
        }
        [] => {
            // The newest that went wrong, among the last few hundred.
            let filter = HistoryFilter { limit: Some(500), ..HistoryFilter::default() };
            let ReplyBody::History { entries, .. } = client.call(Command::History { filter })?.map_err(io::Error::other)? else {
                return Err(io::Error::other("unexpected answer"));
            };
            let mut found = None;
            for entry in entries {
                let (text, failed, _) = explained(&mut client, entry.request_id)?;
                if failed {
                    found = Some(text);
                    break;
                }
            }
            found.ok_or_else(|| io::Error::other("nothing in the last 500 requests went wrong; name one: tracer explain <ID>"))?
        }
        _ => return Err(io::Error::other(USAGE)),
    };
    print!("{text}");
    Ok(ExitCode::SUCCESS)
}

fn import(workspace: PathBuf, mut args: Vec<String>) -> io::Result<ExitCode> {
    let save = take_value(&mut args, "--save");
    let text = match args.as_slice() {
        [] => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            text
        }
        [file] => std::fs::read_to_string(file)?,
        _ => return Err(io::Error::other(USAGE)),
    };
    let mut client = Client::connect(&workspace, ClientKind::Cli, "tracer import")?;
    let (request, notes, http) = match client.call(Command::Import { text })? {
        Ok(ReplyBody::Imported { request, notes, http }) => (request, notes, http),
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    for note in &notes {
        eprintln!("note: {note}");
    }
    match save {
        Some(path) => {
            let path = path.trim_start_matches("requests/").trim_end_matches(".http").to_string();
            client.call(Command::SaveRequest { path: path.clone(), request: request.clone() })?.map_err(io::Error::other)?;
            eprintln!("saved requests/{path}.http: {} {}", request.method, request.url);
        }
        None => print!("{http}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn saved(workspace: &PathBuf, name: &str) -> io::Result<(Vec<SavedRequest>, Vec<String>)> {
    let mut client = Client::connect(workspace, ClientKind::Cli, name)?;
    match client.call(Command::ListRequests)? {
        Ok(ReplyBody::Requests { requests, skipped, .. }) => Ok((requests, skipped)),
        Ok(other) => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => Err(io::Error::other(e)),
    }
}

/// Send a request (`Send` or `Rerun`) and print its answer when it comes;
/// a stream's messages as they come, until it's closed.
fn send_and_wait(workspace: &PathBuf, sending: Command, name: &str, until: Until) -> io::Result<ExitCode> {
    let mut client = Client::connect(workspace, ClientKind::Cli, name)?;
    // Subscribed before sending, so the answer cannot go past unseen.
    client.call(Command::Subscribe { since: None })?.map_err(io::Error::other)?;
    let request_id = match client.call(sending)? {
        Ok(ReplyBody::Accepted { request_id }) => request_id,
        Ok(other) => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        Err(e) => return Err(io::Error::other(e)),
    };
    eprintln!("#{request_id} sent; `tracer cancel {request_id}` stops it");
    // Known from its start: whether a `request_checked` follows its end.
    let mut checked = false;
    // How it ended, kept while its checks are awaited.
    let mut ended: Option<io::Result<ExitCode>> = None;
    // Whether it stayed open as a stream, and the messages in so far.
    let mut streamed = false;
    let mut received = 0u64;
    loop {
        let event = client.next_event()?;
        match &event.kind {
            EventKind::StreamOpened { request_id: id, kind, response } if *id == request_id => {
                streamed = true;
                let (what, how) = match kind {
                    StreamKind::Websocket if until.stdin => ("WebSocket", "type a line to send it; end of input closes it"),
                    StreamKind::Websocket => ("WebSocket", "`tracer cancel` closes it"),
                    StreamKind::Sse => ("server-sent events", "`tracer cancel` closes it"),
                };
                eprintln!(
                    "{} · TTFB {:.0} ms{} · {what} open; {how}",
                    response.status,
                    response.timing.ttfb_ms,
                    phases(&response.timing)
                );
                until.start(workspace, request_id, *kind == StreamKind::Websocket);
                continue;
            }
            EventKind::StreamMessage { request_id: id, message } if *id == request_id => {
                match message.dir {
                    Direction::In => {
                        let tags: Vec<String> = [("event", &message.event), ("id", &message.id)]
                            .iter()
                            .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k} {v}")))
                            .collect();
                        if !tags.is_empty() {
                            eprintln!("· {}", tags.join(" · "));
                        }
                        println!("{}", message.text);
                        received += 1;
                        if until.count == Some(received) {
                            close(workspace, request_id);
                        }
                    }
                    Direction::Out => eprintln!("↑ {}", message.text),
                }
                continue;
            }
            EventKind::StreamClosed { request_id: id, reason, received, sent } if *id == request_id => {
                eprintln!("{reason} · {received} in, {sent} out");
                continue;
            }
            _ => {}
        }
        if let EventKind::RequestStarted { request_id: id, request, .. } = &event.kind {
            if *id == request_id {
                checked = request.is_checked();
            }
            continue;
        }
        if let EventKind::RequestChecked { request_id: id, results } = &event.kind {
            if *id == request_id {
                let failed = results.iter().filter(|r| !r.pass).count();
                for r in results {
                    let mark = if r.pass { "✓" } else { "✗" };
                    eprintln!("{mark} {}   (found {})", r.expect, r.actual);
                }
                eprintln!("checks: {} passed, {failed} failed", results.len() - failed);
                let outcome = ended.take().unwrap_or(Ok(ExitCode::SUCCESS));
                return if failed > 0 { outcome.and(Ok(ExitCode::FAILURE)) } else { outcome };
            }
            continue;
        }
        if let EventKind::RequestCancelled { request_id: id } = event.kind {
            if id == request_id {
                return Err(io::Error::other(format!("cancelled by {}", event.actor.name)));
            }
            continue;
        }

        match event.kind {
            EventKind::RequestFinished { request_id: id, response } if id == request_id => {
                eprintln!(
                    "{} · TTFB {:.0} ms{} · total {:.0} ms · {} B",
                    response.status,
                    response.timing.ttfb_ms,
                    phases(&response.timing),
                    response.timing.total_ms,
                    response.size
                );
                if response.tls_unchecked {
                    eprintln!("TLS not checked: the server's certificate was accepted unchecked (tracer browser)");
                }
                if !streamed {
                    println!("{}", response.body);
                }
                let outcome = Ok(if response.status < 400 { ExitCode::SUCCESS } else { ExitCode::FAILURE });
                if !checked {
                    return outcome;
                }
                ended = Some(outcome);
            }
            EventKind::RequestFailed { request_id: id, error } if id == request_id => {
                if !checked {
                    return Err(io::Error::other(error));
                }
                eprintln!("tracer: {error}");
                ended = Some(Ok(ExitCode::FAILURE));
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

/// ` (DNS 3 · connect 41 · TLS 80 · wait 120)`: where TTFB went, or ""
/// for a request journalled before the phases were measured.
fn phases(t: &Timing) -> String {
    let mut parts = Vec::new();
    for (name, ms) in [("DNS", t.dns_ms), ("connect", t.connect_ms), ("TLS", t.tls_ms)] {
        if let Some(ms) = ms {
            parts.push(format!("{name} {ms:.0}"));
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.push(format!("wait {:.0}", t.wait_ms()));
    format!(" ({})", parts.join(" · "))
}

/// One line per event, for a person reading a terminal.
fn describe(event: &Event) -> String {
    let who = format!("{:?}:{}", event.actor.kind, event.actor.name).to_lowercase();
    let what = match &event.kind {
        EventKind::ClientJoined { client } => format!("joined as client {client}"),
        EventKind::ClientLeft { client } => format!("left (client {client})"),
        EventKind::RequestStarted { request_id, request, env, rerun_of, flow, bench } => {
            let with = env.as_ref().map(|e| format!(" (env {e})")).unwrap_or_default();
            let again = rerun_of.map(|id| format!(", re-run of #{id}")).unwrap_or_default();
            let step = flow.map(|id| format!(", flow {id}")).unwrap_or_default();
            let first = bench.map(|id| format!(", first of bench {id}")).unwrap_or_default();
            format!("#{request_id} {} {}{with}{again}{step}{first}", request.method, request.url)
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
        EventKind::ApprovalAsked { request_id, reason } => {
            format!("#{request_id} waits for a person: {reason}; `tracer approve {request_id}` or `tracer deny {request_id}`")
        }
        EventKind::ApprovalGiven { request_id, allow } => {
            format!("#{request_id} {}", if *allow { "allowed" } else { "refused" })
        }
        EventKind::RequestChecked { request_id, results } => {
            let failed: Vec<&str> = results.iter().filter(|r| !r.pass).map(|r| r.expect.as_str()).collect();
            match failed.as_slice() {
                [] => format!("#{request_id} checks: all {} passed", results.len()),
                _ => format!("#{request_id} checks: {} of {} failed: {}", failed.len(), results.len(), failed.join("; ")),
            }
        }
        EventKind::FlowStarted { flow_id, path, steps, .. } => format!("flow {flow_id} started: {path} ({} steps)", steps.len()),
        EventKind::FlowFinished { flow_id, passed, steps } => {
            let failed = steps.iter().filter(|s| s.outcome == StepOutcome::Failed).count();
            if *passed {
                format!("flow {flow_id} passed ({} steps)", steps.len())
            } else {
                format!("flow {flow_id} failed: {failed} of {} steps", steps.len())
            }
        }
        EventKind::BenchStarted { bench_id, request, plan, .. } => {
            format!("bench {bench_id} started: {} {}, {}", request.method, request.url, plan.describe())
        }
        EventKind::BenchProgress { bench_id, stats } => format!("bench {bench_id}: {}", bench_line(stats)),
        EventKind::BenchFinished { bench_id, stats, stopped } => match stopped {
            Some(why) => format!("bench {bench_id} {why}: {}", bench_line(stats)),
            None => format!("bench {bench_id} finished: {}", bench_line(stats)),
        },
        EventKind::BaselineSaved { name, bench_id } => format!("saved bench {bench_id} as baseline {name}"),
        EventKind::UntilStarted { until_id, request, plan, .. } => {
            format!("run-until {until_id} started: {} {}, {}", request.method, request.url, plan.describe())
        }
        EventKind::UntilProgress { until_id, request_id, counted, streak, .. } => {
            format!("run-until {until_id}: #{request_id} {}, {streak} in a row", if *counted { "counts" } else { "doesn't count" })
        }
        EventKind::UntilFinished { until_id, sent, streak, reached: true, .. } => {
            format!("run-until {until_id} reached {streak} in a row after {sent} sent")
        }
        EventKind::UntilFinished { until_id, sent, stopped, .. } => {
            format!("run-until {until_id} ended after {sent} sent: {}", stopped.as_deref().unwrap_or(""))
        }
        EventKind::AgentsSwitched { allowed } => format!("switched agents {}", if *allowed { "on" } else { "off" }),
        EventKind::BrowserChanged { browser } => format!(
            "browser behaviour: cookies {}, origin {}, TLS unchecked for {}",
            if browser.cookies { "on" } else { "off" },
            browser.origin.as_deref().unwrap_or("none"),
            if browser.unchecked_tls.is_empty() { "none".to_string() } else { browser.unchecked_tls.join(", ") }
        ),
        EventKind::CookiesChanged { cookies } => {
            format!("cookie jar: {} cookie{}", cookies.len(), if cookies.len() == 1 { "" } else { "s" })
        }
        EventKind::PolicyChanged { rules, error: Some(error) } => {
            format!("agents.policy not taken, {} rules stand: {error}", rules.len())
        }
        EventKind::PolicyChanged { rules, error: None } => {
            let each: Vec<String> = rules.iter().map(describe_rules).collect();
            format!("agents.policy changed: {}", each.join("; "))
        }
        EventKind::StreamOpened { request_id, kind, response } => {
            let what = if *kind == StreamKind::Websocket { "WebSocket" } else { "server-sent events" };
            format!("#{request_id} {what} open ({})", response.status)
        }
        EventKind::StreamMessage { request_id, message } => {
            let arrow = if message.dir == Direction::In { "↓" } else { "↑" };
            let event = message.event.as_ref().map(|e| format!("[{e}] ")).unwrap_or_default();
            let line: String = message.text.lines().next().unwrap_or("").chars().take(120).collect();
            format!("#{request_id} {arrow}{} {event}{line}", message.n)
        }
        EventKind::StreamClosed { request_id, reason, received, sent } => {
            format!("#{request_id} {reason} ({received} in, {sent} out)")
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
