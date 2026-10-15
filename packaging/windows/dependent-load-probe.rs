// A small program for check-helper.py. It imports one function from
// version.dll (a system library that this program has no other reason to
// load) and calls it, so Windows resolves that import when the program
// starts. check-helper.py links it with the same /DEPENDENTLOADFLAG:0x800 the
// helper gets, plants a stand-in version.dll beside it, and sees whether the
// stand-in is loaded.
//
// Built with plain rustc (not part of the workspace, so the workspace lints do
// not apply).

#[link(name = "version")]
unsafe extern "system" {
    fn GetFileVersionInfoSizeW(name: *const u16, handle: *mut u32) -> u32;
}

fn main() {
    let mut handle = 0u32;
    // SAFETY: a null file name is allowed (the call then fails and returns 0),
    // and `handle` is a valid place for the call to write one u32.
    let _ = unsafe { GetFileVersionInfoSizeW(std::ptr::null(), &mut handle) };
}
