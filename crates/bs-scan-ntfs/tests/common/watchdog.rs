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

/// How a phase that reports progress is watched: it fails when it stops
/// moving, not when it is slow. A cold walk of a large system drive on a
/// cloud disk with throttled operations per second can legitimately take a
/// long time, and a bound on the total would either fail a slow disk or let a
/// stuck directory run for as long as the bound.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// How often progress is looked at.
    pub poll: Duration,
    /// How often a progress line is printed.
    pub report: Duration,
    /// The longest the count may stay where it is.
    pub stall: Duration,
    /// The longest the whole phase may take, moving or not.
    pub ceiling: Duration,
}

/// Watches a phase that the caller runs itself (so it can keep the handle).
/// `probe` returns `Some(count)` while the work runs, with a number that
/// grows as it makes progress, and `None` once it has finished. Prints the
/// count and its rate every `report`; panics, naming the phase, if the count
/// does not change for `stall` or the phase outlasts `ceiling`.
pub fn watch(name: &str, limits: Limits, mut probe: impl FnMut() -> Option<u64>) {
    say(&format!(
        "{name}: start (stall limit {:?}, ceiling {:?})",
        limits.stall, limits.ceiling
    ));
    let begun = Instant::now();
    let mut last_count = 0u64;
    let mut last_move = begun;
    let mut last_report = (begun, 0u64);
    loop {
        let Some(count) = probe() else {
            say(&format!("{name}: done"));
            return;
        };
        let now = Instant::now();
        if count != last_count {
            last_count = count;
            last_move = now;
        }
        if now.duration_since(last_report.0) >= limits.report {
            say(&format!(
                "{name}: {count} counted, {} in the last {:?}",
                count.saturating_sub(last_report.1),
                now.duration_since(last_report.0)
            ));
            last_report = (now, count);
        }
        if now.duration_since(last_move) >= limits.stall {
            say(&format!("{name}: NO PROGRESS for {:?}", limits.stall));
            panic!(
                "phase '{name}' made no progress for {:?} (stuck at {count})",
                limits.stall
            );
        }
        if now.duration_since(begun) >= limits.ceiling {
            say(&format!("{name}: STILL RUNNING after {:?}", limits.ceiling));
            panic!(
                "phase '{name}' did not finish in {:?} (at {count})",
                limits.ceiling
            );
        }
        std::thread::sleep(limits.poll);
    }
}

/// The walk of a whole system drive. A cold first pass over a hosted runner's
/// system drive has taken most of a quarter of an hour even in a release
/// build, so the phase is bounded by lack of progress (a directory that never
/// returns shows as a count that stops) and by a ceiling well above that.
pub const LIVE_WALK: Limits = Limits {
    poll: Duration::from_secs(1),
    report: Duration::from_secs(60),
    stall: Duration::from_secs(300),
    ceiling: Duration::from_secs(5400),
};

/// How long each other phase of the comparison may take. Each is several
/// times what the phase has needed.
pub const FLUSH_LIMIT: Duration = Duration::from_secs(300);
pub const WALK_LIMIT: Duration = Duration::from_secs(900);
pub const READ_LIMIT: Duration = Duration::from_secs(900);
pub const COMPARE_LIMIT: Duration = Duration::from_secs(600);
pub const PREPARE_LIMIT: Duration = Duration::from_secs(900);
