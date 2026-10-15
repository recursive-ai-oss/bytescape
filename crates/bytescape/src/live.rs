//! Running a scan beside the window: which snapshot to lay out and when.
//!
//! The walker and the tree's applier run on their own threads. The window only
//! asks for the latest published snapshot, never waits for scanning work, and
//! holds no lock while it draws.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use bs_scan::{ScanHandle, ScanOptions, ScanStats, Scanner, StatvfsVolume, VolumeInfo, Walker};
use bs_tree::{Aggregator, Snapshot, SnapshotReader};
use bs_ui::tree::relayout_interval;

/// How often the scanner publishes snapshots. The window lays out less often
/// than this (see `relayout_interval`); a short interval makes the first
/// picture of a big tree arrive quickly.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);

/// What the window should do after asking.
pub enum Tick {
    /// Nothing new, or too soon to lay out again.
    Idle,
    /// A newer snapshot to lay out.
    Data(Arc<Snapshot>),
    /// The scan ended. The snapshot is the final one; the text says why when
    /// it could not run.
    Finished(Arc<Snapshot>, Option<String>),
}

/// A scan in progress and the window's throttle for laying it out.
pub struct ScanDriver {
    handle: Option<ScanHandle>,
    reader: SnapshotReader,
    started: Instant,
    last_epoch: u64,
    last_laid_out: Option<Instant>,
    last_cost: Duration,
    /// The finished tree, kept for the delete marks later work will make.
    finished: Option<(Aggregator, ScanStats)>,
    /// How the scan ended, kept so that a driver that finished in the
    /// background can be shown at once when its drive is picked.
    outcome: Option<(Arc<Snapshot>, Option<String>)>,
    replay: bool,
}

impl ScanDriver {
    /// Starts scanning `root`. Returns at once.
    pub fn start(root: PathBuf) -> ScanDriver {
        let root = std::path::absolute(&root).unwrap_or(root);
        let used = StatvfsVolume.used_bytes(&root).ok();
        Self::start_with(root, used)
    }

    /// Starts scanning `root`, measuring progress against `os_used_bytes`
    /// (what the system reports as used on the drive), when known.
    pub fn start_with(root: PathBuf, os_used_bytes: Option<u64>) -> ScanDriver {
        Self::start_scanner(Arc::new(Walker), root, os_used_bytes)
    }

    /// Starts a scan with any scanner (the walker, or a stand-in in tests).
    pub fn start_scanner(
        scanner: Arc<dyn Scanner>,
        root: PathBuf,
        os_used_bytes: Option<u64>,
    ) -> ScanDriver {
        let options = ScanOptions {
            os_used_bytes,
            publish_interval: PUBLISH_INTERVAL,
            ..ScanOptions::default()
        };
        let handle = bs_scan::start(scanner, root, options);
        ScanDriver {
            reader: handle.reader(),
            handle: Some(handle),
            started: Instant::now(),
            last_epoch: 0,
            last_laid_out: None,
            last_cost: Duration::ZERO,
            finished: None,
            outcome: None,
            replay: false,
        }
    }

    /// The latest published snapshot, whatever the throttle says.
    pub fn latest(&self) -> Arc<Snapshot> {
        self.reader.load()
    }

    /// Counters of the running scan (ahead of the published snapshot).
    pub fn progress(&self) -> Option<bs_scan::ScanProgress> {
        self.handle.as_ref().map(ScanHandle::progress)
    }

    /// Time since the scan started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Makes the next ticks offer the current picture again, as if nothing
    /// had been laid out yet. A finished scan offers its final snapshot.
    pub fn rewind(&mut self) {
        self.last_epoch = 0;
        self.last_laid_out = None;
        self.replay = self.outcome.is_some();
    }

    /// Stops the scan and releases its tree on another thread. Nothing waits
    /// for it; the returned channel receives one message once everything the
    /// scan held (tree, snapshots, worker threads) has been dropped.
    pub fn discard(mut self) -> Receiver<()> {
        let (done_tx, done_rx) = mpsc::channel();
        match self.handle.take() {
            Some(handle) => {
                handle.cancel();
                let spawned = std::thread::Builder::new()
                    .name("scan-discard".into())
                    .spawn(move || {
                        drop(handle.wait());
                        let _ = done_tx.send(());
                    });
                // Without a thread there is nothing to wait for.
                drop(spawned);
            }
            None => {
                let _ = done_tx.send(());
            }
        }
        done_rx
    }

    /// Whether the scan is still running.
    pub fn running(&self) -> bool {
        self.handle.is_some()
    }

