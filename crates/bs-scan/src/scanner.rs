//! The scanner interface: what a scan is, how it reports, and how its output
//! reaches a size tree.
//!
//! # Threading model
//!
//! [`start`] launches a coordinator thread that runs the scanner and an
//! applier thread that owns the [`Aggregator`]. The scanner (the walker, or a
//! fast backend) may use as many threads as it likes; they all hand finished
//! batches to [`ScanContext::send`], which feeds a bounded queue to the one
//! applier thread. The applier applies batches in arrival order and
//! publishes snapshots at most once per [`ScanOptions::publish_interval`].
//!
//! The interface thread never touches the aggregator while a scan runs. It
//! reads:
//!
//! * [`ScanHandle::reader`]: a [`SnapshotReader`]. `load()` gives the latest
//!   published snapshot without waiting for any scanning work.
//! * [`ScanHandle::progress`]: counters the scanner updates as it goes, which
//!   are ahead of the published snapshot.
//!
//! When the scan ends, [`ScanHandle::wait`] returns the aggregator (already
//! finished, so it holds only the tree and can still mark and unmark
//! deletions), the final snapshot and the scan's statistics.
//!
//! Cancelling keeps what was counted: the final snapshot is marked stopped
//! and everything unfinished stays incomplete.

use crate::boundary::Boundary;
use crate::classify::Classifier;
use bs_knowledge::Environment;
use bs_tree::{Aggregator, Batch, DirKey, SnapshotReader};
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Why a scan could not run or ended early.
#[derive(Debug)]
#[non_exhaustive]
pub enum ScanError {
    /// The root could not be opened or examined.
    Root(io::Error),
    /// The root is not a directory.
    NotADirectory(PathBuf),
    /// This platform has no walker.
    Unsupported,
    /// A worker thread could not be started.
    Threads(io::Error),
    /// A thread of the scan panicked. The scan was stopped and what had been
    /// counted is kept, marked incomplete. The text is the panic message.
    Panicked(String),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Root(e) => write!(f, "cannot open the scan root: {e}"),
            ScanError::NotADirectory(p) => write!(f, "{} is not a directory", p.display()),
            ScanError::Unsupported => f.write_str("scanning is not supported on this platform"),
            ScanError::Threads(e) => write!(f, "cannot start scan threads: {e}"),
            ScanError::Panicked(m) => write!(f, "the scan stopped after an internal error: {m}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// How a scanner's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanEnd {
    /// Everything reachable was visited.
    Completed,
    /// The scan was cancelled; what was sent is partial.
    Cancelled,
}

/// What the caller decides about a scan.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Which mounts the scan may be in. See [`Boundary`].
    pub boundary: Boundary,
    /// Worker threads; `None` picks a number from the storage type.
    pub threads: Option<usize>,
    /// The volume's used bytes as the system reports them, for progress as a
    /// percentage and for noticing when the counted bytes exceed it. See
    /// [`crate::VolumeInfo`].
    pub os_used_bytes: Option<u64>,
    /// The shortest time between published snapshots.
    pub publish_interval: Duration,
    /// Whether the walker sets each file's category (what colors it on the
    /// map) while it reads the tree. On by default. Turning it off leaves every
    /// entry unclassified, which only helps to measure what classification
    /// costs.
    pub categories: bool,
    /// The places the category lookups resolve against: the user's home
    /// folder, the system folders, and so on. `None`, the default, reads the
    /// process environment once when the scan starts. A caller whose own
    /// environment is not the user's (the elevated helper runs as another
    /// user) gives the user's here instead, so that no scan has to change
    /// the process environment first.
    pub environment: Option<Environment>,
}

impl ScanOptions {
    /// These options with the category lookups resolved against `environment`
    /// instead of the process environment.
    #[must_use]
    pub fn with_environment(mut self, environment: Environment) -> ScanOptions {
        self.environment = Some(environment);
        self
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            boundary: Boundary::root_mount(),
            threads: None,
            os_used_bytes: None,
            publish_interval: Duration::from_millis(100),
            categories: true,
            environment: None,
        }
    }
}

