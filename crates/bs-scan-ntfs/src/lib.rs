//! The NTFS master file table reader, used by the elevated helper on Windows.
//! The crate is empty on other systems. It reads untrusted disk data, so the
//! panicking shortcuts are denied.
#![cfg(windows)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-scan-ntfs");
    }
}
