//! File-system calls for trashing and deleting on Linux.
//!
//! Every call is relative to an open directory and treats the last component
//! as a name, never as a place to follow: a link swapped in for a file or a
//! folder while an operation runs is acted on as a link, not as its target.

#![allow(unsafe_code)]

use crate::bins::walk::{DirStream, Stat, open_dir_at};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn retry<T>(mut call: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Opens a folder the caller named, following links in every component
/// including the last. Used only for the folder that holds an item: the item
/// itself is never opened through a link.
pub(crate) fn open_dir_following(path: &Path) -> io::Result<OwnedFd> {
    let path = crate::bins::walk::c_name(path.as_os_str().as_bytes())?;
    retry(|| {
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `open` returned a new descriptor that nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    })
}

/// Removes the name `name` inside `dir`. A link is removed, not followed.
pub(crate) fn unlink_at(dir: RawFd, name: &CStr, is_dir: bool) -> io::Result<()> {
    retry(|| {
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated.
        let status = unsafe { libc::unlinkat(dir, name.as_ptr(), flags) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
}

/// Creates a folder inside `dir`. The error is `AlreadyExists` when the name
/// is taken, whatever it is.
pub(crate) fn mkdir_at(dir: RawFd, name: &CStr, mode: u32) -> io::Result<()> {
    retry(|| {
        // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated.
        let status = unsafe { libc::mkdirat(dir, name.as_ptr(), mode as libc::mode_t) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
}

/// Creates a new file inside `dir` for writing. Fails if the name exists in
/// any form, including as a link, so an existing file is never overwritten.
pub(crate) fn create_new_at(dir: RawFd, name: &CStr, mode: u32) -> io::Result<File> {
    retry(|| {
        // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated; the
        // mode is passed as the variadic argument `openat` reads with `O_CREAT`.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `openat` returned a new descriptor that nothing else owns.
            Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    })
}

/// Opens an existing regular file inside `dir` for reading. A link is refused.
pub(crate) fn open_file_at(dir: RawFd, name: &CStr) -> io::Result<File> {
    let file = retry(|| {
        // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0 as libc::c_uint,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `openat` returned a new descriptor that nothing else owns.
            Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    })?;
    // `O_NONBLOCK` keeps a named pipe from hanging the open; only plain files
    // are read.
    let stat = fstat(file.as_raw_fd())?;
    if stat.mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Reads at most `limit` bytes of a file; `None` when it is longer.
pub(crate) fn read_limited(mut file: File, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() <= limit).then_some(bytes))
}

/// Renames `from` in `from_dir` to `to` in `to_dir`, failing if `to` exists.
pub(crate) fn rename_noreplace(
    from_dir: RawFd,
    from: &CStr,
    to_dir: RawFd,
    to: &CStr,
) -> io::Result<()> {
    retry(|| {
        // SAFETY: both descriptors are open and both names are NUL-terminated.
        // `renameat2` is called through `syscall` because not every C library
        // exports it.
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                from_dir,
                from.as_ptr(),
                to_dir,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
}

/// Renames `from` to `to` inside `dir`, replacing `to` if it exists.
pub(crate) fn rename_replace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    retry(|| {
        // SAFETY: the descriptor is open and both names are NUL-terminated.
        let status = unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
}

/// The path an open folder has now, from the kernel's own record.
pub(crate) fn fd_path(fd: RawFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
}

/// Whether the current user may create entries in the folder.
pub(crate) fn can_create_in(dir: RawFd) -> bool {
    // SAFETY: `dir` is an open descriptor and "." is a valid NUL-terminated
    // name; `AT_EACCESS` asks for the effective user's rights.
    unsafe {
        libc::faccessat(
            dir,
            c".".as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EACCESS,
        ) == 0
    }
}

/// The seconds since 1970.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Local time as `YYYY-MM-DDThh:mm:ss`, which is how the Trash records when
/// something was deleted.
pub(crate) fn local_date_now() -> String {
    let now = libc::time_t::try_from(now_secs()).unwrap_or(0);
    let mut parts = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `now` is readable and `parts` is writable storage of the type the
    // call fills in.
    let filled = unsafe { libc::localtime_r(&now, parts.as_mut_ptr()) };
    if filled.is_null() {
        return super::trashinfo::format_date(1970, 1, 1, 0, 0, 0);
    }
    // SAFETY: `localtime_r` returned non-null, so it filled in the struct.
    let parts = unsafe { parts.assume_init() };
    super::trashinfo::format_date(
        i64::from(parts.tm_year) + 1900,
        u32::try_from(parts.tm_mon + 1).unwrap_or(1),
        u32::try_from(parts.tm_mday).unwrap_or(1),
        u32::try_from(parts.tm_hour).unwrap_or(0),
        u32::try_from(parts.tm_min).unwrap_or(0),
        u32::try_from(parts.tm_sec).unwrap_or(0),
    )
}

/// Modification time of an open file in whole seconds.
pub(crate) fn mtime_secs(file: &File) -> u64 {
    file.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn name_of(path: &Path) -> Option<CString> {
    CString::new(path.file_name()?.as_bytes()).ok()
}

pub(crate) fn cstring(bytes: Vec<u8>) -> io::Result<CString> {
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

pub(crate) fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// What was counted while a tree was measured or removed.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    /// Bytes the filesystem reserved for what is gone (or, when measuring, what
    /// is there). A file with several names counts once, when its last name
    /// goes.
    pub allocated: u64,
    /// Entries removed or looked at.
    pub entries: u64,
    /// Files that other names still keep alive.
    pub kept_by_other_names: u64,
    /// The first thing that stopped the work, if anything did.
    pub first_error: Option<io::Error>,
    /// Files with more names than have been removed so far, by device and
    /// inode: the names left, and the bytes the file takes.
    linked: std::collections::HashMap<(u64, u64), (u64, u64)>,
}

impl Tally {
    pub fn fail(&mut self, error: io::Error) {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
    }

    /// Counts one name of a file that has gone (or is being measured). Returns
    /// nothing; bytes are added when the last name is accounted for.
    fn count_file(&mut self, stat: &Stat) {
        self.entries += 1;
        // A name seen before makes the link count read lower than it was when
        // the first name went, so the table is consulted first.
        let known = self.linked.contains_key(&(stat.dev, stat.ino));
        if stat.is_dir() || (stat.nlink <= 1 && !known) {
            self.allocated = self.allocated.saturating_add(stat.allocated);
            return;
        }
        let entry = self
            .linked
            .entry((stat.dev, stat.ino))
            .or_insert((stat.nlink, stat.allocated));
        entry.0 = entry.0.saturating_sub(1);
        if entry.0 == 0 {
            let bytes = entry.1;
            self.linked.remove(&(stat.dev, stat.ino));
            self.allocated = self.allocated.saturating_add(bytes);
        }
    }

    /// Counts one name of a file while measuring. The bytes count on the first
    /// name seen, so a file with several names is counted once; names are
    /// tracked so that files with names outside the measured tree show up in
    /// `kept_by_other_names` after `finish`.
    fn measure_name(&mut self, stat: &Stat) {
        self.entries += 1;
        if stat.nlink <= 1 || stat.is_dir() {
            self.allocated = self.allocated.saturating_add(stat.allocated);
            return;
        }
        let key = (stat.dev, stat.ino);
        if !self.linked.contains_key(&key) {
            self.allocated = self.allocated.saturating_add(stat.allocated);
        }
        let entry = self
            .linked
            .entry(key)
            .or_insert((stat.nlink, stat.allocated));
        entry.0 = entry.0.saturating_sub(1);
        if entry.0 == 0 {
            self.linked.remove(&key);
        }
    }

    /// Settles the files whose other names were not part of the work.
    pub fn finish(&mut self) {
        self.kept_by_other_names += self.linked.len() as u64;
        self.linked.clear();
    }
}

/// What `statx` tells about a name or an open folder: the usual status, and the
/// mount the thing is on. A bind mount of the same filesystem has the same
/// device number as its source, so only the mount ID tells two mounts apart.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Meta {
    stat: Stat,
    /// `None` when the kernel gave no mount ID (before Linux 5.8).
    pub mnt: Option<u64>,
}

impl std::ops::Deref for Meta {
    type Target = Stat;
    fn deref(&self) -> &Stat {
        &self.stat
    }
}

const WANT: libc::c_uint = libc::STATX_BASIC_STATS | libc::STATX_MNT_ID;

fn statx_raw(dir: RawFd, path: *const libc::c_char, flags: libc::c_int) -> io::Result<Meta> {
    retry(|| {
        let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: `path` is a NUL-terminated string that outlives the call, `buf`
        // is writable storage of the type the call fills in, and the kernel sets
        // only the fields it reports in `stx_mask`. The buffer was zeroed, so
        // every field is initialised either way.
        let status = unsafe {
            libc::statx(
                dir,
                path,
                flags | libc::AT_NO_AUTOMOUNT,
                WANT,
                buf.as_mut_ptr(),
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed above and, on success, filled in by the kernel.
        let raw = unsafe { buf.assume_init() };
        Ok(Meta {
            stat: Stat {
                dev: libc::makedev(raw.stx_dev_major, raw.stx_dev_minor),
                ino: raw.stx_ino,
                mode: u32::from(raw.stx_mode),
                nlink: u64::from(raw.stx_nlink),
                uid: raw.stx_uid,
                size: raw.stx_size,
                allocated: raw.stx_blocks.saturating_mul(512),
            },
            mnt: (raw.stx_mask & libc::STATX_MNT_ID != 0).then_some(raw.stx_mnt_id),
        })
    })
}

/// The status of `name` inside `dir`, not following a final link.
pub(crate) fn stat_at(dir: RawFd, name: &CStr) -> io::Result<Meta> {
    statx_raw(dir, name.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
}

/// The status of an open folder or file.
pub(crate) fn fstat(fd: RawFd) -> io::Result<Meta> {
    statx_raw(fd, c"".as_ptr(), libc::AT_EMPTY_PATH)
}

/// The limits of one operation: the device and mount it started on. Anything
/// on another device or another mount is left alone, because a bind mount
/// shows other data under a name inside the folder being worked on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Boundary {
    pub dev: u64,
    pub mnt: Option<u64>,
}

impl Boundary {
    pub fn of(meta: &Meta) -> Self {
        Self {
            dev: meta.dev,
            mnt: meta.mnt,
        }
    }

    /// Whether the mounts can be told apart on this kernel.
    fn knows_mounts(&self) -> bool {
        self.mnt.is_some()
    }

    /// Why `meta` is outside the boundary, if it is.
    fn refuses(&self, meta: &Meta) -> Option<&'static str> {
        if meta.dev != self.dev {
            return Some("on another filesystem");
        }
        match (self.mnt, meta.mnt) {
            (Some(wanted), Some(found)) if wanted != found => Some("on another mount"),
            _ => None,
        }
    }
}

/// Folders nested deeper than this are not entered.
const MAX_DEPTH: usize = 256;

/// Removes `name` inside `parent`, and everything below it when it is a
/// folder. Nothing is followed: a link is removed as a link, and a folder is
/// opened with `O_NOFOLLOW` and checked against the status read first, so
/// swapping a link in for a folder makes the open fail. Anything on another
/// filesystem or another mount than the boundary (a bind mount shows other
/// data under a name in the folder) is left alone. On a kernel that cannot
/// tell mounts apart, folders are not entered at all.
///
/// Work continues past entries that fail, so one bad file does not hide the
/// rest; the first error is kept in the tally.
pub(crate) fn remove_entry(
    parent: RawFd,
    name: &CStr,
    bound: Boundary,
    depth: usize,
    tally: &mut Tally,
) {
    let stat = match stat_at(parent, name) {
        Ok(stat) => stat,
        // Already gone: someone else removed it, which is what we wanted.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => return tally.fail(error),
    };
    if let Some(why) = bound.refuses(&stat) {
        return tally.fail(io::Error::new(io::ErrorKind::InvalidInput, why));
    }
    if !stat.is_dir() {
        match unlink_at(parent, name, false) {
            Ok(()) => tally.count_file(&stat),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tally.fail(error),
        }
        return;
    }
    if !bound.knows_mounts() {
        return tally.fail(io::Error::new(
            io::ErrorKind::Unsupported,
            "this kernel cannot tell mounts apart, so folders are not entered",
        ));
    }
    if depth >= MAX_DEPTH {
        return tally.fail(io::Error::new(
            io::ErrorKind::InvalidInput,
            "folders are nested too deeply",
        ));
    }
    let dir_fd = match open_dir_at(parent, name) {
        Ok(fd) => fd,
        Err(error) => return tally.fail(error),
    };
    match fstat(dir_fd.as_raw_fd()) {
        Ok(opened)
            if opened.dev == stat.dev && opened.ino == stat.ino && opened.mnt == stat.mnt => {}
        Ok(_) => {
            return tally.fail(io::Error::other(
                "the folder was replaced during the delete",
            ));
        }
        Err(error) => return tally.fail(error),
    }
    let mut stream = match DirStream::new(dir_fd) {
        Ok(stream) => stream,
        Err(error) => return tally.fail(error),
    };
    let mut names = Vec::new();
    loop {
        match stream.next_name() {
            Ok(Some(child)) => names.push(child.to_owned()),
            Ok(None) => break,
            Err(error) => {
                tally.fail(error);
                break;
            }
        }
    }
    let fd = stream.fd();
    for child in &names {
        remove_entry(fd, child, bound, depth + 1, tally);
    }
    drop(stream);
    // A folder that still has entries cannot be removed; that failure repeats
    // the cause already recorded.
    match unlink_at(parent, name, true) {
        Ok(()) => tally.count_file(&stat),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tally.fail(error),
    }
}

/// Measures `name` inside `parent` without changing anything: nothing is
/// followed and other filesystems are not entered. Bytes are counted the way
/// the scanner counts them, with a file that has several names counted once.
/// Call [`Tally::finish`] afterwards.
pub(crate) fn measure_entry(
    parent: RawFd,
    name: &CStr,
    bound: Boundary,
    depth: usize,
    tally: &mut Tally,
) {
    let Ok(stat) = stat_at(parent, name) else {
        return;
    };
    if bound.refuses(&stat).is_some() {
        return;
    }
    tally.measure_name(&stat);
    if !stat.is_dir() || depth >= MAX_DEPTH {
        return;
    }
    let Ok(dir_fd) = open_dir_at(parent, name) else {
        return;
    };
    let Ok(opened) = fstat(dir_fd.as_raw_fd()) else {
        return;
    };
    if opened.dev != stat.dev || opened.ino != stat.ino || opened.mnt != stat.mnt {
        return;
    }
    let Ok(mut stream) = DirStream::new(dir_fd) else {
        return;
    };
    let mut names = Vec::new();
    while let Ok(Some(child)) = stream.next_name() {
        names.push(child.to_owned());
    }
    let fd = stream.fd();
    for child in &names {
        measure_entry(fd, child, bound, depth + 1, tally);
    }
}