/// Live counters. All values only grow, and saturate at `u64::MAX` rather
/// than wrap (sizes come from the disk, and a damaged or hostile filesystem
/// can report huge ones).
///
/// The byte counters add up **every name** the walker meets, so a file with
/// several hard links is added once per name and the figures can run ahead of
/// the tree. They are for showing that work is happening and for a rough
/// percentage. The exact, de-duplicated totals are in the published
/// snapshot, which is what any figure shown as final must come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanProgress {
    /// Allocated bytes counted so far, once per name (hard links included
    /// each time they are met).
    pub allocated_bytes: u64,
    /// Logical bytes counted so far, once per name.
    pub logical_bytes: u64,
    /// Non-directory entries seen.
    pub files: u64,
    /// Directories seen.
    pub dirs: u64,
    /// Entries or directories that could not be read.
    pub unreadable: u64,
    /// Entries that disappeared between being listed and being examined.
    pub vanished: u64,
    /// Batches handed to the tree.
    pub batches: u64,
    /// Entries whose allocated size is an estimate, because the filesystem
    /// gave no allocation figure (Windows, on filesystems that reject the
    /// handle-based directory classes). Zero everywhere else.
    pub estimated: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    allocated: AtomicU64,
    logical: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    unreadable: AtomicU64,
    vanished: AtomicU64,
    batches: AtomicU64,
    estimated: AtomicU64,
    threads: AtomicU64,
}

/// Adds to a counter without ever wrapping.
fn add_saturating(counter: &AtomicU64, n: u64) {
    if n != 0 {
        // The closure always returns `Some`, so this cannot fail.
        let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_add(n))
        });
    }
}

impl Counters {
    fn snapshot(&self) -> ScanProgress {
        ScanProgress {
            allocated_bytes: self.allocated.load(Ordering::Relaxed),
            logical_bytes: self.logical.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            unreadable: self.unreadable.load(Ordering::Relaxed),
            vanished: self.vanished.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            estimated: self.estimated.load(Ordering::Relaxed),
        }
    }
}

/// A scanner's view of one scan: where to send batches, whether to stop, and
/// where to count.
///
/// Fast backends implement [`Scanner`] and use only this.
pub struct ScanContext {
    tx: SyncSender<Batch>,
    cancel: Arc<AtomicBool>,
    counters: Arc<Counters>,
    // 64 bits so that running past the last usable key can never wrap around
    // to small keys that are already taken.
    next_key: AtomicU64,
    options: ScanOptions,
}

impl ScanContext {
    /// A context that sends to a plain channel, for consumers other than
    /// [`start`] (tests, tools). At most `capacity` batches wait in the
    /// channel; senders block beyond that.
    pub fn channel(options: ScanOptions, capacity: usize) -> (ScanContext, Receiver<Batch>) {
        let (tx, rx) = mpsc::sync_channel(capacity.max(1));
        let ctx = ScanContext {
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(Counters::default()),
            next_key: AtomicU64::new(0),
            options,
        };
        (ctx, rx)
    }

    /// Hands a batch to the tree's applier. Returns `false` when nobody is
    /// listening any more, in which case the scanner should stop.
    pub fn send(&self, batch: Batch) -> bool {
        if batch.is_empty() {
            return true;
        }
        self.counters.batches.fetch_add(1, Ordering::Relaxed);
        self.tx.send(batch).is_ok()
    }

    /// Whether the scan was cancelled. Check it often (at least once per
    /// directory) and return [`ScanEnd::Cancelled`].
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Asks the scan to stop.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The caller's options.
    pub fn options(&self) -> &ScanOptions {
        &self.options
    }

    /// The category lookup this scan uses, or `None` when the caller turned
    /// categories off. It resolves against [`ScanOptions::environment`] when
    /// that is set and against the process environment otherwise, so call it
    /// once, on the thread that starts the scan, before any worker exists.
    pub fn classifier(&self) -> Option<Classifier> {
        self.options
            .categories
            .then(|| match &self.options.environment {
                Some(environment) => Classifier::with_environment(environment),
                None => Classifier::from_process_environment(),
            })
    }

