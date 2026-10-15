//! The two ioctls the reader makes, as small functions over byte buffers.
//!
//! This is the only place in the crate with `unsafe`. The reader never writes
//! to the file system: `TREE_SEARCH_V2` and `INO_LOOKUP` only read. Both need
//! administrator rights, which is why the reader runs in the helper.
#![allow(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use crate::BtrfsError;
use crate::items::FIRST_FREE;
use crate::le::{u32_at, u64_at};
use crate::model::Start;
use crate::mounts::{self, MountPoint, Mounted};
use crate::search::{Flow, Item, Key, PageEnd, TreeSource, read_page};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `BTRFS_IOCTL_MAGIC`.
const MAGIC: u8 = 0x94;
const IOC_READ_WRITE: libc::c_ulong = 3;

/// `_IOWR(type, nr, size)`.
const fn iowr(ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    (IOC_READ_WRITE << 30)
        | ((size as libc::c_ulong) << 16)
        | ((ty as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

/// `struct btrfs_ioctl_search_args_v2` without its buffer: the 104-byte search
/// key and the 8-byte buffer size.
const ARGS_HEADER: usize = 112;
const IOC_TREE_SEARCH_V2: libc::c_ulong = iowr(MAGIC, 17, ARGS_HEADER);
/// `struct btrfs_ioctl_ino_lookup_args`: two ids and 4080 bytes of path.
const INO_LOOKUP_ARGS: usize = 4096;
const IOC_INO_LOOKUP: libc::c_ulong = iowr(MAGIC, 18, INO_LOOKUP_ARGS);

// Offsets in the search key.
const OFF_TREE_ID: usize = 0;
const OFF_MIN_OBJECTID: usize = 8;
const OFF_MAX_OBJECTID: usize = 16;
const OFF_MIN_OFFSET: usize = 24;
const OFF_MAX_OFFSET: usize = 32;
const OFF_MAX_TRANSID: usize = 48;
const OFF_MIN_TYPE: usize = 56;
const OFF_MAX_TYPE: usize = 60;
const OFF_NR_ITEMS: usize = 64;
const OFF_BUF_SIZE: usize = 104;

/// The kernel caps the result buffer at 16 MiB.
const MAX_BUF: usize = 16 << 20;
const START_BUF: usize = 4 << 20;

/// `BTRFS_SUPER_MAGIC`.
const BTRFS_MAGIC: i64 = 0x9123_683E;

/// A mounted btrfs file system, searched through a directory on it.
#[derive(Debug)]
pub struct IoctlSource {
    fd: OwnedFd,
    /// The start directory's canonical path, to find the mounts below it.
    root: PathBuf,
    /// The file system's identity, to tell its mounts from other file
    /// systems' that are also btrfs.
    fsid: [i32; 2],
}

fn put64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn put32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

impl IoctlSource {
    /// Opens the directory `path` and checks that it is on btrfs. Returns the
    /// source and where the scan starts: the directory's own subvolume and
    /// inode.
    pub fn open(path: &Path) -> Result<(IoctlSource, Start), BtrfsError> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        let fd: OwnedFd = file.into();
        let (magic, fsid) = statfs_of(fd.as_raw_fd())?;
        if magic != BTRFS_MAGIC {
            return Err(BtrfsError::NotBtrfs);
        }
        let stat = fstat_of(fd.as_raw_fd())?;
        let tree = tree_of_fd(fd.as_raw_fd())?;
        let source = IoctlSource {
            fd,
            // The mount table names canonical paths.
            root: std::fs::canonicalize(path)?,
            fsid,
        };
        Ok((
            source,
            Start {
                tree,
                inode: stat.st_ino,
            },
        ))
    }

    /// What is mounted at `path`, one mount of the mount table: a directory
    /// of this file system, traced to its subvolume, or something else.
    fn what_is_mounted(&self, path: &Path, mount_id: u64) -> Mounted {
        let opened = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path);
        let Ok(file) = opened else {
            return Mounted::HereUnplaced;
        };
        let fd: OwnedFd = file.into();
        // The mount table may have changed since it was read: the descriptor
        // must be on the mount the table line is about.
        match bs_scan::rawdir::stat_fd(fd.as_fd()) {
            Ok(st) if st.mnt_id == Some(mount_id) => {}
            _ => return Mounted::HereUnplaced,
        }
        // And it must be the root of that mount. A path whose parent was
        // renamed since the table was read can land on a folder inside the
        // mount, which would be taken for the mount's root.
        if !is_mount_root(fd.as_raw_fd()) {
            return Mounted::HereUnplaced;
        }
        match statfs_of(fd.as_raw_fd()) {
            Ok((BTRFS_MAGIC, fsid)) if fsid == self.fsid => {}
            Ok(_) => return Mounted::Other,
            Err(_) => return Mounted::HereUnplaced,
        }
        let (Ok(stat), Ok(tree)) = (fstat_of(fd.as_raw_fd()), tree_of_fd(fd.as_raw_fd())) else {
            return Mounted::HereUnplaced;
        };
        Mounted::Here(Start {
            tree,
            inode: stat.st_ino,
        })
    }
}

/// `STATX_ATTR_MOUNT_ROOT`: the directory is the root of its mount.
const STATX_ATTR_MOUNT_ROOT: u64 = 0x0000_2000;

/// Whether the attributes say a directory is its mount's root. A kernel that
/// does not report the attribute (`mask` lacks it) cannot say, which counts
/// as not.
fn mount_root_from(attributes: u64, mask: u64) -> bool {
    mask & STATX_ATTR_MOUNT_ROOT != 0 && attributes & STATX_ATTR_MOUNT_ROOT != 0
}

/// Whether the directory `fd` is the root of the mount it is on.
fn is_mount_root(fd: RawFd) -> bool {
    let mut buf = MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: `fd` is an open descriptor, the path is the empty string with
    // `AT_EMPTY_PATH` (so it names the descriptor itself), and `buf` is a
    // writable `statx` buffer that was zeroed, so it is initialized whether
    // or not the call fills it.
    let rc = unsafe {
        libc::statx(
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_DONT_SYNC,
            libc::STATX_TYPE,
            buf.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return false;
    }
    // SAFETY: zero-initialized above, and a plain-data struct.
    let raw = unsafe { buf.assume_init() };
    mount_root_from(raw.stx_attributes, raw.stx_attributes_mask)
}

/// `f_type` and `f_fsid` of the file system of a descriptor.
fn statfs_of(fd: RawFd) -> Result<(i64, [i32; 2]), BtrfsError> {
    let mut st = MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `fd` is an open descriptor and `st` is a writable buffer of the
    // size `fstatfs` fills; it was zeroed, so it is initialized whether or
    // not the call fills it.
    let rc = unsafe { libc::fstatfs(fd, st.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: zero-initialized above, and a plain-data struct.
    let st = unsafe { st.assume_init() };
    // The type of `f_type` differs between targets.
    #[allow(clippy::unnecessary_cast)]
    let magic = st.f_type as i64;
    const _: () = assert!(std::mem::size_of::<libc::fsid_t>() == std::mem::size_of::<[i32; 2]>());
    // SAFETY: `fsid_t` is two plain integers, the same size as the array
    // (asserted above); reading the struct that was just filled as the array
    // is reading initialized memory.
    let raw: [i32; 2] = unsafe { std::mem::transmute_copy(&st.f_fsid) };
    Ok((magic, raw))
}

fn fstat_of(fd: RawFd) -> Result<libc::stat, BtrfsError> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: as for `fstatfs`: a valid descriptor and a writable, zeroed
    // buffer of the right size.
    let rc = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: zero-initialized above, and a plain-data struct.
    Ok(unsafe { stat.assume_init() })
}

/// The tree id of the subvolume the descriptor's directory is in.
fn tree_of_fd(fd: RawFd) -> Result<u64, BtrfsError> {
    let mut args = vec![0u8; INO_LOOKUP_ARGS];
    // A tree id of zero means "the subvolume of the descriptor".
    put64(&mut args, 0, 0);
    put64(&mut args, 8, FIRST_FREE);
    loop {
        // SAFETY: `args` is a writable buffer of exactly the size the ioctl
        // number encodes, and it stays alive for the call.
        let rc = unsafe {
            libc::ioctl(
                fd,
                IOC_INO_LOOKUP as _,
                args.as_mut_ptr().cast::<libc::c_void>(),
            )
        };
        if rc == 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e.into());
        }
    }
    u64_at(&args, 0).ok_or(BtrfsError::Hostile("tree id"))
}

impl TreeSource for IoctlSource {
    fn search(
        &self,
        tree: u64,
        from: Key,
        to: Key,
        each: &mut dyn FnMut(Item<'_>) -> Flow,
    ) -> Result<(), BtrfsError> {
        let mut cursor = from;
        let mut buf_bytes = START_BUF;
        let mut args = vec![0u8; ARGS_HEADER + buf_bytes];
        while cursor <= to {
            args.truncate(ARGS_HEADER);
            args.resize(ARGS_HEADER + buf_bytes, 0);
            put64(&mut args, OFF_TREE_ID, tree);
            put64(&mut args, OFF_MIN_OBJECTID, cursor.objectid);
            put64(&mut args, OFF_MAX_OBJECTID, to.objectid);
            put64(&mut args, OFF_MIN_OFFSET, cursor.offset);
            put64(&mut args, OFF_MAX_OFFSET, to.offset);
            put64(&mut args, OFF_MAX_TRANSID, u64::MAX);
            put32(&mut args, OFF_MIN_TYPE, u32::from(cursor.ty));
            put32(&mut args, OFF_MAX_TYPE, u32::from(to.ty));
            put32(&mut args, OFF_NR_ITEMS, u32::MAX);
            put64(&mut args, OFF_BUF_SIZE, buf_bytes as u64);
            // SAFETY: `args` is writable and holds the 112-byte header the
            // ioctl number encodes followed by the `buf_size` bytes the
            // header promises; it stays alive and unmoved for the call.
            let rc = unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    IOC_TREE_SEARCH_V2 as _,
                    args.as_mut_ptr().cast::<libc::c_void>(),
                )
            };
            if rc != 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // The next item does not fit the buffer; the kernel has
                    // written the size it needs.
                    Some(libc::EOVERFLOW | libc::ENOBUFS) if buf_bytes < MAX_BUF => {
                        buf_bytes = (buf_bytes * 4).min(MAX_BUF);
                        continue;
                    }
                    _ => return Err(e.into()),
                }
            }
            let nr = u32_at(&args, OFF_NR_ITEMS).ok_or(BtrfsError::Hostile("item count"))?;
            let results = args
                .get(ARGS_HEADER..)
                .ok_or(BtrfsError::Hostile("result buffer"))?;
            match read_page(results, nr, cursor, to, each)? {
                PageEnd::Empty | PageEnd::Stopped => return Ok(()),
                PageEnd::Last(last) => match last.successor() {
                    Some(next) => cursor = next,
                    None => return Ok(()),
                },
            }
        }
        Ok(())
    }

    fn mounts(&self) -> Result<Vec<MountPoint>, BtrfsError> {
        let text = std::fs::read("/proc/self/mountinfo")?;
        let mut below = Vec::new();
        for entry in mounts::parse_table(&text) {
            let Some(path) = mounts::below(self.root.as_os_str().as_bytes(), &entry.mount_point)
            else {
                continue;
            };
            if below.len() >= mounts::MAX_MOUNTS {
                return Err(BtrfsError::TooLarge(
                    "more mounts than the reader works with",
                ));
            }
            below.push((path, entry));
        }
        // A mount that another is stacked on is hidden, and a walk sees the
        // one on top. Opening a hidden mount's folder would reach the top
        // one instead.
        let tops: Vec<mounts::TableEntry> =
            mounts::without_covered(below.iter().map(|(_, e)| e.clone()).collect());
        let mut out = Vec::new();
        for (path, entry) in below {
            if !tops.iter().any(|t| t.id == entry.id) {
                continue;
            }
            let mounted = if entry.fs_type == b"btrfs" {
                let mut at = self.root.clone();
                for name in &path {
                    at.push(OsStr::from_bytes(name));
                }
                self.what_is_mounted(&at, entry.id)
            } else {
                Mounted::Other
            };
            out.push(MountPoint {
                path,
                mount_id: entry.id,
                device: entry.device,
                mounted,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn only_a_reported_and_set_attribute_makes_a_mount_root() {
        assert!(mount_root_from(
            STATX_ATTR_MOUNT_ROOT,
            STATX_ATTR_MOUNT_ROOT
        ));
        // Set but the kernel never said it reports it, or reported unset.
        assert!(!mount_root_from(STATX_ATTR_MOUNT_ROOT, 0));
        assert!(!mount_root_from(0, STATX_ATTR_MOUNT_ROOT));
        assert!(!mount_root_from(
            !STATX_ATTR_MOUNT_ROOT,
            !STATX_ATTR_MOUNT_ROOT
        ));
    }

    fn open_dir(path: &str) -> OwnedFd {
        File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
            .unwrap()
            .into()
    }

    #[test]
    fn a_directory_inside_a_mount_is_not_its_root() {
        // `/proc` is a mount of its own; `/proc/sys` is a folder inside it.
        let root = open_dir("/proc");
        let inside = open_dir("/proc/sys");
        assert!(is_mount_root(root.as_raw_fd()), "/proc is a mount root");
        assert!(
            !is_mount_root(inside.as_raw_fd()),
            "/proc/sys is inside the mount"
        );
    }
}
