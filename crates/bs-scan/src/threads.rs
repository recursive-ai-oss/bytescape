//! How many threads to walk with.
//!
//! Reading directory metadata is latency-bound: on flash storage many requests
//! in flight overlap and throughput grows with threads, while on a spinning
//! disk too many threads only add seeks. The block device behind the scan
//! root says which it is: `/sys/dev/block/MAJOR:MINOR/queue/rotational`, or
//! the same file one level up for a partition.

use std::path::Path;

/// Whether the device is a spinning disk, from a sysfs tree rooted at `sysfs`
/// (normally `/sys`). `None` when sysfs does not say (virtual devices such as
/// tmpfs have no queue).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn rotational_at(sysfs: &Path, major: u32, minor: u32) -> Option<bool> {
    if major == 0 {
        return None;
    }
    let dev = sysfs.join("dev/block").join(format!("{major}:{minor}"));
    // A whole disk has its queue here; a partition's disk is the parent.
    for candidate in [
        dev.join("queue/rotational"),
        dev.join("../queue/rotational"),
    ] {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            match text.trim() {
                "1" => return Some(true),
                "0" => return Some(false),
                _ => {}
            }
        }
    }
    None
}

/// The thread count to use when the caller does not choose one.
pub(crate) fn auto_threads(rotational: Option<bool>, available: usize) -> usize {
    let available = available.max(1);
    match rotational {
        // Spinning disks: a few requests in flight let the drive reorder them,
        // more only add seeks.
        Some(true) => available.min(4),
        // Flash or unknown (network, virtual): as many as the machine has,
        // within reason.
        _ => available.min(MAX_AUTO_THREADS),
    }
}

/// Upper bound on automatically chosen threads. Directory reads scale well
/// past this, but the tree's single applier thread does not, so beyond it more
/// walker threads add contention rather than speed (measured on a 16-core
/// machine with flash storage; see the benchmark script).
pub(crate) const MAX_AUTO_THREADS: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that removes itself when dropped, even if the test that
    /// made it fails. It lives beside the test executable (in the build
    /// directory), not in the system's temporary directory.
    struct Sysfs(std::path::PathBuf);

    impl std::ops::Deref for Sysfs {
        type Target = std::path::Path;

        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_sysfs(files: &[(&str, &str)]) -> Sysfs {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let exe = std::env::current_exe().unwrap();
        let base = exe.parent().unwrap().join(format!(
            "bs-scan-sysfs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let sysfs = Sysfs(base.clone());
        for (rel, content) in files {
            let path = base.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        sysfs
    }

    #[test]
    fn whole_disk_and_partition_are_both_found() {
        let sys = fake_sysfs(&[
            ("dev/block/8:0/queue/rotational", "1\n"),
            ("dev/block/259:0/queue/rotational", "0\n"),
            // Partition 8:1 has no queue of its own; its parent directory is
            // the disk's. Model the symlink with a sibling layout.
            ("dev/block/8:1/placeholder", ""),
            ("dev/queue/rotational", "1\n"),
        ]);
        assert_eq!(rotational_at(&sys, 8, 0), Some(true));
        assert_eq!(rotational_at(&sys, 259, 0), Some(false));
        // 8:1 -> ../queue/rotational resolves to dev/block/queue/..., which the
        // fake does not have: unknown.
        assert_eq!(rotational_at(&sys, 8, 1), None);
        // Virtual devices (major 0) are never looked up.
        assert_eq!(rotational_at(&sys, 0, 44), None);
        assert_eq!(rotational_at(&sys, 9, 9), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_partition_uses_its_disks_queue() {
        // Real sysfs: /sys/dev/block/8:1 is a symlink to .../block/sda/sda1, so
        // ../queue is the disk's. Build that shape.
        let sys = fake_sysfs(&[
            ("devices/block/sda/queue/rotational", "1\n"),
            ("devices/block/sda/sda1/uevent", ""),
        ]);
        let link_dir = sys.join("dev/block");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::os::unix::fs::symlink(sys.join("devices/block/sda/sda1"), link_dir.join("8:1"))
            .unwrap();
        assert_eq!(rotational_at(&sys, 8, 1), Some(true));
    }

    #[test]
    fn garbage_in_sysfs_is_unknown() {
        let sys = fake_sysfs(&[("dev/block/8:0/queue/rotational", "maybe\n")]);
        assert_eq!(rotational_at(&sys, 8, 0), None);
    }

    #[test]
    fn thread_counts() {
        assert_eq!(auto_threads(Some(true), 32), 4);
        assert_eq!(auto_threads(Some(true), 2), 2);
        assert_eq!(auto_threads(Some(false), 16), 16);
        assert_eq!(auto_threads(None, 64), MAX_AUTO_THREADS);
        assert_eq!(auto_threads(Some(false), 0), 1);
    }
}