    /// The next directory key. Keys are unique within the scan: a counter
    /// shared by all threads of the scanner. The root takes the first one
    /// (key 0). `None` once the key space is used up; the caller must then
    /// record the directory as unreadable instead of descending.
    pub fn next_dir_key(&self) -> Option<DirKey> {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        // `u32::MAX` is reserved as "no parent"; from there on every call
        // fails.
        u32::try_from(key)
            .ok()
            .filter(|&k| k < DirKey::NONE.0)
            .map(DirKey)
    }

    /// Test hook: starts the key counter at `first`.
    #[doc(hidden)]
    pub fn set_first_dir_key(&self, first: u32) {
        self.next_key.store(u64::from(first), Ordering::Relaxed);
    }

    /// Current progress.
    pub fn progress(&self) -> ScanProgress {
        self.counters.snapshot()
    }

    /// Records the number of worker threads in use (for the statistics).
    pub fn set_threads(&self, threads: usize) {
        self.counters
            .threads
            .store(threads as u64, Ordering::Relaxed);
    }

    /// Adds counted bytes and entries. `files` and `dirs` are entry counts.
    /// Every counter saturates instead of wrapping.
    pub fn count(&self, allocated: u64, logical: u64, files: u64, dirs: u64) {
        let c = &self.counters;
        add_saturating(&c.allocated, allocated);
        add_saturating(&c.logical, logical);
        add_saturating(&c.files, files);
        add_saturating(&c.dirs, dirs);
    }

    /// Adds entries whose allocated size is an estimate.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn count_estimated(&self, n: u64) {
        add_saturating(&self.counters.estimated, n);
    }

    /// Adds entries that could not be read and entries that vanished.
    pub fn count_problems(&self, unreadable: u64, vanished: u64) {
        let c = &self.counters;
        add_saturating(&c.unreadable, unreadable);
        add_saturating(&c.vanished, vanished);
    }
}

/// Something that walks a root and produces batches.
///
/// The directory walker is one; the fast backends are others. The
/// implementation does the work on the calling thread (it may start its own
/// workers) and returns when the walk is done or cancelled. It must send
/// batches whose directory keys are unique within the scan (take them from
/// [`ScanContext::next_dir_key`]) and whose records for any one directory
/// reach the context in the order they were made.
pub trait Scanner: Send + Sync + 'static {
    /// A short name for statistics.
    fn name(&self) -> &'static str;

    /// Scans `root`, sending batches to `ctx`.
    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError>;
}

/// What happened over a whole scan.
#[derive(Debug)]
pub struct ScanStats {
    /// The scanner that produced the records.
    pub scanner: &'static str,
    /// Time from start to the final snapshot.
    pub elapsed: Duration,
    /// Worker threads the scanner reported using (0 if it did not say).
    pub threads: usize,
    /// How the scanner ended; `None` if it failed.
    pub end: Option<ScanEnd>,
    /// Why it failed, if it did.
    pub error: Option<ScanError>,
    /// Final counters.
    pub progress: ScanProgress,
    /// Records the tree applied.
    pub records_applied: u64,
    /// Records the tree had to hold back for a missing parent. A scanner that
    /// keeps its contract has none.
    pub records_parked: u64,
    /// Records the tree refused.
    pub records_rejected: u64,
}

/// The outcome of [`ScanHandle::wait`].
pub struct ScanResult {
    /// The finished aggregator. It holds only the tree and delete marks.
    pub aggregator: Aggregator,
    /// The final published snapshot.
    pub snapshot: Arc<bs_tree::Snapshot>,
    /// Statistics.
    pub stats: ScanStats,
}

/// A scan in progress.
pub struct ScanHandle {
    reader: SnapshotReader,
    cancel: Arc<AtomicBool>,
    counters: Arc<Counters>,
    done: Arc<AtomicBool>,
    scanner: &'static str,
    run: Result<JoinHandle<ScanResult>, io::Error>,
}

impl ScanHandle {
    /// Latest published snapshots, for any thread.
    pub fn reader(&self) -> SnapshotReader {
        self.reader.clone()
    }

    /// Live counters.
    pub fn progress(&self) -> ScanProgress {
        self.counters.snapshot()
    }

