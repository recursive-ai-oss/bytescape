//! Paths on the wire.
//!
//! Linux paths are arbitrary bytes and go as they are. Windows paths are
//! sequences of 16-bit units (not always valid UTF-16) and go as those units in
//! little-endian order, so the receiver rebuilds exactly the path that was
//! sent without trusting any encoding.

use std::path::{Path, PathBuf};

/// A path that cannot be rebuilt from the wire form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WirePathError {
    /// A Windows path with an odd number of bytes.
    OddLength,
}

impl std::fmt::Display for WirePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a path has an odd number of bytes")
    }
}

impl std::error::Error for WirePathError {}

/// The wire form of a path.
#[cfg(unix)]
pub fn path_to_wire(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// The path a wire form names.
#[cfg(unix)]
pub fn path_from_wire(bytes: &[u8]) -> Result<PathBuf, WirePathError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// The wire form of a path.
#[cfg(windows)]
pub fn path_to_wire(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// The path a wire form names.
#[cfg(windows)]
pub fn path_from_wire(bytes: &[u8]) -> Result<PathBuf, WirePathError> {
    use std::os::windows::ffi::OsStringExt;
    let (units, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(WirePathError::OddLength);
    }
    let wide: Vec<u16> = units.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
}

/// Whether the wire form of a path holds a NUL character.
///
/// On Linux that is a zero byte. On Windows the wire form is 16-bit units, and
/// every unit of an ordinary letter has a zero byte in it, so only a whole
/// unit of zero is a NUL.
pub fn wire_has_nul(bytes: &[u8]) -> bool {
    has_nul(bytes, cfg!(windows))
}

fn has_nul(bytes: &[u8], units: bool) -> bool {
    if units {
        bytes.chunks(2).any(|unit| unit.iter().all(|b| *b == 0))
    } else {
        bytes.contains(&0)
    }
}

#[cfg(not(any(unix, windows)))]
pub fn path_to_wire(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(any(unix, windows)))]
pub fn path_from_wire(bytes: &[u8]) -> Result<PathBuf, WirePathError> {
    Ok(PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nul_is_a_zero_byte_on_linux_and_a_zero_unit_on_windows() {
        // Linux: any zero byte.
        assert!(has_nul(b"/a\0b", false));
        assert!(!has_nul(b"/ab", false));
        assert!(!has_nul(b"", false));
        // Windows: `C:\a` as little-endian units has a zero byte in every
        // letter, and no NUL.
        let units =
            |text: &str| -> Vec<u8> { text.encode_utf16().flat_map(u16::to_le_bytes).collect() };
        assert!(!has_nul(&units(r"C:\a"), true));
        assert!(!has_nul(&units("\u{e9}\u{100}"), true), "U+0100 is 00 01");
        assert!(has_nul(&units("C:\\a\0b"), true));
        assert!(has_nul(&[0, 0], true));
        // A trailing odd byte is not a unit of zeros unless it is a zero.
        assert!(!has_nul(&[b'a', 0, b'b'], true));
        assert!(has_nul(&[b'a', 0, 0], true));
        assert!(!has_nul(b"", true));
    }

    #[test]
    fn the_answer_for_this_system_matches_the_wire_form_it_uses() {
        let wire = path_to_wire(Path::new(if cfg!(windows) { r"C:\data" } else { "/data" }));
        assert!(!wire_has_nul(&wire));
        let mut with_nul = wire.clone();
        with_nul.extend_from_slice(&[0, 0]);
        assert!(wire_has_nul(&with_nul));
    }
}
