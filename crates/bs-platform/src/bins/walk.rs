//! Reading directories without following links, on Linux.
//!
//! Every step goes through a directory file descriptor and `O_NOFOLLOW`, so a
//! link swapped in for a folder while the walk runs makes the open fail
//! instead of leading somewhere else.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The parts of `struct stat` that the bin measurement uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    /// The apparent size.
    pub size: u64,
    /// Bytes the filesystem allocated (512-byte blocks, times 512).
    pub allocated: u64,
}

impl Stat {
    fn from_raw(raw: &libc::stat) -> Self {
        Self {
            dev: raw.st_dev,
            ino: raw.st_ino,
            mode: raw.st_mode,
            // 64 bits on x86-64 and 32 on ARM64.
            #[allow(clippy::useless_conversion)]
            nlink: u64::from(raw.st_nlink),
            uid: raw.st_uid,
            size: u64::try_from(raw.st_size).unwrap_or(0),
            allocated: u64::try_from(raw.st_blocks)
                .unwrap_or(0)
                .saturating_mul(512),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }

    #[cfg(test)]
    pub fn is_regular(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    pub fn is_sticky(&self) -> bool {
        self.mode & libc::S_ISVTX != 0
    }
}

/// The user id of this process.
pub(crate) fn current_uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Whether this process has the superuser's rights, which can read anything.
#[cfg(test)]
pub(crate) fn running_as_superuser() -> bool {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

pub(crate) fn c_name(name: &[u8]) -> io::Result<CString> {
    CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

/// Repeats a call that was interrupted by a signal.
fn retry<T>(mut call: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Opens a folder named by the caller. The path may go through links: it is
/// a place the user or the mount table named. Only the last component must be
/// a real folder.
pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let path = c_name(path.as_os_str().as_bytes())?;
    retry(|| {
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let fd = unsafe { libc::open(path.as_ptr(), DIR_FLAGS) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `open` returned a new descriptor that nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    })
}

/// Opens the folder `name` inside `parent`. A link is never followed.
pub(crate) fn open_dir_at(parent: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    retry(|| {
        // SAFETY: `parent` is a descriptor the caller holds open and `name` is
        // a valid NUL-terminated string for the call.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), DIR_FLAGS) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `openat` returned a new descriptor that nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    })
}

/// The status of `name` inside `parent`, not following a link.
pub(crate) fn stat_at(parent: RawFd, name: &CStr) -> io::Result<Stat> {
    retry(|| {
        let mut raw = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `parent` is an open descriptor, `name` is NUL-terminated and
        // `raw` is writable storage of the type the call fills in.
        let status = unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                raw.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so it filled in the struct.
        Ok(Stat::from_raw(&unsafe { raw.assume_init() }))
    })
}

/// The status of an open descriptor.
pub(crate) fn fstat(fd: RawFd) -> io::Result<Stat> {
    retry(|| {
        let mut raw = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `fd` is open for the caller and `raw` is writable storage of
        // the type the call fills in.
        let status = unsafe { libc::fstat(fd, raw.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so it filled in the struct.
        Ok(Stat::from_raw(&unsafe { raw.assume_init() }))
    })
}

/// An open directory being read.
pub(crate) struct DirStream {
    dir: *mut libc::DIR,
}

impl DirStream {
    /// Takes over the descriptor.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let raw = fd.into_raw_fd();
        // SAFETY: `raw` is an open descriptor for a directory that this call
        // takes ownership of when it succeeds.
        let dir = unsafe { libc::fdopendir(raw) };
        if dir.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed, so the descriptor is still ours.
            unsafe { libc::close(raw) };
            return Err(error);
        }
        Ok(Self { dir })
    }

    /// The descriptor of the folder, to resolve names inside it.
    pub fn fd(&self) -> RawFd {
        // SAFETY: `self.dir` is a live directory stream.
        unsafe { libc::dirfd(self.dir) }
    }

    /// The next name, without `.` and `..`. The name is valid until the next
    /// call. `Ok(None)` at the end.
    pub fn next_name(&mut self) -> io::Result<Option<&CStr>> {
        loop {
            // SAFETY: `__errno_location` always returns a valid pointer to this
            // thread's error number. It is cleared so that a null result can
            // be told apart: end of folder, or an error.
            unsafe { *libc::__errno_location() = 0 };
            // SAFETY: `self.dir` is a live directory stream.
            let entry = unsafe { libc::readdir(self.dir) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) | None => Ok(None),
                    Some(_) => Err(error),
                };
            }
            // SAFETY: `readdir` returned a valid entry whose name is a
            // NUL-terminated string that stays valid until the next `readdir`
            // on this stream, which needs `&mut self` and so cannot happen
            // while the returned borrow is alive.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                return Ok(Some(name));
            }
        }
    }
}

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: `self.dir` is a live directory stream that is closed once.
        unsafe { libc::closedir(self.dir) };
    }
}

// The stream is used by one thread at a time; it is moved, never shared.
// SAFETY: a `DIR*` has no thread affinity, and `DirStream` offers no way to
// use it from two threads at once.
unsafe impl Send for DirStream {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::os::fd::AsRawFd;

    #[test]
    fn stat_reports_kinds_and_sizes() {
        let f = Fixture::new("walk-stat");
        f.write("a/file", "12345");
        f.symlink("a/file", "link");
        let root = open_dir(f.path()).unwrap();
        let fd = root.as_raw_fd();
        let dir = stat_at(fd, c"a").unwrap();
        assert!(dir.is_dir() && !dir.is_symlink() && !dir.is_regular());
        let a = open_dir_at(fd, c"a").unwrap();
        let file = stat_at(a.as_raw_fd(), c"file").unwrap();
        assert!(file.is_regular());
        assert_eq!(file.size, 5);
        assert!(file.allocated >= 5 && file.allocated.is_multiple_of(512));
        // A link is described, not followed.
        let link = stat_at(fd, c"link").unwrap();
        assert!(link.is_symlink() && !link.is_regular());
        assert_eq!(link.size, "a/file".len() as u64);
        assert_eq!(fstat(fd).unwrap().dev, dir.dev);
    }

    #[test]
    fn a_link_is_never_opened_as_a_folder() {
        let f = Fixture::new("walk-nofollow");
        f.mkdir("real");
        f.symlink("real", "link");
        f.write("plain", "x");
        let root = open_dir(f.path()).unwrap();
        let fd = root.as_raw_fd();
        assert!(open_dir_at(fd, c"real").is_ok());
        // With `O_DIRECTORY` the kernel reports a link as "not a directory".
        let through_link = open_dir_at(fd, c"link").unwrap_err();
        assert!(matches!(
            through_link.raw_os_error(),
            Some(libc::ENOTDIR | libc::ELOOP)
        ));
        let not_a_dir = open_dir_at(fd, c"plain").unwrap_err();
        assert_eq!(not_a_dir.raw_os_error(), Some(libc::ENOTDIR));
        // The path form accepts a link in the middle (the caller named it) but
        // not as the last component.
        assert!(open_dir(&f.path().join("link")).is_err());
        f.mkdir("real/inner");
        assert!(open_dir(&f.path().join("link").join("inner")).is_ok());
    }

    #[test]
    fn directory_streams_list_names_without_dots() {
        let f = Fixture::new("walk-list");
        f.write("one", "");
        f.write("two", "");
        f.mkdir("three");
        let root = open_dir(f.path()).unwrap();
        let mut stream = DirStream::new(root).unwrap();
        let mut names = Vec::new();
        while let Some(name) = stream.next_name().unwrap() {
            names.push(name.to_bytes().to_vec());
        }
        names.sort();
        assert_eq!(
            names,
            vec![b"one".to_vec(), b"three".to_vec(), b"two".to_vec()]
        );
    }

    #[test]
    fn names_with_nul_are_refused() {
        assert!(c_name(b"a\0b").is_err());
        assert!(c_name(b"a b\n").is_ok());
    }
}
