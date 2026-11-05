//! Run-untils: one request sent again and again, one at a time, until a
//! number of answers in a row count.
//!
//! What counts is a failure (no answer, a failed check, or with no checks a
//! status of 400 or more), or, given an expectation, an answer it passes
//! on: `status == 401` four times in a row. Any other answer starts the
//! count again. Unlike a bench, every send is kept: each is an ordinary
//! request, numbered, in history with its answer, held to the sender's
//! policy and asked about if it must be, so the answers that made the
//! streak can be read afterwards. `until_progress` names each one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tracer_protocol::{Actor, CheckResult, HttpRequest, HttpResponse, UntilPlan, UNTIL_MAX};

use crate::conn::{run_one, Ran};
use crate::expect;
use crate::hub::Hub;

/// The most a run-until may send, whoever asks.
pub const UNTIL_MAX_SENDS: u64 = 100_000;

/// The longest gap between sends, in milliseconds.
pub const UNTIL_MAX_GAP_MS: u64 = 60 * 60 * 1000;

/// Check `plan`, start the run on a thread of its own, and give its id.
pub fn start(hub: &Arc<Hub>, actor: &Actor, request: HttpRequest, env: Option<String>, plan: UntilPlan) -> Result<u64, String> {
    if crate::realtime::is_websocket(&request) {
        return Err("a WebSocket stays open, so it can't be sent again and again; use an HTTP request".into());
    }
    if let Some(why) = refused(&plan) {
        return Err(why);
    }
    let (until_id, stop) = hub.start_until(actor.clone(), request.clone(), env.clone(), plan.clone());
    let (hub, actor) = (hub.clone(), actor.clone());
    thread::spawn(move || {
        let end = run(&hub, &actor, until_id, &stop, &request, env, &plan);
        hub.end_until(actor, until_id, end.sent, end.streak, end.reached, end.stopped);
    });
    Ok(until_id)
}

/// Why `plan` can't run, if it can't.
fn refused(plan: &UntilPlan) -> Option<String> {
    if plan.streak == 0 {
        return Some("a run-until's streak is at least 1".into());
    }
    if plan.max == Some(0) {
        return Some("a run-until sends at least 1".into());
    }
    if plan.max.is_some_and(|m| m > UNTIL_MAX_SENDS) {
        return Some(format!("a run-until sends at most {UNTIL_MAX_SENDS}"));
    }
    if plan.gap_ms.is_some_and(|g| g > UNTIL_MAX_GAP_MS) {
        return Some("a run-until waits at most an hour between sends".into());
    }
    if let Some(m) = &plan.matching {
        if let Err(why) = expect::readable(m) {
            return Some(format!("can't read `{m}`: {why}"));
        }
    }
    None
}

struct End {
    sent: u64,
    streak: u32,
    reached: bool,
    stopped: Option<String>,
}

