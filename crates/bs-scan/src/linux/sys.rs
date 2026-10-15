//! The system calls the Linux walker makes, each as a small function.
//!
//! This is the only place with `unsafe` in the Linux walker. Directories are
//! opened with `O_NOFOLLOW` and everything is relative to an open directory
//! handle, so a path that changes under the scan cannot lead it somewhere else.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// When set, `statx` results are treated as having no mount ID, so the device
/// fallback can be exercised on a kernel that reports mount IDs.
static IGNORE_MOUNT_IDS: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_ignore_mount_ids(on: bool) {
    IGNORE_MOUNT_IDS.store(on, Ordering::Relaxed);
}

/// When set, the unique mount ID is left out of a root's identity, so the
/// older, reusable ID is what gets compared (as before Linux 6.8).
static IGNORE_UNIQUE_IDS: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_ignore_unique_mount_ids(on: bool) {
    IGNORE_UNIQUE_IDS.store(on, Ordering::Relaxed);
}

/// The fields of `statx` the walker uses.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub mode: u32,
    pub nlink: u32,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub mtime: i64,
    pub dev: u64,
    /// `None` when the kernel did not return a mount ID (before Linux 5.8).
    pub mnt_id: Option<u64>,
}

/// Only what the walker reads. Asking for less makes the kernel (and network
/// filesystems) do less.
pub(crate) const WANT: libc::c_uint = libc::STATX_TYPE
    | libc::STATX_MODE
    | libc::STATX_NLINK
    | libc::STATX_INO
    | libc::STATX_SIZE
    | libc::STATX_BLOCKS
    | libc::STATX_MTIME
    | libc::STATX_MNT_ID;

/// Never follow a final symbolic link, never trigger an automount, and do not
/// force a network filesystem to refresh its attributes.
pub(crate) const FLAGS: libc::c_int =
    libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT | libc::AT_STATX_DONT_SYNC;

pub(crate) fn convert(raw: &libc::statx) -> Stat {
    let have_mount =
        raw.stx_mask & libc::STATX_MNT_ID != 0 && !IGNORE_MOUNT_IDS.load(Ordering::Relaxed);
    Stat {
        mode: u32::from(raw.stx_mode),
        nlink: raw.stx_nlink,
        ino: raw.stx_ino,
        size: raw.stx_size,
        blocks: raw.stx_blocks,
        mtime: raw.stx_mtime.tv_sec,
        dev: (u64::from(raw.stx_dev_major) << 32) | u64::from(raw.stx_dev_minor),
        mnt_id: have_mount.then_some(raw.stx_mnt_id),
    }
}

