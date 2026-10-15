//! Small helpers shared by the Windows shell calls: COM start-up, wide
//! strings, COM object access and the system folders.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::core::GUID;

pub(crate) type Hresult = i32;

const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106_u32 as i32;
const SIGDN_FILESYSPATH: i32 = 0x8005_8000_u32 as i32;

pub(crate) const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);

/// A NUL-terminated UTF-16 copy. The caller has already refused text with a
/// NUL in it, which would end the string early.
pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Keeps COM started on this thread, as the shell's dialogs and path parsing
/// need, for as long as it lives.
pub(crate) struct ComScope {
    uninitialize: bool,
}

impl ComScope {
    pub fn enter() -> Result<Self, Hresult> {
        // SAFETY: a null reserved pointer and valid flags.
        let hr = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        if hr >= 0 {
            Ok(Self { uninitialize: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // The thread already joined COM another way; use it as it is.
            Ok(Self {
                uninitialize: false,
            })
        } else {
            Err(hr)
        }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balances the successful `CoInitializeEx` in `enter`.
            unsafe { CoUninitialize() };
        }
    }
}

#[repr(C)]
pub(crate) struct UnknownVtbl {
    pub query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> Hresult,
    pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IShellItem`; the slots that are not called are placeholders that keep the
/// layout.
#[repr(C)]
pub(crate) struct ShellItemVtbl {
    pub base: UnknownVtbl,
    bind_to_handler: usize,
    get_parent: usize,
    pub get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> Hresult,
    get_attributes: usize,
    compare: usize,
}

/// Reads the vtable of a COM object.
///
/// # Safety
/// `object` must point to a live COM object whose interface has the vtable
/// layout `V`.
pub(crate) unsafe fn vtbl<'a, V>(object: *mut c_void) -> &'a V {
    // SAFETY: by the caller's contract the first word of the object is a
    // pointer to a vtable of type `V` that lives as long as the object.
    unsafe { &**object.cast::<*const V>() }
}

/// Drops one reference. A null pointer is ignored.
///
/// # Safety
/// `object` must be null or a live COM object this code holds a reference to.
pub(crate) unsafe fn release(object: *mut c_void) {
    if !object.is_null() {
        // SAFETY: by the caller's contract.
        unsafe { (vtbl::<UnknownVtbl>(object).release)(object) };
    }
}

/// The file-system path of a shell item.
///
/// # Safety
/// `item` must be a live `IShellItem`.
pub(crate) unsafe fn item_path(item: *mut c_void) -> Option<PathBuf> {
    if item.is_null() {
        return None;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: by the caller's contract `item` is an `IShellItem`; `text`
    // receives a string the system allocates.
    let hr = unsafe {
        (vtbl::<ShellItemVtbl>(item).get_display_name)(item, SIGDN_FILESYSPATH, &mut text)
    };
    if hr < 0 || text.is_null() {
        return None;
    }
    let mut length = 0usize;
    // SAFETY: the system returned a NUL-terminated string.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `text` points to `length` initialised UTF-16 units.
    let units = unsafe { std::slice::from_raw_parts(text, length) };
    let path = PathBuf::from(OsString::from_wide(units));
    // SAFETY: the string came from the shell's allocator and is freed once.
    unsafe { CoTaskMemFree(text.cast()) };
    Some(path)
}

/// The Windows system folder, such as `C:\Windows\System32`. Programs the
/// system provides are started from here and never found by searching.
pub(crate) fn system_directory() -> Option<PathBuf> {
    let mut buffer = vec![0u16; 260];
    for _ in 0..2 {
        // SAFETY: `buffer` is writable for `len` units.
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        let length = length as usize;
        if length == 0 {
            return None;
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Some(PathBuf::from(OsString::from_wide(&buffer)));
        }
        // Too small: the call returned the size needed, with the NUL.
        buffer = vec![0u16; length + 1];
    }
    None
}
