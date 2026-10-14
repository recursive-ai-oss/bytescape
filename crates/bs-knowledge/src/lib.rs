//! The category table, the recognition notes and the protected-path table,
//! compiled in from data files.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-knowledge");
    }
}
