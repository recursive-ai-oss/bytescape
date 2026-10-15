//! Where a scan may go: the mounts it may be in.
//!
//! A scan covers one drive. On Linux a drive can be several mounts of the same
//! filesystem (a `/home` subvolume mounted under `/`, say), and nested btrfs
//! subvolumes appear inside one mount with a different device number. The walker
//! must enter those and nothing else: a mount of another filesystem is another
//! drive, however deep in the tree it sits.
//!
//! The caller says which mounts belong to the drive by their mount IDs (the
//! `stx_mnt_id` that `statx` returns and the first field of
//! `/proc/self/mountinfo`). A bind mount of a folder the scan reaches anyway
//! must be left out of the list, or its files would be counted twice.
//!
//! The application builds the list from the platform crate's drive: the mount
//! IDs of the drive's mounts that no other mount of the drive already shows
//! (`Mount::mount_id` where `covered_by_other_mount` is false).
//!
//! Where the kernel does not report mount IDs (before Linux 5.8) the walker
//! falls back to device numbers: it stays on the root's device plus any device
//! the caller lists. That fallback cannot tell a bind mount from the original,
//! and it refuses nested btrfs subvolumes, because each has its own device
//! number.

use std::collections::HashSet;

/// The mounts a scan may enter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Boundary {
    mount_ids: HashSet<u64>,
    devices: HashSet<u64>,
}

/// What a directory entry says about where it lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The mount ID, when the kernel reported one.
    pub mount_id: Option<u64>,
    /// The device number, major in the high 32 bits and minor in the low.
    pub device: u64,
}

impl Boundary {
    /// Only the mount that holds the scan root. This is the conservative default for a
    /// path whose drive is not known: nothing is entered that was not asked
    /// for.
    pub fn root_mount() -> Boundary {
        Boundary::default()
    }

    /// The root's mount plus these mounts, by mount ID. Typically the mounts
    /// of a `bs-platform` drive that are not covered by another mount.
    pub fn with_mount_ids(ids: impl IntoIterator<Item = u64>) -> Boundary {
        Boundary {
            mount_ids: ids.into_iter().collect(),
            devices: HashSet::new(),
        }
    }

    /// Also allows these device numbers, for kernels that do not report mount
    /// IDs.
    pub fn and_devices(mut self, devices: impl IntoIterator<Item = u64>) -> Boundary {
        self.devices.extend(devices);
        self
    }

    /// Adds the root's own placement: the mount holding the scan root is
    /// always inside.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn including(&self, root: Placement) -> Boundary {
        let mut copy = self.clone();
        match root.mount_id {
            Some(id) => {
                copy.mount_ids.insert(id);
            }
            None => {
                copy.devices.insert(root.device);
            }
        }
        copy
    }

    /// Whether the scan may enter a directory placed like this.
    pub fn allows(&self, at: Placement) -> bool {
        match at.mount_id {
            Some(id) => self.mount_ids.contains(&id),
            None => self.devices.contains(&at.device),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mount_id: Option<u64>, device: u64) -> Placement {
        Placement { mount_id, device }
    }

    #[test]
    fn only_the_roots_mount_by_default() {
        let b = Boundary::root_mount().including(at(Some(10), 1));
        assert!(b.allows(at(Some(10), 1)));
        // A nested subvolume: same mount, other device.
        assert!(b.allows(at(Some(10), 2)));
        // Another mount, even of the same device number.
        assert!(!b.allows(at(Some(11), 1)));
    }

    #[test]
    fn listed_mounts_are_entered_and_unlisted_are_not() {
        let b = Boundary::with_mount_ids([20, 21]).including(at(Some(20), 1));
        assert!(b.allows(at(Some(21), 9)));
        assert!(!b.allows(at(Some(22), 9)));
    }

    #[test]
    fn without_mount_ids_devices_decide() {
        let b = Boundary::root_mount()
            .and_devices([7])
            .including(at(None, 5));
        assert!(b.allows(at(None, 5)));
        assert!(b.allows(at(None, 7)));
        assert!(!b.allows(at(None, 8)));
        // Mount IDs and devices do not mix: an entry with a mount ID is judged
        // by it alone.
        assert!(!b.allows(at(Some(5), 5)));
    }
}
