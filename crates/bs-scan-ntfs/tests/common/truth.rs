//! What the system itself says about an entry, for the live comparison.
//!
//! Each question opens the entry the way an ordinary program does (for
//! attributes only, as a reparse point so that a link is described and never
//! followed, and never reading its data) and asks the open handle. That is a
//! different path to the facts than a directory listing, which reports the
//! copy a folder keeps, and than the table, which the reader parses.
#![cfg(windows)]
#![allow(unsafe_code, dead_code)]

use super::live::{Answer, Oracle, Truth};
use super::wofrec::{RecordFacts, read_record};
use bs_scan::winrules::filetime_to_unix;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME, ERROR_PATH_NOT_FOUND, GENERIC_READ, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_SPARSE_FILE,
    FILE_ATTRIBUTE_TAG_INFO, FILE_COMPRESSION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
    FileCompressionInfo, FileIdInfo, FileStandardInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_EXTERNAL_BACKING, FSCTL_GET_NTFS_FILE_RECORD};

pub struct SystemOracle {
    root: PathBuf,
    /// The volume's device path (`\\.\C:`), opened on first use to fetch
    /// file records. `None` when the caller gave none.
    volume: Option<PathBuf>,
    volume_handle: OnceLock<Result<OwnedHandle, String>>,
}

impl SystemOracle {
    /// `root` is the scanned folder, as the scan was given it.
    pub fn new(root: &Path) -> SystemOracle {
        SystemOracle {
            root: root.to_path_buf(),
            volume: None,
            volume_handle: OnceLock::new(),
        }
    }

    /// Lets the oracle fetch file records from the volume at `device`.
    #[must_use]
    pub fn with_volume(mut self, device: &Path) -> SystemOracle {
        self.volume = Some(device.to_path_buf());
        self
    }