    /// Asks the scan to stop. It keeps what it has counted.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether the scan has finished and [`ScanHandle::wait`] will not block.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Waits for the scan to end and returns its result.
    ///
    /// This never panics because of the scan. If a scan thread panicked, or
    /// could not be started, the result carries a [`ScanError`] in
    /// `stats.error` and whatever tree had been built, marked incomplete.
    pub fn wait(self) -> ScanResult {
        match self.run {
            Ok(join) => join.join().unwrap_or_else(|panic| {
                failed_result(
                    self.scanner,
                    ScanError::Panicked(panic_message(&*panic)),
                    self.counters.snapshot(),
                    Duration::ZERO,
                )
            }),
            Err(e) => failed_result(
                self.scanner,
                ScanError::Threads(e),
                ScanProgress::default(),
                Duration::ZERO,
            ),
        }
    }
}

/// The text of a panic payload.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// An empty tree that is finished and marked stopped, for scans that could
/// not produce one.
fn empty_stopped_tree() -> (Aggregator, Arc<bs_tree::Snapshot>) {
    let mut aggregator = Aggregator::new();
    aggregator.mark_stopped();
    aggregator.finish();
    let snapshot = aggregator.publish();
    (aggregator, snapshot)
}

/// The result of a scan that failed before or while producing a tree.
fn failed_result(
    scanner: &'static str,
    error: ScanError,
    progress: ScanProgress,
    elapsed: Duration,
) -> ScanResult {
    let (aggregator, snapshot) = empty_stopped_tree();
    ScanResult {
        aggregator,
        snapshot,
        stats: ScanStats {
            scanner,
            elapsed,
            threads: 0,
            end: None,
            error: Some(error),
            progress,
            records_applied: 0,
            records_parked: 0,
            records_rejected: 0,
        },
    }
}

/// Starts a scan of `root` with `scanner`: the coordinator and applier
/// threads described in the module documentation.
///
/// Nothing that goes wrong inside the scan reaches the caller as a panic or
/// as a hang: a panic in the scanner or in any of its workers cancels the
/// scan and ends it with [`ScanError::Panicked`], and a thread that cannot be
/// started ends it with [`ScanError::Threads`]. Both show up in
/// [`ScanStats::error`] of the result.
pub fn start(scanner: Arc<dyn Scanner>, root: PathBuf, options: ScanOptions) -> ScanHandle {
    let name = scanner.name();
    let mut aggregator = Aggregator::new();
    aggregator.set_os_used_bytes(options.os_used_bytes);
    let reader = aggregator.reader();
    let interval = options.publish_interval;
    let (ctx, rx) = ScanContext::channel(options, 64);
    let cancel = Arc::clone(&ctx.cancel);
    let counters = Arc::clone(&ctx.counters);
    let done = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&done);
    let counters_for_thread = Arc::clone(&counters);

    let run = std::thread::Builder::new()
        .name("bs-scan-coordinator".into())
        .spawn(move || {
            let begun = Instant::now();
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_scan(&*scanner, &root, ctx, aggregator, rx, interval, begun)
            }))
            .unwrap_or_else(|panic| {
                failed_result(
                    name,
                    ScanError::Panicked(panic_message(&*panic)),
                    counters_for_thread.snapshot(),
                    begun.elapsed(),
                )
            });
            done_flag.store(true, Ordering::Release);
            result
        });
    if run.is_err() {
        done.store(true, Ordering::Release);
    }

    ScanHandle {
        reader,
        cancel,
        counters,
        done,
        scanner: name,
        run,
    }
}

