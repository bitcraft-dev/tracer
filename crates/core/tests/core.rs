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
use tracer_protocol::{ClientKind, Command, Event, EventKind, HttpRequest, ReplyBody, DISCOVERY_PATH};

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
    Command::Send { request: HttpRequest { method: "GET".into(), url, headers: vec![], body: None }, env: None }
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
        request: HttpRequest { method: "GET".into(), url: url.into(), headers: vec![], body: None },
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
    let request = HttpRequest { method: "GET".into(), url: "http://x.test/a".into(), headers: vec![], body: None };
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
        request: HttpRequest { method: "GET".into(), url: "{{baseUrl}}/me?key={{token}}".into(), headers: vec![], body: None },
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
    Command::Send { request: HttpRequest { method: "POST".into(), url, headers: vec![], body: Some("x".into()) }, env: None }
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
    let Ok(ReplyBody::Response { entry, response }) = window.call(Command::Response { request_id: first }).unwrap() else {
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