    /// Asks for new work. Cheap: it loads a pointer and compares epochs.
    pub fn tick(&mut self, now: Instant) -> Tick {
        if self.replay {
            self.replay = false;
            if let Some((snapshot, problem)) = &self.outcome {
                return Tick::Finished(Arc::clone(snapshot), problem.clone());
            }
        }
        if self.handle.as_ref().is_some_and(ScanHandle::is_done)
            && let Some(handle) = self.handle.take()
        {
            let result = handle.wait();
            let problem = result
                .stats
                .error
                .as_ref()
                .map(|e| format!("Couldn't scan: {e}"));
            let snapshot = result.snapshot;
            self.finished = Some((result.aggregator, result.stats));
            self.outcome = Some((Arc::clone(&snapshot), problem.clone()));
            return Tick::Finished(snapshot, problem);
        }
        let due = self.last_laid_out.is_none_or(|t| {
            now.saturating_duration_since(t)
                >= relayout_interval(now.saturating_duration_since(self.started), self.last_cost)
        });
        if due {
            let snapshot = self.reader.load();
            if !snapshot.is_empty() && snapshot.epoch() != self.last_epoch {
                self.last_epoch = snapshot.epoch();
                self.last_laid_out = Some(now);
                return Tick::Data(snapshot);
            }
        }
        Tick::Idle
    }

    /// Records what the last layout cost, so slow ones are spaced further
    /// apart.
    pub fn note_layout_cost(&mut self, cost: Duration) {
        self.last_cost = cost;
    }

    /// The statistics of a finished scan.
    pub fn stats(&self) -> Option<&ScanStats> {
        self.finished.as_ref().map(|(_, s)| s)
    }

    /// Stops a running scan; what was counted stays.
    pub fn cancel(&self) {
        if let Some(h) = &self.handle {
            h.cancel();
        }
    }
}

/// Files per second over about the last second, so the readout follows the
/// scan's speed now rather than its average since the start.
#[derive(Default)]
pub struct RateMeter {
    last: Option<(Instant, u64)>,
    rate: f64,
}

impl RateMeter {
    /// Notes that `files` have been counted at `now`; returns the rate.
    pub fn sample(&mut self, now: Instant, files: u64) -> f64 {
        match self.last {
            None => self.last = Some((now, files)),
            Some((t, f)) => {
                let dt = now.saturating_duration_since(t).as_secs_f64();
                if dt >= 0.5 {
                    let instant = files.saturating_sub(f) as f64 / dt;
                    self.rate = if self.rate == 0.0 {
                        instant
                    } else {
                        self.rate * 0.5 + instant * 0.5
                    };
                    self.last = Some((now, files));
                }
            }
        }
        self.rate
    }
}

