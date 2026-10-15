//! Starting the helper through the shell's `runas` verb, over a named pipe.
//!
//! Order matters here:
//!
//! 1. The pipe is created first, with a random name, as its first and only
//!    instance, for the current user alone and no remote client. Because it
//!    exists before the helper does, no other process can take the name, and
//!    only the current user's processes can open it.
//! 2. The helper is started with `ShellExecuteExW` and the `runas` verb, which
//!    shows the elevation prompt. The call gives back a handle to the helper
//!    process, which is kept.
//! 3. The wait for the connection also watches that handle, so a helper that
//!    ends without connecting does not leave this process waiting.
//! 4. The process that connected must be the process this call started, and
//!    must run the helper's program.
//!
//! A different administrator account answering the prompt (elevation "over the
//! shoulder") runs the helper as another user, whose identifier the pipe does
//! not admit, so the connection is refused. That is deliberate: admitting more
//! would let other accounts' processes reach the pipe.

#![allow(unsafe_code)]

use super::{ElevateError, Elevation, Launched, hex};
use bs_fsops::winpipe::{PipeEnd, image_path_of_handle};
use bs_ipc::{ClientError, NONCE_LEN, PROTOCOL_VERSION};
use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetProcessId, WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// How long the helper has, once the prompt is answered, to connect.
const CONNECT_WAIT_MS: u32 = 30_000;

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;

/// The started helper process. Dropping it waits a moment for the helper to
/// see its connection close, then lets go of the handle.
pub(crate) struct Guard {
    process: HANDLE,
}

// SAFETY: a process handle may be used from any thread.
unsafe impl Send for Guard {}

impl Guard {
    pub(crate) fn explain(self, error: ClientError) -> ElevateError {
        ElevateError::Protocol(error)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.process.is_null() {
            // SAFETY: the handle came from `ShellExecuteExW` and is closed once.
            unsafe {
                WaitForSingleObject(self.process, 2000);
                CloseHandle(self.process);
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(super) fn random_bytes(buf: &mut [u8]) -> Result<(), ElevateError> {
    let len = u32::try_from(buf.len()).map_err(|_| ElevateError::Unavailable("too many bytes"))?;
    // SAFETY: `buf` is writable for `len` bytes; the system's preferred
    // generator is used with a null algorithm handle.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(ElevateError::Launch(io::Error::other(
            "the system's random number generator failed",
        )))
    }
}

/// The helper's program: `bytescape-helper.exe` in the folder of this program.
pub(super) fn helper_beside_this_program() -> Result<PathBuf, ElevateError> {
    let own = std::env::current_exe().map_err(ElevateError::Launch)?;
    let helper = own
        .parent()
        .map(|folder| folder.join("bytescape-helper.exe"))
        .ok_or(ElevateError::Unavailable("this program has no folder"))?;
    if helper.is_file() {
        Ok(helper)
    } else {
        Err(ElevateError::Unavailable("the helper is not installed"))
    }
}

/// The pipe's access rules: the current user and nobody else (no inherited
/// entries, so no administrators group or system account either).
fn pipe_sddl() -> Option<String> {
    let sid = crate::bins::windows::user_sid()?;
    Some(format!("D:P(A;;GA;;;{sid})"))
}

/// Creates the pipe: first instance, one instance, no remote clients, the
/// current user only, open for overlapped I/O.
fn create_pipe(name: &str) -> Result<PipeEnd, ElevateError> {
    let sddl = pipe_sddl().ok_or(ElevateError::Unavailable("cannot tell who the user is"))?;
    let wide_sddl = wide(&sddl);
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    // SAFETY: `wide_sddl` is NUL-terminated and `descriptor` is writable; the
    // descriptor is released with `LocalFree` below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(ElevateError::Launch(io::Error::last_os_error()));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide_name = wide(name);
    // SAFETY: `wide_name` is NUL-terminated and `attributes` points at a valid
    // descriptor for the length of the call.
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: the descriptor was allocated by the conversion call above.
    unsafe { LocalFree(descriptor as HLOCAL) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(ElevateError::Launch(error));
    }
    // SAFETY: a new pipe handle opened for overlapped I/O, owned here.
    Ok(unsafe { PipeEnd::from_raw(handle) })
}

