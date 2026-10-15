//! Local crash reports.
//!
//! When Bytescape crashes it leaves a text report on this computer and
//! nowhere else: nothing is ever sent. On the next start the program can say
//! so, show the report with paths hidden, and offer a pre-filled GitHub issue
//! that opens in the browser.
//!
//! # What is written, and when
//!
//! [`install`] sets a panic hook and, on Windows, an unhandled-exception
//! filter. Each writes one report: the program, version, build date, target,
//! operating system and architecture, the time (UTC), the thread, the panic
//! message and location (or the exception code and address), the last
//! [`record_action`] entries, and a backtrace. There are no environment
//! variables, no file contents and no memory dump. A report is at most 64 KiB.
//!
//! Reports are in `$XDG_STATE_HOME/bytescape/crashes` (`~/.local/state/...` when
//! unset) on Linux and `%LOCALAPPDATA%\Bytescape\crashes` on Windows, readable
//! by their owner only. Ten are kept.
//!
//! # What is not caught
//!
//! The hook sees Rust panics, including ones the program later catches and
//! recovers from. It does not see a stack overflow, a crash inside native code
//! on Linux (a segmentation fault), the kernel ending the process, or a power
//! cut: those leave no report. On Windows the filter covers exceptions that
//! reach it; a stack overflow and a fast-fail abort do not.

mod actions;
mod date;
mod hook;
mod os;
mod redact;
mod report;
mod store;
mod url;
#[cfg(windows)]
mod windows;

use std::io;
use std::path::{Path, PathBuf};

pub use actions::record_action;
pub use url::{ISSUE_URL, MAX_URL_BYTES, TRUNCATION_MARKER, github_issue_url};

/// A build this many whole calendar months old (or older) gets a note in the
/// About box.
pub const BUILD_AGE_MONTHS: u64 = 6;

/// What a report says about the program that wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppInfo {
    /// For example `bytescape`.
    pub name: &'static str,
    pub version: &'static str,
    /// When the program was built, in Unix seconds.
    pub build_date: i64,
    /// The target triple it was built for.
    pub target: &'static str,
}

impl AppInfo {
    pub const fn new(
        name: &'static str,
        version: &'static str,
        build_date: i64,
        target: &'static str,
    ) -> Self {
        Self {
            name,
            version,
            build_date,
            target,
        }
    }
}

/// A report left by an earlier run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    path: PathBuf,
    text: String,
}

impl Report {
    /// The file the report is kept in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The report as text.
    ///
    /// With `show_paths` true the text is exactly what was written. With it
    /// false, paths and the user name are hidden:
    ///
    /// - every absolute path (Linux and Windows, including home folders,
    ///   `/root`, `/tmp/...`, drive letters, UNC and `\\?\` paths) becomes
    ///   `<path-1>`, `<path-2>`, ..., the same number for the same path,
    ///   wherever it appears, in a panic message or a backtrace line;
    /// - a source file in a backtrace or in the `Location:` line keeps its
    ///   file name and line and loses its folder: `<source>/app.rs:12:5`;
    /// - the user's name, as a whole word, becomes `<user>`.
    ///
    /// A path ends at the end of its line or at a closing quote, a colon
    /// followed by a space or digit, or similar marks; words that follow a
    /// path with nothing in between are hidden with it. Relative paths and
    /// web addresses are left as they are.
    pub fn render(&self, show_paths: bool) -> String {
        if show_paths {
            self.text.clone()
        } else {
            redact::redact(&self.text, &redact::Context::from_env())
        }
    }
}

/// Installs the crash hooks. Reports go to [`crash_dir`]; with no usable
/// folder nothing is written, but the usual panic behaviour is unchanged.
/// Only the first call has any effect.
pub fn install(app: AppInfo) {
    if let Some(dir) = crash_dir() {
        install_in(app, dir);
    }
}

/// Like [`install`], with the folder given: for a program that must not read
/// the environment, such as an elevated one.
pub fn install_in(app: AppInfo, dir: PathBuf) {
    hook::install(app, dir);
}

/// The folder reports are kept in on this system.
pub fn crash_dir() -> Option<PathBuf> {
    store::default_dir()
}

/// The newest report that has not been acknowledged.
pub fn pending_report() -> Option<Report> {
    pending_report_in(&crash_dir()?)
}

/// Like [`pending_report`], for a given folder.
pub fn pending_report_in(dir: &Path) -> Option<Report> {
    store::pending(dir)
}

