//! The Windows folders that the protected-path table names by variable
//! (`%SystemRoot%`, `%ProgramFiles%`, `%USERPROFILE%`, ...), taken from the
//! system and not from the process environment.
//!
//! A process can be started with any environment its parent likes. The
//! elevated helper must not decide what is an operating system folder from
//! text that the program which started it chose, so the values here come from
//! the system's own answers instead:
//!
//! - the Windows folder from `GetSystemWindowsDirectoryW`;
//! - the Program Files folders and the program data folder from keys under
//!   `HKEY_LOCAL_MACHINE`, which only an administrator can change;
//! - the profile folder of the user this process runs as from the profile
//!   list in the same part of the registry, found by the user's identifier.
//!   The application data folders are the standard folders inside it.
//!
//! The per-user part of the registry (`HKEY_CURRENT_USER`) and the shell's
//! known-folder calls, which read it and expand variables from the
//! environment, are not used: the user can change them.
//!
//! The rules live here, behind [`Source`], so they are tested on every system.
//! [`System`] is the Windows implementation.

/// Where the values come from. Everything is text, and anything that is missing
/// or not valid text is `None`.
pub trait Source {
    /// The Windows folder, such as `C:\Windows`.
    fn windows_directory(&self) -> Option<String>;
    /// A string value under `HKEY_LOCAL_MACHINE`, in the 64-bit view, with
    /// its `%` references not expanded.
    fn machine_string(&self, subkey: &str, value: &str) -> Option<String>;
    /// The user this process runs as, as `S-1-5-21-...`.
    fn user_sid(&self) -> Option<String>;
}

const VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";
const PROFILE_LIST: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";

