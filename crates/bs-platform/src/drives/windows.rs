//! Windows drive enumeration: drive letters, volume information and sizes.
//!
//! Volumes that are mounted on a folder instead of a drive letter are not
//! listed by [`list`]. [`volume_for_path`] does resolve a folder on such a
//! volume.
//!
//! Every query about one volume runs on its own thread with a time limit,
//! because a disconnected network drive can block `GetVolumeInformationW` and
//! `GetDiskFreeSpaceExW` for a long time.

#![allow(unsafe_code)]

use super::probe::{InFlight, Job, run_parallel};
use super::winkind::{DRIVE_FIXED, DRIVE_NO_ROOT_DIR, kind_of, letters, volume_id};
use super::{
    Capacity, CapacityProblem, Drive, DriveError, DriveId, DriveKind, Mount,
    eligible_for_background_scan, sort_for_display,
};
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeUsb, CreateFileW, DISK_SPACE_INFORMATION, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetThreadErrorMode};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemServices::FILE_READ_ONLY_VOLUME;

/// Longest label or file system name `GetVolumeInformationW` can return.
const NAME_BUFFER: usize = 261;
/// Room for a path returned by `GetVolumePathNameW`.
const PATH_BUFFER: usize = 1024;

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// The text before the first NUL.
fn from_wide(buffer: &[u16]) -> OsString {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    OsString::from_wide(&buffer[..end])
}

pub(crate) fn list(timeout: Duration) -> Vec<Drive> {
    // SAFETY: takes no arguments and only reads the session's drive table.
    let mask = unsafe { GetLogicalDrives() };
    let roots: Vec<PathBuf> = letters(mask)
        .into_iter()
        .map(|letter| PathBuf::from(format!("{letter}:\\")))
        .collect();
    let mut drives: Vec<Drive> = probe_all(&roots, timeout).into_iter().flatten().collect();
    sort_for_display(&mut drives);
    drives
}

pub(crate) fn volume_for_path(path: &Path, timeout: Duration) -> Result<Drive, DriveError> {
    // Finding the volume of a path can touch a disconnected share, so it runs
    // on a worker with a time limit like the other queries.
    let mut key = b"path\0".to_vec();
    key.extend_from_slice(path.to_string_lossy().to_lowercase().as_bytes());
    let owned = path.to_path_buf();
    let resolve: Job<Result<PathBuf, DriveError>> = Box::new(move || volume_root(&owned));
    let root = run_parallel(&InFlight::shared(), vec![(key, resolve)], timeout)
        .pop()
        .flatten()
        .ok_or(DriveError::TimedOut)??;
    probe_all(&[root], timeout)
        .pop()
        .flatten()
        .ok_or(DriveError::NoVolume)
}

/// The root of the volume that holds `path`, resolved by the system without
/// looking at the path's components one by one.
fn volume_root(path: &Path) -> Result<PathBuf, DriveError> {
    // Lexical only: no link in the path is resolved here.
    let absolute = std::path::absolute(path).map_err(DriveError::Path)?;
    let input = wide(absolute.as_os_str());
    // The system would silently read only the text before a NUL.
    if input[..input.len() - 1].contains(&0) {
        return Err(DriveError::Path(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        )));
    }
    let mut buffer = [0u16; PATH_BUFFER];
    // SAFETY: `input` is NUL-terminated and outlives the call; `buffer` is
    // writable for the `PATH_BUFFER` characters passed as its length.
    let ok = unsafe { GetVolumePathNameW(input.as_ptr(), buffer.as_mut_ptr(), PATH_BUFFER as u32) };
    if ok == 0 {
        return Err(DriveError::Path(io::Error::last_os_error()));
    }
    Ok(PathBuf::from(from_wide(&buffer)))
}

fn drive_type(root: &[u16]) -> u32 {
    // SAFETY: `root` is a NUL-terminated string that outlives the call.
    unsafe { GetDriveTypeW(root.as_ptr()) }
}

/// Probes every root at once. A root that does not answer in time is listed
/// without sizes; a root with no volume behind it yields `None`.
fn probe_all(roots: &[PathBuf], timeout: Duration) -> Vec<Option<Drive>> {
    let jobs = roots
        .iter()
        .map(|root| {
            let mut key = b"probe\0".to_vec();
            key.extend_from_slice(root.to_string_lossy().to_lowercase().as_bytes());
            let owned = root.clone();
            let job: Job<Option<Drive>> = Box::new(move || probe(&owned));
            (key, job)
        })
        .collect();
    run_parallel(&InFlight::shared(), jobs, timeout)
        .into_iter()
        .zip(roots)
        .map(|(result, root)| result.unwrap_or_else(|| Some(timed_out(root))))
        .collect()
}

