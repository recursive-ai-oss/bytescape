// The text of a Windows resource script (icon, version information, manifest).
// Included by windows-resources.rs, which compiles it, and by the xtask tests,
// which check it; it needs no crates, so both can use it.

/// Language 0409 (English, United States) and code page 04b0 (Unicode), as the
/// two halves of the string table's block name.
const LANG_ENGLISH_US: &str = "0409";
const CODEPAGE_UNICODE: &str = "04b0";

/// The text of a resource script. `icon` and `manifest` are absolute paths.
pub fn resource_script(
    version: &str,
    file_name: &str,
    description: &str,
    icon: Option<&str>,
    manifest: &str,
) -> String {
    let mut numbers = version
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .chain(std::iter::repeat(0));
    let (major, minor, patch) = (
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
    );
    // Paths are written with forward slashes: a resource script treats a
    // backslash as the start of an escape.
    let path = |text: &str| text.replace('\\', "/");
    let mut script = String::new();
    script.push_str("#include <winver.h>\n\n");
    if let Some(icon) = icon {
        script.push_str(&format!("1 ICON \"{}\"\n", path(icon)));
    }
    // 24 is RT_MANIFEST; resource id 1 is the one Windows reads for a program.
    script.push_str(&format!("1 24 \"{}\"\n\n", path(manifest)));
    script.push_str(&format!(
        "VS_VERSION_INFO VERSIONINFO\n FILEVERSION {major},{minor},{patch},0\n PRODUCTVERSION {major},{minor},{patch},0\n FILEFLAGSMASK 0x3fL\n FILEFLAGS 0x0L\n FILEOS VOS_NT_WINDOWS32\n FILETYPE VFT_APP\n FILESUBTYPE VFT2_UNKNOWN\nBEGIN\n  BLOCK \"StringFileInfo\"\n  BEGIN\n    BLOCK \"{LANG_ENGLISH_US}{CODEPAGE_UNICODE}\"\n    BEGIN\n      VALUE \"CompanyName\", \"Recursive AI, LLC\\0\"\n      VALUE \"FileDescription\", \"{description}\\0\"\n      VALUE \"FileVersion\", \"{version}\\0\"\n      VALUE \"InternalName\", \"{file_name}\\0\"\n      VALUE \"LegalCopyright\", \"Copyright (c) Recursive AI, LLC. MIT OR Apache-2.0.\\0\"\n      VALUE \"OriginalFilename\", \"{file_name}.exe\\0\"\n      VALUE \"ProductName\", \"Bytescape\\0\"\n      VALUE \"ProductVersion\", \"{version}\\0\"\n    END\n  END\n  BLOCK \"VarFileInfo\"\n  BEGIN\n    VALUE \"Translation\", 0x409, 1200\n  END\nEND\n"
    ));
    script
}

/// Whether the Windows resources are to be skipped. `skip` is the value of
/// `BYTESCAPE_SKIP_WINRES`, `in_ci` says the build runs in CI and `release`
/// says it is a release-profile build. The skip is
/// for a developer who only wants `cargo clippy` or `cargo check` for a
/// Windows target on a machine with no resource compiler: the result is a
/// program without icon, version information or administrator manifest, so
/// it must never be built in CI or released. Unset (or empty) means build the
/// resources, and a missing resource compiler fails the build.
pub fn winres_skip(skip: Option<&str>, in_ci: bool, release: bool) -> Result<bool, String> {
    match skip {
        None | Some("") => Ok(false),
        Some("1") if in_ci => Err(String::from(
            "BYTESCAPE_SKIP_WINRES is set in CI: a build without the Windows resources must never be built there",
        )),
        Some("1") if release => Err(String::from(
            "BYTESCAPE_SKIP_WINRES is set for a release build: a release program must have its Windows resources",
        )),
        Some("1") => Ok(true),
        Some(other) => Err(format!(
            "BYTESCAPE_SKIP_WINRES must be 1 or unset, not {other:?}"
        )),
    }
}
