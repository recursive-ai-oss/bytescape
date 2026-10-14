//! The system's used-bytes figure for a volume, the denominator of a progress
//! percentage.
//!
//! `bs-platform` reports capacity too. The walker is built into the elevated
//! helper, which does not link `bs-platform`, so it asks through this small
//! trait instead; the interface passes `bs-platform`'s number in
//! [`ScanOptions::os_used_bytes`](crate::ScanOptions::os_used_bytes) or lets
//! [`StatvfsVolume`] work it out.

use std::io;
use std::path::Path;

/// Answers how many bytes the system says are in use on the volume holding a
/// path.
pub trait VolumeInfo {
    /// Used bytes: total minus free, where free excludes the reserve that
    /// only privileged users can use, so the figure is comparable to the sum of
    /// allocated sizes.
    fn used_bytes(&self, path: &Path) -> io::Result<u64>;
}

/// A fixed answer, for tests.
#[derive(Clone, Copy, Debug)]
pub struct FixedVolume(pub u64);

impl VolumeInfo for FixedVolume {
    fn used_bytes(&self, _path: &Path) -> io::Result<u64> {
        Ok(self.0)
    }
}

/// The operating system's own answer.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatvfsVolume;

#[cfg(target_os = "linux")]
impl VolumeInfo for StatvfsVolume {
    fn used_bytes(&self, path: &Path) -> io::Result<u64> {
        crate::linux::sys::statvfs_used(path)
    }
}

#[cfg(windows)]
impl VolumeInfo for StatvfsVolume {
    fn used_bytes(&self, path: &Path) -> io::Result<u64> {
        crate::windows::sys::volume_used(path)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
impl VolumeInfo for StatvfsVolume {
    fn used_bytes(&self, _path: &Path) -> io::Result<u64> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_volume_answers_what_it_was_given() {
        assert_eq!(FixedVolume(42).used_bytes(Path::new("/")).unwrap(), 42);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_real_volume_reports_something_for_the_current_directory() {
        let used = StatvfsVolume.used_bytes(Path::new(".")).unwrap();
        assert!(used > 0);
    }
}
