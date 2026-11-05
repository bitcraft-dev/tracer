//! The core as its clients see it: over TCP, through the protocol's client.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use tracer_protocol::client::{Client, Incoming};
use tracer_protocol::{ClientKind, Command, Event, EventKind, HttpRequest, ReplyBody, UntilPlan, DISCOVERY_PATH};

/// A fresh workspace under the temp directory, per test.
fn workspace(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tracer-test-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An HTTP server that answers every request with `200` and a small JSON body
/// naming the path it was asked for. Gives its base URL.
fn stub_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            thread::spawn(move || answer(stream));
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn answer(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
    let reply = format!("{{\"path\":\"{path}\",\"got\":{length}}}");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
}

fn get(url: String) -> Command {
    Command::Send { request: HttpRequest { method: "GET".into(), url, headers: vec![], body: None, ..Default::default() }, env: None }
}

#[test]
fn a_request_is_answered_with_its_timing_and_seen_by_every_subscriber() {
    let ws = workspace("answer");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();

    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    let Ok(ReplyBody::Accepted { request_id }) = agent.call(get(format!("{base}/albums?id=1"))).unwrap() else {
        panic!("send was not accepted");
    };

    // The window sees the agent join, then the request start and finish,
    // all attributed to the agent.
    let mut seen = Vec::new();
    while !matches!(seen.last(), Some(Event { kind: EventKind::RequestFinished { .. }, .. })) {
        seen.push(window.next_event().unwrap());
    }
    let started = seen.iter().position(|e| matches!(e.kind, EventKind::RequestStarted { .. })).unwrap();
    let finished = seen.len() - 1;
    assert!(started < finished);
    let EventKind::RequestFinished { request_id: id, response } = &seen[finished].kind else { unreachable!() };
    assert_eq!(*id, request_id);
    assert_eq!(seen[finished].actor.name, "claude-code");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, r#"{"path":"/albums?id=1","got":0}"#);
    assert!(response.timing.ttfb_ms <= response.timing.total_ms);

    core.stop();
}

/// The property Rux's signals lack (FINDINGS.md AW-011) and the window's live
/// sync depends on: every event, numbered without a gap, in one order for
/// everyone, and a late joiner's replay identical to what was seen live.
#[test]
fn a_burst_reaches_a_watcher_whole_and_in_order_and_replays_identically() {
    const REQUESTS: usize = 200;
    let ws = workspace("burst");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();

    let mut watcher = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    let Ok(ReplyBody::Subscribed { next_seq: first_seq, .. }) =
        watcher.call(Command::Subscribe { since: None }).unwrap()
    else {
        panic!("subscribe failed");
    };

    // Four agents at once, fifty requests each, as fast as they can send.
    let agents: Vec<_> = (0..4)
        .map(|n| {
            let (ws, base) = (ws.clone(), base.clone());
            thread::spawn(move || {
                let mut agent = Client::connect(&ws, ClientKind::Agent, &format!("agent-{n}")).unwrap();
                for i in 0..REQUESTS / 4 {
                    agent.call(get(format!("{base}/r/{n}/{i}"))).unwrap().unwrap();
                }
                // Stay connected until the answers are in, so `client_left`
                // does not interleave with what is being counted.
                agent
            })
        })
        .collect();

    let mut live = Vec::new();
    let mut finished = 0;
    while finished < REQUESTS {
        let event = watcher.next_event().unwrap();
        if matches!(event.kind, EventKind::RequestFinished { .. }) {
            finished += 1;
        }
        live.push(event);
    }
    let agents: Vec<Client> = agents.into_iter().map(|a| a.join().unwrap()).collect();

    // Numbered 1, 2, 3, … from where the watcher came in, with no gap.
    for (i, event) in live.iter().enumerate() {
        assert_eq!(event.seq, first_seq + i as u64, "event {i} out of order or missing");
    }
    // Each request starts before it finishes, and every one finished.
    let mut started_at = HashMap::new();
    for (i, event) in live.iter().enumerate() {
        match &event.kind {
            EventKind::RequestStarted { request_id, .. } => {
                started_at.insert(*request_id, i);
            }
            EventKind::RequestFinished { request_id, response } => {
                assert!(started_at[request_id] < i, "request {request_id} finished before it started");
                assert_eq!(response.status, 200);
            }
            EventKind::RequestFailed { request_id, error } => panic!("request {request_id} failed: {error}"),
            _ => {}
        }
    }
    assert_eq!(started_at.len(), REQUESTS);

    // A window opening now replays exactly what the first one saw live.
    let mut late = Client::connect(&ws, ClientKind::Window, "late window").unwrap();
    let Ok(ReplyBody::Subscribed { replayed, .. }) =
        late.call(Command::Subscribe { since: Some(first_seq - 1) }).unwrap()
    else {
        panic!("late subscribe failed");
    };
    assert!(replayed as usize >= live.len());
    for expected in &live {
        assert_eq!(&late.next_event().unwrap(), expected);
    }

    drop(agents);
    core.stop();
}

#[test]
fn a_restarted_core_carries_on_the_journal_numbering() {
    let ws = workspace("restart");
    let core = tracer_core::start(&ws).unwrap();
    let mut a = Client::connect(&ws, ClientKind::Cli, "first").unwrap();
    let Ok(ReplyBody::Status { next_seq: before, .. }) = a.call(Command::Status).unwrap() else { panic!() };
    drop(a);
    core.stop();

    let core = tracer_core::start(&ws).unwrap();
    let mut b = Client::connect(&ws, ClientKind::Cli, "second").unwrap();
    let Ok(ReplyBody::Status { next_seq: after, .. }) = b.call(Command::Status).unwrap() else { panic!() };
    // `first` joined (and left, unless the old core had stopped first), then
    // `second` joined: numbering moved on and never restarted at 1.
    assert!(after > before, "journal numbering restarted: {before} then {after}");

    // And the whole journal replays from 1 without a gap.
    b.call(Command::Subscribe { since: Some(0) }).unwrap().unwrap();
    for seq in 1..after {
        assert_eq!(b.next_event().unwrap().seq, seq);
    }
    core.stop();
}

#[test]
fn a_client_without_the_token_is_refused() {
    let ws = workspace("token");
    let core = tracer_core::start(&ws).unwrap();
    let mut found = core.discovery();
    found.token = "not-the-token".into();
    let err = Client::connect_to(&found, ClientKind::Agent, "intruder").err().expect("connected without the token");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    core.stop();
}

#[test]
fn nothing_is_accepted_before_hello() {
    let ws = workspace("hello");
    let core = tracer_core::start(&ws).unwrap();
    let mut raw = TcpStream::connect(("127.0.0.1", core.port)).unwrap();
    writeln!(raw, r#"{{"id":1,"cmd":"status"}}"#).unwrap();
    let mut line = String::new();
    BufReader::new(raw.try_clone().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains("say hello first"), "{line}");
    core.stop();
}

/// The window's way of talking: one connection, calls from several threads
/// while a reader thread hands over the events, none lost or reordered.
#[test]
fn a_shared_client_takes_calls_from_many_threads_and_keeps_its_events_in_order() {
    const PER_THREAD: usize = 25;
    let ws = workspace("shared");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();

    let mut client = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    let Ok(ReplyBody::Subscribed { next_seq: first_seq, .. }) =
        client.call(Command::Subscribe { since: None }).unwrap()
    else {
        panic!("subscribe failed");
    };
    let (tx, events) = mpsc::channel();
    let shared = Arc::new(client.into_shared(move |incoming| {
        let _ = tx.send(incoming);
    }).unwrap());

    let callers: Vec<_> = (0..4)
        .map(|n| {
            let (shared, base) = (shared.clone(), base.clone());
            thread::spawn(move || {
                (0..PER_THREAD)
                    .map(|i| match shared.call(get(format!("{base}/s/{n}/{i}"))).unwrap() {
                        Ok(ReplyBody::Accepted { request_id }) => request_id,
                        other => panic!("send {n}/{i}: {other:?}"),
                    })
                    .collect::<Vec<u64>>()
            })
        })
        .collect();
    let mut accepted: Vec<u64> = callers.into_iter().flat_map(|c| c.join().unwrap()).collect();
    accepted.sort();
    accepted.dedup();
    assert_eq!(accepted.len(), 4 * PER_THREAD, "each call got its own reply");

    let mut finished = 0;
    let mut expected_seq = first_seq;
    while finished < 4 * PER_THREAD {
        match events.recv_timeout(Duration::from_secs(10)).expect("events stopped coming") {
            Incoming::Event(event) => {
                assert_eq!(event.seq, expected_seq, "event out of order or missing");
                expected_seq += 1;
                if let EventKind::RequestFinished { request_id, .. } = event.kind {
                    assert!(accepted.binary_search(&request_id).is_ok());
                    finished += 1;
                }
            }
            Incoming::Closed(why) => panic!("closed early: {why}"),
        }
    }
    core.stop();
}

#[test]
fn shutdown_answers_the_asker_tells_every_watcher_and_stops_the_core() {
    let ws = workspace("shutdown");
    let core = tracer_core::start(&ws).unwrap();
    let mut watcher = Client::connect(&ws, ClientKind::Cli, "watcher").unwrap();
    watcher.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let (tx, events) = mpsc::channel();
    let _watcher = watcher.into_shared(move |incoming| {
        let _ = tx.send(incoming);
    }).unwrap();

    let waiting = thread::spawn(move || core.wait());
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    assert_eq!(window.call(Command::Shutdown).unwrap(), Ok(ReplyBody::Stopping));

    // The watcher hears who stopped it.
    loop {
        match events.recv_timeout(Duration::from_secs(5)).expect("no core_stopping") {
            Incoming::Event(Event { kind: EventKind::CoreStopping, actor, .. }) => {
                assert_eq!(actor.name, "window");
                break;
            }
            Incoming::Event(_) => continue,
            Incoming::Closed(why) => panic!("closed before core_stopping: {why}"),
        }
    }
    // `wait` returns, and nothing new can find the core.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !waiting.is_finished() {
        assert!(std::time::Instant::now() < deadline, "the core did not stop");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!ws.join(DISCOVERY_PATH).exists(), "the discovery file was left behind");
    assert!(Client::connect(&ws, ClientKind::Cli, "late").is_err());
}

/// A server that sends its headers at once, then a body of `chunks` pieces
/// with `gap` between them. Gives its base URL.
fn trickle_server(chunks: usize, gap: Duration) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {chunks}\r\nConnection: close\r\n\r\n");
                for _ in 0..chunks {
                    thread::sleep(gap);
                    if stream.write_all(b"x").and_then(|_| stream.flush()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_cancelled_request_ends_once_for_everyone_and_says_who_cancelled_it() {
    let ws = workspace("cancel");
    let slow = trickle_server(40, Duration::from_millis(100));
    let fast = stub_server();
    let core = tracer_core::start(&ws).unwrap();

    let mut watcher = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    watcher.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let (tx, events) = mpsc::channel();
    let _watcher = watcher.into_shared(move |incoming| {
        let _ = tx.send(incoming);
    }).unwrap();

    // The CLI sends; an agent cancels it while the body is still arriving.
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer send").unwrap();
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("{slow}/big"))).unwrap() else {
        panic!("send was not accepted");
    };
    thread::sleep(Duration::from_millis(300));
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();
    assert_eq!(agent.call(Command::Cancel { request_id }).unwrap(), Ok(ReplyBody::Cancelled));
    // A second cancel is refused: it has already ended.
    assert!(agent.call(Command::Cancel { request_id }).unwrap().is_err());

    // The watcher sees it cancelled by the agent, and nothing after that for
    // this request, though the server would have finished in about 4 s.
    let mut cancelled = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Incoming::Event(event)) = events.recv_timeout(Duration::from_millis(200)) else { continue };
        match event.kind {
            EventKind::RequestCancelled { request_id: id } if id == request_id => {
                assert_eq!(event.actor.name, "claude-code");
                cancelled += 1;
            }
            EventKind::RequestFinished { request_id: id, .. } | EventKind::RequestFailed { request_id: id, .. }
                if id == request_id =>
            {
                panic!("a cancelled request also ended: {:?}", event.kind);
            }
            _ => {}
        }
    }
    assert_eq!(cancelled, 1);

    // A request that has finished can't be cancelled.
    let Ok(ReplyBody::Accepted { request_id: done }) = cli.call(get(format!("{fast}/a"))).unwrap() else {
        panic!("send was not accepted");
    };
    loop {
        if let Ok(Incoming::Event(Event { kind: EventKind::RequestFinished { request_id: id, .. }, .. })) =
            events.recv_timeout(Duration::from_secs(5))
        {
            if id == done {
                break;
            }
        }
    }
    let refused = agent.call(Command::Cancel { request_id: done }).unwrap();
    assert_eq!(refused, Err(format!("request #{done} is not running")));
    core.stop();
}

