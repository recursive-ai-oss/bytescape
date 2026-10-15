//! File-system calls for trashing and deleting on Linux.
//!
//! Every call is relative to an open directory and treats the last component
//! as a name, never as a place to follow: a link swapped in for a file or a
//! folder while an operation runs is acted on as a link, not as its target.

#![allow(unsafe_code)]

pub(crate) use bs_fsops::remove::{
    Boundary, Tally, fd_path, fstat, measure_entry, name_of, open_dir_following, remove_entry,
    stat_at, unlink_at,
};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

fn retry<T>(mut call: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
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

pub(crate) fn cstring(bytes: Vec<u8>) -> io::Result<CString> {
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

pub(crate) fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}
