//! A one-line description of the operating system for the crash report. It is
//! read once, when the hook is installed, so the hook itself reads no files.

/// For example `Ubuntu 24.04.1 LTS (kernel 6.8.0-45-generic)` or
/// `Windows 10.0 build 26100`.
pub(crate) fn describe() -> String {
    #[cfg(target_os = "linux")]
    {
        linux()
    }
    #[cfg(windows)]
    {
        windows()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        std::env::consts::OS.to_owned()
    }
}

/// The `PRETTY_NAME` of an os-release file, or its `NAME`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_os_release(text: &str) -> Option<String> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(key)?.strip_prefix('=')?;
            let rest = rest.trim();
            let unquoted = rest
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .or_else(|| rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
                .unwrap_or(rest);
            // One short line: a report line cannot be forged from this file.
            let clean: String = unquoted
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect();
            (!clean.trim().is_empty()).then_some(clean)
        })
    };
    value("PRETTY_NAME").or_else(|| value("NAME"))
}

/// Reads at most `limit` bytes of a small system file.
#[cfg(target_os = "linux")]
fn read_small(path: &str, limit: u64) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(limit)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

#[cfg(target_os = "linux")]
fn linux() -> String {
    let name = read_small("/etc/os-release", 16 * 1024)
        .and_then(|text| parse_os_release(&text))
        .unwrap_or_else(|| "Linux".to_owned());
    let kernel = read_small("/proc/sys/kernel/osrelease", 256)
        .map(|text| {
            text.trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect::<String>()
        })
        .filter(|text| !text.is_empty());
    match kernel {
        Some(kernel) => format!("{name} (kernel {kernel})"),
        None => name,
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows() -> String {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: plain data that the call fills in.
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: `info` is writable and its size field is set. Unlike the
    // documented version call, this one reports the real version whatever the
    // program's manifest says.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status == 0 {
        format!(
            "Windows {}.{} build {}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
        )
    } else {
        "Windows".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_names() {
        let text = "NAME=\"Ubuntu\"\nVERSION=\"24.04.1 LTS (Noble Numbat)\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(
            parse_os_release(text).as_deref(),
            Some("Ubuntu 24.04.1 LTS")
        );
        assert_eq!(
            parse_os_release("NAME=Arch Linux\nID=arch").as_deref(),
            Some("Arch Linux")
        );
        // Control characters are dropped, so a file cannot add report lines.
        assert_eq!(
            parse_os_release("PRETTY_NAME='Alpine \u{1b}[31mLinux'\n").as_deref(),
            Some("Alpine [31mLinux")
        );
        assert_eq!(parse_os_release("PRETTY_NAME=\n"), None);
        assert_eq!(parse_os_release("ID=x\n"), None);
        assert_eq!(parse_os_release(""), None);
        let long = format!("PRETTY_NAME={}", "x".repeat(500));
        assert_eq!(parse_os_release(&long).map(|s| s.len()), Some(80));
    }

    #[test]
    fn this_system_describes_itself_on_one_line() {
        let text = describe();
        assert!(!text.is_empty() && !text.contains('\n'), "{text:?}");
    }
}
