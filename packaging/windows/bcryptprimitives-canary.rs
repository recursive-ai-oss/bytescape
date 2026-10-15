// A stand-in for the system's bcryptprimitives.dll, used only by
// check-helper.py. When Windows loads it, DllMain writes a small file in the
// current folder, so a test can tell whether a program loaded it. It also
// exports ProcessPrng, the one function Rust's standard library asks that
// library for, so the loader accepts it as that library.
//
// Built with plain rustc (not part of the workspace, so the workspace lints do
// not apply): the foreign calls below are the whole program.

use std::ffi::c_void;

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

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_module: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        let name: Vec<u16> = "canary-loaded.txt\0".encode_utf16().collect();
        // SAFETY: `name` is a valid, NUL-terminated wide string that lives
        // for the whole call; the other arguments are plain values or null,
        // as CreateFileW allows. The handle is closed at once.
        unsafe {
            let handle = CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                CREATE_ALWAYS,
                0x80,
                std::ptr::null_mut(),
            );
            if handle as isize != -1 {
                CloseHandle(handle);
            }
        }
    }
    1
}

#[unsafe(no_mangle)]
pub extern "system" fn ProcessPrng(_data: *mut u8, _len: usize) -> i32 {
    1
}