/// Starts the helper elevated and gives back its process handle.
fn start_elevated(helper: &std::path::Path, parameters: &str) -> Result<HANDLE, ElevateError> {
    let file = wide(&helper.to_string_lossy());
    let verb = wide("runas");
    let parameters = wide(parameters);
    // SAFETY: an all-zero structure is the documented starting point.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: every string in `info` is NUL-terminated and outlives the call.
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            ElevateError::Declined
        } else {
            ElevateError::Launch(error)
        });
    }
    if info.hProcess.is_null() {
        return Err(ElevateError::Launch(io::Error::other(
            "the shell gave no process handle",
        )));
    }
    Ok(info.hProcess)
}

/// Waits until a client connects to the pipe, or the helper process ends, or
/// the time is up.
fn wait_for_client(pipe: &PipeEnd, process: HANDLE) -> Result<(), ElevateError> {
    // SAFETY: a manual-reset, unnamed event.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(ElevateError::Launch(io::Error::last_os_error()));
    }
    // SAFETY: an all-zero OVERLAPPED is the documented initial state.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;
    let result = (|| {
        // SAFETY: the pipe handle is valid and `overlapped` outlives the wait
        // below, which does not return before the operation has finished or
        // been cancelled.
        let started = unsafe { ConnectNamedPipe(pipe.as_raw(), &mut overlapped) };
        if started != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|c| c as u32) {
            // The helper connected between the creation of the pipe and now.
            Some(ERROR_PIPE_CONNECTED) => return Ok(()),
            Some(ERROR_IO_PENDING) => {}
            _ => return Err(ElevateError::Launch(error)),
        }
        let handles = [event, process];
        // SAFETY: both handles are valid for the call.
        let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, CONNECT_WAIT_MS) };
        if waited == WAIT_OBJECT_0 {
            let mut transferred = 0u32;
            // SAFETY: the operation has finished, as the event says.
            if unsafe { GetOverlappedResult(pipe.as_raw(), &overlapped, &mut transferred, 0) } == 0
            {
                return Err(ElevateError::Launch(io::Error::last_os_error()));
            }
            return Ok(());
        }
        // Stop the connect so that `overlapped` can be released.
        // SAFETY: cancels the one operation started on this handle above, then
        // waits for it to be over.
        unsafe {
            CancelIoEx(pipe.as_raw(), &overlapped);
            let mut transferred = 0u32;
            GetOverlappedResult(pipe.as_raw(), &overlapped, &mut transferred, 1);
        }
        if waited == WAIT_TIMEOUT {
            Err(ElevateError::TimedOut)
        } else {
            Err(ElevateError::PeerNotVerified(
                "the helper ended before it connected",
            ))
        }
    })();
    // SAFETY: the event was created above and is closed once.
    unsafe { CloseHandle(event) };
    result
}

