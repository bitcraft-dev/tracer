//! A stream's transcript: a WebSocket's or an event stream's messages,
//! both ways, as Markdown to keep or share. Secret values were already
//! replaced by their names in every message the core published.

use std::fmt::Write as _;

use tracer_protocol::{Direction, Ended, HistoryEntry, StreamItem};

pub fn markdown(entry: &HistoryEntry, items: &[StreamItem], open: bool) -> String {
    let r = &entry.request;
    let mut out = String::new();
    let _ = writeln!(out, "# Transcript of #{}: {} {}\n", entry.request_id, r.method, r.url);
    let env = entry.env.as_deref().map(|e| format!(", environment `{e}`")).unwrap_or_default();
    let _ = writeln!(out, "Opened by {} ({}) at {} UTC{env}.", kind(&entry.actor.kind), entry.actor.name, time(entry.at_ms, true));
    let state = match (&entry.ended, open) {
        (_, true) => "Still open when this was written.".to_string(),
        (Ended::Done { total_ms, .. }, false) => format!("Closed after {:.1} s.", total_ms / 1000.0),
        (Ended::Failed { error }, false) => format!("Ended: {error}."),
        (Ended::Cancelled { by }, false) => format!("Closed by {}.", by.name),
        _ => String::new(),
    };
    let (ins, outs) = items.iter().fold((0, 0), |(i, o), m| if m.message.dir == Direction::In { (i + 1, o) } else { (i, o + 1) });
    let _ = writeln!(out, "{state} {} messages: {ins} in, {outs} out.\n", items.len());
    for item in items {
        let m = &item.message;
        let arrow = if m.dir == Direction::In { "←" } else { "→" };
        let mut line = format!("**{}** {arrow} #{}", time(item.at_ms, false), m.n);
        if m.dir == Direction::Out {
            let _ = write!(line, " by {}", item.by.name);
        }
        if let Some(event) = &m.event {
            let _ = write!(line, " · event `{event}`");
        }
        if let Some(id) = &m.id {
            let _ = write!(line, " · id `{id}`");
        }
        if m.binary {
            let _ = write!(line, " · binary, {} bytes", m.size);
        } else if (m.size as usize) > m.text.len() {
            let _ = write!(line, " · cut, {} bytes in all", m.size);
        }
        let _ = writeln!(out, "{line}\n");
        let fence = if m.text.contains("```") { "````" } else { "```" };
        let lang = if serde_json::from_str::<serde_json::Value>(&m.text).is_ok() { "json" } else { "" };
        let _ = writeln!(out, "{fence}{lang}\n{}\n{fence}\n", m.text.trim_end());
    }
    out
}

fn kind(kind: &tracer_protocol::ClientKind) -> &'static str {
    match kind {
        tracer_protocol::ClientKind::Window => "the window",
        tracer_protocol::ClientKind::Cli => "the CLI",
        tracer_protocol::ClientKind::Agent => "agent",
        tracer_protocol::ClientKind::Disk => "a file change",
    }
}

/// `05:49:12.031`, with the date before it when `date`.
fn time(ms: u64, date: bool) -> String {
    let secs = ms / 1000;
    let day = secs % 86_400;
    let clock = format!("{:02}:{:02}:{:02}.{:03}", day / 3600, day % 3600 / 60, day % 60, ms % 1000);
    if !date {
        return clock;
    }
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {clock}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::{Actor, ClientKind, HttpRequest, StreamMessage};

    #[test]
    fn both_ways_with_times_and_senders() {
        let entry = HistoryEntry {
            request_id: 4,
            at_ms: 1_793_857_752_000,
            actor: Actor { kind: ClientKind::Cli, name: "tracer ws".into() },
            request: HttpRequest { method: "WEBSOCKET".into(), url: "wss://echo.test/".into(), ..Default::default() },
            env: None,
            rerun_of: None,
            flow: None,
            bench: None,
            ended: Ended::Cancelled { by: Actor { kind: ClientKind::Window, name: "USER".into() } },
            checks: None,
        };
        let item = |n, dir, text: &str, by: &str, at_ms| StreamItem {
            at_ms,
            by: Actor { kind: ClientKind::Window, name: by.into() },
            message: StreamMessage { n, dir, text: text.into(), binary: false, size: text.len() as u64, event: None, id: None },
        };
        let items = [item(1, Direction::Out, "{\"hi\":1}", "USER", 1_793_857_753_005), item(2, Direction::In, "hello", "tracer ws", 1_793_857_753_120)];
        let text = markdown(&entry, &items, false);
        assert!(text.starts_with("# Transcript of #4: WEBSOCKET wss://echo.test/\n"), "{text}");
        assert!(text.contains("Opened by the CLI (tracer ws) at 2026-11-05 05:49:12.000 UTC."), "{text}");
        assert!(text.contains("Closed by USER. 2 messages: 1 in, 1 out."));
        assert!(text.contains("**05:49:13.005** → #1 by USER\n\n```json\n{\"hi\":1}\n```"), "{text}");
        assert!(text.contains("**05:49:13.120** ← #2\n\n```\nhello\n```"), "{text}");
    }
}