    fn volume(&self) -> Result<&OwnedHandle, String> {
        self.volume_handle
            .get_or_init(|| {
                let device = self
                    .volume
                    .as_ref()
                    .ok_or_else(|| "no volume was given for file records".to_owned())?;
                let name = wide(device);
                // SAFETY: `name` is NUL-terminated and outlives the call; the
                // other arguments are plain values or null.
                let h = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_READ,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if h == INVALID_HANDLE_VALUE {
                    return Err(format!("cannot open the volume: {}", last_error()));
                }
                // SAFETY: `CreateFileW` returned a new handle that nothing else owns.
                Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

fn last_error() -> io::Error {
    // SAFETY: `GetLastError` has no preconditions.
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn wide(path: &Path) -> Vec<u16> {
    let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
    w.push(0);
    w
}

/// The open handle's own answers.
fn truth_of(h: &OwnedHandle) -> io::Result<Truth> {
    let raw = h.as_raw_handle() as HANDLE;

    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the handle is open for the call and `info` is a writable
    // structure of the right type.
    if unsafe { GetFileInformationByHandle(raw, info.as_mut_ptr()) } == 0 {
        return Err(last_error());
    }
    // SAFETY: zero-initialized and, on success, filled in by the call.
    let info = unsafe { info.assume_init() };
    let ticks = (i64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
        | i64::from(info.ftLastWriteTime.dwLowDateTime);

    let mut std_info = std::mem::MaybeUninit::<FILE_STANDARD_INFO>::zeroed();
    // SAFETY: as above; the size passed is the structure's.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw,
            FileStandardInfo,
            std_info.as_mut_ptr().cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    // A failed call is no answer, never an allocation of 0.
    let raw_size = if ok == 0 {
        0
    } else {
        // SAFETY: filled in by the successful call.
        unsafe { std_info.assume_init() }.AllocationSize
    };
    let allocation = super::live::allocation_answer(ok != 0, raw_size).map_err(|why| {
        if ok == 0 {
            last_error()
        } else {
            io::Error::other(why)
        }
    })?;

    let mut tag = std::mem::MaybeUninit::<FILE_ATTRIBUTE_TAG_INFO>::zeroed();
    // SAFETY: as above.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw,
            FileAttributeTagInfo,
            tag.as_mut_ptr().cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    let reparse_tag = if ok == 0 {
        0
    } else {
        // SAFETY: filled in by the successful call.
        unsafe { tag.assume_init() }.ReparseTag
    };

    // The provider that backs the file's data from elsewhere, if any: the
    // answer is a version and a provider number (8 bytes at least) followed by
    // the provider's own data.
    let mut out = [0u8; 64];
    let mut returned = 0u32;
    // SAFETY: the handle is open; no input; `out` is writable for its length
    // and `returned` for the byte count; no overlapped I/O is used.
    let ok = unsafe {
        DeviceIoControl(
            raw,
            FSCTL_GET_EXTERNAL_BACKING,
            std::ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    let backing = (ok != 0 && returned >= 8)
        .then(|| out.get(4..8))
        .flatten()
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes);

    // What the file holds in clusters when it is sparse or compressed. For
    // any other file the system reports the plain length here, which says
    // nothing about clusters, so it is not kept.
    let mut comp = std::mem::MaybeUninit::<FILE_COMPRESSION_INFO>::zeroed();
    // SAFETY: as above.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw,
            FileCompressionInfo,
            comp.as_mut_ptr().cast(),
            size_of::<FILE_COMPRESSION_INFO>() as u32,
        )
    };
    let held = (ok != 0
        && info.dwFileAttributes & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED) != 0)
        // SAFETY: filled in by the successful call.
        .then(|| u64::try_from(unsafe { comp.assume_init() }.CompressedFileSize).ok())
        .flatten();

    Ok(Truth {
        attributes: info.dwFileAttributes,
        reparse_tag,
        backing,
        mtime: filetime_to_unix(ticks),
        logical: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        allocation,
        held,
    })
}

fn open(name: &[u16]) -> io::Result<OwnedHandle> {
    // SAFETY: `name` is NUL-terminated and outlives the call; the other
    // arguments are plain values or null. Only attributes are requested, and
    // a link is opened as itself, so nothing is read and nothing is followed
    // or fetched.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
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

/// The file reference (record number and sequence) the open handle's file ID
/// holds, for a file on NTFS.
fn file_reference(h: &OwnedHandle) -> Result<(u64, u16), String> {
    let mut id = FILE_ID_INFO::default();
    // SAFETY: the handle is open for the call; `id` is a writable
    // `FILE_ID_INFO` and the size passed is its size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle() as HANDLE,
            FileIdInfo,
            std::ptr::from_mut(&mut id).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(format!("no file ID: {}", last_error()));
    }
    let bytes = id.FileId.Identifier;
    if bytes
        .get(8..)
        .is_some_and(|rest| rest.iter().any(|&b| b != 0))
    {
        return Err("the file ID is not an NTFS file reference".into());
    }
    let reference = u64::from_le_bytes(bytes[..8].try_into().map_err(|_| "file ID")?);
    Ok((reference & 0x0000_FFFF_FFFF_FFFF, (reference >> 48) as u16))
}

/// The most a record fetch returns: its header and a record of at most 4 KiB.
const RECORD_REPLY: usize = 12 + 4096;

/// Fetches the file record `number` from the volume. The call returns the
/// nearest record at or below the one asked for, so the number is checked.
fn fetch_record(volume: &OwnedHandle, number: u64) -> Result<Vec<u8>, String> {
    let input = i64::try_from(number).map_err(|_| "record number".to_owned())?;
    let mut out = vec![0u64; RECORD_REPLY.div_ceil(8) + 1];
    let mut returned = 0u32;
    // SAFETY: the volume handle is open; the input is the 8-byte structure the
    // control code takes, the output a writable buffer of the length passed,
    // `returned` a writable count; no overlapped I/O is used.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle() as HANDLE,
            FSCTL_GET_NTFS_FILE_RECORD,
            std::ptr::from_ref(&input).cast(),
            size_of::<i64>() as u32,
            out.as_mut_ptr().cast(),
            (out.len() * 8) as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!(
            "FSCTL_GET_NTFS_FILE_RECORD failed: {}",
            last_error()
        ));
    }
    // SAFETY: a `Vec<u64>` is `len * 8` initialized bytes.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), out.len() * 8) };
    let returned = usize::try_from(returned).unwrap_or(0).min(bytes.len());
    let head = bytes.get(..12).ok_or("short reply")?;
    let got = u64::from_le_bytes(head[..8].try_into().map_err(|_| "reply")?);
    if got & 0x0000_FFFF_FFFF_FFFF != number {
        return Err(format!(
            "the volume returned record {} for record {number}",
            got & 0x0000_FFFF_FFFF_FFFF
        ));
    }
    let len = usize::try_from(u32::from_le_bytes(
        head[8..12].try_into().map_err(|_| "reply")?,
    ))
    .map_err(|_| "record length")?;
    let end = 12usize.checked_add(len).ok_or("record length")?;
    if end > returned {
        return Err(format!(
            "the record is {len} bytes but the reply holds {returned}"
        ));
    }
    Ok(bytes.get(12..end).ok_or("record bytes")?.to_vec())
}

impl Oracle for SystemOracle {
    /// The record of the file the path names now. Like every answer here it
    /// is asked by path, not by a file reference the reader recorded (its tree
    /// carries none); the sequence check ties the record to the file opened
    /// by that path, and the class that uses it also needs the lengths to
    /// agree, so a file replaced since the scan does not explain a difference
    /// by accident.
    fn file_record(&self, path: &str) -> Result<RecordFacts, String> {
        if path.contains('\u{FFFD}') {
            return Err("the name is not valid text".into());
        }
        let handle = open(&wide(&self.root.join(path.replace('/', "\\"))))
            .map_err(|e| format!("cannot open the file: {e}"))?;
        let (number, sequence) = file_reference(&handle)?;
        let raw = fetch_record(self.volume()?, number)?;
        let facts = read_record(&raw)?;
        // The record must be this file's: a record number can be reused.
        if facts.sequence != sequence {
            return Err(format!(
                "record {number} has sequence {}, the file's ID says {sequence}",
                facts.sequence
            ));
        }
        Ok(facts)
    }

    fn ask(&self, path: &str) -> Answer {
        // A name that was not valid text was replaced when the trees were
        // compared; it cannot be asked for, and a failed lookup of the
        // replacement must not be read as "gone".
        if path.contains('\u{FFFD}') {
            return Answer::Unavailable("the name is not valid text".into());
        }
        let full = self.root.join(path.replace('/', "\\"));
        let name = wide(&full);
        let handle = match open(&name) {
            Ok(h) => h,
            Err(e) => {
                let code = e.raw_os_error().map(|c| c as u32);
                return if code == Some(ERROR_FILE_NOT_FOUND) || code == Some(ERROR_PATH_NOT_FOUND) {
                    Answer::Gone
                } else if code == Some(ERROR_INVALID_NAME) {
                    Answer::Unavailable("the name cannot be opened".into())
                } else {
                    Answer::Unavailable(e.to_string())
                };
            }
        };
        match truth_of(&handle) {
            Ok(t) => Answer::Found(t),
            Err(e) => Answer::Unavailable(e.to_string()),
        }
    }
}
