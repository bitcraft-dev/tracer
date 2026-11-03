//! Baselines: finished benches kept to compare later ones with, and the
//! comparison itself, which also sets two benches side by side (the same
//! request against two environments, say).
//!
//! A baseline is `baselines/<name>.json` in the workspace: the request as
//! written, its environment, the plan and the stats. Plain files, so they
//! can be checked in and a CI run compared against the one in the repo.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracer_protocol::{Baseline, BenchComparison, BenchPlan, BenchStats, HttpRequest, BASELINES_DIR};

/// `name` as a baseline is named, or why it can't be: letters, digits,
/// `-`, `_` and `.`, such as `main` or `before-cache`.
pub fn normalize(name: &str) -> Result<String, String> {
    let name = name.trim();
    let name = name.strip_suffix(".json").unwrap_or(name);
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok {
        Ok(name.to_string())
    } else {
        Err(format!("`{name}` is not a baseline name: one word, such as `main` or `before-cache`"))
    }
}

pub fn file_of(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(BASELINES_DIR).join(format!("{name}.json"))
}

pub fn write(workspace: &Path, baseline: &Baseline) -> io::Result<PathBuf> {
    let path = file_of(workspace, &baseline.name);
    fs::create_dir_all(path.parent().unwrap())?;
    let text = serde_json::to_string_pretty(baseline).map_err(io::Error::other)?;
    fs::write(&path, text + "\n")?;
    Ok(path)
}

pub fn read(workspace: &Path, name: &str) -> Result<Baseline, String> {
    let name = normalize(name)?;
    let text = fs::read_to_string(file_of(workspace, &name)).map_err(|_| format!("there is no baseline `{name}`"))?;
    serde_json::from_str(&text).map_err(|e| format!("baselines/{name}.json is not a baseline: {e}"))
}

/// Every baseline, by name, and the files that aren't one.
pub fn list(workspace: &Path) -> (Vec<Baseline>, Vec<String>) {
    let (mut found, mut skipped) = (Vec::new(), Vec::new());
    let Ok(dir) = fs::read_dir(workspace.join(BASELINES_DIR)) else { return (found, skipped) };
    for entry in dir.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".json") else { continue };
        match read(workspace, name) {
            Ok(b) => found.push(b),
            Err(e) => skipped.push(e),
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    (found, skipped)
}

/// A bench as compared: what it sent, how, and how it went.
pub struct Side {
    pub label: String,
    pub request: HttpRequest,
    pub env: Option<String>,
    pub plan: BenchPlan,
    pub stats: BenchStats,
    pub stopped: Option<String>,
}

/// `after` against `before`, with what makes them less comparable.
pub fn compare(before: &Side, after: &Side, tolerance_pct: f64) -> BenchComparison {
    let deltas = before.stats.deltas(&after.stats, tolerance_pct);
    let mut notes = Vec::new();
    let line = |r: &HttpRequest| format!("{} {}", r.method, r.url);
    if line(&before.request) != line(&after.request) {
        notes.push(format!("different requests: {} and {}", line(&before.request), line(&after.request)));
    }
    if before.env != after.env {
        let name = |e: &Option<String>| e.clone().unwrap_or_else(|| "none".into());
        notes.push(format!("environments {} and {}", name(&before.env), name(&after.env)));
    }
    if before.plan != after.plan {
        notes.push(format!("different plans: {} and {}", before.plan.describe(), after.plan.describe()));
    }
    for side in [before, after] {
        if let Some(why) = &side.stopped {
            notes.push(format!("{} stopped early: {why}", side.label));
        }
        if side.plan.rate.is_some() {
            notes.push(format!("{} was held to a rate, so its throughput says little", side.label));
        }
        if let Some(limit) = side.stats.rate_limit.as_ref().filter(|r| r.limited > 0) {
            notes.push(format!(
                "{} was rate limited {} times; refusals come back fast, so its throughput and latencies flatter it",
                side.label, limit.limited
            ));
        }
    }
    let regressed = deltas.iter().any(|d| d.worse);
    BenchComparison { before: before.label.clone(), after: after.label.clone(), tolerance_pct, deltas, regressed, notes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(p95: f64, per_sec: f64, done: u64, bad_status: u64) -> BenchStats {
        BenchStats {
            done,
            bad_status,
            mean_ms: p95 / 2.0,
            p50_ms: p95 / 2.0,
            p95_ms: p95,
            p99_ms: p95 * 1.2,
            per_sec,
            ..Default::default()
        }
    }

    fn side(label: &str, stats: BenchStats) -> Side {
        Side {
            label: label.into(),
            request: HttpRequest { method: "GET".into(), url: "http://x/".into(), ..Default::default() },
            env: None,
            plan: BenchPlan { total: Some(100), concurrency: 10, ..Default::default() },
            stats,
            stopped: None,
        }
    }

    #[test]
    fn a_slower_or_failing_bench_is_flagged_and_noise_is_not() {
        let before = side("baseline main", stats(100.0, 50.0, 100, 0));
        // 30 % slower at p95, a fifth of the throughput lost, 5 % errors.
        let after = side("bench 2", stats(130.0, 39.0, 100, 5));
        let c = compare(&before, &after, 20.0);
        let worse: Vec<&str> = c.deltas.iter().filter(|d| d.worse).map(|d| d.metric.as_str()).collect();
        assert_eq!(worse, ["mean_ms", "p50_ms", "p95_ms", "p99_ms", "per_sec", "error_pct"]);
        assert!(c.regressed && c.notes.is_empty());

        // Within the tolerance, or a millisecond either way, is no change.
        let c = compare(&before, &side("bench 3", stats(115.0, 45.0, 100, 1)), 20.0);
        assert!(!c.regressed, "{:?}", c.deltas);
        let fast = side("a", stats(1.0, 50.0, 100, 0));
        assert!(!compare(&fast, &side("b", stats(2.5, 50.0, 100, 0)), 20.0).regressed);

        // Faster is better, and differences of plan and env are noted.
        let mut quicker = side("staging", stats(50.0, 90.0, 100, 0));
        quicker.env = Some("staging".into());
        quicker.plan.concurrency = 20;
        let c = compare(&before, &quicker, 20.0);
        assert!(c.deltas.iter().find(|d| d.metric == "p95_ms").unwrap().better);
        assert_eq!(c.notes, ["environments none and staging", "different plans: 100 requests, 10 at a time and 100 requests, 20 at a time"]);
    }

    #[test]
    fn a_baseline_is_a_plain_file_that_reads_back() {
        let ws = std::env::temp_dir().join(format!("tracer-baselines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        let b = Baseline {
            name: "main".into(),
            bench_id: 3,
            saved_at_ms: 1,
            request: HttpRequest { method: "GET".into(), url: "http://x/".into(), ..Default::default() },
            env: Some("local".into()),
            plan: BenchPlan { total: Some(10), concurrency: 2, ..Default::default() },
            stats: stats(10.0, 5.0, 10, 0),
        };
        let path = write(&ws, &b).unwrap();
        assert!(path.ends_with("baselines/main.json") || path.ends_with("baselines\\main.json"));
        assert_eq!(read(&ws, "main"), Ok(b.clone()));
        fs::write(ws.join("baselines/broken.json"), "{").unwrap();
        let (found, skipped) = list(&ws);
        assert_eq!((found, skipped.len()), (vec![b], 1));
        assert!(normalize("../x").is_err() && normalize("").is_err());
        let _ = fs::remove_dir_all(&ws);
    }
}