fn statx_raw(
    dirfd: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
) -> io::Result<Stat> {
    loop {
        let mut buf = MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: `path` points to a NUL-terminated string that outlives the
        // call, `buf` is a writable `statx`-sized buffer, and the kernel
        // reports success only after filling the fields it sets in `stx_mask`.
        // The buffer was zeroed, so every field is initialized either way.
        let rc = unsafe { libc::statx(dirfd, path, flags, WANT, buf.as_mut_ptr()) };
        if rc == 0 {
            // SAFETY: zero-initialized above and, on success, written by the kernel.
            let raw = unsafe { buf.assume_init() };
            return Ok(convert(&raw));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// `statx` of `name` inside the directory `dir`, without following a final
/// symbolic link.
pub fn stat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Stat> {
    statx_raw(dir.as_raw_fd(), name.as_ptr(), FLAGS)
}

/// `statx` of an open directory itself.
pub fn stat_fd(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    statx_raw(fd.as_raw_fd(), c"".as_ptr(), FLAGS | libc::AT_EMPTY_PATH)
}

/// `STATX_MNT_ID_UNIQUE` (Linux 6.8 and later): a mount ID that the system
/// never hands out a second time. The `libc` crate has no name for it yet.
const STATX_MNT_ID_UNIQUE: libc::c_uint = 0x4000;

/// What identifies the mount and file system an open directory is on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RootIdentity {
    /// The unique mount ID where the system has one, else the older one;
    /// `None` when the system gives neither (before Linux 5.8, or the test
    /// switch for that case is on).
    pub key: Option<crate::RootKey>,
    /// The device number of the file system, as `stat` reports it.
    pub major: u32,
    pub minor: u32,
}

/// The identity of the mount an open directory is on, asked the same way for
/// the descriptor a caller judged and the one a scanner opened, so the two can
/// be compared.
pub(crate) fn root_identity(fd: BorrowedFd<'_>) -> io::Result<RootIdentity> {
    // The unique ID is not asked for where a test stands in for an older
    // kernel: asking for it makes the kernel put it in the one field both
    // kinds share.
    let want = if IGNORE_UNIQUE_IDS.load(Ordering::Relaxed) {
        libc::STATX_MNT_ID
    } else {
        libc::STATX_MNT_ID | STATX_MNT_ID_UNIQUE
    };
    loop {
        let mut buf = MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: the path is the empty string with `AT_EMPTY_PATH`, so the
        // call describes `fd` itself; the buffer is writable and statx-sized,
        // and was zeroed, so every field is initialized either way. Mask bits
        // an older kernel does not know are ignored by it and left clear in
        // the result mask.
        let rc = unsafe {
            libc::statx(
                fd.as_raw_fd(),
                c"".as_ptr(),
                FLAGS | libc::AT_EMPTY_PATH,
                want,
                buf.as_mut_ptr(),
            )
        };
        if rc == 0 {
            // SAFETY: zero-initialized above and, on success, written by the kernel.
            let raw = unsafe { buf.assume_init() };
            let key = if IGNORE_MOUNT_IDS.load(Ordering::Relaxed) {
                None
            } else if raw.stx_mask & STATX_MNT_ID_UNIQUE != 0 {
                Some(crate::RootKey::Mount {
                    id: raw.stx_mnt_id,
                    unique: true,
                })
            } else if raw.stx_mask & libc::STATX_MNT_ID != 0 {
                Some(crate::RootKey::Mount {
                    id: raw.stx_mnt_id,
                    unique: false,
                })
            } else {
                None
            };
            return Ok(RootIdentity {
                key,
                major: raw.stx_dev_major,
                minor: raw.stx_dev_minor,
            });
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Opens the scan root. The caller has resolved the path, so symbolic links in
/// it are the user's choice; but the last part is opened without following a
/// link, so a root that has become a link since it was resolved is refused
/// (`ENOTDIR` or `ELOOP`) and not followed. Nothing below the root is followed
/// either.
pub fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    open_raw(libc::AT_FDCWD, &c, DIR_FLAGS)
}

/// Opens the directory `name` inside `dir`. Fails with `ELOOP` or `ENOTDIR` if
/// it has been replaced by a link or a file since it was listed.
pub fn open_dir_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    open_raw(dir.as_raw_fd(), name, DIR_FLAGS)
}

fn open_raw(dirfd: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    loop {
        // SAFETY: `name` is NUL-terminated and outlives the call; the flags are
        // plain constants.
        let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags) };
        if fd >= 0 {
            // SAFETY: `openat` returned a new descriptor that nothing else owns.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Reads directory records into `buf`. Returns the number of bytes filled, 0 at
/// the end of the directory.
pub fn getdents(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` is a valid writable region of `buf.len()` bytes; the
        // kernel writes at most that many.
        let n = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd.as_raw_fd(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        if n >= 0 {
            // Never trust the count beyond the buffer.
            return Ok((n as usize).min(buf.len()));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Raises the soft limit on open files to the hard limit, once. A deep tree
/// keeps one handle per level that still has unvisited subdirectories.
pub fn raise_open_file_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, writable `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return None;
    }
    if lim.rlim_cur < lim.rlim_max {
        let want = libc::rlimit {
            rlim_cur: lim.rlim_max,
            rlim_max: lim.rlim_max,
        };
        // SAFETY: `want` is a valid `rlimit` and does not exceed the hard limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &want) } == 0 {
            return Some(want.rlim_cur);
        }
    }
    Some(lim.rlim_cur)
}

/// Used bytes on the filesystem holding `path`: blocks minus free blocks, which
/// leaves out the part reserved for the superuser.
pub(crate) fn statvfs_used(path: &Path) -> io::Result<u64> {
    let c = CString::new(OsStr::as_bytes(path.as_os_str()))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut buf = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c` is NUL-terminated; `buf` is a writable `statvfs`.
    let rc = unsafe { libc::statvfs(c.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zero-initialized and, on success, filled by the kernel.
    let st = unsafe { buf.assume_init() };
    // 64-bit fields on the supported targets (x86-64 and ARM64 Linux).
    Ok(st
        .f_blocks
        .saturating_sub(st.f_bfree)
        .saturating_mul(st.f_frsize))
}
