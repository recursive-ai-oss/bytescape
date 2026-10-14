//! The Bytescape application. This is the user interface process: it runs as
//! the normal user and never with elevated rights. Work that needs
//! administrator rights goes to the separate bytescape-helper program.

fn main() {}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bytescape");
    }

    #[test]
    fn build_metadata_is_exported() {
        assert_eq!(env!("BYTESCAPE_VERSION"), env!("CARGO_PKG_VERSION"));
        assert!(!env!("BYTESCAPE_BUILD_DATE").is_empty());
        assert!(!env!("BYTESCAPE_TARGET").is_empty());
    }
}
