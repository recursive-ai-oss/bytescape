//! The XFS reader: sizes from the bulk-stat ioctl, names from a directory
//! walk. The crate is empty on other systems. It reads untrusted disk data, so
//! the panicking shortcuts are denied.
#![cfg(target_os = "linux")]
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
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-scan-xfs");
    }
}
