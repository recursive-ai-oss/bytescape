//! Linux drive enumeration from `/proc/self/mountinfo`, sysfs and `statvfs`.

#![allow(unsafe_code)]

use super::mountinfo::{self, MountEntry};
use super::probe::{InFlight, Job, run_parallel};
use super::rules::{self, Candidate, Devices};
use super::sysfs::Sysfs;
use super::{
    CAPACITY_TIMEOUT, Capacity, CapacityProblem, Drive, DriveError, DriveId, Mount,
    eligible_for_background_scan, sort_for_display,
};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Reports the size of the filesystem mounted at a directory.
pub(crate) trait StatFs: Send + Sync {
    fn stat(&self, mount_point: &Path) -> io::Result<Capacity>;
}

/// The real `statvfs` call.
struct Statvfs;

impl StatFs for Statvfs {
    fn stat(&self, mount_point: &Path) -> io::Result<Capacity> {
        let path = CString::new(mount_point.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        let mut raw = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        loop {
            // SAFETY: `path` is a valid NUL-terminated string that lives for
            // the call, and `raw` is writable storage of exactly the type the
            // call fills in.
            let status = unsafe { libc::statvfs(path.as_ptr(), raw.as_mut_ptr()) };
            if status == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        // SAFETY: `statvfs` returned success, so it has filled in the struct;
        // it was also zero-initialised, which is valid for these integer fields.
        let raw = unsafe { raw.assume_init() };
        let unit = if raw.f_frsize != 0 {
            raw.f_frsize
        } else {
            raw.f_bsize
        };
        Capacity::from_blocks(unit, raw.f_blocks, raw.f_bfree, raw.f_bavail).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "implausible size from statvfs")
        })
    }
}

/// Where a Linux machine keeps its drive facts. Tests point these at fixtures.
pub(crate) struct Host {
    mountinfo: PathBuf,
    sysfs: Sysfs,
    dev_root: PathBuf,
    disk_names: PathBuf,
    stat: Arc<dyn StatFs>,
    timeout: Duration,
    /// Probes still running from earlier calls.
    in_flight: Arc<InFlight>,
}

impl Host {
    pub fn system() -> Self {
        Self {
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            sysfs: Sysfs::system(),
            dev_root: PathBuf::from("/dev"),
            disk_names: PathBuf::from("/dev/disk"),
            stat: Arc::new(Statvfs),
            timeout: CAPACITY_TIMEOUT,
            in_flight: InFlight::shared(),
        }
    }

    fn candidates(&self) -> Result<Vec<Candidate>, DriveError> {
        let text = fs::read(&self.mountinfo).map_err(DriveError::MountTable)?;
        let devices = SysfsDevices { host: self };
        Ok(rules::plan(mountinfo::parse(&text), &devices))
    }

    pub fn list(&self) -> Result<Vec<Drive>, DriveError> {
        let candidates: Vec<Candidate> = self
            .candidates()?
            .into_iter()
            .filter(|c| !c.hidden)
            .collect();
        let mut drives = self.build(&candidates);
        sort_for_display(&mut drives);
        Ok(drives)
    }

    pub fn volume_for_path(&self, path: &Path) -> Result<Drive, DriveError> {
        // Resolving the path can touch a dead network mount, so it runs on a
        // worker with a time limit like the size queries do.
        let mut key = b"path\0".to_vec();
        key.extend_from_slice(path.as_os_str().as_bytes());
        let owned = path.to_path_buf();
        let resolve: Job<io::Result<(PathBuf, (u32, u32))>> = Box::new(move || {
            let canonical = fs::canonicalize(&owned)?;
            let device = fs::metadata(&canonical)?.dev();
            Ok((canonical, (libc::major(device), libc::minor(device))))
        });
        let resolved = run_parallel(&self.in_flight, vec![(key, resolve)], self.timeout)
            .pop()
            .flatten()
            .ok_or(DriveError::TimedOut)?;
        let (canonical, device) = resolved.map_err(DriveError::Path)?;
        let candidates = self.candidates()?;
        let found = rules::find(&candidates, device, canonical.as_os_str().as_bytes())
            .ok_or(DriveError::NoVolume)?;
        self.build(std::slice::from_ref(found))
            .pop()
            .ok_or(DriveError::NoVolume)
    }