/// A drive whose queries did not answer. Even its kind is unknown, because the
/// system calls that tell it may be the ones that hang.
fn timed_out(root: &Path) -> Drive {
    Drive {
        id: DriveId::new(fallback_id(root)),
        mount_point: root.to_path_buf(),
        label: None,
        fs_type: String::new(),
        kind: DriveKind::Other,
        read_only: false,
        eligible_for_background_scan: false,
        capacity: None,
        capacity_problem: Some(CapacityProblem::TimedOut),
        mounts: vec![only_mount(root)],
    }
}

/// A Windows drive is one mount that shows the whole volume.
fn only_mount(root: &Path) -> Mount {
    Mount {
        mount_point: root.to_path_buf(),
        mount_id: None,
        root: PathBuf::from("\\"),
        covered_by_other_mount: false,
    }
}

/// An identifier for a volume that has no GUID name, such as a network share.
fn fallback_id(root: &Path) -> String {
    let text = root.to_string_lossy();
    if text.starts_with(r"\\") {
        format!("unc:{}", text.to_lowercase())
    } else {
        format!("path:{}", text.to_uppercase())
    }
}

/// Asks the operating system about one volume. Runs on a worker thread.
/// `None` when the letter or folder has no volume.
fn probe(root: &Path) -> Option<Drive> {
    // Do not let a missing disc or card raise a system dialog.
    let mut previous = 0u32;
    // SAFETY: `previous` is writable; the call only changes this thread's error mode.
    unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, &mut previous) };

    let root_wide = wide(root.as_os_str());
    let drive_type = drive_type(&root_wide);
    if drive_type == DRIVE_NO_ROOT_DIR {
        return None;
    }
    let info = volume_information(&root_wide);
    let volume_name = volume_name(&root_wide);
    let (usb_bus, removable_media) = if drive_type == DRIVE_FIXED {
        storage_flags(volume_name.as_deref(), root)
    } else {
        (false, false)
    };
    let kind = kind_of(drive_type, usb_bus, removable_media)?;
    let (capacity, capacity_problem) = match disk_space(&root_wide) {
        Ok(capacity) => (Some(capacity), None),
        Err(problem) => (None, Some(problem)),
    };
    let (label, fs_type, flags) = info.unwrap_or_default();
    let id = volume_name
        .as_deref()
        .and_then(volume_id)
        .unwrap_or_else(|| fallback_id(root));
    Some(Drive {
        id: DriveId::new(id),
        mount_point: root.to_path_buf(),
        label: (!label.is_empty()).then_some(label),
        fs_type,
        kind,
        read_only: flags & FILE_READ_ONLY_VOLUME != 0,
        eligible_for_background_scan: eligible_for_background_scan(kind, capacity.as_ref()),
        capacity,
        capacity_problem,
        mounts: vec![only_mount(root)],
    })
}

/// The label, file system name and file system flags of a volume.
fn volume_information(root: &[u16]) -> Option<(String, String, u32)> {
    let mut label = [0u16; NAME_BUFFER];
    let mut fs_name = [0u16; NAME_BUFFER];
    let mut flags = 0u32;
    // SAFETY: `root` is NUL-terminated; both name buffers are writable for the
    // lengths passed; `flags` is writable; the optional out-parameters are null.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            NAME_BUFFER as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            fs_name.as_mut_ptr(),
            NAME_BUFFER as u32,
        )
    };
    (ok != 0).then(|| {
        (
            from_wide(&label).to_string_lossy().into_owned(),
            from_wide(&fs_name).to_string_lossy().into_owned(),
            flags,
        )
    })
}

/// The `\\?\Volume{GUID}\` name of the volume mounted at `root`.
fn volume_name(root: &[u16]) -> Option<String> {
    let mut buffer = [0u16; 64];
    // SAFETY: `root` is NUL-terminated; `buffer` is writable for the length passed.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32)
    };
    (ok != 0).then(|| from_wide(&buffer).to_string_lossy().into_owned())
}

type DiskSpaceInformationFn =
    unsafe extern "system" fn(*const u16, *mut DISK_SPACE_INFORMATION) -> i32;

