//! The clipboard on Windows: Unicode text through `SetClipboardData`.

#![allow(unsafe_code)]

use super::ClipboardError;
use std::io;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

/// The clipboard format for UTF-16 text (`CF_UNICODETEXT`).
const UNICODE_TEXT: u32 = 13;
/// Another program may hold the clipboard for a moment; wait this long.
const PATIENCE: Duration = Duration::from_millis(1000);

fn failed(code: u32) -> ClipboardError {
    ClipboardError::Failed(io::ErrorKind::Other, Some(code as i32))
}

/// A window that exists only to own the clipboard. With no owner, emptying
/// the clipboard makes `SetClipboardData` fail.
struct Owner(HWND);

impl Owner {
    fn create() -> Result<Self, ClipboardError> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: `STATIC` is a system window class; a message-only window is
        // never shown. All other arguments are zero or null.
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            // SAFETY: reads this thread's last error.
            return Err(failed(unsafe { GetLastError() }));
        }
        Ok(Self(window))
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread and is destroyed once.
        unsafe { DestroyWindow(self.0) };
    }
}

/// An open clipboard, closed when dropped.
struct Open;

impl Open {
    fn acquire(owner: &Owner) -> Result<Self, ClipboardError> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            // SAFETY: `owner` is a live window of this thread.
            if unsafe { OpenClipboard(owner.0) } != 0 {
                return Ok(Self);
            }
            if Instant::now() >= deadline {
                // SAFETY: reads this thread's last error.
                return Err(failed(unsafe { GetLastError() }));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: balances the successful `OpenClipboard` in `acquire`.
        unsafe { CloseClipboard() };
    }
}

pub(crate) fn set_text(text: &str) -> Result<(), ClipboardError> {
    if text.contains('\0') {
        return Err(ClipboardError::ContainsNul);
    }
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * std::mem::size_of::<u16>();

    // SAFETY: allocates a movable block of `bytes` bytes; null means failure.
    let block: HANDLE = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if block.is_null() {
        // SAFETY: reads this thread's last error.
        return Err(failed(unsafe { GetLastError() }));
    }
    // SAFETY: `block` is a movable block; locking gives its address.
    let target = unsafe { GlobalLock(block) };
    if target.is_null() {
        // SAFETY: reads this thread's last error, then frees the unused block.
        let code = unsafe { GetLastError() };
        // SAFETY: `block` was allocated above and not handed to anyone.
        unsafe { GlobalFree(block) };
        return Err(failed(code));
    }
    // SAFETY: `target` has room for `units.len()` UTF-16 units, and `units`
    // does not overlap it.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), target.cast::<u16>(), units.len()) };
    // SAFETY: balances the lock above.
    unsafe { GlobalUnlock(block) };

    let owner = match Owner::create() {
        Ok(owner) => owner,
        Err(error) => {
            // SAFETY: `block` was allocated above and not handed to anyone.
            unsafe { GlobalFree(block) };
            return Err(error);
        }
    };
    let clipboard = match Open::acquire(&owner) {
        Ok(open) => open,
        Err(error) => {
            // SAFETY: `block` was allocated above and not handed to anyone.
            unsafe { GlobalFree(block) };
            return Err(error);
        }
    };
    // SAFETY: the clipboard is open by this task.
    if unsafe { EmptyClipboard() } == 0 {
        // SAFETY: reads this thread's last error, then frees the unused block.
        let code = unsafe { GetLastError() };
        // SAFETY: `block` was allocated above and not handed to anyone.
        unsafe { GlobalFree(block) };
        return Err(failed(code));
    }
    // SAFETY: the clipboard is open and emptied, and `block` is a movable
    // block holding NUL-terminated UTF-16 text.
    let stored = unsafe { SetClipboardData(UNICODE_TEXT, block) };
    if stored.is_null() {
        // SAFETY: reads this thread's last error, then frees the block, which
        // the system did not take.
        let code = unsafe { GetLastError() };
        // SAFETY: `block` was not taken, so it is still ours to free.
        unsafe { GlobalFree(block) };
        return Err(failed(code));
    }
    // The system owns the block now. Closing the clipboard publishes it, and
    // the text stays after the owner window is gone.
    drop(clipboard);
    drop(owner);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard is shared with the whole session, so these run only where
    /// the environment says the machine is disposable.
    fn disposable_host() -> bool {
        let yes = std::env::var_os("BYTESCAPE_DISPOSABLE_HOST").is_some_and(|v| v == "1");
        if !yes {
            eprintln!("skipped: BYTESCAPE_DISPOSABLE_HOST is not set to 1");
        }
        yes
    }

    fn read_back() -> Option<String> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        let owner = Owner::create().ok()?;
        let _open = Open::acquire(&owner).ok()?;
        // SAFETY: the clipboard is open; the format is the one we write.
        let data = unsafe { GetClipboardData(UNICODE_TEXT) };
        if data.is_null() {
            return None;
        }
        // SAFETY: `data` is a global block holding NUL-terminated UTF-16.
        let pointer = unsafe { GlobalLock(data) }.cast::<u16>();
        if pointer.is_null() {
            return None;
        }
        let mut length = 0;
        // SAFETY: the text is NUL-terminated.
        while unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `pointer` holds `length` initialised units.
        let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) });
        // SAFETY: balances the lock above.
        unsafe { GlobalUnlock(data) };
        Some(text)
    }

    #[test]
    fn text_with_a_nul_is_refused_without_touching_the_clipboard() {
        assert!(matches!(set_text("a\0b"), Err(ClipboardError::ContainsNul)));
    }

    #[test]
    fn text_survives_the_round_trip() {
        if !disposable_host() {
            return;
        }
        for text in [
            "plain",
            "C:\\Users\\a\\Video \u{202e}ünï",
            "line one\r\nline two",
            "emoji 😀 and ☃",
            "",
        ] {
            set_text(text).expect("copy");
            assert_eq!(read_back().as_deref(), Some(text));
        }
    }
}
