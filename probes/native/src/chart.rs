//! Probe 4: a live chart fed from Rust.
//!
//! A benchmark streams latency samples while it runs. This produces a fake
//! latency at `hz` samples a second on a thread and publishes the last 240,
//! so the page can draw them as a `<path>` and show how often it repainted.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

const KEEP: usize = 240;

#[rux::export]
#[derive(Clone)]
pub struct Series {
    pub seq: u32,
    /// Milliseconds, oldest first, at most 240.
    pub points: Vec<f64>,
}

#[rux::export]
pub static SERIES: rux::Signal<Series> = rux::Signal::new();

static RUNNING: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Start producing samples, replacing any run already going.
#[rux::export]
pub fn start(hz: u32) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    RUNNING.store(true, Ordering::SeqCst);
    let period = Duration::from_secs_f64(1.0 / hz.max(1) as f64);
    thread::spawn(move || {
        let mut points: Vec<f64> = Vec::with_capacity(KEEP);
        let mut seed: u32 = 0x2545_f491;
        let mut seq = 0;
        while RUNNING.load(Ordering::SeqCst) && GENERATION.load(Ordering::SeqCst) == generation {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed % 1000) as f64 / 1000.0;
            let spike = if seed % 97 == 0 { 180.0 } else { 0.0 };
            seq += 1;
            let ms = 40.0 + 25.0 * ((seq as f64) / 30.0).sin() + 30.0 * noise + spike;
            if points.len() == KEEP {
                points.remove(0);
            }
            points.push(ms);
            SERIES.set(Series { seq, points: points.clone() });
            thread::sleep(period);
        }
    });
}

#[rux::export]
pub fn stop() {
    RUNNING.store(false, Ordering::SeqCst);
}
