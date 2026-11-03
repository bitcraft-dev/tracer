//! Benches: one request sent over and over, its latencies and errors told
//! as they come in.
//!
//! The first request is an ordinary one: numbered, in history with its
//! whole response and waterfall, held to the sender's policy and asked
//! about if it must be (an agent's bench is, by default). Once it has gone,
//! `concurrency` workers send the rest, each on a connection it keeps, as a
//! client under load would. Their answers are counted, not kept: the bench
//! publishes `bench_progress` a few times a second and `bench_finished` at
//! the end, so a bench of thousands is a few hundred events, not thousands
//! of history rows.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracer_protocol::{Access, Actor, BenchPlan, BenchStats, CheckResult, HttpRequest, HttpResponse, BENCH_BUCKETS_MS};

use crate::conn::{run_one, Ran};
use crate::hub::Hub;
use crate::{envs, graphql, guard, http};

/// How often a running bench tells how it's going.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Check `plan` against the sender's policy, start the bench on a thread of
/// its own, and give its id.
pub fn start(hub: &Arc<Hub>, actor: &Actor, request: HttpRequest, env: Option<String>, plan: BenchPlan) -> Result<u64, String> {
    if let Some(why) = guard::bench_refused(&plan, hub.rules_for(actor).as_ref()) {
        return Err(why);
    }
    let (bench_id, stop) = hub.start_bench(actor.clone(), request.clone(), env.clone(), plan.clone());
    let (hub, actor) = (hub.clone(), actor.clone());
    thread::spawn(move || {
        let (stats, stopped) = run(&hub, &actor, bench_id, &stop, &request, env, &plan);
        hub.end_bench(actor, bench_id, stats, stopped);
    });
    Ok(bench_id)
}

/// The bench's stats at its end, and why it ended early if it did.
fn run(
    hub: &Hub,
    actor: &Actor,
    bench_id: u64,
    stop: &Arc<AtomicBool>,
    request: &HttpRequest,
    env: Option<String>,
    plan: &BenchPlan,
) -> (BenchStats, Option<String>) {
    let samples = Arc::new(Mutex::new(Samples::default()));
    let (request_id, cancelled) = hub.start_request(actor.clone(), request.clone(), env.clone(), None, None, Some(bench_id));
    let first = match run_one(hub, actor, request_id, &cancelled, request, env.as_deref(), &[], Some(plan)) {
        Ran::Stopped(why) => return (BenchStats::default(), Some(why)),
        Ran::Ended(outcome, checks) => {
            let took = outcome.as_ref().map_or(0.0, |r| r.timing.total_ms);
            samples.lock().unwrap().add(outcome.as_ref().map_err(String::as_str), &checks);
            took
        }
    };
    // From when the first was sent, not from when a person allowed it.
    let started = Instant::now() - Duration::from_secs_f64(first / 1000.0);
    let stats = |s: &Mutex<Samples>| s.lock().unwrap().stats(started.elapsed());
    if stop.load(Ordering::SeqCst) {
        return (stats(&samples), None);
    }

    // Resolved once for every worker, as `run_one` resolved the first.
    let (sending, secrets) = match resolve(hub, request, env.as_deref()) {
        Ok(found) => found,
        Err(why) => return (stats(&samples), Some(why)),
    };
    let redirects = hub.rules_for(actor).is_none_or(|r| r.access != Access::Local);
    let total = plan.total.unwrap_or(u64::MAX);
    let deadline = plan.duration_secs.map(|s| started + Duration::from_secs(s));
    // The number of the next request to send; the first has gone.
    let next = Arc::new(AtomicU64::new(1));
    let shared = Arc::new(Worker { sending, secrets, written: request.clone(), total, deadline, rate: plan.rate, started });
    let workers: Vec<_> = (0..plan.concurrency.min(total.saturating_sub(1).min(u32::MAX as u64) as u32))
        .map(|_| {
            let (shared, next, stop, samples) = (shared.clone(), next.clone(), stop.clone(), samples.clone());
            thread::spawn(move || shared.work(redirects, &next, &stop, &samples))
        })
        .collect();
    while !workers.iter().all(|w| w.is_finished()) {
        thread::sleep(PROGRESS_EVERY);
        hub.bench_progress(actor.clone(), bench_id, stats(&samples));
    }
    (stats(&samples), None)
}

