//! The text of a crash report.
//!
//! This is the part that runs inside the panic hook, so it takes care never
//! to panic, never to wait on another thread, and never to write more than a
//! fixed number of bytes.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use super::{AppInfo, date};
use std::fmt::{self, Write};

/// The most bytes in one report. A real one is a few thousand.
pub(crate) const MAX_REPORT_BYTES: usize = 64 * 1024;
/// The most bytes of a panic message kept.
const MAX_MESSAGE_BYTES: usize = 4 * 1024;
/// Added to the limit so the note that the report was cut always fits.
const CUT_NOTE_ROOM: usize = 96;

/// A string that stops growing at a limit. Writing past it keeps what fits and
/// reports an error, which ends a `write!` early.
pub(crate) struct Bounded {
    text: String,
    limit: usize,
    cut: bool,
}

impl Bounded {
    pub fn new(limit: usize) -> Self {
        Self {
            text: String::with_capacity(limit.min(8 * 1024)),
            limit,
            cut: false,
        }
    }

    pub fn into_string(self) -> String {
        self.text
    }
}

impl Write for Bounded {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.limit.saturating_sub(self.text.len());
        if s.len() <= room {
            self.text.push_str(s);
            return Ok(());
        }
        let mut end = room;
        while end > 0 && !s.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        self.text.push_str(s.get(..end).unwrap_or(""));
        self.cut = true;
        Err(fmt::Error)
    }
}

/// Where a panic happened.
pub(crate) struct Location<'a> {
    pub file: &'a str,
    pub line: u32,
    pub column: u32,
}

/// What went wrong.
pub(crate) enum Event<'a> {
    Panic {
        message: &'a str,
        location: Option<Location<'a>>,
    },
    /// Only the Windows exception filter makes these.
    #[cfg_attr(not(windows), allow(dead_code))]
    Exception { code: u32, address: u64 },
}

/// What the report says about the program and the moment.
pub(crate) struct Facts<'a> {
    pub app: &'a AppInfo,
    pub os: &'a str,
    /// Unix seconds.
    pub now: i64,
    pub thread: &'a str,
}

/// What a frame in the hook's own part of the stack looks like, whatever the
/// platform spells it: the backtrace library, this module's hook, and the
/// standard library's panic machinery (`std::panicking`, `core::panicking`,
/// the `__rust_end_short_backtrace` marker, `rust_begin_unwind`, and the boxed
/// hook closure the standard library calls). Windows writes `closure$0` and
/// `<..>` generics where Linux writes `{closure#0}`, so only the module paths
/// are compared.
const MACHINERY: [&str; 8] = [
    "std::backtrace::",
    "backtrace_rs",
    "::crash::hook::",
    "std::panicking::",
    "core::panicking::",
    "rust_end_short_backtrace",
    "rust_begin_unwind",
    "PanicHookInfo",
];

/// A backtrace taken inside the panic hook starts with the backtrace
/// library's own frames, the hook's frames, and the standard library's panic
/// machinery. This drops that leading run, so the first frame left is the code
/// that panicked. Only the run at the top is dropped: `catch_unwind` and
/// similar frames further down are the program's own callers and stay. Text
/// with no such frame, or with nothing else, is returned as it is.
pub(crate) fn without_panic_frames(backtrace: &str) -> &str {
    let mut offset = 0usize;
    // Where the first frame that is not machinery starts.
    let mut first_kept: Option<usize> = None;
    let mut dropped_any = false;
    for line in backtrace.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        let is_frame = digits > 0
            && trimmed
                .get(digits..)
                .is_some_and(|rest| rest.starts_with(": "));
        if is_frame {
            if MACHINERY.iter().any(|marker| line.contains(marker)) {
                dropped_any = true;
            } else {
                first_kept = Some(offset);
                break;
            }
        }
        offset = offset.saturating_add(line.len());
    }
    match first_kept {
        Some(start) if dropped_any => backtrace.get(start..).unwrap_or(backtrace),
        // Nothing to drop, or nothing but machinery: keep it rather than
        // show nothing.
        _ => backtrace,
    }
}

/// A name for the exceptions a program is likely to meet.
pub(crate) fn exception_name(code: u32) -> &'static str {
    match code {
        0xC000_0005 => "access violation",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer division by zero",
        0xC000_00FD => "stack overflow",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "stack buffer overrun",
        0x8000_0003 => "breakpoint",
        _ => "unknown exception",
    }
}

