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
    Command::Send { request: HttpRequest { method: "GET".into(), url, headers: vec![], body: None } }
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
