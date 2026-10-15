//! Reading the registry's list of installed programs.

#![allow(unsafe_code)]

use super::programs::Installed;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RRF_RT_REG_DWORD,
    RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};

/// Most entries read from one list. A real machine has a few hundred.
const MAX_ENTRIES: u32 = 20_000;
/// The longest registry key name, in UTF-16 units, without the NUL.
const MAX_KEY_NAME: usize = 255;
/// The longest value read, in UTF-16 units. Paths and command lines are
/// shorter than this.
const MAX_VALUE_UNITS: usize = 32 * 1024;

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const UNINSTALL_32_BIT: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An open key, closed when dropped.
struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, subkey: &str, view: u32) -> Option<Self> {
        let name = wide(subkey);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; `key` is
        // writable.
        let status = unsafe { RegOpenKeyExW(parent, name.as_ptr(), 0, KEY_READ | view, &mut key) };
        (status == ERROR_SUCCESS).then_some(Self(key))
    }

    /// The names of the subkeys.
    fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        for index in 0..MAX_ENTRIES {
            let mut buffer = [0u16; MAX_KEY_NAME + 1];
            let mut length = buffer.len() as u32;
            // SAFETY: `buffer` is writable for `length` units; the optional
            // pointers are null.
            let status = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    buffer.as_mut_ptr(),
                    &mut length,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }
            let length = (length as usize).min(MAX_KEY_NAME);
            names.push(String::from_utf16_lossy(&buffer[..length]));
        }
        names
    }

    /// A text value. Values of the expandable kind come back expanded.
    fn string(&self, value: &str) -> Option<String> {
        let name = wide(value);
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        let mut bytes = 0u32;
        // SAFETY: `name` is NUL-terminated; a null data pointer asks for the
        // size, which is written to `bytes`.
        let status = unsafe {
            RegGetValueW(
                self.0,
                std::ptr::null(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
            return None;
        }
        let units = (bytes as usize).div_ceil(2);
        if units == 0 || units > MAX_VALUE_UNITS {
            return None;
        }
        let mut buffer = vec![0u16; units + 1];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: `buffer` is writable for `size` bytes.
        let status = unsafe {
            RegGetValueW(
                self.0,
                std::ptr::null(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let end = buffer.iter().position(|u| *u == 0).unwrap_or(buffer.len());
        Some(
            OsString::from_wide(&buffer[..end])
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn dword(&self, value: &str) -> Option<u32> {
        let name = wide(value);
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: `name` is NUL-terminated; `data` is writable for `size` bytes.
        let status = unsafe {
            RegGetValueW(
                self.0,
                std::ptr::null(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(data).cast(),
                &mut size,
            )
        };
        (status == ERROR_SUCCESS).then_some(data)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by `Key::open` and is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Reads the programs listed under `path` of `root`, in the given registry
/// `view` (0 for the default). Entries that are hidden from the installed list
/// (system components and updates), have no name, or name no install location
/// are left out.
pub(crate) fn read_installed_from(root: HKEY, path: &str, view: u32) -> Vec<Installed> {
    let Some(list) = Key::open(root, path, view) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for name in list.subkeys() {
        let Some(entry) = Key::open(list.0, &name, view) else {
            continue;
        };
        if entry.dword("SystemComponent") == Some(1) || entry.string("ParentKeyName").is_some() {
            continue;
        }
        let Some(display_name) = entry.string("DisplayName").map(|n| n.trim().to_owned()) else {
            continue;
        };
        let location = entry
            .string("InstallLocation")
            .map(|l| l.trim().to_owned())
            .unwrap_or_default();
        if display_name.is_empty() || location.is_empty() {
            continue;
        }
        found.push(Installed {
            name: display_name,
            publisher: entry.string("Publisher"),
            location,
            uninstall_string: entry.string("UninstallString"),
        });
    }
    found
}

/// Every program the system lists: the 64-bit view of the machine's list, its
/// 32-bit view, and the current user's list.
pub(crate) fn read_all() -> Vec<Installed> {
    let mut all = read_installed_from(HKEY_LOCAL_MACHINE, UNINSTALL, KEY_WOW64_64KEY);
    all.extend(read_installed_from(
        HKEY_LOCAL_MACHINE,
        UNINSTALL_32_BIT,
        KEY_WOW64_64KEY,
    ));
    all.extend(read_installed_from(HKEY_CURRENT_USER, UNINSTALL, 0));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::{
        KEY_ALL_ACCESS, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, RegCreateKeyExW,
        RegDeleteTreeW, RegSetValueExW,
    };

    /// The tests that write change the current user's registry, so they run
    /// only where the environment says the machine is disposable.
    fn disposable_host() -> bool {
        let yes = std::env::var_os("BYTESCAPE_DISPOSABLE_HOST").is_some_and(|v| v == "1");
        if !yes {
            eprintln!("skipped: BYTESCAPE_DISPOSABLE_HOST is not set to 1");
        }
        yes
    }

    /// A key under `HKEY_CURRENT_USER` removed when dropped. It is only ever
    /// made inside a folder named for this test program.
    struct Fixture {
        root: String,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = format!(r"Software\BytescapeTest-{label}-{}", std::process::id());
            let fixture = Self { root };
            let key = fixture.create("Uninstall");
            // SAFETY: closes the key opened above.
            unsafe { RegCloseKey(key) };
            fixture
        }

        fn path(&self) -> String {
            format!(r"{}\Uninstall", self.root)
        }

        fn create(&self, relative: &str) -> HKEY {
            assert!(self.root.starts_with(r"Software\BytescapeTest-"));
            let name = wide(&format!(r"{}\{relative}", self.root));
            let mut key: HKEY = std::ptr::null_mut();
            // SAFETY: `name` is NUL-terminated; `key` is writable; the other
            // pointers may be null.
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    name.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_ALL_ACCESS,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(status, ERROR_SUCCESS);
            key
        }

        fn entry(&self, id: &str, values: &[(&str, &str, bool)], dwords: &[(&str, u32)]) {
            let key = self.create(&format!(r"Uninstall\{id}"));
            for (name, text, expand) in values {
                let name = wide(name);
                let data = wide(text);
                // SAFETY: `key` is open for writing; the buffers are valid for
                // the sizes given.
                let status = unsafe {
                    RegSetValueExW(
                        key,
                        name.as_ptr(),
                        0,
                        if *expand { REG_EXPAND_SZ } else { REG_SZ },
                        data.as_ptr().cast(),
                        (data.len() * 2) as u32,
                    )
                };
                assert_eq!(status, ERROR_SUCCESS);
            }
            for (name, value) in dwords {
                let name = wide(name);
                // The data is the number itself. (A pointer to the loop's
                // reference would store the reference's address instead.)
                let data: u32 = *value;
                // SAFETY: as above, with four bytes of data.
                let status = unsafe {
                    RegSetValueExW(
                        key,
                        name.as_ptr(),
                        0,
                        REG_DWORD,
                        std::ptr::addr_of!(data).cast::<u8>(),
                        4,
                    )
                };
                assert_eq!(status, ERROR_SUCCESS);
            }
            // SAFETY: closes the key opened above.
            unsafe { RegCloseKey(key) };
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.root.starts_with(r"Software\BytescapeTest-") {
                let name = wide(&self.root);
                // SAFETY: `name` is NUL-terminated and names the key this
                // fixture made.
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, name.as_ptr()) };
            }
        }
    }

    #[test]
    fn programs_are_read_with_their_location_and_uninstaller() {
        if !disposable_host() {
            return;
        }
        let fx = Fixture::new("read");
        fx.entry(
            "Foo",
            &[
                ("DisplayName", "Foo Tools", false),
                ("Publisher", "Foo Inc", false),
                ("InstallLocation", r"C:\Program Files\Foo\", false),
                (
                    "UninstallString",
                    r#""C:\Program Files\Foo\unins000.exe" /SILENT"#,
                    false,
                ),
            ],
            &[],
        );
        fx.entry(
            "{0A1B2C3D-0000-4000-8000-0123456789AB}",
            &[
                ("DisplayName", "Msi Thing", false),
                ("InstallLocation", r"D:\Apps\Msi", false),
                (
                    "UninstallString",
                    "MsiExec.exe /X{0A1B2C3D-0000-4000-8000-0123456789AB}",
                    false,
                ),
            ],
            &[],
        );
        let found = read_installed_from(HKEY_CURRENT_USER, &fx.path(), 0);
        let find = |name: &str| {
            found
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name}: {found:?}"))
        };
        let foo = find("Foo Tools");
        assert_eq!(foo.publisher.as_deref(), Some("Foo Inc"));
        assert_eq!(foo.location, r"C:\Program Files\Foo\");
        assert_eq!(
            foo.uninstall_string.as_deref(),
            Some(r#""C:\Program Files\Foo\unins000.exe" /SILENT"#)
        );
        assert_eq!(find("Msi Thing").location, r"D:\Apps\Msi");
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn hidden_nameless_and_locationless_entries_are_left_out() {
        if !disposable_host() {
            return;
        }
        let fx = Fixture::new("skip");
        let base = [
            ("DisplayName", "Kept", false),
            ("InstallLocation", r"C:\Apps\Kept", false),
        ];
        fx.entry("kept", &base, &[]);
        fx.entry(
            "component",
            &[
                ("DisplayName", "Component", false),
                ("InstallLocation", r"C:\Apps\Component", false),
            ],
            &[("SystemComponent", 1)],
        );
        fx.entry(
            "update",
            &[
                ("DisplayName", "Update", false),
                ("InstallLocation", r"C:\Apps\Update", false),
                ("ParentKeyName", "OperatingSystem", false),
            ],
            &[],
        );
        fx.entry(
            "nameless",
            &[("InstallLocation", r"C:\Apps\Nameless", false)],
            &[],
        );
        fx.entry("no-location", &[("DisplayName", "No location", false)], &[]);
        fx.entry(
            "blank",
            &[
                ("DisplayName", "  ", false),
                ("InstallLocation", r"C:\Apps\Blank", false),
            ],
            &[],
        );
        let found = read_installed_from(HKEY_CURRENT_USER, &fx.path(), 0);
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Kept"]);
    }

    #[test]
    fn expandable_values_come_back_expanded() {
        if !disposable_host() {
            return;
        }
        let fx = Fixture::new("expand");
        fx.entry(
            "Expand",
            &[
                ("DisplayName", "Expand", false),
                ("InstallLocation", r"%SystemDrive%\Apps\Expand", true),
                (
                    "UninstallString",
                    r#""%SystemDrive%\Apps\Expand\u.exe""#,
                    true,
                ),
            ],
            &[],
        );
        let found = read_installed_from(HKEY_CURRENT_USER, &fx.path(), 0);
        let drive = std::env::var("SystemDrive").expect("SystemDrive");
        assert_eq!(found[0].location, format!(r"{drive}\Apps\Expand"));
        assert!(!found[0].uninstall_string.as_deref().unwrap().contains('%'));
    }

    #[test]
    fn a_missing_key_gives_nothing() {
        assert!(
            read_installed_from(
                HKEY_CURRENT_USER,
                r"Software\BytescapeNoSuchKey\Uninstall",
                0
            )
            .is_empty()
        );
    }

    #[test]
    fn the_machines_own_list_reads_without_trouble() {
        let all = read_all();
        for program in &all {
            assert!(!program.name.trim().is_empty());
            assert!(!program.location.trim().is_empty());
        }
        eprintln!("{} programs with an install location", all.len());
    }
}
