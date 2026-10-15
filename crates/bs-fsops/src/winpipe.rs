//! The named pipe, as a byte stream, on Windows.
//!
//! The pipe handles are opened for overlapped I/O. A handle opened for
//! synchronous I/O serializes its operations: one thread waiting in a read
//! would keep another thread from writing on the same pipe, and the helper
//! must be able to answer a goodbye (and the interface to send one) while a
//! read is waiting. Each read or write here uses its own event and waits for
//! its own completion.

#![allow(unsafe_code)]
// This module is the pipe transport only. It reads no data from the other
// side beyond the byte counts the system reports.
#![allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, SECURITY_ANONYMOUS,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// Closes the handle when the last user lets go.
struct Owned(HANDLE);

// SAFETY: a pipe handle may be used from any thread; the system serializes
// what needs to be.
unsafe impl Send for Owned {}
// SAFETY: as above: reads and writes on one handle from different threads are
// what overlapped I/O is for.
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// One end of a named pipe. Clones share the pipe: give one to a reading
/// thread and keep one for writing.
#[derive(Clone)]
pub struct PipeEnd {
    handle: Arc<Owned>,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

impl PipeEnd {
    /// Takes over a pipe handle that was opened for overlapped I/O.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid pipe handle opened with `FILE_FLAG_OVERLAPPED`
    /// that nothing else will close.
    pub unsafe fn from_raw(handle: HANDLE) -> PipeEnd {
        PipeEnd {
            handle: Arc::new(Owned(handle)),
        }
    }

    /// The raw handle, for calls that the caller makes itself.
    pub fn as_raw(&self) -> HANDLE {
        self.handle.0
    }