#[test]
fn a_core_with_a_cancelled_request_still_stops() {
    let ws = workspace("cancel-stop");
    let slow = trickle_server(100, Duration::from_millis(100));
    let core = tracer_core::start(&ws).unwrap();
    let waiting = thread::spawn(move || core.wait());

    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: Some(0) }).unwrap().unwrap();
    let window = window.into_shared(|_| {}).unwrap();
    let Ok(Ok(ReplyBody::Accepted { request_id })) = window.call(get(format!("{slow}/big"))) else {
        panic!("send was not accepted");
    };
    thread::sleep(Duration::from_millis(300));
    assert_eq!(window.call(Command::Cancel { request_id }).unwrap(), Ok(ReplyBody::Cancelled));
    assert_eq!(window.call(Command::Shutdown).unwrap(), Ok(ReplyBody::Stopping));

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !waiting.is_finished() {
        assert!(std::time::Instant::now() < deadline, "the core did not stop");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn saved_requests_are_plain_files_and_every_client_sees_them_change() {
    let ws = workspace("saved");
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    let request = HttpRequest {
        method: "POST".into(),
        url: "https://x.test/albums".into(),
        headers: vec![("Accept".into(), "application/json".into())],
        body: Some("{\"title\": \"t\"}".into()),
        ..Default::default()
    };
    let saved = agent.call(Command::SaveRequest { path: r"requests\albums\new album.http".into(), request: request.clone() });
    assert_eq!(saved.unwrap(), Ok(ReplyBody::Saved { path: "albums/new album".into() }));

    // A plain file anyone can read, diff or write.
    let text = std::fs::read_to_string(ws.join("requests/albums/new album.http")).unwrap();
    assert_eq!(text, "POST https://x.test/albums\nAccept: application/json\n\n{\"title\": \"t\"}\n");

    // The window sees who saved what.
    let event = loop {
        let e = window.next_event().unwrap();
        if matches!(e.kind, EventKind::RequestSaved { .. }) {
            break e;
        }
    };
    assert_eq!(event.actor.name, "claude-code");
    assert_eq!(event.kind, EventKind::RequestSaved { path: "albums/new album".into(), request: request.clone() });

    // A file written by hand is listed; one that doesn't parse is named.
    std::fs::write(ws.join("requests/health.http"), "# ping\nGET http://localhost:8080/health\n").unwrap();
    std::fs::write(ws.join("requests/albums/broken.http"), "not a request line\n").unwrap();
    let Ok(ReplyBody::Requests { requests, as_of, skipped }) = window.call(Command::ListRequests).unwrap() else {
        panic!("no list");
    };
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["albums/new album", "health"]);
    assert_eq!(requests[1].request.url, "http://localhost:8080/health");
    assert!(as_of > event.seq);
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].starts_with("albums/broken"), "{skipped:?}");

    // Deleting removes the file and says so; a second delete, or a path
    // outside requests/, is refused.
    std::fs::remove_file(ws.join("requests/albums/broken.http")).unwrap();
    assert_eq!(agent.call(Command::DeleteRequest { path: "albums/new album".into() }).unwrap(), Ok(ReplyBody::Deleted));
    assert!(!ws.join("requests/albums").exists(), "the emptied folder was left");
    assert!(agent.call(Command::DeleteRequest { path: "albums/new album".into() }).unwrap().is_err());
    assert!(agent.call(Command::SaveRequest { path: "../escape".into(), request }).unwrap().is_err());
    assert!(!ws.join("escape.http").exists());
    core.stop();
}

#[test]
fn a_request_a_dead_core_left_running_is_closed_when_the_next_one_opens() {
    let ws = workspace("unfinished");
    std::fs::create_dir_all(ws.join(".tracer")).unwrap();
    // What a core killed mid-request leaves: a start with no end.
    let started = r#"{"seq":1,"at_ms":1,"actor":{"kind":"agent","name":"claude-code"},"event":"request_started","request_id":7,"request":{"method":"GET","url":"http://x.test/"}}"#;
    std::fs::write(ws.join(".tracer/journal.ndjson"), format!("{started}\n")).unwrap();

    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: Some(0) }).unwrap().unwrap();
    let mut ended = None;
    for _ in 0..3 {
        let e = window.next_event().unwrap();
        if let EventKind::RequestFailed { request_id: 7, error } = &e.kind {
            ended = Some((e.seq, e.actor.name.clone(), error.clone()));
            break;
        }
    }
    let (seq, actor, error) = ended.expect("the unfinished request was not closed");
    assert_eq!((seq, actor.as_str()), (2, "claude-code"));
    assert!(error.contains("stopped"), "{error}");
    core.stop();

    // Only once: the next core finds it closed.
    let core = tracer_core::start(&ws).unwrap();
    core.stop();
    let journal = std::fs::read_to_string(ws.join(".tracer/journal.ndjson")).unwrap();
    let failures = journal.lines().filter(|l| l.contains("\"request_failed\"") && l.contains("\"request_id\":7")).count();
    assert_eq!(failures, 1, "{journal}");
}

