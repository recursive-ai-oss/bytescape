// A stand-in for the system's version.dll, used only by check-helper.py. When
// Windows loads it, DllMain writes a small file in the current folder, so a
// test can tell whether a program loaded it. It also exports
// GetFileVersionInfoSizeW, the one function the probe program imports from
// version.dll, so the loader accepts it as that library.
//
// It links no standard library: that keeps it a few hundred bytes and free of
// anything it could clash with.
//
// Built with plain rustc, as `-C panic=abort` (not part of the workspace, so
// the workspace lints do not apply): the foreign calls below are the whole
// program.

#![no_std]

use core::ffi::c_void;
use core::ptr::null_mut;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

const DLL_PROCESS_ATTACH: u32 = 1;
const GENERIC_WRITE: u32 = 0x4000_0000;
const CREATE_ALWAYS: u32 = 2;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// "canary-loaded.txt", NUL-terminated, as UTF-16.
const NAME: [u16; 18] = {
    let text = b"canary-loaded.txt\0";
    let mut wide = [0u16; 18];
    let mut i = 0;
    while i < text.len() {
        wide[i] = text[i] as u16;
        i += 1;
    }
    wide
};

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_module: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // SAFETY: `NAME` is a valid, NUL-terminated wide string that lives for
        // the whole call; the other arguments are plain values or null, as
        // CreateFileW allows. The handle is closed at once.
        unsafe {
            let handle = CreateFileW(
                NAME.as_ptr(),
                GENERIC_WRITE,
                0,
                null_mut(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            );
            if handle as isize != -1 {
                CloseHandle(handle);
            }
        }
    }
    1
}

#[unsafe(no_mangle)]
pub extern "system" fn GetFileVersionInfoSizeW(_name: *const u16, _handle: *mut u32) -> u32 {
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
