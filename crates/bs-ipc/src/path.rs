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

#[cfg(not(any(unix, windows)))]
pub fn path_to_wire(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(any(unix, windows)))]
pub fn path_from_wire(bytes: &[u8]) -> Result<PathBuf, WirePathError> {
    Ok(PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
}