#[test]
fn a_request_sent_with_an_environment_is_filled_in_and_the_journal_keeps_it_as_written() {
    let ws = workspace("envs");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    let env = tracer_protocol::Environment { name: "local".into(), vars: vec![("baseUrl".into(), base.clone()), ("id".into(), "7".into())], secrets: vec![] };
    assert_eq!(agent.call(Command::SaveEnvironment { env }).unwrap(), Ok(ReplyBody::EnvironmentSaved { name: "local".into() }));
    let text = std::fs::read_to_string(ws.join("environments/local.env")).unwrap();
    assert_eq!(text, format!("baseUrl={base}\nid=7\n"));

    let send = |url: &str| Command::Send {
        request: HttpRequest { method: "GET".into(), url: url.into(), headers: vec![], body: None, ..Default::default() },
        env: Some("local".into()),
    };
    let Ok(ReplyBody::Accepted { request_id: ok_id }) = agent.call(send("{{baseUrl}}/albums/{{ id }}")).unwrap() else { panic!() };
    let Ok(ReplyBody::Accepted { request_id: bad_id }) = agent.call(send("{{baseUrl}}/{{nope}}")).unwrap() else { panic!() };

    let (mut started, mut finished, mut failed) = (None, None, None);
    while finished.is_none() || failed.is_none() {
        let e = window.next_event().unwrap();
        match e.kind {
            EventKind::RequestStarted { request_id, request, env, .. } if request_id == ok_id => started = Some((request.url, env)),
            EventKind::RequestFinished { request_id, response } if request_id == ok_id => finished = Some(response.body),
            EventKind::RequestFailed { request_id, error } if request_id == bad_id => failed = Some(error),
            _ => {}
        }
    }
    // As written in the journal; filled in on the wire.
    assert_eq!(started.unwrap(), ("{{baseUrl}}/albums/{{ id }}".to_string(), Some("local".to_string())));
    assert_eq!(finished.unwrap(), r#"{"path":"/albums/7","got":0}"#);
    assert_eq!(failed.unwrap(), "{{nope}} not defined in environment `local`");

    let Ok(ReplyBody::Environments { environments, .. }) = window.call(Command::ListEnvironments).unwrap() else { panic!() };
    assert_eq!(environments.len(), 1);
    // Deleting one is a person's call (the guardrails), not an agent's.
    let delete = || Command::DeleteEnvironment { name: "local".into() };
    assert!(agent.call(delete()).unwrap().is_err());
    assert_eq!(window.call(delete()).unwrap(), Ok(ReplyBody::EnvironmentDeleted));
    assert!(!ws.join("environments/local.env").exists());
    core.stop();
}

/// Waits for request `id` to end; gives its `request_started` and how it ended.
fn ended(client: &mut Client, id: u64) -> (Event, EventKind) {
    let mut started = None;
    loop {
        let e = client.next_event().unwrap();
        match &e.kind {
            EventKind::RequestStarted { request_id, .. } if *request_id == id => started = Some(e),
            EventKind::RequestFinished { request_id, .. } | EventKind::RequestFailed { request_id, .. }
                if *request_id == id =>
            {
                return (started.expect("started before it ended"), e.kind);
            }
            _ => {}
        }
    }
}

#[test]
fn a_rerun_sends_the_request_as_written_with_its_environment_unless_told_otherwise() {
    let ws = workspace("rerun");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    // The CLI, not an agent: an agent's POST would wait for approval.
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    for (name, id) in [("a", "1"), ("b", "2")] {
        let vars = vec![("base".into(), base.clone()), ("id".into(), id.into())];
        let env = tracer_protocol::Environment { name: name.into(), vars, secrets: vec![] };
        cli.call(Command::SaveEnvironment { env }).unwrap().unwrap();
    }

    let request = HttpRequest {
        method: "POST".into(),
        url: "{{base}}/items/{{id}}".into(),
        headers: vec![("X-Id".into(), "{{id}}".into())],
        body: Some("abc".into()),
        ..Default::default()
    };
    let Ok(ReplyBody::Accepted { request_id: first }) =
        cli.call(Command::Send { request: request.clone(), env: Some("a".into()) }).unwrap()
    else {
        panic!("send was not accepted")
    };
    ended(&mut window, first);

    // Again, by someone else: as written, with the env it had.
    let rerun = |env: Option<&str>| Command::Rerun { request_id: first, env: env.map(String::from) };
    let Ok(ReplyBody::Accepted { request_id: again }) = window.call(rerun(None)).unwrap() else { panic!() };
    let (started, end) = ended(&mut window, again);
    assert_eq!(started.actor.name, "window");
    let EventKind::RequestStarted { request: r, env, rerun_of, .. } = started.kind else { unreachable!() };
    assert_eq!((r, env, rerun_of), (request.clone(), Some("a".to_string()), Some(first)));
    let EventKind::RequestFinished { response, .. } = end else { panic!("{end:?}") };
    assert_eq!(response.body, r#"{"path":"/items/1","got":3}"#);

    // With another env, and with none.
    let Ok(ReplyBody::Accepted { request_id: in_b }) = window.call(rerun(Some("b"))).unwrap() else { panic!() };
    let EventKind::RequestFinished { response, .. } = ended(&mut window, in_b).1 else { panic!() };
    assert_eq!(response.body, r#"{"path":"/items/2","got":3}"#);
    let Ok(ReplyBody::Accepted { request_id: bare }) = window.call(rerun(Some(""))).unwrap() else { panic!() };
    let EventKind::RequestFailed { error, .. } = ended(&mut window, bare).1 else { panic!() };
    assert!(error.contains("choose an environment"), "{error}");

    // A request that never was.
    let refused = window.call(Command::Rerun { request_id: 999, env: None }).unwrap();
    assert_eq!(refused, Err("there is no request #999 in the history".to_string()));
    core.stop();
}

#[test]
fn a_flow_step_sent_again_is_filled_with_the_captures_it_had() {
    let ws = workspace("flow-rerun");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    let step = |url: String, capture: Vec<String>| HttpRequest { method: "GET".into(), url, capture, ..Default::default() };
    let first = step(format!("{base}/first"), vec!["p = $.path".into()]);
    cli.call(Command::SaveRequest { path: "f/1 first".into(), request: first }).unwrap().unwrap();
    cli.call(Command::SaveRequest { path: "f/2 second".into(), request: step(format!("{base}/x{{{{p}}}}"), vec![]) }).unwrap().unwrap();

    cli.call(Command::RunFlow { path: "f".into(), env: None, keep_going: false, data: None }).unwrap().unwrap();
    let steps = wait_for(&mut window, |e| match &e.kind {
        EventKind::FlowFinished { passed: true, steps, .. } => Some(steps.clone()),
        EventKind::FlowFinished { steps, .. } => panic!("the flow failed: {steps:?}"),
        _ => None,
    });
    let second = steps[1].request_id.unwrap();

    // Sent again, the step has `p` as the flow had it; and so does a
    // request sent again from that one.
    let Ok(ReplyBody::Accepted { request_id: again }) = window.call(Command::Rerun { request_id: second, env: None }).unwrap() else { panic!() };
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, again) else { panic!() };
    assert_eq!(response.body, r#"{"path":"/x/first","got":0}"#);
    let Ok(ReplyBody::Accepted { request_id: twice }) = window.call(Command::Rerun { request_id: again, env: None }).unwrap() else { panic!() };
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, twice) else { panic!() };
    assert_eq!(response.body, r#"{"path":"/x/first","got":0}"#);

    // The captured value stays out of what the journal says was sent: the
    // URL as written. (The stub's answers echo it, as answers may.)
    let journal = std::fs::read_to_string(ws.join(tracer_protocol::JOURNAL_PATH)).unwrap();
    let sent: Vec<&str> = journal.lines().filter(|l| l.contains("request_started")).collect();
    assert_eq!(sent.len(), 4);
    assert!(sent.iter().all(|l| !l.contains("/x/first")), "a capture's value reached the journal");
    core.stop();
}

#[test]
fn a_data_driven_flow_runs_once_per_row_with_its_values() {
    let ws = workspace("flow-data");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    let step = |url: String, capture: Vec<String>| HttpRequest { method: "GET".into(), url, capture, ..Default::default() };
    cli.call(Command::SaveRequest { path: "d/1 user".into(), request: step(format!("{base}/users/{{{{id}}}}"), vec!["p = $.path".into()]) })
        .unwrap()
        .unwrap();
    cli.call(Command::SaveRequest { path: "d/2 again".into(), request: step(format!("{base}/again{{{{p}}}}?n={{{{name}}}}"), vec![]) })
        .unwrap()
        .unwrap();
    std::fs::create_dir_all(ws.join("data")).unwrap();
    std::fs::write(ws.join("data/users.csv"), "id,name\n7,Ada\n8,\"Grace,H\"\n").unwrap();

    let started = cli.call(Command::RunFlow { path: "d".into(), env: None, keep_going: false, data: Some("data/users.csv".into()) }).unwrap();
    let Ok(ReplyBody::FlowStarted { steps: names, .. }) = started else { panic!("{started:?}") };
    assert_eq!(names, ["row 1: d/1 user", "row 1: d/2 again", "row 2: d/1 user", "row 2: d/2 again"]);
    let steps = wait_for(&mut window, |e| match &e.kind {
        EventKind::FlowFinished { passed: true, steps, .. } => Some(steps.clone()),
        EventKind::FlowFinished { steps, .. } => panic!("the flow failed: {steps:?}"),
        _ => None,
    });
    assert_eq!(steps.iter().map(|s| s.row).collect::<Vec<_>>(), [Some(1), Some(1), Some(2), Some(2)]);
    let body = |cli: &mut Client, id: u64| match cli.call(Command::Response { request_id: id }).unwrap() {
        Ok(ReplyBody::Response { response: Some(r), .. }) => r.body,
        other => panic!("{other:?}"),
    };
    // Each row's values, and its own captures: row 2 doesn't see row 1's.
    assert_eq!(body(&mut cli, steps[1].request_id.unwrap()), r#"{"path":"/again/users/7?n=Ada","got":0}"#);
    assert_eq!(body(&mut cli, steps[3].request_id.unwrap()), r#"{"path":"/again/users/8?n=Grace,H","got":0}"#);

    let missing = cli.call(Command::RunFlow { path: "d".into(), env: None, keep_going: false, data: Some("../users.csv".into()) }).unwrap();
    assert!(missing.unwrap_err().contains("inside the workspace"));
    core.stop();
}

#[test]
fn a_mock_answers_with_each_saved_requests_last_answer() {
    let ws = workspace("mock");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    let get = |url: &str| HttpRequest { method: "GET".into(), url: url.into(), ..Default::default() };
    cli.call(Command::SaveRequest { path: "shop/item".into(), request: get("{{base}}/items/{{id}}") }).unwrap().unwrap();
    cli.call(Command::SaveRequest { path: "shop/new".into(), request: get("{{base}}/items/new") }).unwrap().unwrap();

    // One of them answered once, through an env and a variable.
    std::fs::create_dir_all(ws.join("environments")).unwrap();
    std::fs::write(ws.join("environments/local.env"), format!("base={base}\nid=7\n")).unwrap();
    let Ok(ReplyBody::Accepted { request_id }) =
        cli.call(Command::Send { request: get("{{base}}/items/{{id}}"), env: Some("local".into()) }).unwrap()
    else {
        panic!()
    };
    let EventKind::RequestFinished { .. } = end_of(&mut window, request_id) else { panic!() };

    let started = cli.call(Command::StartMock { path: "shop".into(), port: None }).unwrap();
    let Ok(ReplyBody::MockStarted { mock_id, url, routes, .. }) = started else { panic!("{started:?}") };
    assert_eq!(routes, ["GET /items/{id}", "GET /items/new"]);
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build());
    let mut answer = agent.get(&format!("{url}/items/99?x=1")).call().unwrap();
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.headers().get("access-control-allow-origin").unwrap(), "*");
    assert_eq!(answer.body_mut().read_to_string().unwrap(), r#"{"path":"/items/7","got":0}"#);
    // The literal route wins, and has no answer yet.
    let mut new = agent.get(&format!("{url}/items/new")).call().unwrap();
    assert_eq!(new.status(), 501);
    assert!(new.body_mut().read_to_string().unwrap().contains("requests/shop/new.http"));
    let mut none = agent.delete(&format!("{url}/items/1")).call().unwrap();
    assert_eq!(none.status(), 404);
    assert!(none.body_mut().read_to_string().unwrap().contains("GET /items/{id}"));
    let pre = agent.run(ureq::http::Request::builder().method("OPTIONS").uri(format!("{url}/items/1")).body(()).unwrap()).unwrap();
    assert_eq!(pre.status(), 204);

    let hits = wait_for(&mut window, |e| match &e.kind {
        EventKind::MockHit { status: 204, .. } => Some(()),
        _ => None,
    });
    let () = hits;
    cli.call(Command::StopMock { mock_id }).unwrap().unwrap();
    assert!(agent.get(&format!("{url}/items/1")).call().is_err() || {
        // The listener closes once it wakes; a connection racing it may get through.
        std::thread::sleep(std::time::Duration::from_millis(200));
        agent.get(&format!("{url}/items/1")).call().is_err()
    });
    assert!(cli.call(Command::StopMock { mock_id }).unwrap().is_err());
    core.stop();
}

/// An HTTP server answering with `statuses` in turn, then the last one
/// again. Gives its base URL.
fn status_server(statuses: Vec<u16>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let status = statuses[n.min(statuses.len() - 1)];
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                line.clear();
            }
            let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_run_until_stops_at_its_streak_in_a_row_or_its_most_or_when_stopped() {
    let ws = workspace("until");
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    let until = |url: String, plan: UntilPlan| Command::RunUntil {
        request: HttpRequest { method: "GET".into(), url, ..Default::default() },
        env: None,
        plan,
    };
    let finished = |client: &mut Client, id: u64| {
        wait_for(client, |e| match &e.kind {
            EventKind::UntilFinished { until_id, sent, streak, reached, stopped } if *until_id == id => {
                Some((*sent, *streak, *reached, stopped.clone()))
            }
            _ => None,
        })
    };

    // Four 401s in a row; the 500 between them starts the count again.
    let base = status_server(vec![200, 401, 401, 500, 401, 401, 401, 401, 200]);
    let plan = UntilPlan { streak: 4, matching: Some("status == 401".into()), ..Default::default() };
    let Ok(ReplyBody::UntilStarted { until_id }) = cli.call(until(base, plan)).unwrap() else { panic!() };
    let mut streaks = Vec::new();
    let end = wait_for(&mut window, |e| match &e.kind {
        EventKind::UntilProgress { until_id: id, streak, .. } if *id == until_id => {
            streaks.push(*streak);
            None
        }
        EventKind::UntilFinished { until_id: id, sent, reached, stopped, .. } if *id == until_id => {
            Some((*sent, *reached, stopped.clone()))
        }
        _ => None,
    });
    assert_eq!(streaks, vec![0, 1, 2, 0, 1, 2, 3, 4]);
    assert_eq!(end, (8, true, None));
    // Each send is in history.
    let Ok(ReplyBody::History { matched, .. }) = cli.call(Command::History { filter: Default::default() }).unwrap() else { panic!() };
    assert_eq!(matched, 8);

    // Never failing, it stops at its most.
    let base = status_server(vec![200]);
    let plan = UntilPlan { streak: 2, max: Some(3), ..Default::default() };
    let Ok(ReplyBody::UntilStarted { until_id }) = cli.call(until(base, plan)).unwrap() else { panic!() };
    let (sent, streak, reached, stopped) = finished(&mut window, until_id);
    assert_eq!((sent, streak, reached), (3, 0, false));
    assert!(stopped.unwrap().contains("sent 3, its most"));

    // Failing but slowly, a person stops it between sends.
    let base = status_server(vec![500]);
    let plan = UntilPlan { streak: 100, gap_ms: Some(10_000), ..Default::default() };
    let Ok(ReplyBody::UntilStarted { until_id }) = cli.call(until(base, plan)).unwrap() else { panic!() };
    wait_for(&mut window, |e| matches!(&e.kind, EventKind::UntilProgress { until_id: id, .. } if *id == until_id).then_some(()));
    assert_eq!(window.call(Command::StopUntil { until_id }).unwrap(), Ok(ReplyBody::UntilStopped));
    let (sent, streak, reached, stopped) = finished(&mut window, until_id);
    assert_eq!((sent, streak, reached, stopped.as_deref()), (1, 1, false, Some("stopped by window")));
    assert!(window.call(Command::StopUntil { until_id }).unwrap().is_err());

    // A plan it can't run is refused up front.
    let plan = UntilPlan { streak: 2, matching: Some("stauts 401".into()), ..Default::default() };
    assert!(cli.call(until("http://127.0.0.1:1".into(), plan)).unwrap().unwrap_err().contains("can't read"));
    core.stop();
}

/// A server for browser behaviour: `/login` sets a cookie, `/me` answers
/// with the `Cookie` it was sent, `OPTIONS /api` allows PUT and X-Token
/// from https://app.test, `PUT /api` allows that origin, and `/open`
/// allows none. Gives its base URL.
fn browser_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let mut cookie = String::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("cookie:") {
                    cookie = v.trim().to_string();
                }
                line.clear();
            }
            let mut words = first.split_whitespace();
            let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let allow = "Access-Control-Allow-Origin: https://app.test\r\n";
            let (head, body) = match (method, path) {
                (_, "/login") => ("Set-Cookie: sid=abc; Path=/; HttpOnly\r\n".to_string(), String::new()),
                (_, "/me") => (String::new(), cookie),
                ("OPTIONS", "/api") => (format!("{allow}Access-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Token\r\n"), String::new()),
                ("PUT", "/api") => (allow.to_string(), "put".into()),
                _ => (String::new(), "open".into()),
            };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\n{head}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn browser_behaviour_keeps_cookies_and_preflights_as_a_browser_would_once_switched_on() {
    let ws = workspace("browser");
    let base = browser_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "agent").unwrap();
    let send = |client: &mut Client, method: &str, path: &str, headers: Vec<(String, String)>| {
        let request = HttpRequest { method: method.into(), url: format!("{base}{path}"), headers, ..Default::default() };
        let Ok(ReplyBody::Accepted { request_id }) = client.call(Command::Send { request, env: None }).unwrap() else { panic!() };
        request_id
    };

    // Off by default: a cookie isn't kept.
    let id = send(&mut window, "GET", "/login", vec![]);
    end_of(&mut window, id);
    let id = send(&mut window, "GET", "/me", vec![]);
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, id) else { panic!() };
    assert_eq!(response.body, "");

    // On, it's kept and sent back; an agent may not switch it, nor see values.
    let on = tracer_protocol::Browser { cookies: true, origin: None, unchecked_tls: vec![], jar_per_env: false };
    assert!(agent.call(Command::SetBrowser { browser: on.clone() }).unwrap().is_err());
    assert_eq!(window.call(Command::SetBrowser { browser: on }).unwrap(), Ok(ReplyBody::BrowserSet));
    let id = send(&mut window, "GET", "/login", vec![]);
    end_of(&mut window, id);
    let id = send(&mut window, "GET", "/me", vec![]);
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, id) else { panic!() };
    assert_eq!(response.body, "sid=abc");
    let Ok(ReplyBody::Cookies { cookies, .. }) = window.call(Command::Cookies).unwrap() else { panic!() };
    assert_eq!((cookies[0].name.as_str(), cookies[0].value.as_deref(), cookies[0].http_only), ("sid", Some("abc"), true));
    let Ok(ReplyBody::Cookies { cookies, .. }) = agent.call(Command::Cookies).unwrap() else { panic!() };
    assert_eq!(cookies[0].value, None);
    // Kept out of the journal: sent as written, and the jar's event has no values.
    let journal = std::fs::read_to_string(ws.join(tracer_protocol::JOURNAL_PATH)).unwrap();
    assert!(journal.lines().filter(|l| !l.contains("request_finished")).all(|l| !l.contains("abc")), "a cookie's value reached the journal");
    assert_eq!(window.call(Command::ClearCookies { domain: None, env: None }).unwrap(), Ok(ReplyBody::CookiesCleared { removed: 1 }));

    // A jar per environment: a login with `a` isn't sent with `b`, or with none.
    std::fs::create_dir_all(ws.join("environments")).unwrap();
    for name in ["a", "b"] {
        std::fs::write(ws.join(format!("environments/{name}.env")), "x=1\n").unwrap();
    }
    let per_env = tracer_protocol::Browser { cookies: true, origin: None, unchecked_tls: vec![], jar_per_env: true };
    window.call(Command::SetBrowser { browser: per_env }).unwrap().unwrap();
    let send_in = |client: &mut Client, path: &str, env: Option<&str>| {
        let request = HttpRequest { method: "GET".into(), url: format!("{base}{path}"), ..Default::default() };
        let Ok(ReplyBody::Accepted { request_id }) = client.call(Command::Send { request, env: env.map(String::from) }).unwrap() else { panic!() };
        request_id
    };
    let id = send_in(&mut window, "/login", Some("a"));
    end_of(&mut window, id);
    for (env, want) in [(Some("a"), "sid=abc"), (Some("b"), ""), (None, "")] {
        let id = send_in(&mut window, "/me", env);
        let EventKind::RequestFinished { response, .. } = end_of(&mut window, id) else { panic!() };
        assert_eq!(response.body, want, "sent with {env:?}");
    }
    let Ok(ReplyBody::Cookies { cookies, .. }) = window.call(Command::Cookies).unwrap() else { panic!() };
    assert_eq!(cookies.iter().map(|c| c.env.as_deref()).collect::<Vec<_>>(), [Some("a")]);
    assert_eq!(window.call(Command::ClearCookies { domain: None, env: Some("b".into()) }).unwrap(), Ok(ReplyBody::CookiesCleared { removed: 0 }));
    assert_eq!(window.call(Command::ClearCookies { domain: None, env: Some("a".into()) }).unwrap(), Ok(ReplyBody::CookiesCleared { removed: 1 }));

    // With a page's origin: PUT with X-Token is preflighted (a request of
    // its own), allowed, and its answer passes the CORS check.
    let page = tracer_protocol::Browser { cookies: false, origin: Some("HTTPS://app.test:443/index.html".into()), unchecked_tls: vec![], jar_per_env: false };
    window.call(Command::SetBrowser { browser: page }).unwrap().unwrap();
    let Ok(ReplyBody::Cookies { browser, .. }) = window.call(Command::Cookies).unwrap() else { panic!() };
    assert_eq!(browser.origin.as_deref(), Some("https://app.test"));
    let id = send(&mut window, "PUT", "/api", vec![("X-Token".into(), "1".into())]);
    let preflight = id + 1;
    let (started, _) = ended(&mut window, preflight);
    let EventKind::RequestStarted { request, .. } = started.kind else { panic!() };
    assert_eq!(request.method, "OPTIONS");
    assert!(request.headers.contains(&("Access-Control-Request-Headers".into(), "x-token".into())));
    let checks = wait_for(&mut window, |e| match &e.kind {
        EventKind::RequestChecked { request_id, results } if *request_id == id => Some(results.clone()),
        _ => None,
    });
    assert!(checks[0].pass, "{checks:?}");

    // A header the preflight doesn't allow: blocked, nothing sent.
    let id = send(&mut window, "PUT", "/api", vec![("X-Other".into(), "1".into())]);
    let EventKind::RequestFailed { error, .. } = end_of(&mut window, id) else { panic!() };
    assert!(error.contains("blocked by CORS") && error.contains("x-other"), "{error}");
    // A simple GET goes without a preflight, but a browser keeps the answer from the page.
    let id = send(&mut window, "GET", "/open", vec![]);
    let checks = wait_for(&mut window, |e| match &e.kind {
        EventKind::RequestChecked { request_id, results } if *request_id == id => Some(results.clone()),
        EventKind::RequestStarted { request, .. } if request.method == "OPTIONS" => panic!("a simple GET was preflighted"),
        _ => None,
    });
    assert!(!checks[0].pass && checks[0].actual.contains("no Access-Control-Allow-Origin"), "{checks:?}");
    core.stop();
}