/// The coordinator's work: runs the scanner while the applier thread builds
/// the tree, then collects both.
fn run_scan(
    scanner: &dyn Scanner,
    root: &Path,
    ctx: ScanContext,
    aggregator: Aggregator,
    rx: Receiver<Batch>,
    interval: Duration,
    begun: Instant,
) -> ScanResult {
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_for_applier = Arc::clone(&stopped);
    let applier = match std::thread::Builder::new()
        .name("bs-scan-applier".into())
        .spawn(move || run_applier(aggregator, rx, interval, &stopped_for_applier))
    {
        Ok(handle) => handle,
        Err(e) => {
            return failed_result(
                scanner.name(),
                ScanError::Threads(e),
                ctx.counters.snapshot(),
                begun.elapsed(),
            );
        }
    };

    let outcome = catch_unwind(AssertUnwindSafe(|| scanner.scan(root, &ctx)));
    let (end, mut error) = match outcome {
        Ok(Ok(end)) => (Some(end), None),
        Ok(Err(e)) => (None, Some(e)),
        Err(panic) => {
            // Stop whatever threads the scanner left running.
            ctx.cancel();
            (None, Some(ScanError::Panicked(panic_message(&*panic))))
        }
    };
    if end == Some(ScanEnd::Cancelled) || matches!(error, Some(ScanError::Panicked(_))) {
        stopped.store(true, Ordering::Relaxed);
    }
    let progress = ctx.counters.snapshot();
    let threads = usize::try_from(ctx.counters.threads.load(Ordering::Relaxed)).unwrap_or(0);
    // Closing the channel lets the applier finish.
    drop(ctx);
    let (aggregator, snapshot, applied) = match applier.join() {
        Ok(done) => done,
        Err(panic) => {
            // The tree itself failed; what readers last saw stays published,
            // and the caller gets an empty, stopped tree and the reason.
            error.get_or_insert(ScanError::Panicked(format!(
                "tree applier: {}",
                panic_message(&*panic)
            )));
            let (aggregator, snapshot) = empty_stopped_tree();
            (aggregator, snapshot, Applied::default())
        }
    };
    ScanResult {
        aggregator,
        snapshot,
        stats: ScanStats {
            scanner: scanner.name(),
            elapsed: begun.elapsed(),
            threads,
            end,
            error,
            progress,
            records_applied: applied.applied,
            records_parked: applied.parked,
            records_rejected: applied.rejected,
        },
    }
}

#[derive(Default)]
struct Applied {
    applied: u64,
    parked: u64,
    rejected: u64,
}

