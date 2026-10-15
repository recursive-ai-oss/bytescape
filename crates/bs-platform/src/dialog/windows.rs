//! The folder dialog on Windows: `IFileOpenDialog` in folder-picking mode.

#![allow(unsafe_code)]

use super::DialogError;
use crate::winshell::{
    ComScope, Hresult, IID_ISHELLITEM, UnknownVtbl, item_path, release, vtbl, wide,
};
use std::ffi::{OsStr, c_void};
use std::path::{Path, PathBuf};
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows_sys::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PICKFOLDERS, FileOpenDialog,
    SHCreateItemFromParsingName,
};
use windows_sys::core::GUID;

const IID_IFILEOPENDIALOG: GUID = GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
/// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`: the user closed the dialog.
const CANCELLED: Hresult = 0x8007_04C7_u32 as i32;

type Slot = usize;

/// `IFileOpenDialog`, in the order the interfaces declare their methods
/// (`IModalWindow`, then `IFileDialog`, then `IFileOpenDialog`). Slots that
/// are not called are placeholders that keep the layout.
#[repr(C)]
struct FileOpenDialogVtbl {
    base: UnknownVtbl,
    show: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Hresult,
    set_file_types: Slot,
    set_file_type_index: Slot,
    get_file_type_index: Slot,
    advise: Slot,
    unadvise: Slot,
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> Hresult,
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    set_default_folder: Slot,
    set_folder: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Hresult,
    get_folder: Slot,
    get_current_selection: Slot,
    set_file_name: Slot,
    get_file_name: Slot,
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> Hresult,
    set_ok_button_label: Slot,
    set_file_name_label: Slot,
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult,
    add_place: Slot,
    set_default_extension: Slot,
    close: Slot,
    set_client_guid: Slot,
    clear_client_data: Slot,
    set_filter: Slot,
    get_results: Slot,
    get_selected_items: Slot,
}

pub(crate) fn pick_folder(start: &Path) -> Result<Option<PathBuf>, DialogError> {
    let _com = ComScope::enter().map_err(|_| DialogError::Unavailable)?;
    let mut dialog: *mut c_void = std::ptr::null_mut();
    // SAFETY: the class and interface ids are valid and `dialog` is writable.
    let hr = unsafe {
        CoCreateInstance(
            &FileOpenDialog,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IFILEOPENDIALOG,
            &mut dialog,
        )
    };
    if hr < 0 || dialog.is_null() {
        return Err(DialogError::Unavailable);
    }
    let answer = show(dialog, start);
    // SAFETY: `dialog` is the reference `CoCreateInstance` gave this code.
    unsafe { release(dialog) };
    answer
}

fn show(dialog: *mut c_void, start: &Path) -> Result<Option<PathBuf>, DialogError> {
    // SAFETY: `dialog` is a live `IFileOpenDialog`.
    let methods = unsafe { vtbl::<FileOpenDialogVtbl>(dialog) };

    let mut options = 0u32;
    // SAFETY: `dialog` is live and `options` is writable.
    let hr = unsafe { (methods.get_options)(dialog, &mut options) };
    if hr < 0 {
        return Err(DialogError::Unavailable);
    }
    // Folders only, real file-system folders only, and no change to the
    // process's working folder.
    // SAFETY: `dialog` is live.
    let hr = unsafe {
        (methods.set_options)(
            dialog,
            options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR,
        )
    };
    if hr < 0 {
        return Err(DialogError::Unavailable);
    }
    let title = wide(OsStr::new("Choose a folder"));
    // SAFETY: `dialog` is live and `title` is NUL-terminated. A title that
    // cannot be set is not worth failing for.
    unsafe { (methods.set_title)(dialog, title.as_ptr()) };

    if start.is_absolute() && start.is_dir() {
        let name = wide(start.as_os_str());
        let mut folder: *mut c_void = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; `folder` is
        // writable.
        let hr = unsafe {
            SHCreateItemFromParsingName(
                name.as_ptr(),
                std::ptr::null_mut(),
                &IID_ISHELLITEM,
                &mut folder,
            )
        };
        if hr >= 0 && !folder.is_null() {
            // SAFETY: `dialog` and `folder` are live; the dialog takes its own
            // reference. A start folder that is refused is not an error.
            unsafe {
                (methods.set_folder)(dialog, folder);
                release(folder);
            }
        }
    }

    // SAFETY: `dialog` is live. A null owner makes the dialog a top-level
    // window of its own.
    let hr = unsafe { (methods.show)(dialog, std::ptr::null_mut()) };
    if hr == CANCELLED {
        return Ok(None);
    }
    if hr < 0 {
        return Err(DialogError::Unavailable);
    }
    let mut item: *mut c_void = std::ptr::null_mut();
    // SAFETY: `dialog` is live and `item` is writable.
    let hr = unsafe { (methods.get_result)(dialog, &mut item) };
    if hr < 0 || item.is_null() {
        return Err(DialogError::BadAnswer);
    }
    // SAFETY: `item` is a live `IShellItem` that this code holds a reference to.
    let path = unsafe { item_path(item) };
    // SAFETY: balances the reference `GetResult` gave.
    unsafe { release(item) };
    match path {
        Some(path) if path.is_absolute() && path.is_dir() => Ok(Some(path)),
        _ => Err(DialogError::BadAnswer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vtable_has_the_slots_of_the_interfaces_in_order() {
        // IUnknown (3) + IModalWindow (1) + IFileDialog (23) + IFileOpenDialog (2).
        let pointer = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<FileOpenDialogVtbl>(), 29 * pointer);
        let methods = std::mem::offset_of!(FileOpenDialogVtbl, show) / pointer;
        assert_eq!(methods, 3);
        // `SetOptions` is the 6th method of `IFileDialog` after the 3 of
        // `IUnknown` and `IModalWindow::Show`.
        for (slot, expected) in [
            (std::mem::offset_of!(FileOpenDialogVtbl, set_options), 9),
            (std::mem::offset_of!(FileOpenDialogVtbl, get_options), 10),
            (std::mem::offset_of!(FileOpenDialogVtbl, set_folder), 12),
            (std::mem::offset_of!(FileOpenDialogVtbl, set_title), 17),
            (std::mem::offset_of!(FileOpenDialogVtbl, get_result), 20),
        ] {
            assert_eq!(slot / pointer, expected);
        }
    }
}
