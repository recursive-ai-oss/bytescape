//! Removing and measuring trees on Linux without following links, relative to
//! open directory descriptors, staying on the filesystem and mount the work
//! started on.

#![allow(unsafe_code)]

use crate::walk::{DirStream, Stat, c_name, open_dir_at};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
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
pub fn open_dir_following(path: &Path) -> io::Result<OwnedFd> {
    let path = c_name(path.as_os_str().as_bytes())?;
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
pub fn unlink_at(dir: RawFd, name: &CStr, is_dir: bool) -> io::Result<()> {
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

/// The path an open folder has now, from the kernel's own record.
pub fn fd_path(fd: RawFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
}

pub fn name_of(path: &Path) -> Option<CString> {
    CString::new(path.file_name()?.as_bytes()).ok()
}

/// What was counted while a tree was measured or removed.
#[derive(Debug, Default)]
pub struct Tally {
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
pub struct Meta {
    stat: Stat,
    /// `None` when the kernel gave no mount ID (before Linux 5.8).
    pub mnt: Option<u64>,
    /// When the file was created, in nanoseconds since 1970, when the
    /// filesystem keeps it and the kernel reports it (Linux 4.11 and later).
    /// Unlike an inode number it is not handed on to the next file that takes
    /// the inode.
    pub birth: Option<u64>,
}

impl std::ops::Deref for Meta {
    type Target = Stat;
    fn deref(&self) -> &Stat {
        &self.stat
    }
}

const WANT: libc::c_uint = libc::STATX_BASIC_STATS | libc::STATX_MNT_ID | libc::STATX_BTIME;

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
            birth: (raw.stx_mask & libc::STATX_BTIME != 0)
                .then(|| birth_nanos(raw.stx_btime.tv_sec, raw.stx_btime.tv_nsec))
                .flatten(),
        })
    })
}

/// A creation time as nanoseconds since 1970. `None` for a time before 1970 or
/// one that does not fit in 64 bits, which is taken as not reported.
fn birth_nanos(seconds: i64, nanos: u32) -> Option<u64> {
    let total = i128::from(seconds)
        .checked_mul(1_000_000_000)?
        .checked_add(i128::from(nanos))?;
    u64::try_from(total).ok()
}