/// Writes the whole report. The sections come in order of how much a reader
/// needs them, so a report that has to be cut loses the end of the backtrace
/// first.
pub(crate) fn write_report(
    out: &mut Bounded,
    facts: &Facts<'_>,
    event: &Event<'_>,
    actions: &[(u32, String)],
    backtrace: &dyn fmt::Display,
) {
    let _ = write_sections(out, facts, event, actions, backtrace);
    if out.cut {
        out.limit = out.limit.saturating_add(CUT_NOTE_ROOM);
        let _ = writeln!(out, "\n[Report cut at {MAX_REPORT_BYTES} bytes]");
    }
}

fn write_sections(
    out: &mut Bounded,
    facts: &Facts<'_>,
    event: &Event<'_>,
    actions: &[(u32, String)],
    backtrace: &dyn fmt::Display,
) -> fmt::Result {
    let app = facts.app;
    writeln!(out, "Bytescape crash report")?;
    writeln!(out, "Format: 1")?;
    writeln!(out, "Program: {} {}", app.name, app.version)?;
    writeln!(out, "Built: {} UTC", date::format_date(app.build_date))?;
    writeln!(out, "Target: {}", app.target)?;
    writeln!(out, "Operating system: {}", facts.os)?;
    writeln!(out, "Architecture: {}", std::env::consts::ARCH)?;
    writeln!(out, "Time: {} UTC", date::format_date_time(facts.now))?;
    writeln!(out, "Thread: {}", facts.thread)?;
    writeln!(out)?;

    match event {
        Event::Panic { message, location } => {
            writeln!(out, "Panic:")?;
            let mut kept = 0usize;
            for line in message.lines() {
                if kept >= MAX_MESSAGE_BYTES {
                    writeln!(out, "    [message cut]")?;
                    break;
                }
                kept = kept.saturating_add(line.len());
                writeln!(out, "    {line}")?;
            }
            if let Some(at) = location {
                writeln!(out, "Location: {}:{}:{}", at.file, at.line, at.column)?;
            }
        }
        Event::Exception { code, address } => {
            writeln!(out, "Exception: {code:#010X} ({})", exception_name(*code))?;
            writeln!(out, "Address: {address:#018X}")?;
        }
    }
    writeln!(out)?;

    writeln!(out, "Recent actions (oldest first):")?;
    if actions.is_empty() {
        writeln!(out, "  none recorded")?;
    }
    for (milliseconds, text) in actions {
        writeln!(out, "  +{milliseconds} ms  {text}")?;
    }
    writeln!(out)?;

    writeln!(out, "Backtrace:")?;
    write!(out, "{backtrace}")?;
    writeln!(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    fn app() -> AppInfo {
        AppInfo::new(
            "bytescape",
            "0.1.0",
            1_790_000_000,
            "x86_64-unknown-linux-gnu",
        )
    }

    fn facts(app: &AppInfo) -> Facts<'_> {
        Facts {
            app,
            os: "Test Linux 1.0 (kernel 6.1)",
            now: 1_800_000_000,
            thread: "main",
        }
    }

    fn render(event: &Event<'_>, actions: &[(u32, String)], backtrace: &str) -> String {
        let app = app();
        let mut out = Bounded::new(MAX_REPORT_BYTES);
        write_report(&mut out, &facts(&app), event, actions, &backtrace);
        out.into_string()
    }

    #[test]
    fn a_panic_report_has_what_the_brief_asks_for() {
        let text = render(
            &Event::Panic {
                message: "index out of bounds\nsecond line",
                location: Some(Location {
                    file: "crates/bs-ui/src/app.rs",
                    line: 12,
                    column: 5,
                }),
            },
            &[(10, "open folder".to_owned()), (250, "zoom in".to_owned())],
            "   0: frame_a\n   1: frame_b\n",
        );
        let expected = "Bytescape crash report
Format: 1
Program: bytescape 0.1.0
Built: 2026-09-21 UTC
Target: x86_64-unknown-linux-gnu
Operating system: Test Linux 1.0 (kernel 6.1)
Architecture: ARCH
Time: 2027-01-15 08:00:00 UTC
Thread: main

Panic:
    index out of bounds
    second line
Location: crates/bs-ui/src/app.rs:12:5

Recent actions (oldest first):
  +10 ms  open folder
  +250 ms  zoom in

Backtrace:
   0: frame_a
   1: frame_b

"
        .replace("ARCH", std::env::consts::ARCH);
        assert_eq!(text, expected);
    }

    #[test]
    fn an_exception_report_names_the_code_and_address() {
        let text = render(
            &Event::Exception {
                code: 0xC000_0005,
                address: 0x7FF6_1234_5678,
            },
            &[],
            "frames",
        );
        assert!(text.contains("\nException: 0xC0000005 (access violation)\n"));
        assert!(text.contains("\nAddress: 0x00007FF612345678\n"));
        assert!(text.contains("  none recorded\n"));
        assert!(!text.contains("Panic:"));
    }

    #[test]
    fn the_report_never_passes_its_size_cap() {
        let huge_message = "x".repeat(1_000_000);
        let huge_trace = "frame\n".repeat(1_000_000);
        let actions: Vec<(u32, String)> = (0..32).map(|n| (n, "a".repeat(96))).collect();
        let text = render(
            &Event::Panic {
                message: &huge_message,
                location: None,
            },
            &actions,
            &huge_trace,
        );
        assert!(
            text.len() <= MAX_REPORT_BYTES + CUT_NOTE_ROOM,
            "{}",
            text.len()
        );
        assert!(
            text.ends_with("[Report cut at 65536 bytes]\n"),
            "{}",
            &text[text.len() - 60..]
        );
        // The message is cut well before the cap, and a long backtrace is cut
        // after the sections a reader needs.
        let long_trace = render(
            &Event::Panic {
                message: "short",
                location: None,
            },
            &[(1, "act".to_owned())],
            &huge_trace,
        );
        assert!(long_trace.contains("  +1 ms  act\n"));
        assert!(long_trace.ends_with("[Report cut at 65536 bytes]\n"));
    }

    #[test]
    fn the_bounded_writer_cuts_on_a_character_boundary() {
        let mut out = Bounded::new(5);
        assert!(write!(out, "ab€€").is_err());
        assert_eq!(out.into_string(), "ab€");
        let mut out = Bounded::new(0);
        assert!(write!(out, "x").is_err());
        assert_eq!(out.into_string(), "");
    }

    /// A Windows backtrace as a CI run captured it, with MSVC-style names.
    const WINDOWS_TRACE: &str = "   0: std::backtrace_rs::backtrace::win64::trace
             at /rustc/RUSTC_COMMIT/library\\std\\src\\..\\..\\backtrace\\src\\backtrace\\win64.rs:85
   1: std::backtrace_rs::backtrace::trace_unsynchronized
             at /rustc/RUSTC_COMMIT/library\\std\\src\\..\\..\\backtrace\\src\\backtrace\\mod.rs:66
   2: std::backtrace::Backtrace::create
             at /rustc/RUSTC_COMMIT/library\\std\\src\\backtrace.rs:331
   3: std::backtrace::Backtrace::force_capture
             at /rustc/RUSTC_COMMIT/library\\std\\src\\backtrace.rs:312
   4: bs_platform::crash::hook::write_event
             at .\\src\\crash\\hook.rs:110
   5: bs_platform::crash::hook::on_panic
             at .\\src\\crash\\hook.rs:90
   6: bs_platform::crash::hook::install::closure$0
             at .\\src\\crash\\hook.rs:63
   7: std::panicking::panic_with_hook
             at /rustc/RUSTC_COMMIT/library\\std\\src\\panicking.rs:823
   8: std::panicking::panic_handler::closure$0
             at /rustc/RUSTC_COMMIT/library\\std\\src\\panicking.rs:688
   9: std::sys::backtrace::__rust_end_short_backtrace<std::panicking::panic_handler::closure_env$0,never$>
             at /rustc/RUSTC_COMMIT/library\\std\\src\\sys\\backtrace.rs:182
  10: std::panicking::panic_handler
             at /rustc/RUSTC_COMMIT/library\\std\\src\\panicking.rs:679
  11: core::panicking::panic_fmt
             at /rustc/RUSTC_COMMIT/library\\core\\src\\panicking.rs:80
  12: bs_platform::crash::tests::child_panics
             at .\\src\\crash\\mod.rs:378
  13: bs_platform::crash::tests::child_panics::closure$0
             at .\\src\\crash\\mod.rs:367
  14: core::ops::function::FnOnce::call_once<bs_platform::crash::tests::child_panics::closure_env$0,tuple$<> >
             at /rustc/RUSTC_COMMIT/library\\core\\src\\ops\\function.rs:250
  23: std::panicking::catch_unwind::do_call
             at /rustc/RUSTC_COMMIT/library\\std\\src\\panicking.rs:576
  24: std::panicking::catch_unwind
             at /rustc/RUSTC_COMMIT/library\\std\\src\\panicking.rs:544
";

    /// A Linux backtrace taken from a real report.
    const LINUX_TRACE: &str = "   0: bs_platform::crash::hook::write_event
             at ./crates/bs-platform/src/crash/hook.rs:105:21
   1: bs_platform::crash::hook::on_panic
             at ./crates/bs-platform/src/crash/hook.rs:85:5
   2: bs_platform::crash::hook::install::{closure#0}
             at ./crates/bs-platform/src/crash/hook.rs:58:9
   3: <alloc::boxed::Box<dyn for<'a, 'b> core::ops::function::Fn<(&'a std::panic::PanicHookInfo<'b>,), Output = ()> + core::marker::Send + core::marker::Sync> as core::ops::function::Fn<(&std::panic::PanicHookInfo,)>>::call
             at /rustc/RUSTC_COMMIT/library/alloc/src/boxed.rs:2334:9
   4: std::panicking::panic_with_hook
             at /rustc/RUSTC_COMMIT/library/std/src/panicking.rs:823:13
   5: std::panicking::panic_handler::{closure#0}
             at /rustc/RUSTC_COMMIT/library/std/src/panicking.rs:688:13
   6: std::sys::backtrace::__rust_end_short_backtrace::<std::panicking::panic_handler::{closure#0}, !>
             at /rustc/RUSTC_COMMIT/library/std/src/sys/backtrace.rs:182:18
   7: __rustc::rust_begin_unwind
             at /rustc/RUSTC_COMMIT/library/std/src/panicking.rs:679:5
   8: core::panicking::panic_fmt
             at /rustc/RUSTC_COMMIT/library/core/src/panicking.rs:80:14
   9: bs_platform::crash::tests::child_panics
             at ./crates/bs-platform/src/crash/mod.rs:378:17
  10: bs_platform::crash::tests::child_panics::{closure#0}
             at ./crates/bs-platform/src/crash/mod.rs:367:22
  16: std::panicking::catch_unwind::do_call::<core::panic::unwind_safe::AssertUnwindSafe<test::run_test_in_process::{closure#0}>, core::result::Result<(), alloc::string::String>>
             at /rustc/RUSTC_COMMIT/library/std/src/panicking.rs:576:43
";

    fn from_frame<'a>(trace: &'a str, marker: &str) -> &'a str {
        &trace[trace.find(marker).unwrap()..]
    }

    #[test]
    fn the_hooks_own_frames_are_dropped_on_windows() {
        // Cut at the panic site; the callers below it, including the
        // `std::panicking::catch_unwind` frames, stay.
        assert_eq!(
            without_panic_frames(WINDOWS_TRACE),
            from_frame(
                WINDOWS_TRACE,
                "  12: bs_platform::crash::tests::child_panics\n"
            )
        );
        assert!(without_panic_frames(WINDOWS_TRACE).contains("std::panicking::catch_unwind\n"));
    }

    #[test]
    fn the_hooks_own_frames_are_dropped_on_linux() {
        assert_eq!(
            without_panic_frames(LINUX_TRACE),
            from_frame(
                LINUX_TRACE,
                "   9: bs_platform::crash::tests::child_panics\n"
            )
        );
        assert!(
            without_panic_frames(LINUX_TRACE).contains("std::panicking::catch_unwind::do_call")
        );
    }

    #[test]
    fn odd_backtraces_are_left_alone() {
        // Nothing recognised to drop, or nothing left once it is dropped.
        let other = "   0: a\n   1: b\n";
        assert_eq!(without_panic_frames(other), other);
        let only = "   0: std::panicking::begin_panic\n   1: core::panicking::panic\n";
        assert_eq!(without_panic_frames(only), only);
        assert_eq!(without_panic_frames(""), "");
        assert_eq!(
            without_panic_frames("disabled backtrace"),
            "disabled backtrace"
        );
        // Frames numbered above 9 are still frames, and the program's own
        // frame is kept even when its name only resembles the machinery.
        let wide = "   9: rust_begin_unwind\n  10: core::panicking::panic_fmt\n  11: app::run\n  12: core::panicking::like_a_caller\n";
        assert_eq!(
            without_panic_frames(wide),
            "  11: app::run\n  12: core::panicking::like_a_caller\n"
        );
    }

    #[test]
    fn exception_names() {
        assert_eq!(exception_name(0xC000_0005), "access violation");
        assert_eq!(exception_name(1), "unknown exception");
    }
}
