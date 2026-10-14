//! The versioned, length-prefixed binary protocol between the interface and
//! the elevated helper. It sits on the privilege boundary and parses data from
//! another process, so the panicking shortcuts and unchecked arithmetic and casts are denied.
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
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-ipc");
    }
}
