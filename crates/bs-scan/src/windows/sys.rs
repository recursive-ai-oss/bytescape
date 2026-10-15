//! The Windows calls the walker makes, each as a small function.
//!
//! This is the only place with `unsafe` in the Windows walker.
//!
//! The scan root is opened by path (the user's own choice). Every other
//! directory is opened with `NtCreateFile`, **relative to the handle of the
//! directory it was listed in** and by that one name, with
//! `FILE_OPEN_REPARSE_POINT`: no path is resolved, so no directory above the
//! target can be swapped for a link between listing and opening, and a target
//! swapped for a link is opened as the link itself (never followed) and
//! refused by the caller. Entries are then read with
//! `GetFileInformationByHandleEx` on the handle, which returns sizes,
//! allocation, attributes and file IDs without opening any child.
#![allow(unsafe_code)]

use crate::wininfo::{self, Layout};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT,
    FILE_OPEN_REPARSE_POINT as NT_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_FILENAME_EXCED_RANGE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FIND_FIRST_EX_LARGE_FETCH, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FindClose,
    FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetDiskFreeSpaceExW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, SYNCHRONIZE, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

// The byte offsets `wininfo` reads are the layouts Windows defines. Checking
// them against the system headers' types at compile time catches a mistake
// without a Windows machine.
mod layout_checks {
    use std::mem::offset_of;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO};

    const _: () = {
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, LastWriteTime) == 24);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, EndOfFile) == 40);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, AllocationSize) == 48);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes) == 56);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength) == 60);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, EaSize) == 64);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag) == 68);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, FileId) == 72);
        assert!(offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) == 88);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, LastWriteTime) == 24);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, EndOfFile) == 40);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, AllocationSize) == 48);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes) == 56);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength) == 60);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, EaSize) == 64);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileId) == 96);
        assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) == 104);
    };
}

/// A path as the wide, NUL-terminated string Windows wants.
pub(crate) fn wide(path: &Path) -> Vec<u16> {
    let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
    w.push(0);
    w
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(
        // SAFETY: `GetLastError` has no preconditions.
        unsafe { GetLastError() } as i32,
    )
}

/// Opens the scan root by path, without following a final link. Only the root
/// is opened this way: it is the user's own choice, and the caller has
/// already resolved it.
pub(crate) fn open_dir(path_nul: &[u16]) -> io::Result<OwnedHandle> {
    // SAFETY: `path_nul` is NUL-terminated (the callers build it with
    // `wide`/`child_path`) and outlives the call; the other arguments are
    // plain values or null.
    let h = unsafe {
        CreateFileW(
            path_nul.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: `CreateFileW` returned a new handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
}

/// Opens the directory called `name` inside the open directory `parent`, by
/// that one name and relative to the parent's handle (`NtCreateFile` with a
/// root directory), so no path is resolved. A name that has since become a
/// link is opened as the link itself (`FILE_OPEN_REPARSE_POINT`), not
/// followed; the caller checks the attributes of what it got.
///
/// `name` must be a single path component (see
/// [`wininfo::is_plain_component`]); the name is matched exactly, as listed,
/// which in a case-sensitive directory is what picks the right one of two
/// names that differ only in case.
pub(crate) fn open_dir_at(parent: &OwnedHandle, name: &[u16]) -> io::Result<OwnedHandle> {
    if !wininfo::is_plain_component(name) {
        return Err(io::Error::from_raw_os_error(
            ERROR_FILENAME_EXCED_RANGE as i32,
        ));
    }
    let Some(bytes) = name
        .len()
        .checked_mul(2)
        .and_then(|b| u16::try_from(b).ok())
    else {
        return Err(io::Error::from_raw_os_error(
            ERROR_FILENAME_EXCED_RANGE as i32,
        ));
    };
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &object_name,
        // Not OBJ_CASE_INSENSITIVE: the name is exactly as listed.
        Attributes: 0,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `handle` and `status_block` are writable and live for the call;
    // `attributes` points to `object_name`, whose buffer is `name`, all of
    // which outlive the call; `parent` is an open directory handle; the
    // allocation size and extended-attribute pointers are null with a zero
    // length, which the call allows.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE
                | NT_OPEN_REPARSE_POINT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: `RtlNtStatusToDosError` only translates a number.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: the call succeeded and returned a new handle that nothing else
    // owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
}

/// The current path of the object an open handle refers to, as UTF-16 without
/// a terminator (like `\\?\C:\dir`). This is the path of the very object held, at
/// the moment of the call.
pub(crate) fn final_path(h: &OwnedHandle) -> io::Result<Vec<u16>> {
    // Paths are at most 32,767 units; anything much longer is not real.
    const LIMIT: usize = 1 << 16;
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: the handle is open; `buf` is writable for `buf.len()` units
        // (a length below 2^32 by the limit above).
        let n = unsafe {
            GetFinalPathNameByHandleW(
                h.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED,
            )
        } as usize;
        if n == 0 {
            return Err(last_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        // Too small: `n` is the size needed, with the terminator.
        if n > LIMIT {
            return Err(io::Error::from_raw_os_error(
                ERROR_FILENAME_EXCED_RANGE as i32,
            ));
        }
        buf.resize(n, 0);
    }
}

/// What `GetFileInformationByHandle` says about an open directory.
pub(crate) struct HandleInfo {
    pub attributes: u32,
    pub volume_serial: u64,
    /// The 64-bit file index. A best-effort identity together with the
    /// volume serial; zero where the filesystem has none.
    pub file_index: u64,
    pub last_write_ticks: i64,
}

pub(crate) fn handle_info(h: &OwnedHandle) -> io::Result<HandleInfo> {
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the handle is open for the duration of the call and `info` is a
    // writable structure of the right type.
    let ok = unsafe { GetFileInformationByHandle(h.as_raw_handle() as HANDLE, info.as_mut_ptr()) };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: zero-initialized and, on success, filled in by the call.
    let info = unsafe { info.assume_init() };
    let ft = &info.ftLastWriteTime;
    Ok(HandleInfo {
        attributes: info.dwFileAttributes,
        volume_serial: u64::from(info.dwVolumeSerialNumber),
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        last_write_ticks: (i64::from(ft.dwHighDateTime) << 32) | i64::from(ft.dwLowDateTime),
    })
}

/// The key that identifies the folder an open handle is: the volume serial
/// number and the 128-bit file ID (`FILE_ID_INFO`). Asked the same way for the
/// handle a caller judged and the one the walker opened, so the two can be
/// compared. Fails on a volume that gives no 128-bit file ID.
pub(crate) fn root_key_of_borrowed(h: BorrowedHandle<'_>) -> io::Result<crate::RootKey> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the handle is open for the duration of the call; `info` is a
    // writable `FILE_ID_INFO` and the size passed is its size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle() as HANDLE,
            FileIdInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(crate::RootKey::Volume {
        serial: info.VolumeSerialNumber,
        file_id: u128::from_le_bytes(info.FileId.Identifier),
    })
}

/// [`root_key_of_borrowed`] for an owned handle.
pub(crate) fn root_key(h: &OwnedHandle) -> io::Result<crate::RootKey> {
    root_key_of_borrowed(h.as_handle())
}

/// A buffer for directory records, aligned the way the system structures
/// need.
pub(crate) struct ListBuffer {
    words: Vec<u64>,
}

impl ListBuffer {
    pub fn new(bytes: usize) -> ListBuffer {
        ListBuffer {
            words: vec![0; bytes.div_ceil(8)],
        }
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: a `Vec<u64>` is `len * 8` initialized bytes at an aligned
        // address; viewing them as bytes is always valid.
        unsafe {
            std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.words.len() * 8)
        }
    }
}

/// How one listing call ended.
pub(crate) enum Listed {
    /// The buffer holds records.
    Records,
    /// No more entries.
    End,
    /// The class is not supported by this filesystem.
    Unsupported,
    Failed(io::Error),
}

/// Fills `buf` with the next group of records. `restart` begins the listing
/// again.
pub(crate) fn list(h: &OwnedHandle, layout: Layout, restart: bool, buf: &mut ListBuffer) -> Listed {
    let class = match (layout, restart) {
        (Layout::Extended, true) => FileIdExtdDirectoryRestartInfo,
        (Layout::Extended, false) => FileIdExtdDirectoryInfo,
        (Layout::Both, true) => FileIdBothDirectoryRestartInfo,
        (Layout::Both, false) => FileIdBothDirectoryInfo,
    };
    // The previous contents must not be mistaken for records.
    buf.words.fill(0);
    let len = (buf.words.len() * 8).min(u32::MAX as usize) as u32;
    // SAFETY: the handle is open; the buffer is writable, 8-byte aligned and
    // `len` bytes long.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle() as HANDLE,
            class,
            buf.words.as_mut_ptr().cast(),
            len,
        )
    };
    if ok != 0 {
        return Listed::Records;
    }
    match last_error().raw_os_error().map(|c| c as u32) {
        Some(windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES) => Listed::End,
        Some(
            windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER
            | windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED
            | windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION,
        ) if restart => Listed::Unsupported,
        _ => Listed::Failed(last_error()),
    }
}

