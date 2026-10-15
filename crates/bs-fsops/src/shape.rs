//! The shape of a path, checked before anything is looked up.

use std::path::Path;

/// The longest path accepted, in bytes (or UTF-16 units times two on
/// Windows). Longer paths are refused rather than cut.
pub const MAX_PATH_BYTES: usize = 32 * 1024;

/// What is wrong with a path before anything is looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadPath {
    Empty,
    NotAbsolute,
    /// A `.` or `..` part, or a NUL.
    NotCanonical,
    TooLong,
    /// The path has no last name (a root).
    NoName,
}

/// Checks the shape of a path before anything is looked up.
pub fn validate_path(path: &Path) -> Result<(), BadPath> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() {
        return Err(BadPath::Empty);
    }
    if bytes.len() > MAX_PATH_BYTES {
        return Err(BadPath::TooLong);
    }
    if !path.is_absolute() || (cfg!(windows) && path.has_root() && !is_drive_or_unc(bytes)) {
        return Err(BadPath::NotAbsolute);
    }
    if bytes.contains(&0) {
        return Err(BadPath::NotCanonical);
    }
    let separator = |b: u8| b == b'/' || (cfg!(windows) && b == b'\\');
    if bytes
        .split(|&b| separator(b))
        .any(|part| part == b"." || part == b"..")
    {
        return Err(BadPath::NotCanonical);
    }
    if path.file_name().is_none() {
        return Err(BadPath::NoName);
    }
    Ok(())
}

/// A Windows path that starts with a drive letter, or `\\` (UNC, `\\?\`).
fn is_drive_or_unc(bytes: &[u8]) -> bool {
    bytes.starts_with(br"\\") || (bytes.len() >= 3 && bytes.get(1) == Some(&b':'))
}