pub(super) fn launch(
    elevation: &Elevation,
    mode: &'static str,
    nonce: [u8; NONCE_LEN],
) -> Result<Launched, ElevateError> {
    let helper = std::fs::canonicalize(&elevation.helper)
        .map_err(|_| ElevateError::Unavailable("the helper is not installed"))?;
    let mut name_bytes = [0u8; 16];
    super::random_bytes(&mut name_bytes)?;
    let pipe_name = format!(r"\\.\pipe\bytescape-{}", hex(&name_bytes));
    let pipe = create_pipe(&pipe_name)?;
    let parameters = format!(
        "{mode} --protocol {PROTOCOL_VERSION} --nonce {} --pipe {pipe_name} --parent {}",
        hex(&nonce),
        std::process::id()
    );
    let process = start_elevated(&helper, &parameters)?;
    let guard = Guard { process };
    wait_for_client(&pipe, process)?;
    // The process that connected must be the process that was started...
    let client = pipe
        .client_process_id()
        .map_err(|_| ElevateError::PeerNotVerified("the connecting process is unknown"))?;
    // SAFETY: `process` is the valid handle returned by the shell.
    let started = unsafe { GetProcessId(process) };
    if client != started {
        return Err(ElevateError::PeerNotVerified(
            "the connecting process is not the one that was started",
        ));
    }
    // ...and must be running the helper's program, not another that took its
    // place.
    // SAFETY: `process` is a valid process handle opened by the shell with
    // query rights (it is the full-rights handle of a process just created).
    let image = unsafe { image_path_of_handle(process) }
        .map_err(|_| ElevateError::PeerNotVerified("the connecting program is unknown"))?;
    let image = std::fs::canonicalize(image)
        .map_err(|_| ElevateError::PeerNotVerified("the connecting program is unknown"))?;
    if !bs_fsops::winpath::same_path(&image.to_string_lossy(), &helper.to_string_lossy()) {
        return Err(ElevateError::PeerNotVerified(
            "the connecting program is not the helper",
        ));
    }
    Ok(Launched {
        reader: Box::new(pipe.clone()),
        writer: Box::new(pipe),
        guard,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetKernelObjectSecurity};

    fn unique_name() -> String {
        let mut bytes = [0u8; 16];
        random_bytes(&mut bytes).unwrap();
        format!(r"\\.\pipe\bytescape-test-{}", hex(&bytes))
    }

    /// The pipe's access rules as the system reports them, in text form.
    fn dacl_of(pipe: &PipeEnd) -> String {
        let mut needed = 0u32;
        // SAFETY: a null buffer of length 0 asks only for the size needed.
        unsafe {
            GetKernelObjectSecurity(
                pipe.as_raw(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        assert!(needed > 0 && needed < 8192);
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buffer` is writable for at least `needed` bytes.
        let ok = unsafe {
            GetKernelObjectSecurity(
                pipe.as_raw(),
                DACL_SECURITY_INFORMATION,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        let mut text: *mut u16 = std::ptr::null_mut();
        let mut length = 0u32;
        // SAFETY: the descriptor was filled in above; the text is released with
        // `LocalFree` below.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                buffer.as_ptr().cast_mut().cast(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut length,
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        // SAFETY: the call returned `length` units at `text`.
        let units = unsafe { std::slice::from_raw_parts(text, length as usize) };
        let out = String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_owned();
        // SAFETY: allocated by the conversion call above.
        unsafe { LocalFree(text.cast()) };
        out
    }

    #[test]
    fn the_pipe_admits_the_current_user_and_nobody_else() {
        let pipe = create_pipe(&unique_name()).unwrap();
        let sid = crate::bins::windows::user_sid().unwrap();
        let text = dacl_of(&pipe);
        // Protected (no inherited entries), one entry that allows, naming the
        // user: no administrators group, no system account, no "everyone".
        assert!(text.starts_with("D:P(A;;"), "{text}");
        assert!(text.ends_with(&format!(";;;{sid})")), "{text}");
        assert_eq!(text.matches('(').count(), 1, "{text}");
        assert!(
            !text.contains("WD") && !text.contains("BA") && !text.contains("SY"),
            "{text}"
        );
    }

    #[test]
    fn a_pipe_name_can_be_used_only_once() {
        let name = unique_name();
        let first = create_pipe(&name).unwrap();
        // Another process (or this one) cannot create a second instance, so the
        // name cannot be taken over before the helper connects.
        assert!(create_pipe(&name).is_err());
        drop(first);
    }

    #[test]
    fn the_random_values_differ_and_the_hex_is_as_long_as_it_should_be() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        random_bytes(&mut a).unwrap();
        random_bytes(&mut b).unwrap();
        assert_ne!(a, b);
        assert_eq!(hex(&a).len(), 32);
        assert!(
            hex(&a)
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
    }

    #[test]
    fn the_helper_is_looked_for_beside_this_program() {
        // The test program is not followed by a helper, so it is not found.
        let error = helper_beside_this_program().unwrap_err();
        assert!(matches!(error, ElevateError::Unavailable(_)));
    }
}
