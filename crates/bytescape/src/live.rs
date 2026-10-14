//! Running a scan beside the window: which snapshot to lay out and when.
//!
//! The walker and the tree's applier run on their own threads. The window only
//! asks for the latest published snapshot, never waits for scanning work, and
//! holds no lock while it draws.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bs_scan::{ScanHandle, ScanOptions, ScanStats, StatvfsVolume, VolumeInfo, Walker};
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
}

impl ScanDriver {
    /// Starts scanning `root`. Returns at once.
    pub fn start(root: PathBuf) -> ScanDriver {
        let root = std::path::absolute(&root).unwrap_or(root);
        let options = ScanOptions {
            os_used_bytes: StatvfsVolume.used_bytes(&root).ok(),
            publish_interval: PUBLISH_INTERVAL,
            ..ScanOptions::default()
        };
        let handle = bs_scan::start(Arc::new(Walker), root, options);
        ScanDriver {
            reader: handle.reader(),
            handle: Some(handle),
            started: Instant::now(),
            last_epoch: 0,
            last_laid_out: None,
            last_cost: Duration::ZERO,
            finished: None,
        }
    }

    /// Whether the scan is still running.
    pub fn running(&self) -> bool {
        self.handle.is_some()
    }

    /// Asks for new work. Cheap: it loads a pointer and compares epochs.
    pub fn tick(&mut self, now: Instant) -> Tick {
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
