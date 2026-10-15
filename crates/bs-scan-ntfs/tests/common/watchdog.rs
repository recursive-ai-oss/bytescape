//! A bound on each phase of a long test, so that a hang names the phase it
//! was in and fails, instead of running until the job's own time limit and
//! saying nothing.
#![allow(dead_code)]

use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// A progress line with the time since the first one, for reading logs.
pub fn say(what: &str) {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let t = START.get_or_init(Instant::now).elapsed();
    eprintln!("[{:8.1}s] {what}", t.as_secs_f64());
}

/// Runs `work` as the phase `name`, printing when it starts and ends. If it
/// has not finished after `limit`, the test fails with the phase's name. (The
/// work itself cannot be stopped from outside; the thread is left behind and
/// ends with the test process.) A panic in the work is passed on unchanged.
pub fn phase<T: Send + 'static>(
    name: &str,
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    say(&format!("{name}: start (limit {limit:?})"));
    let (tx, rx) = channel();
    let handle = std::thread::Builder::new()
        .name(format!("phase: {name}"))
        .spawn(move || {
            let _ = tx.send(work());
        })
        .unwrap_or_else(|e| panic!("cannot start the phase '{name}': {e}"));
    match rx.recv_timeout(limit) {
        Ok(value) => {
            let _ = handle.join();
            say(&format!("{name}: done"));
            value
        }
        Err(RecvTimeoutError::Timeout) => {
            say(&format!("{name}: STILL RUNNING after {limit:?}"));
            panic!("phase '{name}' did not finish in {limit:?}");
        }
        Err(RecvTimeoutError::Disconnected) => match handle.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => panic!("phase '{name}' ended without a result"),
        },
    }
}

/// How long each phase of the system-drive comparison may take. The longest
/// observed was a few minutes for the whole test in a debug build; these are
/// several times what a phase has needed.
pub const FLUSH_LIMIT: Duration = Duration::from_secs(300);
pub const WALK_LIMIT: Duration = Duration::from_secs(900);
pub const READ_LIMIT: Duration = Duration::from_secs(900);
pub const COMPARE_LIMIT: Duration = Duration::from_secs(600);
pub const PREPARE_LIMIT: Duration = Duration::from_secs(900);