/// `GetDiskSpaceInformationW`, looked up at run time because it exists only
/// from Windows 10 version 1809. Linking to it directly would stop the program
/// from starting on an older system.
fn disk_space_information_fn() -> Option<DiskSpaceInformationFn> {
    static FUNCTION: OnceLock<Option<DiskSpaceInformationFn>> = OnceLock::new();
    *FUNCTION.get_or_init(|| {
        let module_name = wide(OsStr::new("kernel32.dll"));
        // SAFETY: `module_name` is NUL-terminated and outlives the call; kernel32
        // is loaded in every process, and this call takes no reference on it.
        let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
        if module.is_null() {
            return None;
        }
        // SAFETY: `module` is a valid module handle and the name is a
        // NUL-terminated byte string.
        let address =
            unsafe { GetProcAddress(module, c"GetDiskSpaceInformationW".as_ptr().cast()) }?;
        // SAFETY: the export of this name has the signature in
        // `DiskSpaceInformationFn`: a root path and a pointer to the structure,
        // returning an HRESULT. Function pointers have the same size.
        Some(unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, DiskSpaceInformationFn>(
                address,
            )
        })
    })
}

/// The sizes of a volume without the effect of per-user quotas.
///
/// The disk-space report gives the volume's own total and free space beside
/// the figures limited for the calling user, so a quota shows up as free space
/// the user cannot use. It works without elevation. Where it does not exist or
/// the volume refuses it, `GetDiskFreeSpaceExW` is used, whose total is the
/// user's own and can be smaller than the volume; the figures are then
/// accepted only when no quota shows (see [`Capacity::from_user_scoped_bytes`]).
fn disk_space(root: &[u16]) -> Result<Capacity, CapacityProblem> {
    if let Some(function) = disk_space_information_fn() {
        let mut info = DISK_SPACE_INFORMATION::default();
        // SAFETY: `root` is NUL-terminated and outlives the call; `info` is a
        // writable structure of the type the function fills in.
        let result = unsafe { function(root.as_ptr(), &mut info) };
        if result >= 0 {
            let unit = u64::from(info.SectorsPerAllocationUnit) * u64::from(info.BytesPerSector);
            return Capacity::from_allocation_units(
                unit,
                info.ActualTotalAllocationUnits,
                info.ActualAvailableAllocationUnits,
                info.CallerAvailableAllocationUnits,
            )
            .ok_or(CapacityProblem::Failed(io::ErrorKind::InvalidData));
        }
    }
    let (mut available, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: `root` is NUL-terminated; the three outputs are writable `u64`s.
    let ok =
        unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut available, &mut total, &mut total_free) };
    if ok == 0 {
        return Err(CapacityProblem::Failed(io::Error::last_os_error().kind()));
    }
    Capacity::from_user_scoped_bytes(total, total_free, available)
        .ok_or(CapacityProblem::QuotaHidesVolumeSize)
}

/// Asks the storage stack whether the disk under a volume is on the USB bus or
/// is removable media. Opens the volume with no access rights, which any user
/// may do. A volume that cannot be queried (for example one spread over
/// several disks) is reported as neither.
fn storage_flags(volume_name: Option<&str>, root: &Path) -> (bool, bool) {
    let device = match volume_name {
        Some(name) => name.trim_end_matches('\\').to_owned(),
        None => {
            let text = root.to_string_lossy();
            let letter = text.trim_end_matches('\\');
            format!(r"\\.\{letter}")
        }
    };
    let device = wide(OsStr::new(&device));
    // SAFETY: `device` is NUL-terminated; zero access, no security attributes
    // and no template handle are all valid for opening a volume to query it.
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return (false, false);
    }
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut reply = [0u8; 1024];
    let mut returned = 0u32;
    // SAFETY: `handle` is a valid open handle; the input is a live
    // `STORAGE_PROPERTY_QUERY` and its size is passed; the output buffer is
    // writable for its full length; no overlapped I/O is used.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            std::ptr::addr_of!(query).cast::<c_void>(),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            reply.as_mut_ptr().cast::<c_void>(),
            reply.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `handle` was opened above and is closed exactly once.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return (false, false);
    }
    let filled = &reply[..(returned as usize).min(reply.len())];
    parse_descriptor(filled)
}

