//! The Recycle Bin on Windows.

#![allow(unsafe_code)]

use super::winpure;
use super::{BinControl, BinInfo, BinLocation, BinName, BinProblem, LocationRole};
use crate::drives::{Drive, DriveKind};
use crate::winreg::read_dword;
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};

/// The text form of the current user's security identifier.
pub(crate) fn user_sid() -> Option<&'static str> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(read_user_sid).as_deref()
}

fn read_user_sid() -> Option<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo handle of the current process is always valid and
    // `token` is writable.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return None;
    }
    let text = sid_of_token(token);
    // SAFETY: `token` was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };
    text.filter(|t| winpure::is_sid_text(t))
}

fn sid_of_token(token: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 asks only for the size needed; `needed`
    // is writable.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 || needed > 4096 {
        return None;
    }
    // Eight-byte units keep the buffer aligned for `TOKEN_USER`.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` is writable for at least `needed` bytes, which is the
    // length passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: the call filled the buffer with a `TOKEN_USER` and the buffer is
    // aligned for it. The SID it points to lives inside the same buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID inside `buffer`; `text` is writable and
    // receives a string that the system allocated.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return None;
    }
    let mut length = 0usize;
    // SAFETY: the system returned a NUL-terminated string, so every index up to
    // the terminator can be read.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `text` points to `length` initialised UTF-16 units.
    let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    // SAFETY: the string was allocated by the system for this call, to be freed
    // with `LocalFree`, once.
    unsafe { LocalFree(text.cast()) };
    Some(value)
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// The folder that holds the current user's recycled items on a drive, if it
/// exists. No other user's folder is looked at.
pub(crate) fn locate_drive(drive: &Drive) -> Vec<BinLocation> {
    let Some(sid) = user_sid() else {
        return Vec::new();
    };
    let Some(folder) = winpure::recycle_folder(&drive.mount_point, sid) else {
        return Vec::new();
    };
    match std::fs::symlink_metadata(&folder) {
        Ok(meta) if meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 => {
            vec![BinLocation {
                path: folder,
                role: LocationRole::UserRecycleFolder,
            }]
        }
        _ => Vec::new(),
    }
}

/// Asks the system for the size and item count of the bin on a drive. The
/// error is the HRESULT.
pub(crate) fn query(root: &Path) -> Result<(u64, u64), i32> {
    let root = wide(root.as_os_str());
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        i64Size: 0,
        i64NumItems: 0,
    };
    // SAFETY: `root` is NUL-terminated and outlives the call; `info` is a
    // writable structure with its size set, as the function requires.
    let result = unsafe { SHQueryRecycleBinW(root.as_ptr(), &mut info) };
    if result < 0 {
        return Err(result);
    }
    // The structure is packed, so its fields are copied out, not borrowed.
    let (size, items) = (info.i64Size, info.i64NumItems);
    Ok(winpure::query_figures(size, items))
}

pub(crate) fn policy(drive: &Drive) -> super::BinPolicy {
    let Some(guid) = winpure::volume_guid(drive.id.as_str()) else {
        return super::BinPolicy::default();
    };
    let key = winpure::bit_bucket_key(guid);
    winpure::policy_from(
        read_dword(&key, "MaxCapacity"),
        read_dword(&key, "NukeOnDelete"),
    )
}

pub(crate) fn measure_drive(drive: &Drive, control: &BinControl) -> BinInfo {
    let mut info = BinInfo::empty(drive.id.clone());
    info.name = BinName::RecycleBin;
    info.locations = locate_drive(drive);
    info.policy = policy(drive);
    // Files deleted from a network share are removed at once; there is no bin
    // to ask.
    if drive.kind == DriveKind::Network {
        return info;
    }
    if control.is_cancelled() {
        info.complete = false;
        info.problems.push(BinProblem::Cancelled);
        return info;
    }
    match query(&drive.mount_point) {
        Ok((size, items)) => {
            info.logical_bytes = size;
            info.items = items;
        }
        Err(code) => {
            info.complete = false;
            info.problems.push(BinProblem::QueryFailed { code });
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn system_drive() -> Drive {
        let root = format!(
            "{}\\",
            std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
        );
        crate::drives::list_drives()
            .expect("drives")
            .into_iter()
            .find(|d| d.mount_point.to_string_lossy().eq_ignore_ascii_case(&root))
            .expect("the system drive is listed")
    }

    #[test]
    fn the_current_user_has_a_sid() {
        let sid = user_sid().expect("a SID");
        assert!(winpure::is_sid_text(sid), "{sid}");
        assert!(sid.starts_with("S-1-"));
    }

    #[test]
    fn the_system_drive_bin_can_be_queried() {
        let drive = system_drive();
        let info = measure_drive(&drive, &BinControl::new());
        assert_eq!(info.drive, drive.id);
        assert_eq!(info.name, BinName::RecycleBin);
        assert_eq!(info.allocated_bytes, None);
        assert!(
            !info
                .problems
                .iter()
                .any(|p| matches!(p, BinProblem::QueryFailed { .. })),
            "{:?}",
            info.problems
        );
        assert!(info.complete);
        // Only the current user's folder is ever named.
        for location in &info.locations {
            assert_eq!(location.role, LocationRole::UserRecycleFolder);
            assert_eq!(
                location.path.file_name().and_then(|n| n.to_str()),
                user_sid()
            );
        }
        assert_eq!(info.locations, locate_drive(&drive));
    }

    #[test]
    fn a_query_runs_on_a_worker_and_reports() {
        let drive = system_drive();
        let mut query = super::super::start(&drive);
        let info = query
            .wait_timeout(Duration::from_secs(120))
            .expect("finished in time");
        assert_eq!(info.drive, drive.id);
    }

    #[test]
    fn a_cancelled_query_is_incomplete() {
        let drive = system_drive();
        let control = BinControl::new();
        control.cancel();
        let info = measure_drive(&drive, &control);
        assert!(!info.complete);
        assert!(info.problems.contains(&BinProblem::Cancelled));
    }

    #[test]
    fn the_volume_policy_is_read_without_failing() {
        let drive = system_drive();
        let policy = policy(&drive);
        // Whatever the user set, a stored limit is a whole number of megabytes.
        if let Some(max) = policy.max_bytes {
            assert_eq!(max % (1024 * 1024), 0);
        }
    }
}