    /// Connects to the pipe `name` (`\\.\pipe\...`) as a client.
    ///
    /// The connection is opened at the anonymous impersonation level: the
    /// process that created the pipe can read and write it but can never act
    /// with this process's rights, which the interface's process would
    /// otherwise be able to do through the pipe.
    pub fn connect(name: &str) -> io::Result<PipeEnd> {
        let wide_name = wide(name);
        for _ in 0..20 {
            // SAFETY: `wide_name` is NUL-terminated and outlives the call; the
            // security attributes and template handle are null.
            let handle = unsafe {
                CreateFileW(
                    wide_name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                // SAFETY: a new handle opened for overlapped I/O, owned here.
                return Ok(unsafe { PipeEnd::from_raw(handle) });
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
                return Err(error);
            }
            // SAFETY: `wide_name` is NUL-terminated.
            unsafe { WaitNamedPipeW(wide_name.as_ptr(), 500) };
        }
        Err(io::Error::from(io::ErrorKind::TimedOut))
    }

    /// The process that created the pipe (the server end), as the system
    /// records it.
    pub fn server_process_id(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        // SAFETY: the handle is a valid pipe handle and `pid` is writable.
        if unsafe { GetNamedPipeServerProcessId(self.as_raw(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }

    /// The process at the client end of the pipe, as the system records it.
    pub fn client_process_id(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        // SAFETY: the handle is a valid pipe handle and `pid` is writable.
        if unsafe { GetNamedPipeClientProcessId(self.as_raw(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }

    /// Runs one overlapped operation to completion.
    fn transfer(&self, start: impl FnOnce(*mut OVERLAPPED, &mut u32) -> i32) -> io::Result<usize> {
        // SAFETY: a manual-reset, unnamed event; the null arguments are allowed.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Owned(event);
        // SAFETY: an all-zero OVERLAPPED is the documented initial state.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0;
        let mut done = 0u32;
        let started = start(&mut overlapped, &mut done);
        if started == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
            // SAFETY: the handle and the OVERLAPPED are the ones the operation
            // was started with, and both outlive this call; it waits for the
            // operation to finish, so the buffer is no longer in use after it.
            let finished = unsafe { GetOverlappedResult(self.as_raw(), &overlapped, &mut done, 1) };
            if finished == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(done as usize)
    }
}

fn is_closed(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    )
}

impl Read for PipeEnd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let ptr = buf.as_mut_ptr();
        let handle = self.as_raw();
        let result = self.transfer(|overlapped, done| {
            // SAFETY: `ptr` is writable for `len` bytes and stays valid until
            // `transfer` has seen the operation finish; the OVERLAPPED is
            // valid for the same time.
            unsafe { ReadFile(handle, ptr.cast(), len, done, overlapped) }
        });
        match result {
            // A closed pipe is the end of the stream.
            Err(error) if is_closed(&error) => Ok(0),
            other => other,
        }
    }
}

impl Write for PipeEnd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let ptr = buf.as_ptr();
        let handle = self.as_raw();
        let result = self.transfer(|overlapped, done| {
            // SAFETY: `ptr` is readable for `len` bytes and stays valid until
            // `transfer` has seen the operation finish; the OVERLAPPED is
            // valid for the same time.
            unsafe { WriteFile(handle, ptr.cast(), len, done, overlapped) }
        });
        match result {
            Err(error) if is_closed(&error) => Err(io::ErrorKind::BrokenPipe.into()),
            other => other,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The path of the program a process is running, in the form `C:\...`.
pub fn process_image_path(pid: u32) -> io::Result<PathBuf> {
    // SAFETY: plain integers; the result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = Owned(process);
    // SAFETY: `process` is a valid process handle, kept open by `process`.
    unsafe { image_path_of_handle(process.0) }
}

/// The path of the program a process handle refers to.
///
/// # Safety
///
/// `process` must be a valid process handle opened with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
pub unsafe fn image_path_of_handle(process: HANDLE) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = vec![0u16; 1024];
    loop {
        let mut len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: `buffer` is writable for `len` units and `len` is updated to
        // the length written.
        let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len) };
        if ok != 0 {
            buffer.truncate(len as usize);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
        }
        let error = io::Error::last_os_error();
        // The call fails with "insufficient buffer" for a long path.
        if error.raw_os_error() == Some(122) && buffer.len() < 32 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
        } else {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_FIRST_PIPE_INSTANCE;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn name() -> String {
        format!(
            r"\\.\pipe\bytescape-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A pipe pair: the server end made here, a client connected to it.
    fn pair() -> (PipeEnd, PipeEnd) {
        let name = name();
        let wide = wide(&name);
        // SAFETY: `wide` is NUL-terminated; the security attributes are null.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                0x3 | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(
            handle,
            INVALID_HANDLE_VALUE,
            "{}",
            io::Error::last_os_error()
        );
        // SAFETY: a new handle opened for overlapped I/O, owned here.
        let server = unsafe { PipeEnd::from_raw(handle) };
        let client = PipeEnd::connect(&name).unwrap();
        // The client is already there, so this call reports it at once.
        // SAFETY: an all-zero OVERLAPPED is the documented initial state.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: an unnamed event, closed below once the call is over.
        overlapped.hEvent = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        // SAFETY: the handle and the OVERLAPPED are valid for the call.
        unsafe { ConnectNamedPipe(server.as_raw(), &mut overlapped) };
        // SAFETY: the event was created above and is closed once.
        unsafe { CloseHandle(overlapped.hEvent) };
        (server, client)
    }

    #[test]
    fn bytes_cross_in_both_directions() {
        let (mut server, mut client) = pair();
        client.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        server.write_all(b"world!").unwrap();
        let mut buf = [0u8; 6];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"world!");
    }

    #[test]
    fn a_waiting_read_does_not_hold_up_a_write_on_the_same_pipe() {
        // The reason the pipes are opened for overlapped I/O: on a handle
        // opened for synchronous I/O the write below would wait for the read.
        let (server, mut client) = pair();
        let mut reader = client.clone();
        let waiting = std::thread::spawn(move || {
            let mut buf = [0u8; 4];
            reader.read_exact(&mut buf).unwrap();
            buf
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        client.write_all(b"ping").unwrap();
        let mut server_reader = server.clone();
        let mut got = [0u8; 4];
        server_reader.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");
        let mut server_writer = server;
        server_writer.write_all(b"pong").unwrap();
        assert_eq!(&waiting.join().unwrap(), b"pong");
    }

    #[test]
    fn a_closed_pipe_reads_as_the_end_and_refuses_writes() {
        let (server, mut client) = pair();
        drop(server);
        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
        assert_eq!(
            client.write_all(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn both_ends_know_the_processes_at_the_other_end() {
        let (server, client) = pair();
        let me = std::process::id();
        assert_eq!(client.server_process_id().unwrap(), me);
        assert_eq!(server.client_process_id().unwrap(), me);
    }

    #[test]
    fn the_image_of_a_process_is_its_program() {
        let path = process_image_path(std::process::id()).unwrap();
        let own = std::env::current_exe().unwrap();
        assert_eq!(
            std::fs::canonicalize(path).unwrap(),
            std::fs::canonicalize(own).unwrap()
        );
    }

    #[test]
    fn a_pipe_that_does_not_exist_cannot_be_connected_to() {
        assert!(PipeEnd::connect(&name()).is_err());
    }
}
