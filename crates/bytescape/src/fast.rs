//! The full scan: the administrator helper's result, fed into the same
//! machinery that holds an ordinary scan.
//!
//! The helper reads the drive with administrator rights and streams the same
//! batches of tree records that the walker makes. [`HelperScanner`] is a
//! [`Scanner`] whose "walk" is that stream, so a full scan is an ordinary
//! `ScanDriver` underneath: it builds its own tree beside the walk and the
//! window swaps one for the other when the race (see
//! `bs_ui::elevation::Race`) is decided.
//!
//! The prompt for administrator rights is shown by the system while this
//! scanner waits inside `launch`, on the scan's own thread; the window is never
//! held up by it.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bs_platform::elevate::{ElevateError, Progress, ScanEnd, ScanEvent};
use bs_scan::{ScanContext, ScanError, Scanner};
use bs_ui::elevation::{FastEnd, fallback_note};

/// A running helper scan, as the scanner needs it: events in, a way to stop.
pub trait FastStream: Send {
    /// The next event; `Ok(None)` after the end.
    fn next_event(&mut self) -> Result<Option<ScanEvent>, ElevateError>;
    /// A handle that stops the scan from another thread.
    fn canceller(&self) -> Box<dyn Fn() + Send + Sync>;
}

/// Starts helper scans. The real one asks the system for administrator
/// rights; tests use a stand-in.
pub trait FastLauncher: Send + Sync + 'static {
    fn launch(&self, volume: &Path) -> Result<Box<dyn FastStream>, ElevateError>;
}

/// The helper through the platform's elevation prompt.
pub struct SystemLauncher {
    /// Mounts of the same drive the scan may enter, besides the one it starts
    /// at (the ones no other mount already covers).
    pub extra_mount_ids: Vec<u64>,
}

struct SystemStream(bs_platform::elevate::ElevatedScan);

impl FastStream for SystemStream {
    fn next_event(&mut self) -> Result<Option<ScanEvent>, ElevateError> {
        self.0.next_event()
    }

    fn canceller(&self) -> Box<dyn Fn() + Send + Sync> {
        let canceller = self.0.canceller();
        Box::new(move || canceller.cancel())
    }
}

impl FastLauncher for SystemLauncher {
    fn launch(&self, volume: &Path) -> Result<Box<dyn FastStream>, ElevateError> {
        let scan = bs_platform::elevate::elevate_and_scan(
            volume,
            bs_platform::elevate::Backend::Auto,
            self.extra_mount_ids.clone(),
        )?;
        Ok(Box::new(SystemStream(scan)))
    }
}

/// How the helper scan ended, for the window to read once the driver is done.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Finish {
    /// Not over yet.
    #[default]
    Running,
    /// Over; this is the verdict for the race.
    Ended(FastEnd),
}

/// A [`Scanner`] that reads the drive through the administrator helper.
pub struct HelperScanner {
    launcher: Arc<dyn FastLauncher>,
    finish: Arc<Mutex<Finish>>,
}

impl HelperScanner {
    pub fn new(launcher: Arc<dyn FastLauncher>) -> Arc<HelperScanner> {
        Arc::new(HelperScanner {
            launcher,
            finish: Arc::new(Mutex::new(Finish::Running)),
        })
    }

    /// Where the scanner leaves its verdict.
    pub fn finish(&self) -> Arc<Mutex<Finish>> {
        Arc::clone(&self.finish)
    }

    fn end(&self, end: FastEnd) {
        if let Ok(mut f) = self.finish.lock() {
            *f = Finish::Ended(end);
        }
    }
}

/// The counters of the helper's progress report that are new since the last
/// one, so the window's readout follows the scan.
fn count_new(ctx: &ScanContext, last: &mut Progress, now: &Progress) {
    ctx.count(
        now.allocated_bytes.saturating_sub(last.allocated_bytes),
        now.logical_bytes.saturating_sub(last.logical_bytes),
        now.files.saturating_sub(last.files),
        now.dirs.saturating_sub(last.dirs),
    );
    ctx.count_problems(
        now.unreadable.saturating_sub(last.unreadable),
        now.vanished.saturating_sub(last.vanished),
    );
    *last = *now;
}