#[test]
fn history_gives_the_newest_first_filtered_and_survives_a_restart() {
    use tracer_protocol::{Ended, HistoryFilter, StatusClass};
    let ws = workspace("history");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    cli.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    let mut ids = Vec::new();
    for (who, path) in [("cli", "/albums/1"), ("agent", "/photos/1"), ("cli", "/albums/2")] {
        let sender = if who == "cli" { &mut cli } else { &mut agent };
        let Ok(ReplyBody::Accepted { request_id }) = sender.call(get(format!("{base}{path}"))).unwrap() else { panic!() };
        ids.push(request_id);
    }
    // One that fails: nothing listens on port 9.
    let Ok(ReplyBody::Accepted { request_id: dead }) = cli.call(get("http://127.0.0.1:9/".into())).unwrap() else { panic!() };
    let mut open: Vec<u64> = ids.iter().copied().chain([dead]).collect();
    while !open.is_empty() {
        if let EventKind::RequestFinished { request_id, .. } | EventKind::RequestFailed { request_id, .. } =
            cli.next_event().unwrap().kind
        {
            open.retain(|id| *id != request_id);
        }
    }

    let history = |client: &mut Client, filter: HistoryFilter| {
        let Ok(ReplyBody::History { entries, matched, .. }) = client.call(Command::History { filter }).unwrap() else {
            panic!()
        };
        (entries.iter().map(|e| e.request_id).collect::<Vec<_>>(), matched, entries)
    };
    let all = HistoryFilter::default();
    let (order, matched, entries) = history(&mut cli, all.clone());
    assert_eq!(order, vec![dead, ids[2], ids[1], ids[0]]);
    assert_eq!(matched, 4);
    assert!(matches!(entries[0].ended, Ended::Failed { .. }));
    assert!(matches!(entries[1].ended, Ended::Done { status: 200, .. }), "{:?}", entries[1].ended);

    let text = |t: &str| HistoryFilter { text: Some(t.into()), ..HistoryFilter::default() };
    assert_eq!(history(&mut cli, text("albums")).0, vec![ids[2], ids[0]]);
    let agents = HistoryFilter { actor: Some(ClientKind::Agent), ..HistoryFilter::default() };
    assert_eq!(history(&mut cli, agents).0, vec![ids[1]]);
    let failed = HistoryFilter { status: Some(StatusClass::Failed), ..HistoryFilter::default() };
    assert_eq!(history(&mut cli, failed).0, vec![dead]);
    let (newest, matched, _) = history(&mut cli, HistoryFilter { limit: Some(2), ..HistoryFilter::default() });
    assert_eq!((newest, matched), (vec![dead, ids[2]], 4));

    // A fresh core reads the same history from the journal.
    drop((cli, agent));
    core.stop();
    let core = tracer_core::start(&ws).unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer").unwrap();
    assert_eq!(history(&mut cli, all).0, vec![dead, ids[2], ids[1], ids[0]]);
    core.stop();
}

#[test]
fn files_changed_behind_the_core_are_published_once_and_its_own_saves_are_not_echoed() {
    let ws = workspace("disk");
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let (tx, events) = mpsc::channel();
    let window = window.into_shared(move |incoming| {
        if let Incoming::Event(e) = incoming {
            let _ = tx.send(e);
        }
    }).unwrap();
    // Everything that arrives within `secs`.
    let collect = |secs: u64| {
        let mut got = Vec::new();
        let until = std::time::Instant::now() + Duration::from_secs(secs);
        while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
            if let Ok(e) = events.recv_timeout(left) {
                got.push(e);
            }
        }
        got
    };

    // The core's own save: one event, from the window, none from disk.
    let request = HttpRequest { method: "GET".into(), url: "http://x.test/a".into(), headers: vec![], body: None, ..Default::default() };
    window.call(Command::SaveRequest { path: "a".into(), request }).unwrap().unwrap();
    let got = collect(3);
    let saves: Vec<&Event> = got.iter().filter(|e| matches!(e.kind, EventKind::RequestSaved { .. })).collect();
    assert_eq!(saves.len(), 1, "{got:?}");
    assert_eq!(saves[0].actor.kind, ClientKind::Window);

    // Half a file, as an editor might leave it for a moment: nothing yet.
    std::fs::write(ws.join("requests/b.http"), "GE").unwrap();
    assert!(collect(2).is_empty());
    // Finished, then an environment written by hand, then a delete.
    std::fs::write(ws.join("requests/b.http"), "GET http://x.test/b\n").unwrap();
    std::fs::create_dir_all(ws.join("environments")).unwrap();
    std::fs::write(ws.join("environments/local.env"), "baseUrl=http://localhost\n").unwrap();
    std::fs::remove_file(ws.join("requests/a.http")).unwrap();
    let got = collect(3);
    let kinds: Vec<String> = got.iter().map(|e| format!("{:?}:{:?}", e.actor.kind, e.kind)).collect();
    assert!(got.iter().all(|e| e.actor.kind == ClientKind::Disk), "{kinds:?}");
    assert!(got.iter().any(|e| matches!(&e.kind, EventKind::RequestSaved { path, request } if path == "b" && request.url == "http://x.test/b")), "{kinds:?}");
    assert!(got.iter().any(|e| matches!(&e.kind, EventKind::RequestDeleted { path } if path == "a")), "{kinds:?}");
    assert!(got.iter().any(|e| matches!(&e.kind, EventKind::EnvironmentSaved { env } if env.name == "local")), "{kinds:?}");
    assert_eq!(got.len(), 3, "{kinds:?}");
    core.stop();
}