    /// Asks every candidate for its size at once, then assembles the drives.
    fn build(&self, candidates: &[Candidate]) -> Vec<Drive> {
        let jobs = candidates
            .iter()
            .map(|candidate| {
                let stat = Arc::clone(&self.stat);
                let mount_point = path_of(&candidate.entry.mount_point);
                let mut key = b"stat\0".to_vec();
                key.extend_from_slice(&candidate.entry.mount_point);
                let job: Job<io::Result<Capacity>> = Box::new(move || stat.stat(&mount_point));
                (key, job)
            })
            .collect();
        let results = run_parallel(&self.in_flight, jobs, self.timeout);
        let names = NameIndex::load(&self.dev_root, &self.disk_names);
        candidates
            .iter()
            .zip(results)
            .map(|(candidate, result)| {
                let (capacity, problem) = match result {
                    Some(Ok(capacity)) => (Some(capacity), None),
                    Some(Err(error)) => (None, Some(CapacityProblem::Failed(error.kind()))),
                    None => (None, Some(CapacityProblem::TimedOut)),
                };
                drive_from(candidate, capacity, problem, &names, &self.dev_root)
            })
            .collect()
    }
}

fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

fn drive_from(
    candidate: &Candidate,
    capacity: Option<Capacity>,
    capacity_problem: Option<CapacityProblem>,
    names: &NameIndex,
    dev_root: &Path,
) -> Drive {
    let entry = &candidate.entry;
    let device_key = names.key_for(&entry.source, dev_root);
    let uuid = device_key.as_ref().and_then(|k| names.uuids.get(k));
    let label = device_key
        .as_ref()
        .and_then(|k| names.labels.get(k))
        .cloned();
    let source = String::from_utf8_lossy(&entry.source);
    let id = match uuid {
        Some(uuid) => format!("uuid:{uuid}"),
        None if entry.source.starts_with(b"/dev/") => format!("dev:{source}"),
        None => format!(
            "mount:{}:{source}:{}",
            entry.fs_type,
            String::from_utf8_lossy(&entry.mount_point)
        ),
    };
    let covered = rules::covered_flags(&candidate.members);
    let mounts = candidate
        .members
        .iter()
        .zip(covered)
        .map(|(member, covered_by_other_mount)| Mount {
            mount_point: path_of(&member.mount_point),
            mount_id: Some(member.mount_id),
            root: path_of(&member.root),
            covered_by_other_mount,
        })
        .collect();
    Drive {
        id: DriveId::new(id),
        mount_point: path_of(&entry.mount_point),
        label,
        fs_type: entry.fs_type.clone(),
        kind: candidate.kind,
        read_only: candidate.members.iter().all(MountEntry::read_only),
        eligible_for_background_scan: !candidate.hidden
            && eligible_for_background_scan(candidate.kind, capacity.as_ref()),
        capacity,
        capacity_problem,
        mounts,
    }
}

/// Removable-device questions answered from sysfs.
struct SysfsDevices<'a> {
    host: &'a Host,
}

impl Devices for SysfsDevices<'_> {
    fn is_removable(&self, entry: &MountEntry) -> bool {
        // Ask about the device node itself when there is one: a btrfs
        // subvolume reports an anonymous number in the mount table, but its
        // source still names the real device.
        if entry.source.starts_with(b"/dev/") {
            let relative = &entry.source[b"/dev/".len()..];
            let node = self.host.dev_root.join(OsStr::from_bytes(relative));
            if let Ok(metadata) = fs::metadata(node)
                && metadata.file_type().is_block_device()
            {
                let rdev = metadata.rdev();
                return self
                    .host
                    .sysfs
                    .is_removable(libc::major(rdev), libc::minor(rdev));
            }
        }
        entry.major != 0 && self.host.sysfs.is_removable(entry.major, entry.minor)
    }
}