/// One entry from `FindFirstFileExW`, which reports no allocation size and no
/// file ID.
pub(crate) struct FoundEntry {
    pub name: Vec<u16>,
    pub attributes: u32,
    pub reparse_tag: u32,
    pub logical: u64,
    pub last_write_ticks: i64,
}

/// Lists a directory with `FindFirstFileExW`, for filesystems that reject the
/// handle-based classes. `dir_path` is the directory's path, not NUL
/// terminated. This is the one place a path is resolved to read a directory;
/// the caller checks afterwards that the path still names the directory it
/// holds open.
pub(crate) fn find_all(dir_path: &[u16]) -> io::Result<Vec<FoundEntry>> {
    let mut pattern = wininfo::join_path(dir_path, &[u16::from(b'*')]);
    pattern.push(0);
    let mut data = std::mem::MaybeUninit::<WIN32_FIND_DATAW>::zeroed();
    // SAFETY: `pattern` is NUL-terminated; `data` is a writable find-data
    // structure.
    let h = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            data.as_mut_ptr().cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: `data` was filled by the call that returned success.
        let d = unsafe { &*data.as_ptr() };
        let len = d
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(d.cFileName.len());
        let ft = &d.ftLastWriteTime;
        out.push(FoundEntry {
            name: d.cFileName[..len].to_vec(),
            attributes: d.dwFileAttributes,
            reparse_tag: d.dwReserved0,
            logical: (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow),
            last_write_ticks: (i64::from(ft.dwHighDateTime) << 32) | i64::from(ft.dwLowDateTime),
        });
        // SAFETY: `h` is a live find handle and `data` is writable.
        if unsafe { FindNextFileW(h, data.as_mut_ptr().cast()) } == 0 {
            break;
        }
    }
    let end = last_error();
    // SAFETY: `h` is a live find handle, closed exactly once.
    unsafe { FindClose(h) };
    if end.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES as i32) {
        Ok(out)
    } else {
        Err(end)
    }
}

/// Used bytes on the volume holding `path`: its size minus all of its free
/// space.
pub(crate) fn volume_used(path: &Path) -> io::Result<u64> {
    let w = wide(path);
    let (mut to_caller, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `w` is NUL-terminated; the three outputs are writable `u64`s.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut to_caller, &mut total, &mut free) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(total.saturating_sub(free))
}