/// Scans `root` to the end without a window, for measurements and pictures.
pub fn scan_to_end(root: PathBuf) -> Result<(Arc<Snapshot>, ScanStats), String> {
    let mut driver = ScanDriver::start(root);
    loop {
        match driver.tick(Instant::now()) {
            Tick::Finished(snapshot, problem) => {
                if let Some(p) = problem {
                    return Err(p);
                }
                let stats = driver.finished.take().map(|(_, s)| s);
                return stats
                    .map(|s| (snapshot, s))
                    .ok_or_else(|| "scan produced no statistics".to_string());
            }
            Tick::Data(_) | Tick::Idle => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Scans until `files` entries are counted, then returns the snapshot at that
/// moment with the scan still running, and the driver so the caller can stop
/// it.
pub fn scan_until(root: PathBuf, files: u32) -> Result<(Arc<Snapshot>, ScanDriver), String> {
    let driver = ScanDriver::start(root);
    // A mid-scan picture wants every published snapshot, not a throttled one.
    loop {
        let snapshot = driver.reader.load();
        if let Some(root) = snapshot.root()
            && root.file_count() >= files
        {
            if !root.is_in_progress() {
                return Err(format!(
                    "the scan finished before {files} files were counted; use a smaller --midscan-files"
                ));
            }
            return Ok((snapshot, driver));
        }
        if driver.handle.as_ref().is_some_and(ScanHandle::is_done) {
            return Err(format!(
                "the scan finished before {files} files were counted; use a smaller --midscan-files"
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(dir: &std::path::Path) {
        for d in 0..3 {
            let sub = dir.join(format!("d{d}"));
            fs::create_dir_all(&sub).expect("mkdir");
            for f in 0..5 {
                fs::write(sub.join(format!("f{f}")), vec![0u8; 5000 * (f + 1)]).expect("write");
            }
        }
    }

    #[test]
    fn a_scan_ends_with_a_complete_snapshot_and_statistics() {
        let dir = std::env::temp_dir().join(format!("bytescape-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        tree(&dir);
        let (snapshot, stats) = scan_to_end(dir.clone()).expect("scan");
        let _ = fs::remove_dir_all(&dir);
        assert!(snapshot.scan_complete());
        assert_eq!(snapshot.root().map(|r| r.file_count()), Some(15));
        assert!(stats.end.is_some());
    }

    #[test]
    fn a_driver_that_finished_in_the_background_can_be_shown_again() {
        let dir = std::env::temp_dir().join(format!("bytescape-replay-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        tree(&dir);
        let mut driver = ScanDriver::start(dir.clone());
        loop {
            if matches!(driver.tick(Instant::now()), Tick::Finished(..)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(driver.tick(Instant::now()), Tick::Idle));
        driver.rewind();
        let Tick::Finished(s, problem) = driver.tick(Instant::now()) else {
            panic!("the final picture is offered again");
        };
        let _ = fs::remove_dir_all(&dir);
        assert!(problem.is_none() && s.scan_complete());
    }

    /// A scanner that makes `dirs × files` entries out of nothing and then
    /// waits to be stopped, like a long scan of a big drive.
    struct Synthetic {
        dirs: u32,
        files: u32,
    }

    impl Scanner for Synthetic {
        fn name(&self) -> &'static str {
            "synthetic"
        }

        fn scan(
            &self,
            _root: &std::path::Path,
            ctx: &bs_scan::ScanContext,
        ) -> Result<bs_scan::ScanEnd, bs_scan::ScanError> {
            use bs_tree::{Batch, Category, Meta, NodeKind};
            let meta = Meta {
                logical: 4096,
                allocated: 4096,
                category: Category(1),
                ..Meta::default()
            };
            let root = ctx.next_dir_key().expect("root key");
            let mut b = Batch::new();
            let _ = b.dir(root, bs_tree::DirKey::NONE, b"/", &meta);
            ctx.send(b);
            for d in 0..self.dirs {
                let key = ctx.next_dir_key().expect("key");
                let mut b = Batch::new();
                let _ = b.dir(key, root, format!("directory-{d}").as_bytes(), &meta);
                for f in 0..self.files {
                    let _ = b.file(
                        NodeKind::File,
                        key,
                        format!("file-with-a-longish-name-{f}.dat").as_bytes(),
                        &meta,
                    );
                }
                b.listing_done(key);
                if !ctx.send(b) {
                    return Ok(bs_scan::ScanEnd::Cancelled);
                }
            }
            while !ctx.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(bs_scan::ScanEnd::Cancelled)
        }
    }

    /// Picking a drive stops the scans of the others and releases their trees:
    /// the discard thread finishes, and nothing keeps a snapshot of the tree
    /// alive afterwards.
    #[test]
    fn discarding_a_scan_releases_its_tree() {
        let driver = ScanDriver::start_scanner(
            Arc::new(Synthetic {
                dirs: 200,
                files: 500,
            }),
            PathBuf::from("/synthetic"),
            None,
        );
        let mut watch = None;
        for _ in 0..4000 {
            let snap = driver.latest();
            if snap.root().is_some_and(|r| r.file_count() >= 90_000) {
                watch = Some(Arc::downgrade(&snap));
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let watch = watch.expect("the scan counted its entries");
        assert!(watch.strong_count() >= 1, "the published snapshot is alive");
        let done = driver.discard();
        done.recv_timeout(Duration::from_secs(20))
            .expect("the discarded scan ended and dropped its tree");
        assert_eq!(
            watch.strong_count(),
            0,
            "a snapshot of the discarded scan is still held"
        );
    }

    #[test]
    fn the_rate_follows_the_last_second() {
        let mut m = RateMeter::default();
        let t0 = Instant::now();
        assert_eq!(m.sample(t0, 0), 0.0);
        assert_eq!(
            m.sample(t0 + Duration::from_millis(100), 500),
            0.0,
            "too soon"
        );
        let r = m.sample(t0 + Duration::from_secs(1), 10_000);
        assert!((r - 10_000.0).abs() < 1.0, "{r}");
        let r = m.sample(t0 + Duration::from_secs(2), 10_000);
        assert!(r < 6_000.0, "slows down when the scan does: {r}");
    }

    #[test]
    fn a_missing_folder_reports_a_problem_instead_of_hanging() {
        let result = scan_to_end(PathBuf::from("/definitely/not/here"));
        let Err(text) = result else {
            panic!("expected a problem");
        };
        assert!(text.starts_with("Couldn't scan"), "{text}");
    }

    #[test]
    fn the_first_snapshot_is_offered_at_once_and_repeats_are_not() {
        let dir = std::env::temp_dir().join(format!("bytescape-tick-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        tree(&dir);
        let mut driver = ScanDriver::start(dir.clone());
        let mut data = 0;
        let mut finished = false;
        for _ in 0..2000 {
            match driver.tick(Instant::now()) {
                Tick::Data(_) => data += 1,
                Tick::Finished(s, problem) => {
                    assert!(problem.is_none());
                    assert!(s.scan_complete());
                    finished = true;
                    break;
                }
                Tick::Idle => {}
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(finished, "the scan finishes");
        // The throttle lets very few layouts through a scan this short.
        assert!(data <= 3, "{data} layouts for a tiny scan");
        assert!(!driver.running());
        assert!(driver.stats().is_some());
    }
}