/// Filesystem UUIDs and labels, by the device they belong to.
struct NameIndex {
    uuids: HashMap<PathBuf, String>,
    labels: HashMap<PathBuf, String>,
}

impl NameIndex {
    fn load(dev_root: &Path, disk_names: &Path) -> Self {
        let dev_root = fs::canonicalize(dev_root).unwrap_or_else(|_| dev_root.to_path_buf());
        let read = |dir: &str, decode: bool| {
            let mut map = HashMap::new();
            let Ok(entries) = fs::read_dir(disk_names.join(dir)) else {
                return map;
            };
            for entry in entries.flatten() {
                let Ok(target) = fs::canonicalize(entry.path()) else {
                    continue;
                };
                let Ok(key) = target.strip_prefix(&dev_root) else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let name = if decode {
                    decode_udev_escapes(&name)
                } else {
                    name
                };
                map.insert(key.to_path_buf(), name);
            }
            map
        };
        Self {
            uuids: read("by-uuid", false),
            labels: read("by-label", true),
        }
    }

    /// The key of a device node: its resolved path relative to the device
    /// directory. `None` when the source is not a device node.
    fn key_for(&self, source: &[u8], dev_root: &Path) -> Option<PathBuf> {
        let relative = source.strip_prefix(b"/dev/")?;
        let node = dev_root.join(OsStr::from_bytes(relative));
        let canonical_root = fs::canonicalize(dev_root).unwrap_or_else(|_| dev_root.to_path_buf());
        let resolved = fs::canonicalize(&node).ok()?;
        resolved
            .strip_prefix(canonical_root)
            .ok()
            .map(Path::to_path_buf)
    }
}

