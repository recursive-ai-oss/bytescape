//! The panic hook, and the shared code the Windows exception filter uses.
//!
//! A panic inside a panic hook aborts the process, so nothing here may
//! panic: no `unwrap`, no indexing, no arithmetic that can overflow. The
//! lints below hold the code to that. The hook also never waits for another
//! thread, writes a bounded amount, and writes only a few reports per run so
//! a loop of caught panics cannot fill the disk.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use super::report::{
    Bounded, Event, Facts, Location, MAX_REPORT_BYTES, without_panic_frames, write_report,
};
use super::{AppInfo, actions, os, store};
use std::fmt::Write;
use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Reports written by one run of the program, at most. Panics beyond this are
/// still printed by the usual hook, just not saved.
const MAX_REPORTS_PER_RUN: u32 = 5;
/// The most bytes of backtrace text kept.
const MAX_BACKTRACE_BYTES: usize = 48 * 1024;

struct State {
    app: AppInfo,
    os: String,
    dir: PathBuf,
}

static STATE: OnceLock<State> = OnceLock::new();
static WRITTEN: AtomicU32 = AtomicU32::new(0);
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Installs the hooks. Only the first call has any effect; it returns whether
/// this call was the first.
pub(crate) fn install(app: AppInfo, dir: PathBuf) -> bool {
    let state = State {
        app,
        os: os::describe(),
        dir,
    };
    if STATE.set(state).is_err() {
        return false;
    }
    actions::start_clock();
    if let Some(state) = STATE.get() {
        store::prune(&state.dir, store::MAX_REPORTS);
    }
    let previous = std::panic::take_hook();
    // The usual hook goes first, so the message appears even if writing the
    // report is slow, and the test harness still shows what it expects.
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        on_panic(info);
    }));
    #[cfg(windows)]
    super::windows::install_filter();
    true
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

fn on_panic(info: &PanicHookInfo<'_>) {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(the panic carried no text)");
    let location = info.location().map(|at| Location {
        file: at.file(),
        line: at.line(),
        column: at.column(),
    });
    write_event(&Event::Panic { message, location });
}

/// Writes a report for `event`, if this run still has room for one.
pub(crate) fn write_event(event: &Event<'_>) {
    let Some(state) = STATE.get() else {
        return;
    };
    if WRITTEN.fetch_add(1, Ordering::Relaxed) >= MAX_REPORTS_PER_RUN {
        return;
    }
    let thread = std::thread::current();
    let facts = Facts {
        app: &state.app,
        os: &state.os,
        now: unix_now(),
        thread: thread.name().unwrap_or("<unnamed>"),
    };
    // Captured even when `RUST_BACKTRACE` is not set. Capturing is bounded by
    // the depth of this thread's stack; what is written is bounded below.
    let captured = std::backtrace::Backtrace::force_capture();
    let mut trace = Bounded::new(MAX_BACKTRACE_BYTES);
    let _ = write!(trace, "{captured}");
    let trace = trace.into_string();
    let trace = match event {
        Event::Panic { .. } => without_panic_frames(&trace),
        Event::Exception { .. } => trace.as_str(),
    };
    let recent = actions::snapshot(false);
    let mut text = Bounded::new(MAX_REPORT_BYTES);
    write_report(&mut text, &facts, event, &recent, &trace);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let _ = store::write_report(&state.dir, &text.into_string(), facts.now, sequence);
}
