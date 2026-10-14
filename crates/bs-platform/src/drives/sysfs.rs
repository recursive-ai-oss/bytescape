//! Reads block-device facts from sysfs.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// How many layers of stacked devices (a LUKS volume on an LVM volume on a
/// partition) are followed.
const MAX_STACK_DEPTH: usize = 5;
/// How many underlying devices of one stacked device are examined.
const MAX_SLAVES: usize = 64;

pub(crate) struct Sysfs {
    root: PathBuf,
}

impl Sysfs {
    /// The sysfs of this machine.
    pub fn system() -> Self {
        Self::with_root("/sys")
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Whether the block device with this number is removable.
    ///
    /// A device counts as removable when its path goes through a USB bus, when
    /// the kernel flags its disk as removable, or when it is an SD card.
    /// Stacked devices (device-mapper, RAID) are removable when anything they
    /// are built on is. A device that cannot be found is not removable.
    pub fn is_removable(&self, major: u32, minor: u32) -> bool {
        let link = self.root.join("dev/block").join(format!("{major}:{minor}"));
        self.check(&link, 0)
    }

    fn check(&self, link: &Path, depth: usize) -> bool {
        let Ok(dir) = fs::canonicalize(link) else {
            return false;
        };
        if self.on_usb_bus(&dir) {
            return true;
        }
        // A partition has no flags of its own; its disk is the parent.
        let disk = if dir.join("partition").exists() {
            dir.parent().unwrap_or(&dir)
        } else {
            dir.as_path()
        };
        if read_trimmed(&disk.join("removable")).as_deref() == Some("1") {
            return true;
        }
        if read_trimmed(&disk.join("device/type")).as_deref() == Some("SD") {
            return true;
        }
        if depth >= MAX_STACK_DEPTH {
            return false;
        }
        let Ok(slaves) = fs::read_dir(dir.join("slaves")) else {
            return false;
        };
        slaves
            .flatten()
            .take(MAX_SLAVES)
            .any(|slave| self.check(&slave.path(), depth + 1))
    }

    /// Whether the path below the sysfs root has a `usbN` component, as in
    /// `devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/.../block/sdb`.
    fn on_usb_bus(&self, resolved: &Path) -> bool {
        let root = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let relative = resolved.strip_prefix(&root).unwrap_or(resolved);
        relative.components().any(|component| match component {
            Component::Normal(name) => name
                .to_str()
                .and_then(|n| n.strip_prefix("usb"))
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())),
            _ => false,
        })
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    Some(text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;

    /// A disk `name` at `devices/<bus>/block/<name>` with one partition, and
    /// the `dev/block/M:m` links that point at them.
    fn disk(fx: &Fixture, bus: &str, name: &str, removable: &str, major: u32) {
        let base = format!("devices/{bus}/block/{name}");
        fx.write(&format!("{base}/removable"), removable);
        fx.write(&format!("{base}/{name}1/partition"), "1\n");
        fx.symlink(&format!("../../{base}"), &format!("dev/block/{major}:0"));
        fx.symlink(
            &format!("../../{base}/{name}1"),
            &format!("dev/block/{major}:1"),
        );
    }

    #[test]
    fn internal_disk_is_not_removable() {
        let fx = Fixture::new("sysfs-internal");
        disk(
            &fx,
            "pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0",
            "sda",
            "0\n",
            8,
        );
        let sysfs = Sysfs::with_root(fx.path());
        assert!(!sysfs.is_removable(8, 0));
        assert!(!sysfs.is_removable(8, 1));
    }

    #[test]
    fn usb_path_makes_a_disk_and_its_partitions_removable() {
        let fx = Fixture::new("sysfs-usb");
        // The kernel reports many USB disks as not removable; the bus decides.
        disk(
            &fx,
            "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0",
            "sdb",
            "0\n",
            8,
        );
        let sysfs = Sysfs::with_root(fx.path());
        assert!(sysfs.is_removable(8, 0));
        assert!(sysfs.is_removable(8, 1));
    }

    #[test]
    fn removable_flag_alone_is_enough_and_is_read_from_the_disk_for_a_partition() {
        let fx = Fixture::new("sysfs-flag");
        disk(
            &fx,
            "pci0000:00/0000:00:1f.2/ata2/host1/target1:0:0/1:0:0:0",
            "sdc",
            "1\n",
            8,
        );
        let sysfs = Sysfs::with_root(fx.path());
        assert!(sysfs.is_removable(8, 1));
    }

    #[test]
    fn sd_card_in_a_built_in_slot_is_removable() {
        let fx = Fixture::new("sysfs-sd");
        let base = "devices/pci0000:00/0000:00:1d.0/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0";
        fx.write(&format!("{base}/removable"), "0\n");
        fx.write(&format!("{base}/device/type"), "SD\n");
        fx.symlink(&format!("../../{base}"), "dev/block/179:0");
        assert!(Sysfs::with_root(fx.path()).is_removable(179, 0));
    }

    #[test]
    fn emmc_is_not_removable() {
        let fx = Fixture::new("sysfs-emmc");
        let base = "devices/platform/soc/mmc_host/mmc1/mmc1:0001/block/mmcblk1";
        fx.write(&format!("{base}/removable"), "0\n");
        fx.write(&format!("{base}/device/type"), "MMC\n");
        fx.symlink(&format!("../../{base}"), "dev/block/179:8");
        assert!(!Sysfs::with_root(fx.path()).is_removable(179, 8));
    }

    #[test]
    fn device_mapper_inherits_from_what_it_is_built_on() {
        let fx = Fixture::new("sysfs-dm");
        disk(
            &fx,
            "pci0000:00/0000:00:14.0/usb2/2-2/2-2:1.0/host7/target7:0:0/7:0:0:0",
            "sdd",
            "0\n",
            8,
        );
        disk(
            &fx,
            "pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0",
            "sda",
            "0\n",
            9,
        );
        // dm-0 sits on the USB partition, dm-1 on the internal one.
        fx.write("devices/virtual/block/dm-0/removable", "0\n");
        fx.symlink(
            "../../../../pci0000:00/0000:00:14.0/usb2/2-2/2-2:1.0/host7/target7:0:0/7:0:0:0/block/sdd/sdd1",
            "devices/virtual/block/dm-0/slaves/sdd1",
        );
        fx.symlink("../../devices/virtual/block/dm-0", "dev/block/253:0");
        fx.write("devices/virtual/block/dm-1/removable", "0\n");
        fx.symlink(
            "../../../../pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda/sda1",
            "devices/virtual/block/dm-1/slaves/sda1",
        );
        fx.symlink("../../devices/virtual/block/dm-1", "dev/block/253:1");
        let sysfs = Sysfs::with_root(fx.path());
        assert!(sysfs.is_removable(253, 0));
        assert!(!sysfs.is_removable(253, 1));
    }

    #[test]
    fn a_stack_loop_terminates() {
        let fx = Fixture::new("sysfs-loop");
        fx.write("devices/virtual/block/dm-0/removable", "0\n");
        fx.write("devices/virtual/block/dm-1/removable", "0\n");
        fx.symlink("../../dm-1", "devices/virtual/block/dm-0/slaves/dm-1");
        fx.symlink("../../dm-0", "devices/virtual/block/dm-1/slaves/dm-0");
        fx.symlink("../../devices/virtual/block/dm-0", "dev/block/253:0");
        assert!(!Sysfs::with_root(fx.path()).is_removable(253, 0));
    }

    #[test]
    fn unknown_devices_and_missing_sysfs_are_not_removable() {
        let fx = Fixture::new("sysfs-missing");
        assert!(!Sysfs::with_root(fx.path()).is_removable(8, 0));
        assert!(!Sysfs::with_root(fx.path().join("nonexistent")).is_removable(8, 0));
    }

    #[test]
    fn usb_component_must_be_usb_followed_by_digits() {
        let fx = Fixture::new("sysfs-usbname");
        disk(
            &fx,
            "pci0000:00/usbhub-controller/usbx/ata1/host0/0:0:0:0",
            "sde",
            "0\n",
            8,
        );
        assert!(!Sysfs::with_root(fx.path()).is_removable(8, 0));
    }
}
