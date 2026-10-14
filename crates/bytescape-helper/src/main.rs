//! The elevated helper. It starts only when the user asks for a full scan or
//! an elevated delete, accepts requests only from the interface that launched
//! it, reads, deletes only the paths the interface lists, never starts other
//! programs, and exits when its work is done. It reads untrusted disk data and
//! sits on the privilege boundary, so the panicking shortcuts are denied in
//! this crate.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

fn main() {}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bytescape-helper");
    }

    #[test]
    fn build_metadata_is_exported() {
        assert_eq!(env!("BYTESCAPE_VERSION"), env!("CARGO_PKG_VERSION"));
        assert!(!env!("BYTESCAPE_BUILD_DATE").is_empty());
        assert!(!env!("BYTESCAPE_TARGET").is_empty());
    }
}
