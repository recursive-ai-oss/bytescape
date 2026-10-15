//! Whether this program is running with administrator rights, which it must
//! not.
//!
//! The interface never runs elevated. Work that needs the rights is done by
//! the helper, which is started when the user asks and exits when its job is
//! done. A window that stays open with those rights would be an interface
//! bug away from being a way to act as the administrator, so the program
//! refuses to start that way and says why.
//!
//! ```
//! if let Some(message) = bs_platform::elevation::refusal() {
//!     // Show `message` once, and exit.
//!     # let _ = message;
//! }
//! ```

#![allow(unsafe_code)]

/// What the interface shows, once, when it finds itself elevated.
pub const ELEVATED_MESSAGE: &str = "Bytescape is running with administrator rights, which it never does. Close it and start it again as your own user. It asks for administrator rights only when you ask for a full scan or a delete that needs them, and only for the helper that does it.";

/// What the system says about the rights of this process.
pub trait ProcessRights {
    /// Whether the process has administrator rights: an elevated token on
    /// Windows, the superuser on Linux.
    fn is_elevated(&self) -> bool;
}

/// This process, as the operating system reports it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThisProcess;

impl ProcessRights for ThisProcess {
    fn is_elevated(&self) -> bool {
        os::this_process_is_elevated()
    }
}

/// Whether this process has administrator rights.
///
/// If the system cannot answer, this says no: refusing to start on a failed
/// query would make the program unusable, and the helper, not this process, is
/// where administrator work happens.
pub fn is_elevated() -> bool {
    ThisProcess.is_elevated()
}

/// The message to show and then exit, if `rights` are elevated.
pub fn refusal_for(rights: &dyn ProcessRights) -> Option<&'static str> {
    rights.is_elevated().then_some(ELEVATED_MESSAGE)
}

/// The message to show and then exit, if this process is elevated.
pub fn refusal() -> Option<&'static str> {
    refusal_for(&ThisProcess)
}

#[cfg(target_os = "linux")]
mod os {
    pub(super) fn this_process_is_elevated() -> bool {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(super) fn this_process_is_elevated() -> bool {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no
        // closing, and `token` is writable storage for the handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let Ok(size) = u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>()) else {
            // SAFETY: `token` is the handle opened above and is closed once.
            unsafe { CloseHandle(token) };
            return false;
        };
        // SAFETY: `elevation` is writable storage of the size passed, and
        // `token` is a valid token handle opened with query rights.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                std::ptr::addr_of_mut!(elevation).cast(),
                size,
                &mut returned,
            )
        };
        // SAFETY: `token` is the handle opened above and is closed once.
        unsafe { CloseHandle(token) };
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod os {
    pub(super) fn this_process_is_elevated() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(bool);

    impl ProcessRights for Fake {
        fn is_elevated(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn an_elevated_process_is_refused_with_one_message() {
        assert_eq!(refusal_for(&Fake(true)), Some(ELEVATED_MESSAGE));
        assert_eq!(refusal_for(&Fake(false)), None);
    }

    #[test]
    fn the_message_says_what_to_do_and_when_rights_are_asked_for() {
        assert!(ELEVATED_MESSAGE.contains("start it again"));
        assert!(ELEVATED_MESSAGE.contains("helper"));
    }

    #[test]
    fn the_real_answer_matches_the_system_where_it_can_be_checked() {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `geteuid` takes no arguments and cannot fail.
            let superuser = unsafe { libc::geteuid() } == 0;
            assert_eq!(is_elevated(), superuser);
            assert_eq!(refusal().is_some(), superuser);
        }
        // On Windows a build machine may run the tests elevated, so the
        // answer is only asked for, and the two entry points agree.
        #[cfg(windows)]
        assert_eq!(refusal().is_some(), is_elevated());
    }
}
