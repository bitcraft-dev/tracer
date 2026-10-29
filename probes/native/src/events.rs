//! Probe 1: an ordered stream of events from Rust.
//!
//! Tracer's live sync is a stream: every command an agent runs, every
//! WebSocket message, every benchmark sample has to reach the window, in
//! order. `rux::Signal` is documented as latest-value-wins. This fires a
//! numbered burst from another thread so the page can count what arrives.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

#[rux::export]
#[derive(Clone)]
pub struct Tick {
    /// Which burst this tick belongs to, so a page ignores an earlier one's.
    pub run: u32,
    pub seq: u32,
    pub total: u32,
}

#[rux::export]
pub static TICK: rux::Signal<Tick> = rux::Signal::new();

static RUNS: AtomicU32 = AtomicU32::new(0);

/// Set `total` ticks, numbered from 1, on a thread of their own, waiting
/// `gap_us` microseconds between them (0: as fast as possible). Gives the
/// burst's run number.
#[rux::export]
pub fn burst(total: u32, gap_us: u32) -> u32 {
    let run = RUNS.fetch_add(1, Ordering::SeqCst) + 1;
    thread::spawn(move || {
        for seq in 1..=total {
            TICK.set(Tick { run, seq, total });
            if gap_us > 0 {
                thread::sleep(Duration::from_micros(gap_us as u64));
            }
        }
    });
    run
}