/// A folder on a drive: a letter, a colon, a backslash, and one or more names
/// between backslashes, none of them empty, `.` or `..`. A drive root alone, a
/// share, a path with a prefix and a relative path are not.
fn is_drive_folder(text: &str) -> bool {
    let mut chars = text.chars();
    let (Some(letter), Some(':'), Some('\\')) = (chars.next(), chars.next(), chars.next()) else {
        return false;
    };
    let rest = chars.as_str();
    letter.is_ascii_alphabetic()
        && !rest.is_empty()
        && !rest.contains(['/', '\0'])
        && rest
            .split('\\')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// `text` with `%SystemDrive%` and `%SystemRoot%` replaced, whatever their
/// case, by the values the system gave. Any other `%` makes it `None`: the
/// rest of the environment is not consulted.
fn expand(text: &str, system_drive: &str, system_root: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('%') {
        out.push_str(before);
        let (name, tail) = after.split_once('%')?;
        if name.eq_ignore_ascii_case("SystemDrive") {
            out.push_str(system_drive);
        } else if name.eq_ignore_ascii_case("SystemRoot") {
            out.push_str(system_root);
        } else {
            return None;
        }
        rest = tail;
    }
    out.push_str(rest);
    Some(out)
}

/// The folders the table names, as (variable name, folder). A folder the
/// system does not give, or gives in a form that is not a plain folder on a
/// drive, is left out: the table then guards the usual place by its drive
/// letter alone, which is how an entry fails closed.
pub fn trusted(source: &dyn Source) -> Vec<(&'static str, String)> {
    let mut found: Vec<(&'static str, String)> = Vec::new();
    let Some(root) = source
        .windows_directory()
        .map(|text| text.trim_end_matches('\\').to_owned())
        .filter(|text| is_drive_folder(text))
    else {
        // Without the Windows folder there is no way to expand anything.
        return found;
    };
    let drive: String = root.chars().take(2).collect();
    let mut add = |name: &'static str, value: Option<String>| {
        if let Some(value) = value.filter(|v| is_drive_folder(v)) {
            found.push((name, value));
        }
    };
    let read = |subkey: &str, value: &str| {
        source
            .machine_string(subkey, value)
            .and_then(|text| expand(&text, &drive, &root))
            .map(|text| text.trim_end_matches('\\').to_owned())
    };
    add("SystemRoot", Some(root.clone()));
    add("ProgramFiles", read(VERSION_KEY, "ProgramFilesDir"));
    add("ProgramW6432", read(VERSION_KEY, "ProgramW6432Dir"));
    add(
        "ProgramFiles(x86)",
        read(VERSION_KEY, "ProgramFilesDir (x86)"),
    );
    add(
        "ProgramFiles(Arm)",
        read(VERSION_KEY, "ProgramFilesDir (Arm)"),
    );
    add("ProgramData", read(PROFILE_LIST, "ProgramData"));
    let profile = source
        .user_sid()
        .filter(|sid| is_sid_text(sid))
        .and_then(|sid| read(&format!(r"{PROFILE_LIST}\{sid}"), "ProfileImagePath"));
    if let Some(profile) = profile.filter(|p| is_drive_folder(p)) {
        add("APPDATA", Some(format!(r"{profile}\AppData\Roaming")));
        add("LOCALAPPDATA", Some(format!(r"{profile}\AppData\Local")));
        add("USERPROFILE", Some(profile));
    }
    // `SystemDrive` is a drive, not a folder; the table's entries for it
    // are written for the drive's own letter.
    found.push(("SystemDrive", drive));
    found
}

/// Whether `text` looks like a security identifier, which becomes part of a
/// registry key name.
fn is_sid_text(text: &str) -> bool {
    text.starts_with("S-1-")
        && text.len() <= 184
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
}

#[cfg(windows)]
pub use system::System;

#[cfg(windows)]
mod system {
    #![allow(unsafe_code)]

    use super::Source;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        RRF_SUBKEY_WOW6464KEY, RegGetValueW,
    };
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The running system.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct System;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Text from `units`, up to the first NUL, if it is valid UTF-16.
    fn text(units: &[u16]) -> Option<String> {
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16(units.get(..end)?).ok()
    }

    impl Source for System {
        fn windows_directory(&self) -> Option<String> {
            let mut buffer = vec![0u16; 260];
            for _ in 0..2 {
                let capacity = u32::try_from(buffer.len()).ok()?;
                // SAFETY: `buffer` is writable for `capacity` units.
                let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), capacity) };
                let length = length as usize;
                if length == 0 {
                    return None;
                }
                if length < buffer.len() {
                    return text(buffer.get(..length)?);
                }
                // Too small: the call returned the size needed, with the NUL.
                buffer = vec![0u16; length.checked_add(1)?];
            }
            None
        }

        fn machine_string(&self, subkey: &str, value: &str) -> Option<String> {
            let (subkey, value) = (wide(subkey), wide(value));
            let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND | RRF_SUBKEY_WOW6464KEY;
            // A folder name is short; a value larger than this is not one.
            let mut buffer = vec![0u16; 520];
            for _ in 0..2 {
                let mut bytes = u32::try_from(buffer.len().checked_mul(2)?).ok()?;
                // SAFETY: `subkey` and `value` are NUL-terminated and outlive
                // the call; `buffer` is writable for `bytes` bytes, which is
                // the size passed.
                let status = unsafe {
                    RegGetValueW(
                        HKEY_LOCAL_MACHINE,
                        subkey.as_ptr(),
                        value.as_ptr(),
                        flags,
                        std::ptr::null_mut(),
                        buffer.as_mut_ptr().cast::<c_void>(),
                        &mut bytes,
                    )
                };
                if status == ERROR_MORE_DATA && bytes <= 65_536 {
                    buffer = vec![0u16; (bytes as usize).div_ceil(2).checked_add(1)?];
                    continue;
                }
                if status != ERROR_SUCCESS {
                    return None;
                }
                return text(&buffer);
            }
            None
        }

        fn user_sid(&self) -> Option<String> {
            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: the pseudo handle of the running process is always valid
            // and `token` is writable.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return None;
            }
            let sid = sid_of_token(token);
            // SAFETY: `token` was opened above and is closed exactly once.
            unsafe { CloseHandle(token) };
            sid
        }
    }

    fn sid_of_token(token: HANDLE) -> Option<String> {
        let mut needed = 0u32;
        // SAFETY: a null buffer of length 0 only asks for the size needed.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 || needed > 4096 {
            return None;
        }
        // Eight-byte units keep the buffer aligned for `TOKEN_USER`.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buffer` is writable for at least `needed` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: the call filled the buffer with a `TOKEN_USER`, aligned; the
        // SID it points to lives inside the same buffer.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut string: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is valid; `string` receives a system allocation.
        if unsafe { ConvertSidToStringSidW(sid, &mut string) } == 0 || string.is_null() {
            return None;
        }
        let mut length = 0usize;
        // SAFETY: the system returned a NUL-terminated string.
        while unsafe { *string.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `string` points to `length` initialised units.
        let value = text(unsafe { std::slice::from_raw_parts(string, length) });
        // SAFETY: allocated by the call above for this purpose, freed once.
        unsafe { LocalFree(string.cast()) };
        value
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::collections::HashMap;

    /// A system with a fixed answer for everything.
    #[derive(Default)]
    struct Fake {
        windows: Option<&'static str>,
        sid: Option<&'static str>,
        values: HashMap<(String, String), &'static str>,
    }

    impl Fake {
        fn with(mut self, subkey: &str, value: &str, data: &'static str) -> Self {
            self.values.insert((subkey.into(), value.into()), data);
            self
        }
    }

    impl Source for Fake {
        fn windows_directory(&self) -> Option<String> {
            self.windows.map(str::to_owned)
        }
        fn machine_string(&self, subkey: &str, value: &str) -> Option<String> {
            self.values
                .get(&(subkey.to_owned(), value.to_owned()))
                .map(|s| (*s).to_owned())
        }
        fn user_sid(&self) -> Option<String> {
            self.sid.map(str::to_owned)
        }
    }

    const SID: &str = "S-1-5-21-1-2-3-1001";

    fn typical() -> Fake {
        Fake {
            windows: Some(r"C:\Windows"),
            sid: Some(SID),
            ..Fake::default()
        }
        .with(VERSION_KEY, "ProgramFilesDir", r"C:\Program Files")
        .with(VERSION_KEY, "ProgramW6432Dir", r"C:\Program Files")
        .with(
            VERSION_KEY,
            "ProgramFilesDir (x86)",
            r"C:\Program Files (x86)",
        )
        .with(PROFILE_LIST, "ProgramData", r"%SystemDrive%\ProgramData")
        .with(
            &format!(r"{PROFILE_LIST}\{SID}"),
            "ProfileImagePath",
            r"%systemdrive%\Users\alice",
        )
    }

    fn get(found: &[(&'static str, String)], name: &str) -> Option<String> {
        found
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn the_folders_are_what_the_system_says() {
        let found = trusted(&typical());
        for (name, want) in [
            ("SystemRoot", r"C:\Windows"),
            ("SystemDrive", "C:"),
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramW6432", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("ProgramData", r"C:\ProgramData"),
            ("USERPROFILE", r"C:\Users\alice"),
            ("APPDATA", r"C:\Users\alice\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\alice\AppData\Local"),
        ] {
            assert_eq!(get(&found, name).as_deref(), Some(want), "{name}");
        }
        // A folder the system does not have is left out, not guessed.
        assert_eq!(get(&found, "ProgramFiles(Arm)"), None);
    }

    #[test]
    fn a_windows_folder_on_another_drive_moves_everything_that_names_the_system_drive() {
        let mut fake = typical();
        fake.windows = Some(r"D:\WINNT\");
        let found = trusted(&fake);
        assert_eq!(get(&found, "SystemRoot").as_deref(), Some(r"D:\WINNT"));
        assert_eq!(get(&found, "SystemDrive").as_deref(), Some("D:"));
        assert_eq!(
            get(&found, "ProgramData").as_deref(),
            Some(r"D:\ProgramData")
        );
        assert_eq!(
            get(&found, "USERPROFILE").as_deref(),
            Some(r"D:\Users\alice")
        );
    }

    #[test]
    fn nothing_comes_from_the_environment_or_from_a_reference_to_it() {
        // A value that refers to any variable but the two the system defines
        // is not expanded, and so is not used.
        let fake = typical()
            .with(VERSION_KEY, "ProgramFilesDir", r"%ProgramFiles%\x")
            .with(PROFILE_LIST, "ProgramData", r"%OtherVar%\ProgramData")
            .with(
                &format!(r"{PROFILE_LIST}\{SID}"),
                "ProfileImagePath",
                r"%USERPROFILE%",
            );
        let found = trusted(&fake);
        for name in ["ProgramFiles", "ProgramData", "USERPROFILE", "APPDATA"] {
            assert_eq!(get(&found, name), None, "{name}");
        }
        // The same behind a drive, where what is left would look like a folder.
        let fake = typical()
            .with(VERSION_KEY, "ProgramFilesDir", r"C:\%ProgramFiles%\x")
            .with(PROFILE_LIST, "ProgramData", r"%SystemDrive%\%OtherVar%")
            .with(
                &format!(r"{PROFILE_LIST}\{SID}"),
                "ProfileImagePath",
                r"%SystemDrive%\Users\%USERNAME%",
            );
        let found = trusted(&fake);
        for name in ["ProgramFiles", "ProgramData", "USERPROFILE", "LOCALAPPDATA"] {
            assert_eq!(get(&found, name), None, "{name}");
        }
        // Unbalanced percent signs are not a reference either.
        assert_eq!(expand("100%", "C:", r"C:\Windows"), None);
        assert_eq!(
            expand("%SYSTEMROOT%\\a%SystemDrive%", "C:", r"C:\Windows").as_deref(),
            Some(r"C:\Windows\aC:")
        );
    }

    #[test]
    fn a_value_that_is_not_a_plain_folder_on_a_drive_is_left_out() {
        for bad in [
            "",
            r"C:\",
            "C:",
            r"\\server\share\Program Files",
            r"\\?\C:\Program Files",
            r"Program Files",
            r"C:\Program Files\..\Windows",
            r"C:\a\.\b",
            "C:/Program Files",
            "C:\\a\0b",
            r"1:\x",
        ] {
            let fake = typical().with(VERSION_KEY, "ProgramFilesDir", Box::leak(bad.into()));
            assert_eq!(get(&trusted(&fake), "ProgramFiles"), None, "{bad:?}");
        }
        // The same rule for the Windows folder: with none the list is empty.
        for bad in [None, Some(""), Some("Windows"), Some(r"\\h\s\Windows")] {
            let mut fake = typical();
            fake.windows = bad;
            assert!(trusted(&fake).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn the_profile_is_found_only_for_a_well_formed_identifier() {
        for sid in [
            None,
            Some(""),
            Some("S-1-5-21\\..\\x"),
            Some("S-1-5-21-1 "),
            Some("sid"),
        ] {
            let mut fake = typical();
            fake.sid = sid;
            let found = trusted(&fake);
            assert_eq!(get(&found, "USERPROFILE"), None, "{sid:?}");
            assert_eq!(get(&found, "LOCALAPPDATA"), None);
            // The system folders do not depend on who the user is.
            assert_eq!(get(&found, "SystemRoot").as_deref(), Some(r"C:\Windows"));
        }
        // A user with no profile-list entry has no profile folder.
        let mut fake = typical();
        fake.sid = Some("S-1-5-21-9-9-9-1");
        assert_eq!(get(&trusted(&fake), "USERPROFILE"), None);
    }
}