impl Scanner for HelperScanner {
    fn name(&self) -> &'static str {
        "administrator helper"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<bs_scan::ScanEnd, ScanError> {
        let mut stream = match self.launcher.launch(root) {
            Ok(s) => s,
            Err(ElevateError::Declined) => {
                self.end(FastEnd::Cancelled);
                return Ok(bs_scan::ScanEnd::Cancelled);
            }
            Err(e) => {
                self.end(FastEnd::Failed(failure_text(&e)));
                return Ok(bs_scan::ScanEnd::Cancelled);
            }
        };
        let cancel = stream.canceller();
        // The user may have stopped the scan (or picked another drive) while
        // the prompt was up.
        if ctx.is_cancelled() {
            cancel();
        }
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut last = Progress::default();
        let result = std::thread::scope(|scope| {
            // Reading blocks until the helper sends something, so a watcher
            // passes the cancel on.
            scope.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if ctx.is_cancelled() {
                        cancel();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            });
            let result = loop {
                match stream.next_event() {
                    Ok(Some(ScanEvent::Batch(batch))) => {
                        if !ctx.send(batch) {
                            cancel();
                        }
                    }
                    Ok(Some(ScanEvent::Progress(p))) => count_new(ctx, &mut last, &p),
                    Ok(Some(ScanEvent::Done(done))) => {
                        count_new(ctx, &mut last, &done.progress);
                        break Ok(done);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break Err(None),
                    Err(e) => break Err(Some(e)),
                }
            };
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            result
        });
        match result {
            Ok(done) if done.end == ScanEnd::Completed => {
                self.end(FastEnd::Completed {
                    note: fallback_note(done.fallback),
                });
                Ok(bs_scan::ScanEnd::Completed)
            }
            Ok(_) => {
                self.end(FastEnd::Cancelled);
                Ok(bs_scan::ScanEnd::Cancelled)
            }
            Err(error) => {
                // A stop the user asked for can end the conversation too.
                if ctx.is_cancelled() {
                    self.end(FastEnd::Cancelled);
                } else {
                    let text = match error {
                        Some(e) => failure_text(&e),
                        None => "the helper closed the connection before it finished.".to_string(),
                    };
                    self.end(FastEnd::Failed(text));
                }
                Ok(bs_scan::ScanEnd::Cancelled)
            }
        }
    }
}