fn run_applier(
    mut agg: Aggregator,
    rx: Receiver<Batch>,
    interval: Duration,
    stopped: &AtomicBool,
) -> (Aggregator, Arc<bs_tree::Snapshot>, Applied) {
    let mut totals = Applied::default();
    let mut dirty = false;
    loop {
        match rx.recv_timeout(interval) {
            Ok(batch) => {
                let report = agg.apply(&batch);
                totals.applied += u64::from(report.applied);
                totals.parked += u64::from(report.parked);
                totals.rejected += u64::from(report.rejected);
                dirty = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if dirty && agg.publish_if_due(interval) {
            dirty = false;
        }
    }
    if stopped.load(Ordering::Relaxed) {
        agg.mark_stopped();
    }
    // `parked` counts records held back when they arrived; those that later
    // found their parent were applied. What is still held is lost.
    let end = agg.finish();
    totals.rejected += end.unresolved;
    let snapshot = agg.publish();
    (agg, snapshot, totals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool;
    use bs_tree::{DirKey, Meta};
    use std::sync::mpsc;

    /// Waits for a scan on another thread, failing the test instead of
    /// hanging it if the scan never ends.
    fn wait_within(handle: ScanHandle, limit: Duration) -> ScanResult {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(handle.wait());
        });
        rx.recv_timeout(limit)
            .expect("the scan ended within the time limit")
    }

    #[test]
    fn the_classifier_follows_the_options() {
        // A place the tables know wherever the user's home folder is.
        let place = b"/srv/elsewhere/.cache";
        let env = || Environment::new().with("HOME", "/srv/elsewhere");

        // Explicit environment: used, and the process environment is not
        // consulted (no panic, no dependence on the test's own HOME).
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default().with_environment(env()), 1);
        assert!(ctx.options().environment.is_some());
        let classifier = ctx.classifier().expect("categories are on by default");
        assert!(classifier.place(place).is_some());

        // Switched off: no lookup at all, whatever the environment says.
        let options = ScanOptions {
            categories: false,
            ..ScanOptions::default().with_environment(env())
        };
        let (ctx, _rx) = ScanContext::channel(options, 1);
        assert!(ctx.classifier().is_none());

        // The default reads the process environment.
        assert!(ScanOptions::default().environment.is_none());
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 1);
        assert!(ctx.classifier().unwrap().place(place).is_some());
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 1);
        ctx.count(u64::MAX - 5, u64::MAX, 1, 1);
        ctx.count(100, 100, 1, 1);
        ctx.count(u64::MAX, 1, u64::MAX, u64::MAX);
        let p = ctx.progress();
        assert_eq!(p.allocated_bytes, u64::MAX);
        assert_eq!(p.logical_bytes, u64::MAX);
        assert_eq!(p.files, u64::MAX);
        assert_eq!(p.dirs, u64::MAX);
        ctx.count_problems(u64::MAX, 3);
        ctx.count_problems(9, u64::MAX);
        let p = ctx.progress();
        assert_eq!((p.unreadable, p.vanished), (u64::MAX, u64::MAX));
    }

    struct PanicsInScan;

    impl Scanner for PanicsInScan {
        fn name(&self) -> &'static str {
            "panics-in-scan"
        }

        fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
            let mut batch = Batch::new();
            batch
                .dir(DirKey(0), DirKey::NONE, b"/root", &Meta::default())
                .unwrap();
            ctx.send(batch);
            panic!("scanner bug");
        }
    }

    #[test]
    fn a_panic_in_the_scanner_ends_the_scan_with_an_error() {
        let handle = start(
            Arc::new(PanicsInScan),
            PathBuf::from("/"),
            ScanOptions::default(),
        );
        let result = wait_within(handle, Duration::from_secs(30));
        match &result.stats.error {
            Some(ScanError::Panicked(m)) => assert!(m.contains("scanner bug"), "{m}"),
            other => panic!("expected a panic error, got {other:?}"),
        }
        assert_eq!(result.stats.end, None);
        result.snapshot.verify().unwrap();
        // The root was sent but never finished: the tree says so.
        assert!(!result.snapshot.scan_complete());
    }

    /// Sends a root and runs a small tree of tasks on the pool, one of which
    /// panics.
    struct PanicsInWorker {
        threads: usize,
    }

    impl Scanner for PanicsInWorker {
        fn name(&self) -> &'static str {
            "panics-in-worker"
        }

        fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
            let mut batch = Batch::new();
            batch
                .dir(DirKey(0), DirKey::NONE, b"/root", &Meta::default())
                .unwrap();
            ctx.send(batch);
            pool::run(ctx, self.threads, vec![12u32], |w, n| {
                if n == 7 {
                    panic!("task bug {n}");
                }
                if n >= 2 {
                    w.spawn(n - 1);
                    w.spawn(n - 2);
                }
            })
        }
    }

    #[test]
    fn a_panic_in_a_pool_task_ends_the_scan_instead_of_hanging_it() {
        for threads in [1, 2, 8] {
            let handle = start(
                Arc::new(PanicsInWorker { threads }),
                PathBuf::from("/"),
                ScanOptions::default(),
            );
            let result = wait_within(handle, Duration::from_secs(30));
            match &result.stats.error {
                Some(ScanError::Panicked(m)) => assert!(m.contains("task bug 7"), "{m}"),
                other => panic!("{threads} threads: expected a panic error, got {other:?}"),
            }
            assert_eq!(result.stats.end, None);
            result.snapshot.verify().unwrap();
            assert!(!result.snapshot.scan_complete());
        }
    }

    #[test]
    fn a_scan_that_could_not_run_still_gives_a_valid_stopped_tree() {
        // The result handed back when a thread cannot be started or the
        // coordinator itself fails.
        for error in [
            ScanError::Threads(io::Error::from(io::ErrorKind::OutOfMemory)),
            ScanError::Panicked("bug".into()),
        ] {
            let text = error.to_string();
            assert!(!text.is_empty());
            let result = failed_result("test", error, ScanProgress::default(), Duration::ZERO);
            result.snapshot.verify().unwrap();
            assert!(!result.snapshot.scan_complete());
            assert!(result.stats.error.is_some());
            assert_eq!(result.stats.end, None);
        }
    }

    #[test]
    fn a_scan_that_is_cancelled_while_a_task_panics_still_returns() {
        let handle = start(
            Arc::new(PanicsInWorker { threads: 4 }),
            PathBuf::from("/"),
            ScanOptions::default(),
        );
        handle.cancel();
        let result = wait_within(handle, Duration::from_secs(30));
        result.snapshot.verify().unwrap();
        assert!(!result.snapshot.scan_complete());
    }
}
