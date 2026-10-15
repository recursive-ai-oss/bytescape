//! Handle-based file calls on Windows: every item is opened with
//! `FILE_FLAG_OPEN_REPARSE_POINT`, so a link or junction is acted on as itself
//! and never entered, and a delete goes through the handle that was opened and
//! checked.

#![allow(unsafe_code)]

use crate::Identity;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_WRITE_ATTRIBUTES, FileBasicInfo, FileDispositionInfo, FileDispositionInfoEx,
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, SetFileInformationByHandle,
};

pub const SYNCHRONIZE: u32 = 0x0010_0000;

// ------------------------------------------------------------------ Handles

pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `CreateFileW` and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// The form of a path the file calls accept for any length: `\\?\` in front,
/// separators as backslashes. Paths that already carry a prefix are kept.
///
/// Built from the path's own 16-bit units: a name with an unpaired surrogate
/// must open that name, not a look-alike that has the replacement character.
pub fn extended(path: &Path) -> Vec<u16> {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    crate::winpath::extended_units(&units)
}

/// Opens whatever `path` names, a link or junction as itself, without
/// following it.
pub fn open_nofollow(path: &Path, access: u32) -> io::Result<Handle> {
    let name = extended(path);
    // SAFETY: `name` is NUL-terminated and outlives the call; the security
    // attributes and template handle are optional and null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

pub fn info_by_handle<T: Default>(handle: &Handle, class: i32) -> io::Result<T> {
    let mut info = T::default();
    // SAFETY: `info` is writable storage for the structure of `class`, and its
    // size is passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            class,
            std::ptr::addr_of_mut!(info).cast(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

pub fn identity_of_handle(handle: &Handle) -> io::Result<Identity> {
    match info_by_handle::<FILE_ID_INFO>(handle, FileIdInfo) {
        Ok(id) => Ok(Identity {
            volume: id.VolumeSerialNumber,
            file: u128::from_le_bytes(id.FileId.Identifier),
        }),
        Err(_) => {
            // Filesystems without 128-bit file IDs (FAT) have the older 64-bit
            // index, which is enough to tell two files apart on one volume.
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: `info` is writable storage of the type the call fills in.
            if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Identity {
                volume: u64::from(info.dwVolumeSerialNumber),
                file: u128::from(info.nFileIndexHigh) << 32 | u128::from(info.nFileIndexLow),
            })
        }
    }
}

pub fn basic_attributes(handle: &Handle) -> io::Result<u32> {
    let basic: FILE_BASIC_INFO = info_by_handle(handle, FileBasicInfo)?;
    Ok(basic.FileAttributes)
}

/// The place an open handle really is, in the form the protected-path table
/// reads. The name keeps its exact units.
pub fn final_path(handle: &Handle) -> io::Result<OsString> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: `buffer` is writable for its length, which is passed.
        let length = unsafe {
            GetFinalPathNameByHandleW(handle.0, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if (length as usize) < buffer.len() {
            buffer.truncate(length as usize);
            return Ok(OsString::from_wide(
                &crate::winpath::strip_extended_prefix_units(&buffer),
            ));
        }
        // The call reports the size needed when the buffer is too small.
        buffer.resize(length as usize + 1, 0);
    }
}

pub fn identity_of(path: &Path) -> io::Result<Identity> {
    identity_of_handle(&open_nofollow(path, FILE_READ_ATTRIBUTES)?)
}

// ------------------------------------------------------------ Hard delete

/// Totals while a tree is removed.
#[derive(Default)]
pub struct Tally {
    pub allocated: u64,
    pub entries: u64,
    /// Files whose other names are not part of the delete.
    linked: HashMap<(u64, u128), (u32, u64)>,
    pub kept_by_other_names: u64,
    pub first_error: Option<io::Error>,
}

impl Tally {
    pub fn fail(&mut self, error: io::Error) {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
    }

    /// Counts a removed name. The bytes of a file with several names count
    /// when its last name goes.
    pub fn count(&mut self, identity: Identity, allocated: u64, links: u32, directory: bool) {
        self.entries += 1;
        let key = (identity.volume, identity.file);
        let known = self.linked.contains_key(&key);
        if directory || (links <= 1 && !known) {
            self.allocated = self.allocated.saturating_add(allocated);
            return;
        }
        let entry = self.linked.entry(key).or_insert((links, allocated));
        entry.0 = entry.0.saturating_sub(1);
        if entry.0 == 0 {
            let bytes = entry.1;
            self.linked.remove(&key);
            self.allocated = self.allocated.saturating_add(bytes);
        }
    }

    pub fn finish(&mut self) {
        self.kept_by_other_names += self.linked.len() as u64;
        self.linked.clear();
    }
}

/// Folders nested deeper than this are not entered.
pub const MAX_DEPTH: usize = 256;

/// Marks the object for deletion through its open handle. The deletion takes
/// effect when the handle closes.
pub fn mark_for_delete(handle: &Handle, attributes: u32) -> io::Result<()> {
    let flags = FILE_DISPOSITION_FLAG_DELETE
        | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
        | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
    let extended = FILE_DISPOSITION_INFO_EX { Flags: flags };
    // SAFETY: the structure matches the class and its size is passed.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.0,
            FileDispositionInfoEx,
            std::ptr::addr_of!(extended).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let first = io::Error::last_os_error();
    // Older systems and some filesystems lack the extended class: clear the
    // read-only attribute and use the plain one.
    if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        let basic = FILE_BASIC_INFO {
            CreationTime: 0,
            LastAccessTime: 0,
            LastWriteTime: 0,
            ChangeTime: 0,
            FileAttributes: attributes & !FILE_ATTRIBUTE_READONLY,
        };
        // SAFETY: the structure matches the class and its size is passed.
        unsafe {
            SetFileInformationByHandle(
                handle.0,
                FileBasicInfo,
                std::ptr::addr_of!(basic).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
    }
    let plain = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the structure matches the class and its size is passed.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.0,
            FileDispositionInfo,
            std::ptr::addr_of!(plain).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok != 0 { Ok(()) } else { Err(first) }
}

/// The names in a folder, read through its handle.
pub fn list_names(handle: &Handle) -> io::Result<Vec<OsString>> {
    const NAME_OFFSET: usize = 68;
    let mut names = Vec::new();
    let mut buffer = vec![0u64; 16 * 1024]; // 128 KiB, aligned for the records
    let mut restart = true;
    loop {
        let class = if restart {
            FileFullDirectoryRestartInfo
        } else {
            FileFullDirectoryInfo
        };
        restart = false;
        // SAFETY: `buffer` is writable for the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                class,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                Ok(names)
            } else {
                Err(error)
            };
        }
        let bytes = buffer.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            if offset + NAME_OFFSET > buffer.len() * 8 {
                return Err(io::Error::other("a directory record overruns its buffer"));
            }
            // SAFETY: the system filled the buffer with a chain of records, each
            // starting at an offset it gave; the reads stay inside the buffer
            // because the lengths come from the same records and are checked.
            let (next, length) = unsafe {
                let record = bytes.add(offset);
                (
                    record.cast::<u32>().read_unaligned() as usize,
                    record.add(60).cast::<u32>().read_unaligned() as usize,
                )
            };
            if offset + NAME_OFFSET + length > buffer.len() * 8 {
                return Err(io::Error::other("a directory record overruns its buffer"));
            }
            // SAFETY: the name lies inside the buffer, checked just above.
            let units = unsafe {
                std::slice::from_raw_parts(
                    bytes.add(offset + NAME_OFFSET).cast::<u16>(),
                    length / 2,
                )
            };
            let name = OsString::from_wide(units);
            if name != "." && name != ".." {
                names.push(name);
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}

/// Removes `path` and, for a real folder, everything below it. `expected` is
/// the place the path must really be, which catches a folder swapped for a
/// junction between listing and opening.
pub fn remove_tree(
    path: &Path,
    expected: &[u16],
    expect: Option<Identity>,
    volume: u64,
    depth: usize,
    tally: &mut Tally,
) {
    let access = DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE;
    let handle = match open_nofollow(path, access) {
        Ok(handle) => handle,
        // Gone already: for a folder's contents that is what was wanted. For the
        // item itself it means there was nothing to delete, and is reported.
        Err(error) if error.kind() == io::ErrorKind::NotFound && depth > 0 => return,
        Err(error) => return tally.fail(error),
    };
    // What the handle really is must be what was meant: a link swapped in for
    // a folder would put the handle somewhere else.
    match final_path(&handle) {
        Ok(real) => {
            let real: Vec<u16> = real.encode_wide().collect();
            if !crate::winpath::same_path_units(&real, expected) {
                return tally.fail(io::Error::other("the item moved during the delete"));
            }
        }
        Err(error) => return tally.fail(error),
    }
    let (identity, attributes, standard) = match (
        identity_of_handle(&handle),
        basic_attributes(&handle),
        info_by_handle::<FILE_STANDARD_INFO>(&handle, FileStandardInfo),
    ) {
        (Ok(identity), Ok(attributes), Ok(standard)) => (identity, attributes, standard),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => return tally.fail(error),
    };
    if identity.volume != volume {
        return tally.fail(io::Error::new(
            io::ErrorKind::InvalidInput,
            "on another volume",
        ));
    }
    if expect.is_some_and(|wanted| wanted != identity) {
        return tally.fail(io::Error::other("the item was replaced during the delete"));
    }
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let is_link = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if is_dir && !is_link {
        if depth >= MAX_DEPTH {
            return tally.fail(io::Error::new(
                io::ErrorKind::InvalidInput,
                "folders are nested too deeply",
            ));
        }
        // Reading the names needs another kind of access than deleting, so the
        // folder is opened again; it must be the same folder.
        let listing = open_nofollow(
            path,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        )
        .and_then(|listing| {
            if identity_of_handle(&listing)? == identity {
                Ok(listing)
            } else {
                Err(io::Error::other(
                    "the folder was replaced during the delete",
                ))
            }
        })
        .and_then(|listing| list_names(&listing));
        match listing {
            Ok(names) => {
                for name in names {
                    let child = path.join(&name);
                    let name_units: Vec<u16> = name.encode_wide().collect();
                    let child_expected = crate::winpath::child_units(expected, &name_units);
                    remove_tree(&child, &child_expected, None, volume, depth + 1, tally);
                }
            }
            Err(error) => tally.fail(error),
        }
    }
    // A link, junction or placeholder is deleted as itself. Opening it without
    // following means no data was fetched for it.
    match mark_for_delete(&handle, attributes) {
        Ok(()) => {
            let allocated = u64::try_from(standard.AllocationSize).unwrap_or(0);
            tally.count(identity, allocated, standard.NumberOfLinks, is_dir);
        }
        Err(error) => tally.fail(error),
    }
    // The delete happens when the handle closes.
    drop(handle);
}

/// Deletes the item at `path`, and everything below it if it is a real folder,
/// provided it is still the thing with identity `expect` and its final path is
/// still `real` (the path that was judged). Another volume is never entered.
pub fn delete_item(
    path: &Path,
    real: &OsStr,
    expect: Identity,
) -> Result<crate::Removed, crate::DeleteError> {
    use crate::DeleteError;
    match identity_of(path) {
        Ok(now) if now == expect => {}
        Ok(_) => return Err(DeleteError::Replaced),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(DeleteError::NotFound);
        }
        Err(error) => return Err(DeleteError::Look(error)),
    }
    let mut tally = Tally::default();
    let real: Vec<u16> = real.encode_wide().collect();
    remove_tree(path, &real, Some(expect), expect.volume, 0, &mut tally);
    tally.finish();
    if tally.entries == 0
        && tally
            .first_error
            .as_ref()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        return Err(DeleteError::NotFound);
    }
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
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_long_form_of_a_name_with_an_unpaired_surrogate_keeps_the_surrogate() {
        let mut units: Vec<u16> = r"C:\fx\a".encode_utf16().collect();
        units.push(0xD800);
        let odd = PathBuf::from(OsString::from_wide(&units));
        let mut want: Vec<u16> = r"\\?\C:\fx\a".encode_utf16().collect();
        want.push(0xD800);
        want.push(0);
        assert_eq!(extended(&odd), want);
        // The look-alike with the replacement character is another name.
        let lookalike = PathBuf::from("C:\\fx\\a\u{FFFD}");
        assert_ne!(extended(&lookalike), want);
    }

    #[test]
    fn ordinary_paths_get_the_long_prefix_and_backslashes() {
        let long = |p: &str| String::from_utf16_lossy(&extended(Path::new(p)));
        assert_eq!(long("C:/a/b"), "\\\\?\\C:\\a\\b\0");
        assert_eq!(long(r"\\host\share\x"), "\\\\?\\UNC\\host\\share\\x\0");
    }
}
