//! Probe 5: a child process the window owns.
//!
//! Tracer's window starts the core as a separate process when none is
//! running. This starts a long-lived stand-in, so the page can check that it
//! runs, and that the app gets a chance to stop it when the window closes.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

static CHILD: Mutex<Option<Child>> = Mutex::new(None);

fn log(line: &str) {
    // Written to a file because nothing is watching the window's stderr
    // after it has closed.
    let path = std::env::temp_dir().join("tracer-probe-proc.log");
    let old = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::write(&path, format!("{old}{line}\n"));
}

/// Start the stand-in (an hour-long ping of localhost) and give its pid.
#[rux::export]
pub fn spawn_child() -> u32 {
    let mut slot = CHILD.lock().unwrap();
    if let Some(child) = slot.as_ref() {
        return child.id();
    }
    let child = Command::new("ping")
        .args(["-n", "3600", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ping is on every Windows machine");
    let pid = child.id();
    log(&format!("spawned {pid}"));
    *slot = Some(child);
    pid
}

#[rux::export]
pub fn child_alive() -> bool {
    let mut slot = CHILD.lock().unwrap();
    match slot.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    }
}

/// Stop the stand-in. `why` is written to the probe log, so a close that
/// reached this can be told apart from one that did not.
#[rux::export]
pub fn stop_child(why: String) {
    let mut slot = CHILD.lock().unwrap();
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
        log(&format!("stopped {} ({why})", child.id()));
    }
}

/// Where the log is, to show on the page.
#[rux::export]
pub fn log_path() -> String {
    std::env::temp_dir().join("tracer-probe-proc.log").display().to_string()
}
