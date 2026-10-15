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

use super::{ElevateError, Elevation, Launched, Unavailable, hex};
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

/// The units of a path with a NUL after them, exactly as the path holds them.
/// Read as text, a name with a lone surrogate would turn into the name of a
/// different file, and that is the program the elevation prompt would start.
fn wide_path(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Whether the program that connected (`image`) is the helper that was
/// started (`helper`): the same place, compared by the exact 16-bit units of
/// the names after the documented normalisation (case, `/`, `\\?\`). A name
/// with a lone surrogate is not the one with U+FFFD in its place.
fn is_the_helper(image: &std::path::Path, helper: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let units = |path: &std::path::Path| -> Vec<u16> { path.as_os_str().encode_wide().collect() };
    bs_fsops::winpath::same_path_units(&units(image), &units(helper))
}

pub(super) fn random_bytes(buf: &mut [u8]) -> Result<(), ElevateError> {
    let len = u32::try_from(buf.len())
        .map_err(|_| ElevateError::Unavailable(Unavailable::Other("too many bytes")))?;
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
        .ok_or(ElevateError::Unavailable(Unavailable::Other(
            "this program has no folder",
        )))?;
    if helper.is_file() {
        Ok(helper)
    } else {
        Err(ElevateError::Unavailable(Unavailable::HelperNotInstalled))
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
    let sddl = pipe_sddl().ok_or(ElevateError::Unavailable(Unavailable::Other(
        "cannot tell who the user is",
    )))?;
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
    let file = wide_path(helper);
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
        .map_err(|_| ElevateError::Unavailable(Unavailable::HelperNotInstalled))?;
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
    // Compared by the 16-bit units of the names, not as text.
    if !is_the_helper(&image, &helper) {
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
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSidToSidW,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, CreateWellKnownSid,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetKernelObjectSecurity,
        GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorLength, SE_DACL_PROTECTED, WinBuiltinAdministratorsSid,
        WinLocalSystemSid, WinWorldSid,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    };

    fn unique_name() -> String {
        let mut bytes = [0u8; 16];
        random_bytes(&mut bytes).unwrap();
        format!(r"\\.\pipe\bytescape-test-{}", hex(&bytes))
    }

    /// A security descriptor in its self-relative form, in storage aligned for
    /// the structures inside it.
    struct Descriptor(Vec<u64>);

    impl Descriptor {
        fn as_ptr(&self) -> *mut c_void {
            self.0.as_ptr().cast_mut().cast()
        }

        /// The descriptor the system builds from rules written in text.
        fn from_text(sddl: &str) -> Descriptor {
            let wide_sddl = wide(sddl);
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: `wide_sddl` is NUL-terminated and `raw` is writable; the
            // result is copied out and released with `LocalFree` below.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide_sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut raw,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "{sddl}: {}", io::Error::last_os_error());
            // SAFETY: the conversion produced a valid self-relative descriptor.
            let length = unsafe { GetSecurityDescriptorLength(raw) } as usize;
            let mut copy = vec![0u64; length.div_ceil(8)];
            // SAFETY: `raw` holds `length` bytes and `copy` has room for them.
            unsafe {
                std::ptr::copy_nonoverlapping(raw.cast::<u8>(), copy.as_mut_ptr().cast(), length);
                LocalFree(raw as HLOCAL);
            }
            Descriptor(copy)
        }

        /// The access rules of a pipe, as the system holds them.
        fn of_pipe(pipe: &PipeEnd) -> Descriptor {
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
            Descriptor(buffer)
        }

        /// The rules in text form, for failure messages only. The text
        /// abbreviates well-known accounts (`LA` is the built-in
        /// Administrator), so no check reads it.
        fn text(&self) -> String {
            let mut text: *mut u16 = std::ptr::null_mut();
            let mut length = 0u32;
            // SAFETY: the descriptor is valid; the text is released with
            // `LocalFree` below.
            let ok = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    self.as_ptr(),
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

        fn dacl_is_protected(&self) -> bool {
            let mut control = 0u16;
            let mut revision = 0u32;
            // SAFETY: the descriptor is valid and both outputs are writable.
            let ok =
                unsafe { GetSecurityDescriptorControl(self.as_ptr(), &mut control, &mut revision) };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            control & SE_DACL_PROTECTED != 0
        }

        /// Every rule in the access list: its type, its rights and the bytes of
        /// the SID it names. Only allow and deny rules have this layout; any
        /// other kind is reported with an empty SID, which matches nobody.
        fn rules(&self) -> Vec<Rule> {
            let mut present = 0i32;
            let mut defaulted = 0i32;
            let mut acl: *mut ACL = std::ptr::null_mut();
            // SAFETY: the descriptor is valid and every output is writable.
            let ok = unsafe {
                GetSecurityDescriptorDacl(self.as_ptr(), &mut present, &mut acl, &mut defaulted)
            };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            // No list at all would admit everyone; report it as a rule.
            if present == 0 || acl.is_null() {
                return vec![Rule {
                    kind: u32::MAX,
                    mask: u32::MAX,
                    sid: Vec::new(),
                }];
            }
            let mut size = ACL_SIZE_INFORMATION {
                AceCount: 0,
                AclBytesInUse: 0,
                AclBytesFree: 0,
            };
            // SAFETY: `acl` is the list inside the descriptor and `size` is a
            // writable structure of the length passed.
            let ok = unsafe {
                GetAclInformation(
                    acl,
                    (&raw mut size).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            (0..size.AceCount)
                .map(|index| {
                    let mut ace: *mut c_void = std::ptr::null_mut();
                    // SAFETY: `index` is below the count the list reported.
                    let ok = unsafe { GetAce(acl, index, &mut ace) };
                    assert_ne!(ok, 0, "{}", io::Error::last_os_error());
                    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                    // SAFETY: every rule starts with this header, and the reads
                    // do not assume the alignment of the pointer.
                    let (kind, mask) = unsafe {
                        (
                            u32::from(std::ptr::addr_of!((*ace).Header.AceType).read_unaligned()),
                            std::ptr::addr_of!((*ace).Mask).read_unaligned(),
                        )
                    };
                    let sid = if kind == ACCESS_ALLOWED_ACE_TYPE || kind == ACCESS_DENIED_ACE_TYPE {
                        // SAFETY: an allow or deny rule holds a SID from
                        // `SidStart` to the end of the rule; its length is
                        // asked of the system and the bytes are copied.
                        unsafe {
                            let start = std::ptr::addr_of_mut!((*ace).SidStart).cast::<c_void>();
                            let length = GetLengthSid(start) as usize;
                            std::slice::from_raw_parts(start.cast::<u8>(), length).to_vec()
                        }
                    } else {
                        Vec::new()
                    };
                    Rule { kind, mask, sid }
                })
                .collect()
        }
    }

    struct Rule {
        kind: u32,
        mask: u32,
        sid: Vec<u8>,
    }

    /// The bytes of a SID written as `S-1-...`.
    fn sid_from_text(text: &str) -> Vec<u8> {
        let wide_text = wide(text);
        let mut sid: *mut c_void = std::ptr::null_mut();
        // SAFETY: `wide_text` is NUL-terminated and `sid` is writable; the
        // result is copied and released with `LocalFree`.
        let ok = unsafe { ConvertStringSidToSidW(wide_text.as_ptr(), &mut sid) };
        assert_ne!(ok, 0, "{text}: {}", io::Error::last_os_error());
        // SAFETY: the conversion produced a valid SID.
        let bytes = unsafe {
            let length = GetLengthSid(sid) as usize;
            std::slice::from_raw_parts(sid.cast::<u8>(), length).to_vec()
        };
        // SAFETY: allocated by the conversion call above.
        unsafe { LocalFree(sid as HLOCAL) };
        bytes
    }

    /// The bytes of a well-known account's SID (`kind` is a `WinXxxSid` value).
    fn well_known_sid(kind: i32) -> Vec<u8> {
        let mut buffer = [0u8; 68];
        let mut length = buffer.len() as u32;
        // SAFETY: `buffer` is writable for `length` bytes, the most a SID needs.
        let ok = unsafe {
            CreateWellKnownSid(
                kind,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut length,
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        buffer[..length as usize].to_vec()
    }

    /// Whether two SIDs are the same account, as the system compares them.
    fn same_sid(a: &[u8], b: &[u8]) -> bool {
        if a.is_empty() || b.is_empty() {
            return false;
        }
        // SAFETY: both are complete SIDs copied out by the helpers above.
        unsafe { EqualSid(a.as_ptr().cast_mut().cast(), b.as_ptr().cast_mut().cast()) != 0 }
    }

    /// Whether the rules admit `user` and nobody else: protected, one rule,
    /// which allows, with some rights, and names `user`.
    fn admits_only(sd: &Descriptor, user: &[u8]) -> Result<(), String> {
        if !sd.dacl_is_protected() {
            return Err("the list is not protected, so it can inherit rules".into());
        }
        let rules = sd.rules();
        let [rule] = rules.as_slice() else {
            return Err(format!("{} rules, not one", rules.len()));
        };
        if rule.kind != ACCESS_ALLOWED_ACE_TYPE {
            return Err("the one rule does not allow".into());
        }
        if rule.mask == 0 {
            return Err("the one rule grants nothing".into());
        }
        if !same_sid(&rule.sid, user) {
            return Err("the one rule names someone else".into());
        }
        Ok(())
    }

    #[test]
    fn the_pipe_admits_the_current_user_and_nobody_else() {
        let pipe = create_pipe(&unique_name()).unwrap();
        let user = sid_from_text(crate::bins::windows::user_sid().unwrap());
        let sd = Descriptor::of_pipe(&pipe);
        // Protected (no inherited entries), one entry that allows, naming the
        // user. Compared by SID value, not by the text, which writes some
        // accounts as two letters (the built-in Administrator is `LA`).
        admits_only(&sd, &user).unwrap_or_else(|why| panic!("{why}: {}", sd.text()));
        // Nobody else: neither everyone, nor the administrators group, nor the
        // system account is named by any rule, unless that is the user itself.
        let rules = sd.rules();
        for kind in [WinWorldSid, WinBuiltinAdministratorsSid, WinLocalSystemSid] {
            let other = well_known_sid(kind);
            if !same_sid(&user, &other) {
                assert!(
                    !rules.iter().any(|r| same_sid(&r.sid, &other)),
                    "{}",
                    sd.text()
                );
            }
        }
    }

    #[test]
    fn the_check_for_the_users_rule_refuses_every_wider_or_other_rule() {
        let user_text = crate::bins::windows::user_sid().unwrap();
        let user = sid_from_text(user_text);
        let good = Descriptor::from_text(&format!("D:P(A;;GA;;;{user_text})"));
        admits_only(&good, &user).unwrap();
        let mut bad = vec![
            // Not protected: inherited rules would come with it.
            format!("D:(A;;GA;;;{user_text})"),
            // An empty list, and a deny rule where an allow belongs.
            "D:P".to_string(),
            format!("D:P(D;;GA;;;{user_text})"),
            // The user plus someone else.
            format!("D:P(A;;GA;;;{user_text})(A;;GA;;;WD)"),
            // Rules for everyone and for the administrators group.
            "D:P(A;;GA;;;WD)".to_string(),
            "D:P(A;;GA;;;BA)".to_string(),
        ];
        if !same_sid(&user, &well_known_sid(WinLocalSystemSid)) {
            bad.push("D:P(A;;GA;;;SY)".to_string());
        }
        for sddl in bad {
            let sd = Descriptor::from_text(&sddl);
            assert!(admits_only(&sd, &user).is_err(), "{sddl}");
        }
        // The text form of a well-known account is two letters, and a SID that
        // is not the user's is not matched by looking for those letters.
        let administrators = Descriptor::from_text("D:P(A;;GA;;;BA)");
        assert!(admits_only(&administrators, &sid_from_text(user_text)).is_err());
        assert!(!same_sid(
            &sid_from_text("S-1-5-32-544"),
            &sid_from_text("S-1-5-32-545")
        ));
    }

    #[test]
    fn the_helper_image_is_compared_by_exact_units() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let named = |tail: &[u16]| -> PathBuf {
            let mut units: Vec<u16> = r"C:\Program Files\Bytescape\".encode_utf16().collect();
            units.extend_from_slice(tail);
            PathBuf::from(OsString::from_wide(&units))
        };
        let plain: Vec<u16> = "bytescape-helper.exe".encode_utf16().collect();
        let mut lone = plain.clone();
        lone.insert(0, 0xD800);
        let mut replaced = plain.clone();
        replaced.insert(0, 0xFFFD);
        // The same file spelled in another case, or with the long-path prefix.
        assert!(is_the_helper(
            &named(&plain),
            &PathBuf::from(named(&plain).to_string_lossy().to_uppercase())
        ));
        assert!(is_the_helper(
            &PathBuf::from(format!(r"\\?\{}", named(&plain).display())),
            &named(&plain)
        ));
        // A lone surrogate is not U+FFFD, and neither is another program.
        assert!(!is_the_helper(&named(&lone), &named(&replaced)));
        assert!(is_the_helper(&named(&lone), &named(&lone)));
        assert!(!is_the_helper(
            &named(&plain),
            &named(&"bytescape.exe".encode_utf16().collect::<Vec<u16>>())
        ));
    }

    #[test]
    fn a_path_is_handed_to_the_shell_as_its_exact_units() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let mut units: Vec<u16> = r"C:\a\".encode_utf16().collect();
        units.push(0xD800);
        let path = PathBuf::from(OsString::from_wide(&units));
        let mut want = units.clone();
        want.push(0);
        assert_eq!(wide_path(&path), want);
        // Not what reading it as text would give.
        assert_ne!(wide(&path.to_string_lossy()), want);
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