/// Marks a report as seen, so it is not offered again. The file is kept.
/// Reports older than this one are marked too, because they were superseded.
pub fn acknowledge(report: &Report) -> io::Result<()> {
    store::acknowledge(&report.path)
}

/// Says that the program is ending normally. Call it once, at the end of a
/// normal shutdown.
///
/// The panic hook writes a report for every panic it sees, because it cannot
/// know whether the panic will be caught. A panic that a scan thread's owner
/// catches, or that kills one worker while the program carries on, is not a
/// reason to tell the user next time that Bytescape closed unexpectedly. This
/// marks the reports written by this run as seen, so [`pending_report`] does
/// not offer them. The files stay in the folder for anyone who looks.
///
/// A process that dies of a panic, an abort or an exception never gets here,
/// and its reports stay pending.
pub fn mark_clean_exit() {
    hook::mark_clean_exit();
}

/// "This version is 14 months old" when `build_date` is at least
/// [`BUILD_AGE_MONTHS`] whole calendar months before `now`; otherwise `None`.
/// Both are Unix seconds. Months are counted by date in UTC: a month is
/// complete on the same day of the next month, or on that month's last day if
/// it is shorter.
pub fn build_age(build_date: i64, now: i64) -> Option<String> {
    let months = date::whole_months_between(build_date, now);
    (months >= BUILD_AGE_MONTHS).then(|| format!("This version is {months} months old"))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh folder under the system temporary folder, removed on drop.
    /// Removal is refused for anything that is not inside it and named like
    /// one of these folders.
    pub(crate) struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        pub fn new(label: &str) -> Self {
            let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let root = base.join(format!(
                "bs-platform-crash-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        pub fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let base = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_default();
            let named = self
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("bs-platform-crash-"));
            if self.root.starts_with(&base) && named {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }

    fn app() -> AppInfo {
        AppInfo::new(
            "bytescape",
            "0.1.0",
            1_790_000_000,
            "x86_64-unknown-linux-gnu",
        )
    }

    // ---- build age -------------------------------------------------------

    const DAY: i64 = 86_400;

    /// Unix seconds at midnight UTC of a date (by counting days, independent
    /// of the code under test).
    fn at(year: i64, month: u32, day: u32) -> i64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = i64::from((month + 9) % 12);
        let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146_097 + doe - 719_468) * DAY
    }

    #[test]
    fn a_build_under_six_months_old_has_no_note() {
        let built = at(2026, 1, 15);
        assert_eq!(build_age(built, built), None);
        assert_eq!(build_age(built, at(2026, 6, 15)), None);
        assert_eq!(build_age(built, at(2026, 7, 14) + DAY - 1), None);
    }

    #[test]
    fn six_months_is_the_threshold() {
        let built = at(2026, 1, 15);
        assert_eq!(
            build_age(built, at(2026, 7, 15)).as_deref(),
            Some("This version is 6 months old")
        );
        // The time of day does not matter; the date does.
        assert_eq!(
            build_age(built + 13 * 3600, at(2026, 7, 15)).as_deref(),
            Some("This version is 6 months old")
        );
    }

    #[test]
    fn older_builds_say_how_many_months() {
        assert_eq!(
            build_age(at(2025, 8, 20), at(2026, 10, 20)).as_deref(),
            Some("This version is 14 months old")
        );
        assert_eq!(
            build_age(at(2025, 8, 20), at(2026, 10, 19)).as_deref(),
            Some("This version is 13 months old")
        );
        assert_eq!(
            build_age(at(2020, 1, 1), at(2026, 10, 14)).as_deref(),
            Some("This version is 81 months old")
        );
    }

    #[test]
    fn the_end_of_a_short_month_completes_a_month() {
        assert_eq!(
            build_age(at(2025, 8, 31), at(2026, 2, 28)).as_deref(),
            Some("This version is 6 months old")
        );
        assert_eq!(build_age(at(2025, 8, 31), at(2026, 2, 27)), None);
    }

    #[test]
    fn a_build_from_the_future_or_a_wild_clock_has_no_note() {
        assert_eq!(build_age(at(2030, 1, 1), at(2026, 1, 1)), None);
        assert_eq!(build_age(i64::MAX, 0), None);
        assert_eq!(build_age(0, i64::MIN), None);
        assert!(build_age(i64::MIN, i64::MAX).is_some());
        assert!(build_age(0, i64::MAX).is_some());
    }

    // ---- reports in a folder ---------------------------------------------

    #[test]
    fn a_report_can_be_found_shown_and_acknowledged() {
        let tmp = Scratch::new("api");
        assert!(pending_report_in(tmp.path()).is_none());
        let written = store::write_report(
            tmp.path(),
            "Bytescape crash report\nProgram: bytescape 0.1.0\nPanic:\n    at /home/zed/x\n",
            1_800_000_000,
            0,
        )
        .unwrap();
        let report = pending_report_in(tmp.path()).unwrap();
        assert_eq!(report.path(), written);
        // Verbatim with paths shown.
        assert!(report.render(true).contains("/home/zed/x"));
        // Hidden otherwise.
        let hidden = report.render(false);
        assert!(!hidden.contains("/home/zed"), "{hidden}");
        assert!(hidden.contains("<path-1>"));
        acknowledge(&report).unwrap();
        assert!(pending_report_in(tmp.path()).is_none());
        assert!(written.exists());
    }

    #[test]
    fn the_issue_link_uses_the_hidden_text_and_stays_short() {
        let tmp = Scratch::new("link");
        let secret = "/home/zed/Secret Project/plan.txt";
        let mut text = format!(
            "Bytescape crash report\nProgram: bytescape 0.1.0\nPanic:\n    cannot open {secret}\n\nBacktrace:\n"
        );
        for n in 0..3000 {
            text.push_str(&format!(
                "   {n}: some::function\n             at /home/zed/src/lib.rs:{n}:1\n"
            ));
        }
        store::write_report(tmp.path(), &text, 1_800_000_000, 1).unwrap();
        let report = pending_report_in(tmp.path()).unwrap();
        let url = github_issue_url(&report);
        assert!(url.starts_with("https://github.com/recursive-ai-oss/bytescape/issues/new?title="));
        assert!(url.len() <= MAX_URL_BYTES, "{}", url.len());
        assert!(url.contains("title=Crash%20report%3A%20bytescape%200.1.0"));
        assert!(!url.contains("zed") && !url.contains("Secret"), "{url}");
        assert!(url.contains("cannot%20open%20%3Cpath-1%3E"));
    }

    // ---- a real crash, in a child process ---------------------------------

    /// The environment variable that tells the copy of this test program
    /// started below what to do.
    const CHILD_DIR: &str = "BS_CRASH_TEST_DIR";
    const CHILD_MODE: &str = "BS_CRASH_TEST_MODE";

    /// Runs one of the `child_*` tests in a new copy of this program, with
    /// the crash folder given, and returns its exit status.
    fn run_child(test: &str, dir: &Path, mode: &str) -> std::process::ExitStatus {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD_DIR, dir)
            .env(CHILD_MODE, mode)
            // A backtrace is wanted whatever the environment says.
            .env_remove("RUST_BACKTRACE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    /// Does nothing unless started by `run_child`.
    #[test]
    fn child_panics() {
        let (Some(dir), Some(mode)) = (std::env::var_os(CHILD_DIR), std::env::var_os(CHILD_MODE))
        else {
            return;
        };
        install_in(app(), PathBuf::from(dir));
        record_action("opened a folder");
        record_action("zoomed into /home/zed/private stuff");
        match mode.to_str() {
            Some("panic") => {
                let path = Path::new("/home/zed/Documents/taxes 2025.pdf");
                panic!("could not read {}: gone", path.display());
            }
            Some("thread") => {
                std::thread::Builder::new()
                    .name("scanner-7".into())
                    .spawn(|| panic!("worker failed"))
                    .unwrap()
                    .join()
                    .ok();
            }
            Some("burst") => {
                for n in 0..40 {
                    let _ = std::panic::catch_unwind(|| panic!("caught {n}"));
                }
            }
            Some("twice") => {
                // A second install changes nothing: still one hook, one report.
                install_in(app(), PathBuf::from("/nonexistent/other"));
                panic!("once");
            }
            _ => {}
        }
    }

    #[test]
    fn a_panic_in_a_child_process_leaves_a_readable_report() {
        let tmp = Scratch::new("child-panic");
        let dir = tmp.path().join("crashes");
        let status = run_child("crash::tests::child_panics", &dir, "panic");
        assert_eq!(status.code(), Some(101), "the child ends by panicking");

        let report = pending_report_in(&dir).expect("a report was left");
        let text = report.render(true);
        assert!(
            text.starts_with("Bytescape crash report\nFormat: 1\n"),
            "{text}"
        );
        assert!(text.contains("Program: bytescape 0.1.0\n"));
        assert!(text.contains("Built: 2026-"));
        assert!(text.contains("Target: x86_64-unknown-linux-gnu\n"));
        assert!(text.contains("Operating system: "));
        assert!(text.contains(&format!("Architecture: {}\n", std::env::consts::ARCH)));
        assert!(text.contains(" UTC\nThread: "));
        assert!(text.contains("could not read /home/zed/Documents/taxes 2025.pdf: gone"));
        assert!(
            text.contains("\nLocation: ") && text.contains("mod.rs:"),
            "{text}"
        );
        assert!(text.contains("  +"));
        assert!(text.contains("opened a folder\n"));
        // A backtrace was captured although RUST_BACKTRACE was not set.
        let backtrace = text
            .split("Backtrace:\n")
            .nth(1)
            .expect("a backtrace section");
        assert!(backtrace.contains("child_panics"), "{backtrace}");
        // The hook's own frames are gone: the first frame is the panic site.
        let first_frame = backtrace.lines().next().unwrap_or_default();
        assert!(first_frame.contains("child_panics"), "{backtrace}");
        // No environment variables or other process data.
        assert!(!text.contains(CHILD_DIR) && !text.contains(CHILD_MODE));

        // Hidden by default.
        let hidden = report.render(false);
        assert!(
            !hidden.contains("taxes") && !hidden.contains("/home/zed"),
            "{hidden}"
        );
        assert!(hidden.contains("could not read <path-1>"), "{hidden}");
        assert!(hidden.contains("opened a folder"));

        // The file is private and alone in its folder.
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = files[0].metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_panic_on_a_named_thread_names_it() {
        let tmp = Scratch::new("child-thread");
        let dir = tmp.path().join("crashes");
        let status = run_child("crash::tests::child_panics", &dir, "thread");
        assert_eq!(status.code(), Some(0), "only the worker panicked");
        let text = pending_report_in(&dir).unwrap().render(true);
        assert!(text.contains("Thread: scanner-7\n"), "{text}");
        assert!(text.contains("worker failed"));
    }

    #[test]
    fn a_burst_of_caught_panics_writes_only_a_few_reports() {
        let tmp = Scratch::new("child-burst");
        let dir = tmp.path().join("crashes");
        let status = run_child("crash::tests::child_panics", &dir, "burst");
        assert_eq!(status.code(), Some(0));
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 5, "at most five reports per run");
    }

    #[test]
    fn installing_twice_changes_nothing() {
        let tmp = Scratch::new("child-twice");
        let dir = tmp.path().join("crashes");
        let status = run_child("crash::tests::child_panics", &dir, "twice");
        assert_eq!(status.code(), Some(101));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_folder_that_cannot_be_written_does_not_disturb_the_panic() {
        let tmp = Scratch::new("child-unwritable");
        // A file where the folder should be.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let status = run_child(
            "crash::tests::child_panics",
            &blocker.join("crashes"),
            "panic",
        );
        // The panic still ends the program the usual way, with no second
        // failure.
        assert_eq!(status.code(), Some(101));
        assert!(blocker.is_file());
    }

    /// An access violation in a child process leaves a report. Only Windows
    /// has the filter, so this runs there.
    #[cfg(windows)]
    #[test]
    fn child_access_violation() {
        let (Some(dir), Some(_)) = (std::env::var_os(CHILD_DIR), std::env::var_os(CHILD_MODE))
        else {
            return;
        };
        install_in(app(), PathBuf::from(dir));
        record_action("about to fault");
        #[allow(unsafe_code)]
        // SAFETY: none. This writes through a null pointer on purpose, to make
        // the system raise an access violation in this throw-away child.
        unsafe {
            std::ptr::null_mut::<u8>().write_volatile(1);
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_access_violation_in_a_child_process_leaves_a_report() {
        let tmp = Scratch::new("child-av");
        let dir = tmp.path().join("crashes");
        let status = run_child("crash::tests::child_access_violation", &dir, "av");
        assert_eq!(status.code(), Some(0xC000_0005_u32 as i32));
        let text = pending_report_in(&dir)
            .expect("a report was left")
            .render(true);
        assert!(
            text.contains("Exception: 0xC0000005 (access violation)\n"),
            "{text}"
        );
        assert!(text.contains("\nAddress: 0x"));
        assert!(text.contains("about to fault"));
        assert!(text.contains("Backtrace:\n"));
    }
}