fn run(hub: &Hub, actor: &Actor, until_id: u64, stop: &AtomicBool, request: &HttpRequest, env: Option<String>, plan: &UntilPlan) -> End {
    let max = plan.max.unwrap_or(UNTIL_MAX);
    let gap = Duration::from_millis(plan.gap_ms.unwrap_or(0));
    let mut end = End { sent: 0, streak: 0, reached: false, stopped: None };
    while end.sent < max {
        if stop.load(Ordering::SeqCst) {
            return end;
        }
        if let Err(why) = hub.take_rate(actor) {
            end.stopped = Some(why);
            return end;
        }
        let (request_id, cancelled) = hub.start_request(actor.clone(), request.clone(), env.clone(), None, None, None);
        hub.until_sending(until_id, request_id);
        let counted = match run_one(hub, actor, request_id, &cancelled, request, env.as_deref(), &[], None) {
            // Refused, not allowed in time, or cancelled: the run ends with
            // it, unless that was the run being stopped, which says so.
            Ran::Stopped(why) => {
                end.sent += 1;
                hub.until_progress(actor.clone(), until_id, request_id, false, end.sent, 0);
                end.streak = 0;
                end.stopped = Some(format!("request #{request_id}: {why}"));
                return end;
            }
            Ran::Ended(outcome, checks) => {
                let o = outcome.as_ref().map_err(String::as_str);
                match plan.matching.as_deref().filter(|m| crate::openapi::spec_line(m).is_some()) {
                    // `spec petstore`: checked against the workspace's spec.
                    Some(m) => {
                        let mut r = expect::check(&[m.to_string()], o);
                        crate::openapi::fill_checks(hub.root(), request, o, &mut r);
                        r.first().is_some_and(|c| c.pass)
                    }
                    None => counts(plan.matching.as_deref(), o, &checks),
                }
            }
        };
        end.sent += 1;
        end.streak = if counted { end.streak + 1 } else { 0 };
        hub.until_progress(actor.clone(), until_id, request_id, counted, end.sent, end.streak);
        if end.streak >= plan.streak {
            end.reached = true;
            return end;
        }
        if end.sent < max && !wait(gap, stop) {
            return end;
        }
    }
    end.stopped = Some(format!("sent {max}, its most, without {} in a row", plan.streak));
    end
}

/// Whether an answer counts towards the streak: one `matching` passes on,
/// or without it, a failure.
fn counts(matching: Option<&str>, outcome: Result<&HttpResponse, &str>, checks: &[CheckResult]) -> bool {
    match (matching, outcome) {
        (Some(m), outcome) => expect::check(&[m.to_string()], outcome).first().is_some_and(|c| c.pass),
        (None, Err(_)) => true,
        (None, Ok(_)) if !checks.is_empty() => checks.iter().any(|c| !c.pass),
        (None, Ok(response)) => response.status >= 400,
    }
}

/// Sleep `gap`, a little at a time so a stop is heard. False when stopped.
fn wait(gap: Duration, stop: &AtomicBool) -> bool {
    let until = Instant::now() + gap;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        thread::sleep((until - now).min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::Timing;

    fn answer(status: u16) -> HttpResponse {
        let timing = Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None };
        HttpResponse { status, headers: vec![], body: "{}".into(), size: 2, timing, tls_unchecked: false }
    }

    fn check(pass: bool) -> CheckResult {
        CheckResult { expect: "x".into(), pass, actual: String::new() }
    }

    #[test]
    fn a_failure_counts_unless_an_expectation_says_what_does() {
        assert!(counts(None, Err("refused"), &[]));
        assert!(counts(None, Ok(&answer(500)), &[]));
        assert!(!counts(None, Ok(&answer(200)), &[]));
        // With checks, they decide, not the status.
        assert!(counts(None, Ok(&answer(200)), &[check(false)]));
        assert!(!counts(None, Ok(&answer(404)), &[check(true)]));
        // A matcher counts what it passes on, failures or not.
        assert!(counts(Some("status == 401"), Ok(&answer(401)), &[]));
        assert!(!counts(Some("status == 401"), Ok(&answer(500)), &[]));
        assert!(!counts(Some("status == 401"), Err("refused"), &[]));
        assert!(counts(Some("status 2xx"), Ok(&answer(204)), &[]));
    }

    #[test]
    fn a_plan_is_refused_for_what_it_cant_do() {
        let plan = |streak, matching: Option<&str>, max| UntilPlan { streak, matching: matching.map(String::from), max, gap_ms: None };
        assert!(refused(&plan(4, None, None)).is_none());
        assert!(refused(&plan(4, Some("status == 401"), Some(10))).is_none());
        assert!(refused(&plan(0, None, None)).unwrap().contains("at least 1"));
        assert!(refused(&plan(1, None, Some(0))).unwrap().contains("at least 1"));
        assert!(refused(&plan(1, None, Some(UNTIL_MAX_SENDS + 1))).unwrap().contains("at most"));
        assert!(refused(&plan(1, Some("stauts 401"), None)).unwrap().contains("can't read"));
    }
}
