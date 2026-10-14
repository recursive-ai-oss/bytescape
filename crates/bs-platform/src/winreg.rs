//! Reading values from the current user's part of the Windows registry.

#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

/// UTF-16 text with the terminating NUL the registry calls expect.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a `REG_DWORD` under `HKEY_CURRENT_USER`. `None` if it is missing or
/// of another type.
pub(crate) fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `subkey` and `value` are NUL-terminated UTF-16 strings that
    // outlive the call; `data` is a writable `u32` and `size` holds its size in
    // bytes; the type pointer may be null.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}