/// What every worker of one bench shares.
struct Worker {
    sending: HttpRequest,
    secrets: Vec<(String, String)>,
    /// The request as written, for its checks.
    written: HttpRequest,
    total: u64,
    deadline: Option<Instant>,
    rate: Option<f64>,
    started: Instant,
}

impl Worker {
    fn work(&self, redirects: bool, next: &AtomicU64, stop: &AtomicBool, samples: &Mutex<Samples>) {
        let agent = http::keep_alive(redirects);
        loop {
            let n = next.fetch_add(1, Ordering::SeqCst);
            if n >= self.total || stop.load(Ordering::SeqCst) {
                return;
            }
            // Request n's turn, when a rate spaces them out.
            if let Some(rate) = self.rate {
                let due = self.started + Duration::from_secs_f64(n as f64 / rate);
                if !self.wait_until(due, stop) {
                    return;
                }
            }
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                return;
            }
            let outcome = http::execute_with(&agent, &self.sending, stop)
                .map(|r| envs::redact_response(r, &self.secrets))
                .map_err(|e| envs::redact(&e, &self.secrets));
            // One cut short by a stop is no sample.
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let checks = graphql::checks(&self.written, outcome.as_ref().map_err(String::as_str));
            samples.lock().unwrap().add(outcome.as_ref().map_err(String::as_str), &checks);
        }
    }

    /// Sleep until `due`, a little at a time so a stop or the deadline
    /// isn't missed. False if either came first.
    fn wait_until(&self, due: Instant, stop: &AtomicBool) -> bool {
        loop {
            let now = Instant::now();
            if stop.load(Ordering::SeqCst) || self.deadline.is_some_and(|d| d <= due && now >= d) {
                return false;
            }
            if now >= due {
                return true;
            }
            thread::sleep((due - now).min(Duration::from_millis(50)));
        }
    }
}

/// `request` as it will go out, `env`'s variables and secrets filled in,
/// and the secrets, for taking back out of what comes back.
fn resolve(hub: &Hub, request: &HttpRequest, env: Option<&str>) -> Result<(HttpRequest, Vec<(String, String)>), String> {
    let (environment, secrets) = match env {
        Some(name) => {
            let (environment, secrets) = hub.environment_for_sending(name)?;
            (Some(environment), secrets)
        }
        None => (None, Vec::new()),
    };
    let sending = envs::resolve(request, environment.as_ref())
        .and_then(|r| if graphql::is_graphql(&r) { graphql::to_http(&r) } else { Ok(r) })
        .map_err(|e| envs::redact(&e, &secrets))?;
    Ok((sending, secrets))
}

/// What a bench has counted so far.
#[derive(Default)]
struct Samples {
    /// Each answer's whole time, in milliseconds.
    latencies: Vec<f64>,
    failed: u64,
    bad_status: u64,
    failed_checks: u64,
    statuses: BTreeMap<u16, u64>,
    errors: HashMap<String, u64>,
}

/// The most distinct errors a bench lists.
const ERRORS_LISTED: usize = 5;

impl Samples {
    fn add(&mut self, outcome: Result<&HttpResponse, &str>, checks: &[CheckResult]) {
        match outcome {
            Ok(response) => {
                self.latencies.push(response.timing.total_ms);
                *self.statuses.entry(response.status).or_default() += 1;
                if response.status >= 400 {
                    self.bad_status += 1;
                } else if checks.iter().any(|c| !c.pass) {
                    self.failed_checks += 1;
                }
            }
            Err(error) => {
                self.failed += 1;
                *self.errors.entry(error.to_string()).or_default() += 1;
            }
        }
    }