/// The status of `name` inside `dir`, not following a final link.
pub fn stat_at(dir: RawFd, name: &CStr) -> io::Result<Meta> {
    statx_raw(dir, name.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
}

/// The status of an open folder or file.
pub fn fstat(fd: RawFd) -> io::Result<Meta> {
    statx_raw(fd, c"".as_ptr(), libc::AT_EMPTY_PATH)
}

/// The kernel's file system number (`f_type` of `statfs`) for an open folder or
/// file, asked of the descriptor itself so that no name is looked up again.
///
/// The field is `__fsword_t`: 32 bits on some targets and 64 on others, and the
/// numbers in the kernel's list are 32-bit values, so only the low 32 bits are
/// returned.
pub fn fstatfs_type(fd: RawFd) -> io::Result<i64> {
    retry(|| {
        let mut buf = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: `buf` is writable storage of the type `fstatfs` fills in, and
        // the caller's descriptor stays open for the call.
        let status = unsafe { libc::fstatfs(fd, buf.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed above and, on success, filled in by the kernel.
        let raw = unsafe { buf.assume_init() };
        #[allow(clippy::useless_conversion)]
        let number = i64::from(raw.f_type) & 0xFFFF_FFFF;
        Ok(number)
    })
}

/// Bytes in use on the file system of an open folder or file: blocks minus free
/// blocks (the part kept back for the superuser is not counted as used), asked
/// of the descriptor itself so that no name is looked up again.
pub fn fstatvfs_used(fd: RawFd) -> io::Result<u64> {
    retry(|| {
        let mut buf = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `buf` is writable storage of the type `fstatvfs` fills in,
        // and the caller's descriptor stays open for the call.
        let status = unsafe { libc::fstatvfs(fd, buf.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed above and, on success, filled in by the kernel.
        let raw = unsafe { buf.assume_init() };
        // The fields are 64 bits on the supported targets; `u64::from` keeps
        // the product exact where they are narrower.
        #[allow(clippy::useless_conversion)]
        let (blocks, free, unit) = (
            u64::from(raw.f_blocks),
            u64::from(raw.f_bfree),
            u64::from(raw.f_frsize),
        );
        Ok(blocks.saturating_sub(free).saturating_mul(unit))
    })
}

/// The limits of one operation: the device and mount it started on. Anything
/// on another device or another mount is left alone, because a bind mount
/// shows other data under a name inside the folder being worked on.
#[derive(Clone, Copy, Debug)]
pub struct Boundary {
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
pub fn remove_entry(parent: RawFd, name: &CStr, bound: Boundary, depth: usize, tally: &mut Tally) {
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
pub fn measure_entry(parent: RawFd, name: &CStr, bound: Boundary, depth: usize, tally: &mut Tally) {
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

/// The identity of a status read: the inode in the low 64 bits and, where the
/// filesystem reports it, the creation time in the high 64 bits.
///
/// The inode alone says little: a file that is deleted and replaced often gets
/// the same inode number back, so a replacement made after the item was looked
/// at would look like the item. The creation time of the new file is a later
/// time. It is as fine as the filesystem's clock, a few milliseconds on
/// common ones, so a replacement within the same tick is not told apart; that
/// is still a far smaller window than the inode alone leaves open.
pub fn identity_of(meta: &Meta) -> crate::Identity {
    crate::Identity {
        volume: meta.dev,
        file: u128::from(meta.ino) | (u128::from(meta.birth.unwrap_or(0)) << 64),
    }
}

/// Deletes `name` inside the open folder `parent`, and everything below it if
/// it is a folder, provided it is still the thing with identity `expect`.
/// Other filesystems and mounts are left alone; the first failure is reported
/// together with what was already removed.
pub fn delete_named(
    parent: RawFd,
    name: &CStr,
    expect: crate::Identity,
) -> Result<crate::Removed, crate::DeleteError> {
    use crate::DeleteError;
    let stat = match stat_at(parent, name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(DeleteError::NotFound);
        }
        Err(error) => return Err(DeleteError::Look(error)),
    };
    if identity_of(&stat) != expect {
        return Err(DeleteError::Replaced);
    }
    let mut tally = Tally::default();
    remove_entry(parent, name, Boundary::of(&stat), 0, &mut tally);
    tally.finish();
    let removed = crate::Removed {
        allocated: tally.allocated,
        entries: tally.entries,
        kept_by_other_names: tally.kept_by_other_names,
    };
    match tally.first_error.take() {
        None => Ok(removed),
        Some(error) => Err(DeleteError::Partial { removed, error }),
    }
}

#[cfg(test)]
mod used_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    // The `statvfs` fields are narrower on some targets; the sums are widened.
    #![allow(clippy::useless_conversion)]
    use crate::testutil::Fixture;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn the_bytes_in_use_are_asked_of_the_descriptor_and_not_of_a_name() {
        use std::os::fd::AsRawFd;
        let f = Fixture::new("target-used");
        f.mkdir("a");
        let folder = fs::File::open(f.path().join("a")).unwrap();
        let used = crate::remove::fstatvfs_used(folder.as_raw_fd()).unwrap();
        // Blocks minus free blocks, times the unit, as `statvfs` reports them.
        let name = std::ffi::CString::new(f.path().join("a").to_str().unwrap()).unwrap();
        let mut raw = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `name` is NUL-terminated; `raw` is a writable `statvfs`.
        assert_eq!(unsafe { libc::statvfs(name.as_ptr(), raw.as_mut_ptr()) }, 0);
        // SAFETY: filled in by the successful call above.
        let raw = unsafe { raw.assume_init() };
        let total = u64::from(raw.f_blocks) * u64::from(raw.f_frsize);
        assert!(used <= total, "{used} > {total}");
        let by_name = u64::from(raw.f_blocks).saturating_sub(u64::from(raw.f_bfree))
            * u64::from(raw.f_frsize);
        // Other tests write meanwhile: the two figures agree to within a little.
        assert!(used.abs_diff(by_name) < (1 << 30), "{used} vs {by_name}");
        // A descriptor that is not open gives an error, not a figure.
        assert!(crate::remove::fstatvfs_used(-1).is_err());
        // The answer follows the descriptor when the name is swapped.
        fs::rename(f.path().join("a"), f.path().join("moved")).unwrap();
        symlink("/dev", f.path().join("a")).unwrap();
        assert!(
            crate::remove::fstatvfs_used(folder.as_raw_fd())
                .unwrap()
                .abs_diff(used)
                < (1 << 30)
        );
    }
}