/// Undoes udev's escaping of link names: `\x20` is a space, `\x5c` a backslash.
fn decode_udev_escapes(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && bytes.get(index + 1) == Some(&b'x')
            && let Some(hex) = bytes.get(index + 2..index + 4)
            && let Ok(text) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(text, 16)
        {
            out.push(byte);
            index += 4;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drives::DriveKind;
    use crate::testutil::Fixture;
    use std::time::Instant;

    /// Answers by mount point after a delay; unknown mount points fail with
    /// `NotFound`.
    struct FakeStat {
        answers: HashMap<PathBuf, (Duration, Result<Capacity, io::ErrorKind>)>,
        /// Every mount point asked about, in order.
        log: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    }

    impl FakeStat {
        fn new() -> Self {
            Self {
                answers: HashMap::new(),
                log: Arc::default(),
            }
        }

        fn ok(mut self, mount: &str, total: u64, used: u64) -> Self {
            let capacity = Capacity {
                total,
                used,
                free: total - used,
                reserved: 0,
                reserved_kind: crate::drives::ReservedKind::FilesystemReserve,
            };
            self.answers
                .insert(PathBuf::from(mount), (Duration::ZERO, Ok(capacity)));
            self
        }

        fn slow(mut self, mount: &str, delay: Duration) -> Self {
            let error = Err(io::ErrorKind::TimedOut);
            self.answers.insert(PathBuf::from(mount), (delay, error));
            self
        }

        fn failing(mut self, mount: &str, kind: io::ErrorKind) -> Self {
            self.answers
                .insert(PathBuf::from(mount), (Duration::ZERO, Err(kind)));
            self
        }
    }

    impl StatFs for FakeStat {
        fn stat(&self, mount_point: &Path) -> io::Result<Capacity> {
            self.log.lock().unwrap().push(mount_point.to_path_buf());
            let (delay, answer) = self
                .answers
                .get(mount_point)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            std::thread::sleep(*delay);
            answer.map_err(io::Error::from)
        }
    }

    fn host(fx: &Fixture, mountinfo: &str, stat: FakeStat, timeout: Duration) -> Host {
        fx.write("mountinfo", mountinfo);
        fx.mkdir("dev");
        fx.mkdir("sys");
        Host {
            mountinfo: fx.path().join("mountinfo"),
            sysfs: Sysfs::with_root(fx.path().join("sys")),
            dev_root: fx.path().join("dev"),
            disk_names: fx.path().join("dev/disk"),
            stat: Arc::new(stat),
            timeout,
            in_flight: Arc::default(),
        }
    }

    const UBUNTU: &str = include_str!("../../tests/fixtures/mountinfo/ubuntu-desktop.txt");
    const FEDORA: &str = include_str!("../../tests/fixtures/mountinfo/fedora.txt");

    #[test]
    fn lists_eligible_fixed_drives_with_sizes_ids_and_labels() {
        let fx = Fixture::new("linux-list");
        fx.write("dev/sda3", "");
        fx.write("dev/sda4", "");
        fx.write("dev/nvme0n1p1", "");
        fx.write("dev/sdb1", "");
        fx.symlink(
            "../../sda3",
            "dev/disk/by-uuid/b1b3729c-6df3-406a-a039-d15c88e386c1",
        );
        fx.symlink(
            "../../sda4",
            "dev/disk/by-uuid/2ddb34f8-bce7-443b-afff-21ed13eb95b5",
        );
        fx.symlink("../../sda4", "dev/disk/by-label/Home\\x20Disk");
        // The USB stick, by sysfs.
        fx.write("sys/devices/pci0/usb1/1-1/host6/block/sdb/removable", "0\n");
        fx.write(
            "sys/devices/pci0/usb1/1-1/host6/block/sdb/sdb1/partition",
            "1\n",
        );
        fx.symlink(
            "../../devices/pci0/usb1/1-1/host6/block/sdb/sdb1",
            "sys/dev/block/8:17",
        );
        let stat = FakeStat::new()
            .ok("/", 1000, 400)
            .ok("/data", 2000, 100)
            .ok("/home", 3000, 3000)
            .ok("/media/alice/USB STICK", 500, 5);
        let host = host(&fx, UBUNTU, stat, Duration::from_secs(5));
        let drives = host.list().unwrap();
        let summary: Vec<_> = drives
            .iter()
            .map(|d| {
                (
                    d.mount_point.to_str().unwrap().to_owned(),
                    d.kind,
                    d.eligible_for_background_scan,
                    d.id.as_str().to_owned(),
                    d.label.clone(),
                )
            })
            .collect();
        let expected = vec![
            (
                "/".to_owned(),
                DriveKind::Fixed,
                true,
                "uuid:b1b3729c-6df3-406a-a039-d15c88e386c1".to_owned(),
                None,
            ),
            // No by-uuid link for this one: falls back to the device node.
            (
                "/data".to_owned(),
                DriveKind::Fixed,
                true,
                "dev:/dev/nvme0n1p1".to_owned(),
                None,
            ),
            (
                "/home".to_owned(),
                DriveKind::Fixed,
                true,
                "uuid:2ddb34f8-bce7-443b-afff-21ed13eb95b5".to_owned(),
                Some("Home Disk".to_owned()),
            ),
            (
                "/media/alice/USB STICK".to_owned(),
                DriveKind::Removable,
                false,
                "dev:/dev/sdb1".to_owned(),
                None,
            ),
        ];
        assert_eq!(summary, expected);
        let root = &drives[0];
        assert_eq!(root.fs_type, "ext4");
        assert_eq!(root.capacity.map(|c| (c.total, c.used)), Some((1000, 400)));
        assert_eq!(root.capacity_problem, None);
        assert!(!root.read_only);
    }

    #[test]
    fn a_dead_network_mount_does_not_hang_the_list() {
        let fx = Fixture::new("linux-slow");
        let stat = FakeStat::new()
            .ok("/", 1000, 400)
            .ok("/boot", 100, 10)
            .ok("/run/media/bob/Backup", 100, 10)
            .slow("/mnt/nas", Duration::from_secs(20))
            .ok("/mnt/share", 100, 1);
        let host = host(&fx, FEDORA, stat, Duration::from_millis(300));
        let start = Instant::now();
        let drives = host.list().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
        let nas = drives
            .iter()
            .find(|d| d.mount_point == Path::new("/mnt/nas"))
            .expect("the slow drive is still listed");
        assert_eq!(nas.kind, DriveKind::Network);
        assert_eq!(nas.capacity, None);
        assert_eq!(nas.capacity_problem, Some(CapacityProblem::TimedOut));
        assert!(!nas.eligible_for_background_scan);
        let share = drives
            .iter()
            .find(|d| d.mount_point == Path::new("/mnt/share"))
            .unwrap();
        assert!(share.capacity.is_some(), "other drives are unaffected");
        let root = drives
            .iter()
            .find(|d| d.mount_point == Path::new("/"))
            .unwrap();
        assert!(root.eligible_for_background_scan);
    }

    #[test]
    fn a_mount_that_is_still_stuck_is_not_probed_again_by_the_next_listing() {
        let fx = Fixture::new("linux-inflight");
        let table = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:40 / /mnt/nas rw - nfs4 nas:/export rw
";
        let stat = FakeStat::new()
            .ok("/", 10, 1)
            .slow("/mnt/nas", Duration::from_millis(700));
        let log = Arc::clone(&stat.log);
        let host = host(&fx, table, stat, Duration::from_millis(100));
        let asked = |name: &str| {
            log.lock()
                .unwrap()
                .iter()
                .filter(|p| p.as_path() == Path::new(name))
                .count()
        };
        let nas_problem = |drives: &[Drive]| {
            drives
                .iter()
                .find(|d| d.mount_point == Path::new("/mnt/nas"))
                .and_then(|d| d.capacity_problem)
        };

        let first = host.list().unwrap();
        assert_eq!(nas_problem(&first), Some(CapacityProblem::TimedOut));
        // The second listing happens while the first probe is still stuck.
        let second = host.list().unwrap();
        assert_eq!(nas_problem(&second), Some(CapacityProblem::TimedOut));
        assert_eq!(asked("/mnt/nas"), 1, "no second thread for the stuck mount");
        assert_eq!(asked("/"), 2, "healthy mounts are probed every time");

        // After the stuck call returns, the mount is probed again.
        std::thread::sleep(Duration::from_millis(900));
        let _ = host.list().unwrap();
        assert_eq!(asked("/mnt/nas"), 2);
    }

    #[test]
    fn drives_carry_their_mounts_with_ids_roots_and_cover_flags() {
        let fx = Fixture::new("linux-mounts");
        let stat = FakeStat::new()
            .ok("/", 10, 1)
            .ok("/data", 10, 1)
            .ok("/home", 10, 1)
            .ok("/media/alice/USB STICK", 10, 1);
        let host = host(&fx, UBUNTU, stat, Duration::from_secs(5));
        let drives = host.list().unwrap();
        let home = drives
            .iter()
            .find(|d| d.mount_point == Path::new("/home"))
            .unwrap();
        let summary: Vec<_> = home
            .mounts
            .iter()
            .map(|m| {
                (
                    m.mount_point.to_str().unwrap(),
                    m.mount_id,
                    m.root.to_str().unwrap(),
                    m.covered_by_other_mount,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("/home", Some(203), "/", false),
                ("/srv/projects", Some(2600), "/alice/projects", true),
            ]
        );
        assert!(home.mounts[1].root_is_within(&home.mounts[0]));
        assert!(!home.mounts[0].root_is_within(&home.mounts[1]));
    }

    #[test]
    fn a_failing_size_query_is_reported_not_hidden() {
        let fx = Fixture::new("linux-fail");
        let stat = FakeStat::new()
            .failing("/", io::ErrorKind::PermissionDenied)
            .ok("/boot", 100, 10);
        let host = host(&fx, FEDORA, stat, Duration::from_secs(5));
        let drives = host.list().unwrap();
        let root = drives
            .iter()
            .find(|d| d.mount_point == Path::new("/"))
            .unwrap();
        assert_eq!(
            root.capacity_problem,
            Some(CapacityProblem::Failed(io::ErrorKind::PermissionDenied))
        );
        assert!(
            !root.eligible_for_background_scan,
            "a drive with no size is not scanned in the background"
        );
    }

    #[test]
    fn read_only_only_when_every_mount_is_read_only() {
        let fx = Fixture::new("linux-ro");
        let table = "\
1 0 8:1 / / ro - ext4 /dev/sda1 ro
2 0 8:2 / /a rw - ext4 /dev/sda2 rw
3 0 8:2 / /b ro - ext4 /dev/sda2 ro
";
        let host = host(
            &fx,
            table,
            FakeStat::new().ok("/", 10, 1).ok("/a", 10, 1),
            Duration::from_secs(5),
        );
        let drives = host.list().unwrap();
        assert!(drives[0].read_only);
        assert!(!drives[1].read_only);
    }

    #[test]
    fn unreadable_mount_table_is_an_error() {
        let fx = Fixture::new("linux-nomount");
        let mut host = host(&fx, "", FakeStat::new(), Duration::from_secs(1));
        host.mountinfo = fx.path().join("missing");
        assert!(matches!(host.list(), Err(DriveError::MountTable(_))));
    }

    #[test]
    fn garbage_mount_table_lists_nothing() {
        let fx = Fixture::new("linux-garbage");
        let host = host(
            &fx,
            "\0\0\0 garbage\n\n- - -\n",
            FakeStat::new(),
            Duration::from_secs(1),
        );
        assert!(host.list().unwrap().is_empty());
    }

    #[test]
    fn non_utf8_mount_point_survives_end_to_end() {
        let fx = Fixture::new("linux-bytes");
        let mut table = b"1 0 8:1 / /mnt/".to_vec();
        table.extend_from_slice(&[0xff, 0xfe]);
        table.extend_from_slice(b" rw - ext4 /dev/sda1 rw\n");
        fx.mkdir("dev");
        fs::write(fx.path().join("mountinfo"), table).unwrap();
        fx.mkdir("sys");
        let mut mount = b"/mnt/".to_vec();
        mount.extend_from_slice(&[0xff, 0xfe]);
        let mut stat = FakeStat::new();
        stat.answers.insert(
            path_of(&mount),
            (
                Duration::ZERO,
                Ok(Capacity::from_bytes(
                    10,
                    5,
                    5,
                    crate::drives::ReservedKind::FilesystemReserve,
                )),
            ),
        );
        let host = Host {
            mountinfo: fx.path().join("mountinfo"),
            sysfs: Sysfs::with_root(fx.path().join("sys")),
            dev_root: fx.path().join("dev"),
            disk_names: fx.path().join("dev/disk"),
            stat: Arc::new(stat),
            timeout: Duration::from_secs(5),
            in_flight: Arc::default(),
        };
        let drives = host.list().unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!(
            drives[0].mount_point.as_os_str().as_bytes(),
            mount.as_slice()
        );
        assert!(drives[0].capacity.is_some());
    }

    #[test]
    fn udev_escapes_decode() {
        assert_eq!(decode_udev_escapes("Home\\x20Disk"), "Home Disk");
        assert_eq!(decode_udev_escapes("a\\x5cb"), "a\\b");
        assert_eq!(decode_udev_escapes("bad\\xZZ"), "bad\\xZZ");
        assert_eq!(decode_udev_escapes("end\\x2"), "end\\x2");
        assert_eq!(decode_udev_escapes("plain"), "plain");
    }

    #[test]
    fn statvfs_on_a_real_directory_adds_up() {
        let fx = Fixture::new("linux-statvfs");
        let capacity = Statvfs.stat(fx.path()).expect("statvfs on the fixture");
        assert!(capacity.total > 0);
        assert_eq!(
            capacity.used + capacity.reserved + capacity.free,
            capacity.total
        );
    }

    #[test]
    fn statvfs_errors_are_typed() {
        let fx = Fixture::new("linux-statvfs-err");
        let missing = fx.path().join("nope");
        assert_eq!(
            Statvfs.stat(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let with_nul = Path::new(OsStr::from_bytes(b"/tmp/a\0b"));
        assert_eq!(
            Statvfs.stat(with_nul).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn volume_for_path_finds_the_drive_that_holds_a_folder() {
        let fx = Fixture::new("linux-volume");
        fx.mkdir("a/b");
        let drive = super::super::volume_for_path(&fx.path().join("a/b")).expect("volume");
        assert!(fx.path().starts_with(&drive.mount_point));
        let capacity = drive.capacity.expect("the host answers statvfs");
        assert!(capacity.total > 0);
        // Asking through a symlink names the same place.
        fx.symlink("a/b", "link");
        let through = super::super::volume_for_path(&fx.path().join("link")).unwrap();
        assert_eq!(through.id, drive.id);
    }

    #[test]
    fn volume_for_path_resolution_is_time_limited_and_not_repeated_while_stuck() {
        // A path that resolves slowly is simulated with a held key: the same
        // key as a real lookup of this path is already in flight.
        let fx = Fixture::new("linux-volume-stuck");
        let host = host(
            &fx,
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n",
            FakeStat::new(),
            Duration::from_millis(100),
        );
        let path = fx.path().join("somewhere");
        let mut key = b"path\0".to_vec();
        key.extend_from_slice(path.as_os_str().as_bytes());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let hold: Job<()> = Box::new(move || {
            let _ = wait.recv();
        });
        let started = run_parallel(
            &host.in_flight,
            vec![(key, hold)],
            Duration::from_millis(10),
        );
        assert_eq!(started, vec![None]);
        let began = Instant::now();
        let result = host.volume_for_path(&path);
        assert!(matches!(result, Err(DriveError::TimedOut)), "{result:?}");
        assert!(began.elapsed() < Duration::from_secs(2));
        let _ = release.send(());
    }

    #[test]
    fn volume_for_path_errors_are_typed() {
        let fx = Fixture::new("linux-volume-err");
        let missing = super::super::volume_for_path(&fx.path().join("nope"));
        assert!(matches!(missing, Err(DriveError::Path(_))));
    }

    #[test]
    fn volume_for_path_refuses_a_path_with_a_nul() {
        let with_nul = Path::new(OsStr::from_bytes(b"/tmp/a\0b"));
        let result = super::super::volume_for_path(with_nul);
        assert!(matches!(result, Err(DriveError::Path(_))));
    }

    #[test]
    fn the_real_host_list_is_self_consistent() {
        let drives = super::super::list_drives().expect("this host has a mount table");
        let mut ids = std::collections::HashSet::new();
        for drive in &drives {
            assert!(drive.mount_point.is_absolute());
            assert!(ids.insert(drive.id.clone()), "duplicate id {}", drive.id);
            if let Some(c) = drive.capacity {
                assert_eq!(c.used + c.reserved + c.free, c.total);
            }
            assert!(!drive.eligible_for_background_scan || drive.kind == DriveKind::Fixed);
            assert_ne!(drive.fs_type, "tmpfs");
            assert_ne!(drive.fs_type, "squashfs");
            assert_ne!(drive.fs_type, "overlay");
        }
    }
}