/// Reads the bus type and removable-media flag out of a
/// `STORAGE_DEVICE_DESCRIPTOR` held as bytes. The bytes come from a driver,
/// so they are read by offset rather than reinterpreted as a struct.
fn parse_descriptor(bytes: &[u8]) -> (bool, bool) {
    const REMOVABLE: usize = std::mem::offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia);
    const BUS_TYPE: usize = std::mem::offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
    let removable = bytes.get(REMOVABLE).is_some_and(|&b| b != 0);
    let bus = bytes
        .get(BUS_TYPE..BUS_TYPE + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(i32::from_ne_bytes);
    (bus == Some(BusTypeUsb), removable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_parsing_is_bounds_checked() {
        const REMOVABLE: usize = std::mem::offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia);
        const BUS_TYPE: usize = std::mem::offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
        assert_eq!(parse_descriptor(&[]), (false, false));
        assert_eq!(parse_descriptor(&[1; 4]), (false, false));
        let mut bytes = [0u8; 64];
        bytes[REMOVABLE] = 1;
        bytes[BUS_TYPE..BUS_TYPE + 4].copy_from_slice(&BusTypeUsb.to_ne_bytes());
        assert_eq!(parse_descriptor(&bytes), (true, true));
        // A truncated reply that ends inside the bus type field.
        assert_eq!(parse_descriptor(&bytes[..BUS_TYPE + 2]), (false, true));
        // Any non-zero byte counts as set; nothing is reinterpreted as a bool.
        bytes[REMOVABLE] = 0xFE;
        assert!(parse_descriptor(&bytes).1);
    }

    #[test]
    fn wide_round_trip_stops_at_nul() {
        let w = wide(OsStr::new("C:\\"));
        assert_eq!(w.last(), Some(&0));
        assert_eq!(from_wide(&w), OsString::from("C:\\"));
        assert_eq!(
            from_wide(&[b'a' as u16, 0, b'b' as u16]),
            OsString::from("a")
        );
        assert_eq!(from_wide(&[]), OsString::new());
    }

    #[test]
    fn fallback_ids() {
        assert_eq!(fallback_id(Path::new(r"c:\")), r"path:C:\");
        assert_eq!(
            fallback_id(Path::new(r"\\Server\Share\")),
            r"unc:\\server\share\"
        );
    }

    fn system_drive_root() -> String {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        format!("{drive}\\")
    }

    #[test]
    fn the_system_drive_is_listed_as_an_eligible_fixed_drive() {
        let drives = list(Duration::from_secs(10));
        let root = system_drive_root();
        let system = drives
            .iter()
            .find(|d| d.mount_point.to_string_lossy().eq_ignore_ascii_case(&root))
            .expect("the system drive is listed");
        assert_eq!(system.kind, DriveKind::Fixed);
        assert!(system.eligible_for_background_scan);
        assert!(
            system.fs_type.eq_ignore_ascii_case("NTFS")
                || system.fs_type.eq_ignore_ascii_case("ReFS")
        );
        assert!(system.id.as_str().starts_with("volume:{"), "{}", system.id);
        assert_eq!(system.mounts.len(), 1);
        assert_eq!(system.mounts[0].mount_id, None);
        assert!(!system.mounts[0].covered_by_other_mount);
        let capacity = system.capacity.expect("sizes");
        assert!(capacity.total > 0);
        assert_eq!(
            capacity.used + capacity.reserved + capacity.free,
            capacity.total
        );
    }

    #[test]
    fn the_volume_size_is_never_smaller_than_the_users_own_total() {
        let root = wide(OsStr::new(&system_drive_root()));
        let capacity = disk_space(&root).expect("sizes for the system drive");
        let (mut available, mut total_for_user, mut total_free) = (0u64, 0u64, 0u64);
        // SAFETY: `root` is NUL-terminated; the outputs are writable `u64`s.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                root.as_ptr(),
                &mut available,
                &mut total_for_user,
                &mut total_free,
            )
        };
        assert_ne!(ok, 0);
        assert!(capacity.total >= total_for_user);
        assert_eq!(
            capacity.used + capacity.reserved + capacity.free,
            capacity.total
        );
        assert_eq!(
            capacity.reserved_kind,
            crate::drives::ReservedKind::UserQuota
        );
    }

    #[test]
    fn drive_ids_and_mount_points_are_unique() {
        let drives = list(Duration::from_secs(10));
        let mut ids = std::collections::HashSet::new();
        for drive in &drives {
            assert!(ids.insert(drive.id.clone()), "duplicate {}", drive.id);
            assert!(drive.mount_point.is_absolute());
        }
    }

    #[test]
    fn a_folder_resolves_to_its_drive() {
        let temp = std::env::temp_dir();
        let drive =
            volume_for_path(&temp, Duration::from_secs(10)).expect("volume of the temp dir");
        let capacity = drive.capacity.expect("sizes");
        assert!(capacity.total > 0);
        let root = drive.mount_point.to_string_lossy().to_lowercase();
        assert!(temp.to_string_lossy().to_lowercase().starts_with(&root));
    }

    #[test]
    fn extended_length_and_plain_paths_name_the_same_drive() {
        let windows = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let plain = volume_for_path(Path::new(&windows), Duration::from_secs(10)).unwrap();
        let prefixed = volume_for_path(
            Path::new(&format!(r"\\?\{windows}")),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(plain.id, prefixed.id);
    }

    #[test]
    fn a_path_with_an_embedded_nul_is_refused() {
        let bad = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0,
            b'x' as u16,
        ]));
        match volume_for_path(&bad, Duration::from_secs(5)) {
            Err(DriveError::Path(error)) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