/// A sentence for a helper that could not run, ending in a full stop.
fn failure_text(error: &ElevateError) -> String {
    match error {
        ElevateError::Unavailable(why) => why.explanation().to_string(),
        ElevateError::Declined => "administrator rights were declined.".to_string(),
        other => format!("{other}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_platform::elevate::{Fallback, ScanDone};
    use bs_tree::{Batch, Category, DirKey, Meta, NodeKind};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn meta(size: u64) -> Meta {
        Meta {
            logical: size,
            allocated: size,
            category: Category(1),
            ..Meta::default()
        }
    }

    fn batches() -> Vec<ScanEvent> {
        let mut b = Batch::new();
        b.dir(DirKey(1), DirKey::NONE, b"/", &meta(4096)).unwrap();
        b.dir(DirKey(2), DirKey(1), b"$MFT", &meta(1_000_000))
            .unwrap();
        b.file(NodeKind::File, DirKey(1), b"a", &meta(5_000_000))
            .unwrap();
        b.listing_done(DirKey(2));
        b.listing_done(DirKey(1));
        vec![ScanEvent::Batch(b)]
    }

    fn done(end: ScanEnd, fallback: Fallback) -> ScanEvent {
        ScanEvent::Done(ScanDone {
            backend: bs_platform::elevate::Backend::Ntfs,
            end,
            fallback,
            progress: Progress::default(),
        })
    }

    struct Script {
        events: Mutex<VecDeque<Result<Option<ScanEvent>, ElevateError>>>,
        cancelled: Arc<AtomicBool>,
        /// When the script runs dry, wait for a cancel instead of ending.
        hang: bool,
        /// Nothing is sent until this is set (a helper still being started).
        gate: Arc<AtomicBool>,
    }

    struct Stream(Arc<Script>);

    impl FastStream for Stream {
        fn next_event(&mut self) -> Result<Option<ScanEvent>, ElevateError> {
            loop {
                if !self.0.gate.load(Ordering::SeqCst) {
                    if self.0.cancelled.load(Ordering::SeqCst) {
                        return Ok(Some(done(ScanEnd::Cancelled, Fallback::None)));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                if let Some(next) = self.0.events.lock().unwrap().pop_front() {
                    return next;
                }
                if !self.0.hang {
                    return Ok(None);
                }
                if self.0.cancelled.load(Ordering::SeqCst) {
                    return Ok(Some(done(ScanEnd::Cancelled, Fallback::None)));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn canceller(&self) -> Box<dyn Fn() + Send + Sync> {
            let flag = Arc::clone(&self.0.cancelled);
            Box::new(move || flag.store(true, Ordering::SeqCst))
        }
    }

    struct Launcher {
        script: Mutex<Option<Result<Arc<Script>, ElevateError>>>,
    }

    impl FastLauncher for Launcher {
        fn launch(&self, _volume: &Path) -> Result<Box<dyn FastStream>, ElevateError> {
            match self.script.lock().unwrap().take().expect("one launch") {
                Ok(s) => Ok(Box::new(Stream(s))),
                Err(e) => Err(e),
            }
        }
    }

    fn scripted(
        events: Vec<Result<Option<ScanEvent>, ElevateError>>,
        hang: bool,
    ) -> (Arc<HelperScanner>, Arc<Script>) {
        let script = Arc::new(Script {
            events: Mutex::new(events.into()),
            cancelled: Arc::new(AtomicBool::new(false)),
            hang,
            gate: Arc::new(AtomicBool::new(true)),
        });
        let launcher = Arc::new(Launcher {
            script: Mutex::new(Some(Ok(Arc::clone(&script)))),
        });
        (HelperScanner::new(launcher), script)
    }

    fn run(scanner: &Arc<HelperScanner>) -> (Arc<bs_tree::Snapshot>, Finish) {
        let handle = bs_scan::start(scanner.clone(), "/".into(), bs_scan::ScanOptions::default());
        let result = handle.wait();
        let finish = scanner.finish().lock().unwrap().clone();
        (result.snapshot, finish)
    }

    #[test]
    fn a_completed_helper_scan_builds_a_complete_tree_with_the_system_files() {
        let mut events: Vec<_> = batches().into_iter().map(|e| Ok(Some(e))).collect();
        events.push(Ok(Some(done(ScanEnd::Completed, Fallback::None))));
        let (scanner, _) = scripted(events, false);
        let (snap, finish) = run(&scanner);
        assert_eq!(finish, Finish::Ended(FastEnd::Completed { note: None }));
        assert!(snap.scan_complete() && !snap.scan_stopped());
        let root = snap.root().unwrap();
        let names: Vec<String> = root
            .children()
            .map(|c| String::from_utf8_lossy(c.name()).into_owned())
            .collect();
        assert!(names.contains(&"$MFT".to_string()), "{names:?}");
    }

    #[test]
    fn a_fallback_is_carried_to_the_verdict() {
        let mut events: Vec<_> = batches().into_iter().map(|e| Ok(Some(e))).collect();
        events.push(Ok(Some(done(ScanEnd::Completed, Fallback::MountNotPlaced))));
        let (scanner, _) = scripted(events, false);
        let (_, finish) = run(&scanner);
        let Finish::Ended(FastEnd::Completed { note: Some(note) }) = finish else {
            panic!("{finish:?}");
        };
        assert!(note.contains("couldn't be placed"));
    }

    #[test]
    fn declining_the_prompt_is_a_cancel_not_a_failure() {
        let launcher = Arc::new(Launcher {
            script: Mutex::new(Some(Err(ElevateError::Declined))),
        });
        let scanner = HelperScanner::new(launcher);
        let (_, finish) = run(&scanner);
        assert_eq!(finish, Finish::Ended(FastEnd::Cancelled));
    }

    #[test]
    fn a_launch_that_cannot_happen_says_why() {
        let launcher = Arc::new(Launcher {
            script: Mutex::new(Some(Err(ElevateError::Unavailable(
                bs_platform::elevate::Unavailable::HelperNotInstalled,
            )))),
        });
        let scanner = HelperScanner::new(launcher);
        let (_, finish) = run(&scanner);
        let Finish::Ended(FastEnd::Failed(text)) = finish else {
            panic!("{finish:?}");
        };
        assert!(text.contains("not installed"), "{text}");
        assert!(text.ends_with('.'));
    }

    #[test]
    fn a_helper_that_vanishes_part_way_is_a_failure_and_never_a_complete_tree() {
        let events: Vec<_> = batches().into_iter().map(|e| Ok(Some(e))).collect();
        let (scanner, _) = scripted(events, false);
        let (snap, finish) = run(&scanner);
        let Finish::Ended(FastEnd::Failed(text)) = finish else {
            panic!("{finish:?}");
        };
        assert!(text.contains("closed the connection"), "{text}");
        assert!(
            snap.scan_stopped(),
            "marked stopped, so it could never be shown as complete"
        );
    }

    #[test]
    fn stopping_the_scan_reaches_the_helper_even_while_it_is_silent() {
        let (scanner, script) = scripted(Vec::new(), true);
        let handle = bs_scan::start(scanner.clone(), "/".into(), bs_scan::ScanOptions::default());
        std::thread::sleep(Duration::from_millis(100));
        handle.cancel();
        let result = handle.wait();
        assert!(
            script.cancelled.load(Ordering::SeqCst),
            "the helper was told"
        );
        assert!(!result.snapshot.scan_complete());
        assert_eq!(
            *scanner.finish().lock().unwrap(),
            Finish::Ended(FastEnd::Cancelled)
        );
    }

    #[test]
    fn a_protocol_error_from_the_helper_is_reported_in_words() {
        let events = vec![Err(ElevateError::TimedOut)];
        let (scanner, _) = scripted(events, false);
        let (_, finish) = run(&scanner);
        let Finish::Ended(FastEnd::Failed(text)) = finish else {
            panic!("{finish:?}");
        };
        assert!(text.contains("did not answer in time"), "{text}");
    }

    /// An ordinary scan stand-in: a small tree, then it waits to be let go.
    struct SlowWalk {
        release: Arc<AtomicBool>,
    }

    impl Scanner for SlowWalk {
        fn name(&self) -> &'static str {
            "slow walk"
        }

        fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<bs_scan::ScanEnd, ScanError> {
            let mut b = Batch::new();
            b.dir(DirKey(1), DirKey::NONE, b"/", &meta(4096)).unwrap();
            b.file(NodeKind::File, DirKey(1), b"a", &meta(5_000_000))
                .unwrap();
            b.listing_done(DirKey(1));
            ctx.send(b);
            while !self.release.load(Ordering::SeqCst) {
                if ctx.is_cancelled() {
                    return Ok(bs_scan::ScanEnd::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(bs_scan::ScanEnd::Completed)
        }
    }

    use crate::live::{ScanDriver, Tick};
    use bs_ui::elevation::{Race, Step};
    use std::time::Instant;

    fn wait_finished(driver: &mut ScanDriver) -> Arc<bs_tree::Snapshot> {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            if let Tick::Finished(snapshot, _) = driver.tick(Instant::now()) {
                return snapshot;
            }
            assert!(Instant::now() < until, "the scan never finished");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn has_name(snap: &bs_tree::Snapshot, name: &str) -> bool {
        snap.root()
            .unwrap()
            .children()
            .any(|c| c.name() == name.as_bytes())
    }

    /// Both scans run; the race is decided with the same calls the window
    /// makes, in the order the two finish.
    fn race_in_order(full_first: bool) -> (Step, Option<Arc<bs_tree::Snapshot>>, bool) {
        let mut events: Vec<_> = batches().into_iter().map(|e| Ok(Some(e))).collect();
        events.push(Ok(Some(done(ScanEnd::Completed, Fallback::None))));
        let (helper, script) = scripted(events, true);
        script.gate.store(false, Ordering::SeqCst);
        let finish = helper.finish();
        let release = Arc::new(AtomicBool::new(false));
        let mut walk = ScanDriver::start_scanner(
            Arc::new(SlowWalk {
                release: Arc::clone(&release),
            }),
            "/".into(),
            None,
        );
        let mut fast = ScanDriver::start_scanner(helper, "/".into(), None);
        let mut race = Race::start(true);
        let (step, shown);
        if full_first {
            script.gate.store(true, Ordering::SeqCst);
            let snap = wait_finished(&mut fast);
            let end = match finish.lock().unwrap().clone() {
                Finish::Ended(end) => end,
                Finish::Running => panic!("no verdict"),
            };
            step = race.fast_finished(end);
            // The walk is stopped and dropped.
            drop(walk.discard());
            shown = Some(snap);
        } else {
            release.store(true, Ordering::SeqCst);
            let snap = wait_finished(&mut walk);
            step = race.walk_finished();
            // The full scan is stopped; its late end changes nothing.
            fast.cancel();
            let _ = wait_finished(&mut fast);
            let late = match finish.lock().unwrap().clone() {
                Finish::Ended(end) => end,
                Finish::Running => panic!("no verdict"),
            };
            assert_eq!(race.fast_finished(late), Step::Nothing);
            shown = Some(snap);
        }
        (step, shown, script.cancelled.load(Ordering::SeqCst))
    }

    #[test]
    fn when_the_full_scan_finishes_first_its_tree_is_shown_and_the_walk_is_dropped() {
        let (step, shown, told_to_stop) = race_in_order(true);
        assert_eq!(step, Step::UseFast { note: None });
        let snap = shown.unwrap();
        assert!(snap.scan_complete());
        assert!(has_name(&snap, "$MFT"), "the system files are items");
        assert!(!told_to_stop, "the helper finished by itself");
    }

    #[test]
    fn when_the_walk_finishes_first_its_tree_is_shown_and_the_helper_is_told_to_stop() {
        let (step, shown, told_to_stop) = race_in_order(false);
        assert!(
            matches!(step, Step::UseWalk { message: Some(_) }),
            "{step:?}"
        );
        let snap = shown.unwrap();
        assert!(snap.scan_complete());
        assert!(!has_name(&snap, "$MFT"));
        assert!(told_to_stop, "the full scan was stopped");
    }

    /// A full scan that starts after the ordinary one has ended is the only
    /// scan running. It reports progress, and what stops it reaches the
    /// helper.
    #[test]
    fn a_full_scan_after_the_walk_can_be_watched_and_cancelled() {
        let (helper, script) = scripted(Vec::new(), true);
        let driver = ScanDriver::start_scanner(helper, "/".into(), Some(1_000_000));
        assert!(driver.running(), "a scan of its own");
        assert!(driver.progress().is_some(), "with counters to read");
        assert!(!script.cancelled.load(Ordering::SeqCst));
        drop(driver.discard());
        let until = Instant::now() + Duration::from_secs(5);
        while !script.cancelled.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the helper was never told");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// What was deleted before the full result came in is still deleted in it.
    #[test]
    fn marks_made_before_the_swap_are_on_the_full_result() {
        use bs_tree::DeleteState;
        use bs_ui::elevation::DeleteMarks;
        let mut events: Vec<_> = batches().into_iter().map(|e| Ok(Some(e))).collect();
        events.push(Ok(Some(done(ScanEnd::Completed, Fallback::None))));
        let (helper, _) = scripted(events, false);
        let mut fast = ScanDriver::start_scanner(helper, "/".into(), None);
        let plain = wait_finished(&mut fast);
        let a = plain
            .root()
            .unwrap()
            .children()
            .find(|c| c.name() == b"a")
            .unwrap();
        assert_eq!(a.effective_delete_state(), DeleteState::None);

        let mut marks = DeleteMarks::default();
        marks.note(vec![b"a".to_vec()], DeleteState::InBin);
        marks.note(vec![b"not-in-the-full-tree".to_vec()], DeleteState::Deleted);
        let changes = marks.changes_in(&fast.latest());
        assert_eq!(changes.len(), 1, "an item the tree lacks is skipped");
        assert!(fast.apply_marks(&changes).is_some());
        // The window rewinds the driver; the first picture of the new map
        // carries the marks.
        fast.rewind();
        let shown = wait_finished(&mut fast);
        let a = shown
            .root()
            .unwrap()
            .children()
            .find(|c| c.name() == b"a")
            .unwrap();
        assert_eq!(a.effective_delete_state(), DeleteState::InBin);
    }
}
