//! A monotonic clock, so a page can time its own work. Script has none of its
//! own (see FINDINGS.md).

use std::sync::OnceLock;
use std::time::Instant;

fn start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// Milliseconds since the first call, with sub-millisecond precision.
#[rux::export]
pub fn now_ms() -> f64 {
    start().elapsed().as_secs_f64() * 1000.0
}