    fn stats(&self, elapsed: Duration) -> BenchStats {
        let mut sorted = self.latencies.clone();
        sorted.sort_by(f64::total_cmp);
        // Nearest rank: the smallest latency at least `p` of them are within.
        let at = |p: f64| match sorted.len() {
            0 => 0.0,
            n => sorted[((p * n as f64).ceil() as usize).clamp(1, n) - 1],
        };
        let mut histogram = vec![0; BENCH_BUCKETS_MS.len() + 1];
        for ms in &sorted {
            histogram[BENCH_BUCKETS_MS.iter().position(|bound| ms <= bound).unwrap_or(BENCH_BUCKETS_MS.len())] += 1;
        }
        let mut errors: Vec<(String, u64)> = self.errors.iter().map(|(e, n)| (e.clone(), *n)).collect();
        errors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        errors.truncate(ERRORS_LISTED);
        let done = sorted.len() as u64 + self.failed;
        let secs = elapsed.as_secs_f64();
        BenchStats {
            elapsed_ms: secs * 1000.0,
            done,
            failed: self.failed,
            bad_status: self.bad_status,
            failed_checks: self.failed_checks,
            statuses: self.statuses.iter().map(|(s, n)| (*s, *n)).collect(),
            errors,
            min_ms: sorted.first().copied().unwrap_or(0.0),
            mean_ms: if sorted.is_empty() { 0.0 } else { sorted.iter().sum::<f64>() / sorted.len() as f64 },
            p50_ms: at(0.50),
            p95_ms: at(0.95),
            p99_ms: at(0.99),
            max_ms: sorted.last().copied().unwrap_or(0.0),
            per_sec: if secs > 0.0 { done as f64 / secs } else { 0.0 },
            histogram,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracer_protocol::Timing;

    fn answer(status: u16, ms: f64) -> HttpResponse {
        let timing = Timing { ttfb_ms: ms, total_ms: ms, dns_ms: None, connect_ms: None, tls_ms: None };
        HttpResponse { status, headers: vec![], body: String::new(), size: 0, timing }
    }

    #[test]
    fn stats_count_every_way_a_request_ends() {
        let mut s = Samples::default();
        for ms in 1..=100 {
            s.add(Ok(&answer(200, ms as f64)), &[]);
        }
        s.add(Ok(&answer(503, 4000.0)), &[]);
        let failed_check = CheckResult { expect: "status 2xx".into(), pass: false, actual: "201".into() };
        s.add(Ok(&answer(201, 3.0)), &[failed_check]);
        s.add(Err("connection refused"), &[]);
        s.add(Err("connection refused"), &[]);
        s.add(Err("timed out"), &[]);

        let stats = s.stats(Duration::from_secs(2));
        assert_eq!((stats.done, stats.failed, stats.bad_status, stats.failed_checks, stats.bad()), (105, 3, 1, 1, 5));
        assert_eq!(stats.statuses, vec![(200, 100), (201, 1), (503, 1)]);
        assert_eq!(stats.errors, vec![("connection refused".into(), 2), ("timed out".into(), 1)]);
        assert_eq!((stats.min_ms, stats.p50_ms, stats.p95_ms, stats.p99_ms, stats.max_ms), (1.0, 50.0, 96.0, 100.0, 4000.0));
        assert_eq!(stats.per_sec, 52.5);
        assert_eq!(stats.histogram.len(), BENCH_BUCKETS_MS.len() + 1);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 102);
        // 1 ms; 2 ms; 3 to 5 ms (with the 3 ms 201); 4000 ms under 5 s.
        assert_eq!(&stats.histogram[..3], &[1, 1, 4]);
        assert_eq!(stats.histogram[12], 1);
    }

    #[test]
    fn no_answers_give_zeros_not_nans() {
        let stats = Samples::default().stats(Duration::ZERO);
        assert_eq!((stats.p50_ms, stats.mean_ms, stats.per_sec), (0.0, 0.0, 0.0));
    }
}