#[test]
fn a_secret_fills_in_but_its_value_never_reaches_a_client_or_the_journal() {
    let ws = workspace("secrets");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();
    const VALUE: &str = "tok-9f8e7d6c5b4a";

    let env = tracer_protocol::Environment { name: "prod".into(), vars: vec![("baseUrl".into(), base)], secrets: vec![] };
    agent.call(Command::SaveEnvironment { env }).unwrap().unwrap();
    let set = Command::SetSecret { env: "prod".into(), name: "token".into(), value: Some(VALUE.into()) };
    assert_eq!(window.call(set).unwrap(), Ok(ReplyBody::SecretSet));

    // Listed by name only.
    let listed = agent.call(Command::ListEnvironments).unwrap();
    let Ok(ReplyBody::Environments { environments, .. }) = &listed else { panic!() };
    assert_eq!(environments[0].secrets, ["token"]);
    assert!(!format!("{listed:?}").contains(VALUE));

    // Sent with it: the stub echoes the path, so the value comes back in the
    // body, and is replaced by its name before anyone sees it.
    let send = Command::Send {
        request: HttpRequest { method: "GET".into(), url: "{{baseUrl}}/me?key={{token}}".into(), headers: vec![], body: None, ..Default::default() },
        env: Some("prod".into()),
    };
    let Ok(ReplyBody::Accepted { request_id }) = agent.call(send).unwrap() else { panic!() };
    // An agent using `prod` waits for a person.
    wait_for(&mut window, |e| matches!(e.kind, EventKind::ApprovalAsked { request_id: id, .. } if id == request_id).then_some(()));
    window.call(Command::Decide { request_id, allow: true }).unwrap().unwrap();
    let body = loop {
        if let EventKind::RequestFinished { request_id: id, response } = window.next_event().unwrap().kind {
            if id == request_id {
                break response.body;
            }
        }
    };
    assert_eq!(body, r#"{"path":"/me?key={{token}}","got":0}"#);

    let journal = std::fs::read_to_string(ws.join(".tracer/journal.ndjson")).unwrap();
    assert!(journal.contains("secret_set"));
    assert!(!journal.contains(VALUE), "the journal holds the secret");
    assert!(std::fs::read_to_string(ws.join(".tracer/secrets/prod.env")).unwrap().contains(VALUE));
    assert!(std::fs::read_to_string(ws.join(".tracer/.gitignore")).unwrap().lines().any(|l| l == "*"));
    assert!(!std::fs::read_to_string(ws.join("environments/prod.env")).unwrap().contains(VALUE));

    // Removing it, then a removal of what isn't there, which is refused.
    let rm = || Command::SetSecret { env: "prod".into(), name: "token".into(), value: None };
    assert_eq!(window.call(rm()).unwrap(), Ok(ReplyBody::SecretSet));
    assert!(window.call(rm()).unwrap().is_err());
    assert!(!ws.join(".tracer/secrets/prod.env").exists());
    core.stop();
}

/// How request `id` ended, its start already seen or not.
fn end_of(client: &mut Client, id: u64) -> EventKind {
    wait_for(client, |e| match &e.kind {
        EventKind::RequestFinished { request_id, .. }
        | EventKind::RequestFailed { request_id, .. }
        | EventKind::RequestCancelled { request_id }
            if *request_id == id =>
        {
            Some(e.kind.clone())
        }
        _ => None,
    })
}

/// The next event `pick` takes, skipping the rest.
fn wait_for<T>(client: &mut Client, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
    loop {
        let e = client.next_event().unwrap();
        if let Some(found) = pick(&e) {
            return found;
        }
    }
}

fn post(url: String) -> Command {
    Command::Send { request: HttpRequest { method: "POST".into(), url, headers: vec![], body: Some("x".into()), ..Default::default() }, env: None }
}

#[test]
fn an_agents_post_waits_for_a_person_who_allows_or_refuses_it() {
    use tracer_protocol::Ended;
    let ws = workspace("approve");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    // A GET goes straight through.
    let Ok(ReplyBody::Accepted { request_id: get_id }) = agent.call(get(format!("{base}/a"))).unwrap() else { panic!() };
    assert!(matches!(ended(&mut window, get_id).1, EventKind::RequestFinished { .. }));

    // A POST waits, and history and `Response` say so.
    let Ok(ReplyBody::Accepted { request_id: first }) = agent.call(post(format!("{base}/b"))).unwrap() else { panic!() };
    let reason = wait_for(&mut window, |e| match &e.kind {
        EventKind::ApprovalAsked { request_id, reason } if *request_id == first => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(reason, "POST from an agent");
    let Ok(ReplyBody::Response { entry, response, .. }) = window.call(Command::Response { request_id: first }).unwrap() else {
        panic!()
    };
    assert_eq!((entry.ended, response), (Ended::AwaitingApproval { reason }, None));

    // An agent can't answer it; a person can.
    assert!(agent.call(Command::Decide { request_id: first, allow: true }).unwrap().is_err());
    assert_eq!(window.call(Command::Decide { request_id: first, allow: true }).unwrap(), Ok(ReplyBody::Decided));
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, first) else { panic!() };
    assert_eq!(response.body, r#"{"path":"/b","got":1}"#);
    let Ok(ReplyBody::Response { response: Some(full), .. }) = agent.call(Command::Response { request_id: first }).unwrap()
    else {
        panic!()
    };
    assert_eq!(full.body, response.body);

    // Refused, it fails naming who refused it, and nothing was sent.
    let Ok(ReplyBody::Accepted { request_id: second }) = agent.call(post(format!("{base}/c"))).unwrap() else { panic!() };
    wait_for(&mut window, |e| matches!(e.kind, EventKind::ApprovalAsked { request_id, .. } if request_id == second).then_some(()));
    window.call(Command::Decide { request_id: second, allow: false }).unwrap().unwrap();
    let EventKind::RequestFailed { error, .. } = end_of(&mut window, second) else { panic!() };
    assert_eq!(error, "refused by you");
    assert!(window.call(Command::Decide { request_id: second, allow: true }).unwrap().is_err(), "answered once");

    // Cancelled while it waits, it ends once.
    let Ok(ReplyBody::Accepted { request_id: third }) = agent.call(post(format!("{base}/d"))).unwrap() else { panic!() };
    wait_for(&mut window, |e| matches!(e.kind, EventKind::ApprovalAsked { request_id, .. } if request_id == third).then_some(()));
    assert_eq!(agent.call(Command::Cancel { request_id: third }).unwrap(), Ok(ReplyBody::Cancelled));
    wait_for(&mut window, |e| matches!(e.kind, EventKind::RequestCancelled { request_id } if request_id == third).then_some(()));
    assert!(window.call(Command::Decide { request_id: third, allow: true }).unwrap().is_err());

    // A person's own POST never asks.
    let Ok(ReplyBody::Accepted { request_id: mine }) = window.call(post(format!("{base}/e"))).unwrap() else { panic!() };
    assert!(matches!(ended(&mut window, mine).1, EventKind::RequestFinished { .. }));
    core.stop();
}

#[test]
fn the_kill_switch_stops_agents_cancels_their_requests_and_outlives_the_core() {
    let ws = workspace("kill");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    let Ok(ReplyBody::Accepted { request_id: waiting }) = agent.call(post(format!("{base}/x"))).unwrap() else { panic!() };
    wait_for(&mut window, |e| matches!(e.kind, EventKind::ApprovalAsked { request_id, .. } if request_id == waiting).then_some(()));

    assert!(agent.call(Command::SetAgents { allowed: false }).unwrap().is_err(), "not an agent's to throw");
    window.call(Command::SetAgents { allowed: false }).unwrap().unwrap();
    let who = wait_for(&mut window, |e| match e.kind {
        EventKind::RequestCancelled { request_id } if request_id == waiting => Some(e.actor.name.clone()),
        _ => None,
    });
    assert_eq!(who, "you");
    let refused = agent.call(get(format!("{base}/y"))).unwrap().unwrap_err();
    assert!(refused.contains("switched off"), "{refused}");
    assert!(agent.call(Command::ListRequests).unwrap().is_ok(), "reading is still allowed");
    let secret = Command::SetSecret { env: "a".into(), name: "b".into(), value: Some("c".into()) };
    assert!(agent.call(secret).unwrap().unwrap_err().contains("for a person"));

    // Still off after a restart, until a person switches it back.
    drop((window, agent));
    core.stop();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();
    let Ok(ReplyBody::Status { agents_allowed, .. }) = window.call(Command::Status).unwrap() else { panic!() };
    assert!(!agents_allowed);
    assert!(agent.call(get(format!("{base}/y"))).unwrap().is_err());
    window.call(Command::SetAgents { allowed: true }).unwrap().unwrap();
    assert!(matches!(agent.call(get(format!("{base}/y"))).unwrap(), Ok(ReplyBody::Accepted { .. })));
    core.stop();
}

/// A server that answers everything with a redirect to `to`. Gives its base
/// URL.
fn redirect_server(to: &'static str) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut line = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
            let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn the_workspace_policy_holds_each_agent_to_its_rules_and_is_followed_as_it_changes() {
    use tracer_protocol::{Access, POLICY_PATH};
    let ws = workspace("policy");
    let base = stub_server();
    let bounce = redirect_server("http://example.invalid/");
    let policy = "[*]\naccess = read-only\n\n[claude-code]\naccess = local\napprove = none\nrate = 5/min\nmax_body = 4\n";
    std::fs::write(ws.join(POLICY_PATH), policy).unwrap();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut claude = Client::connect(&ws, ClientKind::Agent, "Claude-Code").unwrap();
    let mut other = Client::connect(&ws, ClientKind::Agent, "cursor").unwrap();

    // Anyone unnamed may only read, and is told its own rules.
    assert!(other.call(get(format!("{base}/a"))).unwrap().unwrap_err().contains("only read"));
    let Ok(ReplyBody::Policy { rules, error: None }) = other.call(Command::Policy).unwrap() else { panic!() };
    assert_eq!((rules.len(), rules[0].agent.as_str(), rules[0].access), (1, "*", Access::ReadOnly));
    let Ok(ReplyBody::Policy { rules, .. }) = window.call(Command::Policy).unwrap() else { panic!() };
    assert_eq!(rules.len(), 2, "a person sees every rule");

    // Claude, by name whatever its case: a POST with nothing to approve.
    let send = |client: &mut Client, method: &str, url: String, body: &str| {
        let request = HttpRequest { method: method.into(), url, headers: vec![], body: Some(body.into()), ..Default::default() };
        let Ok(ReplyBody::Accepted { request_id }) = client.call(Command::Send { request, env: None }).unwrap() else {
            panic!("not accepted")
        };
        request_id
    };
    let id = send(&mut claude, "POST", format!("{base}/b"), "x");
    assert!(matches!(end_of(&mut window, id), EventKind::RequestFinished { .. }));
    // Off the machine, it fails visibly, before anyone is asked.
    let id = send(&mut claude, "GET", "https://example.com/".into(), "");
    let EventKind::RequestFailed { error, .. } = end_of(&mut window, id) else { panic!() };
    assert!(error.starts_with("refused by the workspace's policy: example.com is not this machine"), "{error}");
    // Too big a body.
    let id = send(&mut claude, "POST", format!("{base}/c"), "12345");
    let EventKind::RequestFailed { error, .. } = end_of(&mut window, id) else { panic!() };
    assert!(error.contains("its body is 5 bytes; the policy allows 4"), "{error}");
    // A redirect off the machine is the answer, not followed.
    let id = send(&mut claude, "GET", format!("{bounce}/d"), "");
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, id) else { panic!() };
    assert_eq!(response.status, 302);
    // The fifth in the minute goes; the sixth is refused, unnumbered.
    send(&mut claude, "GET", format!("{base}/e"), "");
    let refused = claude.call(get(format!("{base}/f"))).unwrap().unwrap_err();
    assert!(refused.contains("start 5 requests every 60 s"), "{refused}");

    // Changed on disk, it's followed and published.
    std::fs::write(ws.join(POLICY_PATH), "[*]\naccess = anywhere\n").unwrap();
    let (rules, error) = wait_for(&mut window, |e| match &e.kind {
        EventKind::PolicyChanged { rules, error } => Some((rules.clone(), error.clone())),
        _ => None,
    });
    assert_eq!((rules[0].access, rules.len(), error), (Access::Anywhere, 1, None));
    assert!(other.call(get(format!("{base}/g"))).unwrap().is_ok());
    // A mistake keeps the rules there were, and says so.
    std::fs::write(ws.join(POLICY_PATH), "[*]\naccess = everywhere\n").unwrap();
    let (rules, error) = wait_for(&mut window, |e| match &e.kind {
        EventKind::PolicyChanged { rules, error } => Some((rules.clone(), error.clone())),
        _ => None,
    });
    assert_eq!(rules[0].access, Access::Anywhere);
    assert!(error.unwrap().contains("line 2"));

    // A core that starts on a mistake lets agents only read.
    drop((window, claude, other));
    core.stop();
    let core = tracer_core::start(&ws).unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "cursor").unwrap();
    assert!(agent.call(get(format!("{base}/h"))).unwrap().unwrap_err().contains("only read"));
    let Ok(ReplyBody::Policy { error: Some(error), .. }) = agent.call(Command::Policy).unwrap() else { panic!() };
    assert!(error.starts_with("agents.policy: line 2"), "{error}");
    core.stop();
}

/// One raw HTTP call to the core; gives the status and the body.
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    let status = answer.split(' ').nth(1).unwrap().parse().unwrap();
    (status, answer.split_once("\r\n\r\n").unwrap().1.to_string())
}

fn ws_send(s: &mut TcpStream, text: &str) {
    let mask = [7u8, 1, 200, 33];
    let mut frame = vec![0x81];
    match text.len() {
        n if n < 126 => frame.push(0x80 | n as u8),
        n => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(text.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    s.write_all(&frame).unwrap();
}

fn ws_read(s: &mut TcpStream) -> (u8, String) {
    let mut two = [0u8; 2];
    s.read_exact(&mut two).unwrap();
    let len = match two[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            s.read_exact(&mut b).unwrap();
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            s.read_exact(&mut b).unwrap();
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut payload = vec![0; len];
    s.read_exact(&mut payload).unwrap();
    (two[0] & 0x0F, String::from_utf8(payload).unwrap())
}

#[test]
fn the_local_api_runs_a_command_over_http_and_the_protocol_over_a_websocket() {
    let ws = workspace("api");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let (port, token) = (core.port, core.token.clone());
    let bearer = format!("Bearer {token}");
    let host = ("Host", "127.0.0.1");

    // One command, as an agent unless told otherwise.
    let (status, body) = http(port, "POST", "/v1/command", &[host, ("Authorization", &bearer)], r#"{"cmd":"status"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""reply":"status""#) && body.contains(r#""kind":"agent","name":"http""#), "{body}");
    let as_ci = [host, ("Authorization", &bearer), ("X-Tracer-Kind", "cli"), ("X-Tracer-Name", "ci")];
    let send = format!(r#"{{"cmd":"send","request":{{"method":"POST","url":"{base}/x","body":"hi"}}}}"#);
    let (status, body) = http(port, "POST", "/v1/command", &as_ci, &send);
    assert_eq!((status, body.as_str()), (200, r#"{"reply":"accepted","request_id":1}"#));
    // An agent's is held to the guardrails, and refusals are 4xx.
    let (status, body) =
        http(port, "POST", "/v1/command", &[host, ("Authorization", &bearer)], r#"{"cmd":"set_agents","allowed":false}"#);
    assert_eq!(status, 400);
    assert!(body.contains("for a person"), "{body}");

    // Kept out: no token, a wrong one, another host, another site's page.
    let status_of = |headers: &[(&str, &str)]| http(port, "POST", "/v1/command", headers, r#"{"cmd":"status"}"#).0;
    assert_eq!(status_of(&[host]), 401);
    assert_eq!(status_of(&[host, ("Authorization", "Bearer nope")]), 401);
    assert_eq!(status_of(&[("Host", "evil.example:80"), ("Authorization", &bearer)]), 403);
    assert_eq!(status_of(&[host, ("Origin", "https://evil.example"), ("Authorization", &bearer)]), 403);
    assert_eq!(status_of(&[host, ("Origin", "http://localhost:3000"), ("Authorization", &bearer)]), 200);
    assert_eq!(http(port, "GET", "/", &[host], "").0, 404);

    // The WebSocket: the protocol as it is, a message a frame.
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let upgrade = "GET /v1/ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    s.write_all(upgrade.as_bytes()).unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 101") && head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{head}");
    ws_send(&mut s, &format!(r#"{{"id":1,"cmd":"hello","protocol":1,"token":"{token}","kind":"window","name":"web"}}"#));
    assert!(ws_read(&mut s).1.contains(r#""reply":"welcome""#));
    ws_send(&mut s, r#"{"id":2,"cmd":"subscribe","since":0}"#);
    assert!(ws_read(&mut s).1.contains(r#""reply":"subscribed""#));
    // The replay, including the HTTP caller's send and how it ended.
    let mut seen = Vec::new();
    while !seen.iter().any(|e: &String| e.contains(r#""event":"request_finished""#)) {
        let (opcode, text) = ws_read(&mut s);
        assert_eq!(opcode, 1);
        seen.push(text);
    }
    assert!(seen.iter().any(|e| e.contains(r#""name":"ci""#) && e.contains("request_started")));
    // A ping is answered, and a long message arrives whole.
    s.write_all(&[0x89, 0x80, 0, 0, 0, 0]).unwrap();
    loop {
        let (opcode, _) = ws_read(&mut s);
        if opcode == 0xA {
            break;
        }
    }
    let long = format!(r#"{{"id":3,"cmd":"send","request":{{"method":"GET","url":"{base}/{}"}}}}"#, "y".repeat(300));
    ws_send(&mut s, &long);
    loop {
        let (_, text) = ws_read(&mut s);
        if text.contains(r#""reply":"accepted""#) {
            assert!(text.contains(r#""id":3"#));
            break;
        }
    }
    // Closed, it's answered and the client leaves.
    s.write_all(&[0x88, 0x80, 0, 0, 0, 0]).unwrap();
    loop {
        let (opcode, _) = ws_read(&mut s);
        if opcode == 0x8 {
            break;
        }
    }
    core.stop();
}

#[test]
fn a_finished_request_says_where_its_time_went() {
    let ws = workspace("timing");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "test").unwrap();
    cli.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("{base}/t"))).unwrap() else { panic!() };
    let EventKind::RequestFinished { response, .. } = end_of(&mut cli, request_id) else { panic!() };
    let t = response.timing;
    let (dns, connect) = (t.dns_ms.expect("looked up"), t.connect_ms.expect("connected"));
    assert_eq!(t.tls_ms, None, "plain http has no handshake");
    assert!(dns >= 0.0 && connect >= 0.0 && t.wait_ms() >= 0.0);
    assert!(dns + connect <= t.ttfb_ms && t.ttfb_ms <= t.total_ms, "{t:?}");
    core.stop();
}

#[test]
fn two_responses_are_compared_by_value_and_an_agent_may_ask() {
    use tracer_protocol::{BodyDiff, ChangeKind};
    let ws = workspace("diff");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "test").unwrap();
    cli.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut ids = Vec::new();
    for path in ["a", "b"] {
        let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("{base}/{path}"))).unwrap() else { panic!() };
        end_of(&mut cli, request_id);
        ids.push(request_id);
    }
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();
    let ask = |a, b, ignore: &[&str]| Command::Diff { a, b, ignore: ignore.iter().map(|s| s.to_string()).collect(), no_defaults: false };
    let Ok(ReplyBody::Diff { diff }) = agent.call(ask(ids[0], ids[1], &[])).unwrap() else { panic!() };
    let BodyDiff::Json { changes, .. } = diff.body else { panic!("{:?}", diff.body) };
    assert_eq!((changes[0].at.as_str(), changes[0].kind), ("$.path", ChangeKind::Changed));
    assert_eq!(changes[0].after.as_deref(), Some("\"/b\""));
    let Ok(ReplyBody::Diff { diff }) = agent.call(ask(ids[0], ids[1], &["path"])).unwrap() else { panic!() };
    assert_eq!((diff.body, diff.ignored), (BodyDiff::Same, vec!["$.path".to_string()]));
    assert!(agent.call(ask(ids[0], 999, &[])).unwrap().unwrap_err().contains("no request #999"));
    core.stop();
}

#[test]
fn expectations_are_checked_when_the_answer_comes_and_kept_with_the_request() {
    let ws = workspace("expect");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "test").unwrap();
    cli.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let expect = vec!["status 2xx".to_string(), "$.path == /x".to_string(), "$.got == 3".to_string()];
    let request = HttpRequest { method: "GET".into(), url: format!("{base}/x"), expect: expect.clone(), ..Default::default() };
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(Command::Send { request, env: None }).unwrap() else { panic!() };
    // The end, then its checks next, nothing between.
    let end_seq = wait_for(&mut cli, |e| matches!(e.kind, EventKind::RequestFinished { request_id: id, .. } if id == request_id).then_some(e.seq));
    let e = cli.next_event().unwrap();
    assert_eq!(e.seq, end_seq + 1);
    let EventKind::RequestChecked { results, .. } = e.kind else { panic!("{:?}", e.kind) };
    let passes: Vec<bool> = results.iter().map(|r| r.pass).collect();
    assert_eq!(passes, [true, true, false]);
    assert_eq!(results[2].actual, "0");
    let Ok(ReplyBody::History { entries, .. }) = cli.call(Command::History { filter: Default::default() }).unwrap() else { panic!() };
    assert_eq!(entries[0].checks, Some((2, 1)));
    let Ok(ReplyBody::Response { checks, .. }) = cli.call(Command::Response { request_id }).unwrap() else { panic!() };
    assert_eq!(checks.len(), 3);

    // Written by hand, `# @expect` lines read as the request's own.
    let dir = ws.join("requests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x.http"), format!("# @expect status 2xx
# a note
# @expect $.path exists
GET {base}/x
")).unwrap();
    let saved = wait_for(&mut cli, |e| match &e.kind {
        EventKind::RequestSaved { path, request } if path == "x" => Some(request.clone()),
        _ => None,
    });
    assert_eq!(saved.expect, ["status 2xx", "$.path exists"]);
    // And a request without any is checked by nobody.
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("{base}/y"))).unwrap() else { panic!() };
    end_of(&mut cli, request_id);
    let Ok(ReplyBody::Response { checks, entry, .. }) = cli.call(Command::Response { request_id }).unwrap() else { panic!() };
    assert_eq!((checks.len(), entry.checks), (0, None));
    core.stop();
}

fn bench(url: String, plan: tracer_protocol::BenchPlan) -> Command {
    let request = HttpRequest { method: "GET".into(), url, ..Default::default() };
    Command::RunBench { request, env: None, plan, vars_of: None }
}

/// How bench `id` ended: its stats and why it stopped, if it did.
fn bench_end(client: &mut Client, id: u64) -> (tracer_protocol::BenchStats, Option<String>) {
    wait_for(client, |e| match &e.kind {
        EventKind::BenchFinished { bench_id, stats, stopped } if *bench_id == id => Some((stats.clone(), stopped.clone())),
        _ => None,
    })
}

#[test]
fn a_bench_against_a_server_that_closes_each_connection_counts_no_failures() {
    use tracer_protocol::BenchPlan;
    let ws = workspace("bench-close");
    // Answers HTTP/1.1 with Connection: close, then closes.
    let base = status_server(vec![200]);
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let plan = BenchPlan { total: Some(30), concurrency: 1, ..BenchPlan::default() };
    let Ok(ReplyBody::BenchStarted { bench_id }) = window.call(bench(format!("{base}/x"), plan)).unwrap() else { panic!() };
    let (stats, _) = bench_end(&mut window, bench_id);
    assert_eq!((stats.done, stats.failed, stats.statuses.clone()), (30, 0, vec![(200, 30)]), "{:?}", stats.errors);
    core.stop();
}

#[test]
fn a_bench_sends_its_total_counts_every_answer_and_keeps_only_its_first_in_history() {
    use tracer_protocol::BenchPlan;
    let ws = workspace("bench");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();

    let plan = BenchPlan { total: Some(60), concurrency: 6, ..BenchPlan::default() };
    let Ok(ReplyBody::BenchStarted { bench_id }) = window.call(bench(format!("{base}/load"), plan)).unwrap() else { panic!() };
    let (stats, stopped) = bench_end(&mut window, bench_id);
    assert_eq!(stopped, None);
    assert_eq!((stats.done, stats.bad()), (60, 0));
    assert_eq!(stats.statuses, vec![(200, 60)]);
    assert_eq!(stats.histogram.iter().sum::<u64>(), 60);
    assert!(stats.min_ms <= stats.p50_ms && stats.p50_ms <= stats.p95_ms && stats.p99_ms <= stats.max_ms);

    let Ok(ReplyBody::History { entries, .. }) = window.call(Command::History { filter: Default::default() }).unwrap() else { panic!() };
    assert_eq!(entries.len(), 1, "only the first is a request of its own");
    assert_eq!(entries[0].bench, Some(bench_id));

    // Neither a total nor a duration: refused before anything starts.
    let endless = BenchPlan { concurrency: 1, ..BenchPlan::default() };
    assert!(window.call(bench(format!("{base}/x"), endless)).unwrap().unwrap_err().contains("total, a duration"));
    core.stop();
}

#[test]
fn a_bench_with_a_rate_keeps_to_it_until_its_time_is_up() {
    use tracer_protocol::BenchPlan;
    let ws = workspace("bench-rate");
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();

    let plan = BenchPlan { duration_secs: Some(2), rate: Some(20.0), concurrency: 4, ..BenchPlan::default() };
    let Ok(ReplyBody::BenchStarted { bench_id }) = window.call(bench(format!("{base}/paced"), plan)).unwrap() else { panic!() };
    let mut progress = 0;
    let (stats, stopped) = wait_for(&mut window, |e| match &e.kind {
        EventKind::BenchProgress { bench_id: id, .. } if *id == bench_id => {
            progress += 1;
            None
        }
        EventKind::BenchFinished { bench_id: id, stats, stopped } if *id == bench_id => Some((stats.clone(), stopped.clone())),
        _ => None,
    });
    assert_eq!(stopped, None);
    // 20 a second for 2 s: 40, give or take the edges.
    assert!((35..=42).contains(&stats.done), "{} sent", stats.done);
    assert!(progress >= 4, "told how it went {progress} times");
    core.stop();
}

#[test]
fn an_agents_bench_is_capped_asked_about_and_stopped_by_a_person_or_the_kill_switch() {
    use tracer_protocol::BenchPlan;
    let ws = workspace("bench-agent");
    std::fs::write(ws.join("agents.policy"), "[*]\nmax_bench = 100\nmax_concurrency = 4\n").unwrap();
    let base = stub_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();

    let big = BenchPlan { total: Some(500), concurrency: 4, ..BenchPlan::default() };
    assert!(agent.call(bench(format!("{base}/x"), big)).unwrap().unwrap_err().contains("max_bench of 100"));
    let wide = BenchPlan { total: Some(100), concurrency: 8, ..BenchPlan::default() };
    assert!(agent.call(bench(format!("{base}/x"), wide)).unwrap().unwrap_err().contains("max_concurrency of 4"));

    // Within the caps, it waits for a person before its first request.
    let plan = BenchPlan { total: Some(100), concurrency: 4, ..BenchPlan::default() };
    let Ok(ReplyBody::BenchStarted { bench_id }) = agent.call(bench(format!("{base}/x"), plan)).unwrap() else { panic!() };
    let (first, reason) = wait_for(&mut window, |e| match &e.kind {
        EventKind::ApprovalAsked { request_id, reason } => Some((*request_id, reason.clone())),
        _ => None,
    });
    assert_eq!(reason, "a bench from an agent: 100 requests, 4 at a time");
    // Stopped while it waits: the first is cancelled, and the bench says who.
    assert_eq!(window.call(Command::StopBench { bench_id }).unwrap(), Ok(ReplyBody::BenchStopped));
    assert!(matches!(end_of(&mut window, first), EventKind::RequestCancelled { .. }));
    let (stats, stopped) = bench_end(&mut window, bench_id);
    assert_eq!((stats.done, stopped.as_deref()), (0, Some("stopped by you")));
    assert!(window.call(Command::StopBench { bench_id }).unwrap().is_err(), "it isn't running");

    // Allowed, it runs until the kill switch stops it.
    std::fs::write(ws.join("agents.policy"), "[*]\napprove = none\n").unwrap();
    wait_for(&mut window, |e| matches!(e.kind, EventKind::PolicyChanged { .. }).then_some(()));
    let slow = BenchPlan { duration_secs: Some(30), rate: Some(20.0), concurrency: 2, ..BenchPlan::default() };
    let Ok(ReplyBody::BenchStarted { bench_id }) = agent.call(bench(format!("{base}/y"), slow)).unwrap() else { panic!() };
    wait_for(&mut window, |e| matches!(e.kind, EventKind::BenchProgress { bench_id: id, .. } if id == bench_id).then_some(()));
    window.call(Command::SetAgents { allowed: false }).unwrap().unwrap();
    let (stats, stopped) = bench_end(&mut window, bench_id);
    assert_eq!(stopped.as_deref(), Some("stopped by you"));
    assert!(stats.done > 0 && stats.done < 100, "{} sent", stats.done);
    core.stop();
}

/// A GraphQL server: introspection gets a small schema, a query naming
/// `boom` gets errors with a 200, anything else has what was sent echoed
/// back as `data.sent`. Gives its URL.
fn graphql_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let (mut length, mut kind) = (0usize, String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if let Some(v) = lower.strip_prefix("content-type:") {
                        kind = v.trim().to_string();
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let body = String::from_utf8_lossy(&body).to_string();
                let reply = if body.contains("__schema") {
                    r#"{"data":{"__schema":{"queryType":{"name":"Query"},"mutationType":null,"subscriptionType":null,"types":[{"kind":"OBJECT","name":"Query","description":null,"fields":[{"name":"user","description":null,"args":[{"name":"id","type":{"kind":"NON_NULL","name":null,"ofType":{"kind":"SCALAR","name":"ID"}}}],"type":{"kind":"OBJECT","name":"User"}}],"inputFields":null,"enumValues":null,"possibleTypes":null}]}}}"#.to_string()
                } else if body.contains("boom") {
                    r#"{"data":null,"errors":[{"message":"Cannot query field boom"}]}"#.to_string()
                } else {
                    let sent = serde_json::json!({ "method": first.split_whitespace().next(), "type": kind, "body": body });
                    serde_json::json!({ "data": { "sent": sent } }).to_string()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            });
        }
    });
    format!("http://127.0.0.1:{port}/graphql")
}

#[test]
fn a_graphql_request_goes_as_json_its_errors_fail_it_and_its_schema_is_read() {
    use tracer_protocol::GRAPHQL_INTROSPECTION;
    let ws = workspace("graphql");
    let url = graphql_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "you").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let gql = |body: &str| Command::Send {
        request: HttpRequest { method: "GRAPHQL".into(), url: url.clone(), body: Some(body.into()), ..Default::default() },
        env: None,
    };
    let checked = |client: &mut Client, id: u64| {
        wait_for(client, |e| match &e.kind {
            EventKind::RequestChecked { request_id, results } if *request_id == id => Some(results.clone()),
            _ => None,
        })
    };

    // Sent as a JSON POST, its variables parsed; kept as written.
    let Ok(ReplyBody::Accepted { request_id }) = window.call(gql("query U($id: ID!) { user(id: $id) { name } }\n\n{\"id\": \"7\"}")).unwrap()
    else {
        panic!()
    };
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, request_id) else { panic!() };
    let echoed: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(echoed["data"]["sent"]["method"], "POST");
    assert_eq!(echoed["data"]["sent"]["type"], "application/json");
    let sent: serde_json::Value = serde_json::from_str(echoed["data"]["sent"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(sent, serde_json::json!({ "query": "query U($id: ID!) { user(id: $id) { name } }", "variables": { "id": "7" } }));
    assert!(checked(&mut window, request_id)[0].pass);
    let Ok(ReplyBody::Response { entry, .. }) = window.call(Command::Response { request_id }).unwrap() else { panic!() };
    assert_eq!(entry.request.method, "GRAPHQL", "history keeps it as written");

    // Errors with a 200 fail it.
    let Ok(ReplyBody::Accepted { request_id }) = window.call(gql("{ boom }")).unwrap() else { panic!() };
    let results = checked(&mut window, request_id);
    assert_eq!((results[0].expect.as_str(), results[0].pass), ("no GraphQL errors", false));
    assert_eq!(results[0].actual, "1 error: Cannot query field boom");

    // Its schema, from an introspection answer.
    let Ok(ReplyBody::Accepted { request_id }) = window.call(gql(GRAPHQL_INTROSPECTION)).unwrap() else { panic!() };
    end_of(&mut window, request_id);
    let Ok(ReplyBody::GraphqlSchema { schema }) = window.call(Command::GraphqlSchema { request_id }).unwrap() else { panic!() };
    assert_eq!(schema.to_sdl(), "type Query {\n  user(id: ID!): User\n}");
    core.stop();
}

/// A WebSocket server that answers every text message with it in capitals,
/// and a close with a close. A path of `/nope` is refused with a 404.
fn shouting_server() -> String {
    use tracer_core::ws::{accept_key, frame, take_frame, CLOSE, TEXT};
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut first, mut key) = (String::new(), String::new());
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("sec-websocket-key") {
                            key = value.trim().to_string();
                        }
                    }
                }
                if first.contains("/nope") {
                    let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 5\r\n\r\nnope!");
                    return;
                }
                let accept = accept_key(&key);
                write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    while let Ok(Some(f)) = take_frame(&mut buf, 1 << 20) {
                        let reply = match f.opcode {
                            TEXT => frame(TEXT, String::from_utf8(f.payload).unwrap().to_uppercase().as_bytes(), None),
                            CLOSE => {
                                let _ = stream.write_all(&frame(CLOSE, &f.payload, None));
                                return;
                            }
                            _ => continue,
                        };
                        stream.write_all(&reply).unwrap();
                    }
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
            });
        }
    });
    format!("ws://127.0.0.1:{port}")
}

fn messages(events: &[Event], id: u64) -> Vec<(u64, tracer_protocol::Direction, String)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::StreamMessage { request_id, message } if *request_id == id => {
                Some((message.n, message.dir, message.text.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Events from `client` until `done` says the ones so far are enough.
fn collect_until(client: &mut Client, seen: &mut Vec<Event>, done: impl Fn(&[Event]) -> bool) {
    while !done(seen) {
        seen.push(client.next_event().unwrap());
    }
}

#[test]
fn a_websocket_sends_its_body_waits_for_the_server_takes_more_and_closes_as_finished() {
    use tracer_protocol::Direction::{In, Out};
    use tracer_protocol::{Ended, HistoryFilter, StreamKind};
    let ws = workspace("websocket");
    let server = shouting_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer ws").unwrap();

    let body = "===\nfirst\n=== wait-for-server\nsecond";
    let request = HttpRequest { method: "WEBSOCKET".into(), url: format!("{server}/chat"), body: Some(body.into()), ..Default::default() };
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(Command::Send { request, env: None }).unwrap() else { panic!() };

    let mut seen = Vec::new();
    collect_until(&mut window, &mut seen, |s| messages(s, request_id).len() >= 4);
    let opened = seen.iter().find_map(|e| match &e.kind {
        EventKind::StreamOpened { request_id: id, kind, response } if *id == request_id => Some((*kind, response.status)),
        _ => None,
    });
    assert_eq!(opened, Some((StreamKind::Websocket, 101)));
    // "second" waited for the answer to "first".
    assert_eq!(
        messages(&seen, request_id),
        vec![(1, Out, "first".into()), (2, In, "FIRST".into()), (3, Out, "second".into()), (4, In, "SECOND".into())]
    );

    // Anyone may send on it; the message is in their name.
    let mut agent = Client::connect(&ws, ClientKind::Agent, "claude-code").unwrap();
    assert_eq!(agent.call(Command::StreamSend { request_id, text: "third".into() }).unwrap(), Ok(ReplyBody::StreamSent));
    collect_until(&mut window, &mut seen, |s| messages(s, request_id).len() >= 6);
    let third = seen.iter().find(|e| matches!(&e.kind, EventKind::StreamMessage { message, .. } if message.n == 5)).unwrap();
    assert_eq!(third.actor.name, "claude-code");

    // Closing it is cancelling it: it ends as finished, saying who closed it.
    assert_eq!(window.call(Command::Cancel { request_id }).unwrap(), Ok(ReplyBody::Cancelled));
    collect_until(&mut window, &mut seen, |s| matches!(s.last(), Some(Event { kind: EventKind::RequestFinished { .. }, .. })));
    let closed = seen.iter().find_map(|e| match &e.kind {
        EventKind::StreamClosed { request_id: id, reason, received, sent } if *id == request_id => {
            Some((reason.clone(), *received, *sent))
        }
        _ => None,
    });
    assert_eq!(closed, Some(("closed by window".to_string(), 3, 3)));
    assert!(!seen.iter().any(|e| matches!(e.kind, EventKind::RequestCancelled { .. })));
    let Some(EventKind::RequestFinished { response, .. }) = seen.last().map(|e| &e.kind) else { unreachable!() };
    assert_eq!((response.status, response.size), (101, "FIRSTSECONDTHIRD".len() as u64));

    // Its messages read back a page at a time; sending on it is over.
    let Ok(ReplyBody::StreamMessages { messages, open, more }) =
        cli.call(Command::StreamMessages { request_id, after: 2, limit: Some(2) }).unwrap()
    else {
        panic!()
    };
    assert_eq!(messages.iter().map(|m| m.message.n).collect::<Vec<_>>(), vec![3, 4]);
    assert_eq!((open, more), (false, 2));
    assert!(cli.call(Command::StreamSend { request_id, text: "late".into() }).unwrap().is_err());
    let Ok(ReplyBody::History { entries, .. }) = cli.call(Command::History { filter: HistoryFilter::default() }).unwrap() else { panic!() };
    assert!(matches!(entries[0].ended, Ended::Done { status: 101, .. }));

    // A refused handshake is an ordinary answer, never a stream.
    let request = HttpRequest { method: "WEBSOCKET".into(), url: format!("{server}/nope"), ..Default::default() };
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(Command::Send { request, env: None }).unwrap() else { panic!() };
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, request_id) else { panic!() };
    assert_eq!((response.status, response.body.as_str()), (404, "nope!"));
    assert!(cli.call(Command::StreamMessages { request_id, after: 0, limit: None }).unwrap().is_err());
    core.stop();
}

/// Server-sent events: `/end` sends two and ends the answer; anything else
/// sends three and then a comment every 100 ms, forever.
fn events_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let end = first.contains("/end");
                let head = if end {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
                };
                stream.write_all(head.as_bytes()).unwrap();
                let chunk = |s: &mut TcpStream, text: &str| {
                    let bytes = if end { text.to_string() } else { format!("{:x}\r\n{text}\r\n", text.len()) };
                    s.write_all(bytes.as_bytes())
                };
                let n = if end { 2 } else { 3 };
                for i in 1..=n {
                    chunk(&mut stream, &format!("event: tick\nid: {i}\ndata: tick {i}\n\n")).unwrap();
                    thread::sleep(Duration::from_millis(30));
                }
                if end {
                    return;
                }
                while chunk(&mut stream, ": still here\n").is_ok() {
                    thread::sleep(Duration::from_millis(100));
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn an_event_stream_is_read_as_it_comes_until_the_server_ends_it_or_someone_closes_it() {
    let ws = workspace("sse");
    let server = events_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer send").unwrap();

    let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("{server}/live"))).unwrap() else { panic!() };
    let mut seen = Vec::new();
    collect_until(&mut window, &mut seen, |s| messages(s, request_id).len() >= 3);
    let ticks: Vec<String> = messages(&seen, request_id).into_iter().map(|m| m.2).collect();
    assert_eq!(ticks, ["tick 1", "tick 2", "tick 3"]);
    let named = seen.iter().find_map(|e| match &e.kind {
        EventKind::StreamMessage { message, .. } if message.n == 2 => Some((message.event.clone(), message.id.clone())),
        _ => None,
    });
    assert_eq!(named, Some((Some("tick".into()), Some("2".into()))));
    // Only the server sends on server-sent events.
    assert!(cli.call(Command::StreamSend { request_id, text: "x".into() }).unwrap().is_err());

    // Closed at once, though the server only sends comments.
    let asked = std::time::Instant::now();
    cli.call(Command::Cancel { request_id }).unwrap().unwrap();
    let closed = wait_for(&mut window, |e| match &e.kind {
        EventKind::StreamClosed { request_id: id, reason, received, .. } if *id == request_id => Some((reason.clone(), *received)),
        _ => None,
    });
    assert_eq!(closed, ("closed by tracer send".into(), 3));
    assert!(asked.elapsed() < Duration::from_secs(2));
    assert!(matches!(end_of(&mut window, request_id), EventKind::RequestFinished { .. }));

    // One the server ends finishes by itself.
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("{server}/end"))).unwrap() else { panic!() };
    let closed = wait_for(&mut window, |e| match &e.kind {
        EventKind::StreamClosed { request_id: id, reason, received, .. } if *id == request_id => Some((reason.clone(), *received)),
        _ => None,
    });
    assert_eq!(closed, ("the server ended the stream".into(), 2));
    let EventKind::RequestFinished { response, .. } = end_of(&mut window, request_id) else { panic!() };
    assert_eq!(response.status, 200);
    core.stop();
}

#[test]
fn an_event_stream_that_said_retry_is_reconnected_from_its_last_id() {
    // First connection: retry 100 ms, ids 1 and 2, then it ends. The next
    // must say Last-Event-ID: 2, and gets id 3; the one after gets 204.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut last = String::from("-");
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("last-event-id:") {
                    last = v.trim().to_string();
                }
            }
            seen.lock().unwrap().push(last);
            let (head, body) = match n {
                0 => ("200 OK", "retry: 100\nid: 1\ndata: a\n\nid: 2\ndata: b\n\n"),
                1 => ("200 OK", "id: 3\ndata: c\n\n"),
                _ => ("204 No Content", ""),
            };
            let _ = write!(stream, "HTTP/1.1 {head}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}");
        }
    });
    let ws = workspace("sse-retry");
    let core = tracer_core::start(&ws).unwrap();
    let mut window = Client::connect(&ws, ClientKind::Window, "window").unwrap();
    window.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer send").unwrap();
    let Ok(ReplyBody::Accepted { request_id }) = cli.call(get(format!("http://127.0.0.1:{port}/feed"))).unwrap() else { panic!() };
    let mut events = Vec::new();
    collect_until(&mut window, &mut events, |s| {
        s.iter().any(|e| matches!(&e.kind, EventKind::StreamClosed { request_id: id, .. } if *id == request_id))
    });
    let data: Vec<String> = messages(&events, request_id).into_iter().map(|m| m.2).collect();
    assert_eq!(data, ["a", "b", "c"]);
    let reason = events.iter().find_map(|e| match &e.kind {
        EventKind::StreamClosed { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(reason.as_deref(), Some("the server ended the stream; the server said no more (204) (reconnected 1 time)"));
    assert_eq!(*asked.lock().unwrap(), ["-", "2", "3"]);
    core.stop();
}

/// `/limited` answers 200 twenty times with `X-RateLimit-*` headers, then
/// 429 with `Retry-After`; anything else is always 200.
fn limiting_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicU64::new(0));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let served = served.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut first = String::new();
                    if reader.read_line(&mut first).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let head = if first.contains("/limited") {
                        let n = served.fetch_add(1, Ordering::SeqCst);
                        if n < 20 {
                            format!("200 OK\r\nX-RateLimit-Limit: 20\r\nX-RateLimit-Remaining: {}", 19 - n)
                        } else {
                            "429 Too Many Requests\r\nRetry-After: 60\r\nX-RateLimit-Remaining: 0".to_string()
                        }
                    } else {
                        "200 OK".to_string()
                    };
                    if write!(stream, "HTTP/1.1 {head}\r\nContent-Length: 2\r\n\r\nok").is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_bench_notices_rate_limiting_and_a_baseline_catches_the_regression() {
    use tracer_protocol::BenchPlan;
    let ws = workspace("baseline");
    let base = limiting_server();
    let core = tracer_core::start(&ws).unwrap();
    let mut cli = Client::connect(&ws, ClientKind::Cli, "tracer bench").unwrap();
    cli.call(Command::Subscribe { since: None }).unwrap().unwrap();
    let plan = BenchPlan { total: Some(40), concurrency: 1, ..Default::default() };
    let run = |cli: &mut Client, path: &str| {
        let Ok(ReplyBody::BenchStarted { bench_id }) = cli.call(bench(format!("{base}{path}"), plan.clone())).unwrap() else { panic!() };
        (bench_id, bench_end(cli, bench_id).0)
    };

    let (good, stats) = run(&mut cli, "/ok");
    assert_eq!((stats.done, stats.bad(), stats.rate_limit), (40, 0, None));
    let Ok(ReplyBody::BaselineSaved { name, path }) = cli.call(Command::SaveBaseline { bench_id: good, name: "main".into() }).unwrap() else {
        panic!()
    };
    assert_eq!((name.as_str(), path.as_str()), ("main", "baselines/main.json"));
    assert!(ws.join("baselines/main.json").exists());

    let (limited, stats) = run(&mut cli, "/limited");
    let seen = stats.rate_limit.clone().unwrap();
    assert_eq!((seen.limited, seen.first_after, seen.remaining_min), (20, Some(20), Some(0)));
    assert_eq!((seen.limit.as_deref(), seen.retry_after.as_deref()), (Some("x-ratelimit-limit: 20"), Some("60")));

    // Against the baseline: half the answers were errors, all of them limits.
    let compare = Command::CompareBench { bench_id: limited, baseline: Some("main".into()), other_bench: None, tolerance_pct: None };
    let Ok(ReplyBody::BenchCompared { comparison }) = cli.call(compare).unwrap() else { panic!() };
    assert!(comparison.regressed);
    let worse: Vec<&str> = comparison.deltas.iter().filter(|d| d.worse).map(|d| d.metric.as_str()).collect();
    assert!(worse.contains(&"error_pct") && worse.contains(&"limited_pct"), "{worse:?}");
    assert_eq!(comparison.before, "baseline main");
    assert!(comparison.notes.iter().any(|n| n.starts_with("different requests")));

    // Side by side with the other bench, it's the same story; the good one
    // against itself is no regression.
    let side = Command::CompareBench { bench_id: limited, baseline: None, other_bench: Some(good), tolerance_pct: Some(50.0) };
    let Ok(ReplyBody::BenchCompared { comparison }) = cli.call(side).unwrap() else { panic!() };
    assert!(comparison.regressed && comparison.tolerance_pct == 50.0);
    let same = Command::CompareBench { bench_id: good, baseline: Some("main".into()), other_bench: None, tolerance_pct: None };
    let Ok(ReplyBody::BenchCompared { comparison }) = cli.call(same).unwrap() else { panic!() };
    assert!(!comparison.regressed, "{:?}", comparison.deltas);

    let Ok(ReplyBody::Baselines { baselines, .. }) = cli.call(Command::ListBaselines).unwrap() else { panic!() };
    assert_eq!(baselines.iter().map(|b| b.bench_id).collect::<Vec<_>>(), vec![good]);
    assert!(cli.call(Command::CompareBench { bench_id: 99, baseline: Some("main".into()), other_bench: None, tolerance_pct: None }).unwrap().is_err());
    assert!(cli.call(Command::SaveBaseline { bench_id: good, name: "../x".into() }).unwrap().is_err());
    core.stop();
}

#[test]
fn a_mock_a_stopped_core_was_serving_is_closed_by_the_next() {
    let ws = workspace("dead-mock");
    let core = tracer_core::start(&ws).unwrap();
    let mut a = Client::connect(&ws, ClientKind::Cli, "first").unwrap();
    let request = HttpRequest { method: "GET".into(), url: "{{base}}/items".into(), ..Default::default() };
    a.call(Command::SaveRequest { path: "shop/items".into(), request }).unwrap().unwrap();
    let Ok(ReplyBody::MockStarted { mock_id, .. }) = a.call(Command::StartMock { path: "shop".into(), port: None }).unwrap() else { panic!() };
    drop(a);
    core.stop();

    let core = tracer_core::start(&ws).unwrap();
    let mut b = Client::connect(&ws, ClientKind::Cli, "second").unwrap();
    let Ok(ReplyBody::Status { next_seq, .. }) = b.call(Command::Status).unwrap() else { panic!() };
    b.call(Command::Subscribe { since: Some(0) }).unwrap().unwrap();
    let mut closed = false;
    for _ in 1..next_seq {
        if matches!(b.next_event().unwrap().kind, EventKind::MockStopped { mock_id: m } if m == mock_id) {
            closed = true;
        }
    }
    assert!(closed, "mock {mock_id} still looks served after a restart");
    core.stop();
}

#[test]
fn a_connection_that_never_says_hello_is_let_go() {
    let ws = workspace("silent");
    let core = tracer_core::start(&ws).unwrap();
    let mut raw = TcpStream::connect(("127.0.0.1", core.port)).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let started = std::time::Instant::now();
    let mut buf = Vec::new();
    // The core closes it: the read ends rather than waiting the 30 s.
    let _ = raw.read_to_end(&mut buf);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(9) && waited < Duration::from_secs(20), "closed after {waited:?}");
    core.stop();
}

#[test]
fn a_client_that_said_hello_may_sit_quiet() {
    let ws = workspace("quiet");
    let core = tracer_core::start(&ws).unwrap();
    let mut c = Client::connect(&ws, ClientKind::Cli, "quiet").unwrap();
    thread::sleep(Duration::from_secs(12));
    assert!(c.call(Command::Status).unwrap().is_ok(), "let go after hello");
    core.stop();
}
