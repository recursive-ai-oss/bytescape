//! The scanner interface and the parallel directory walker that works on every
//! platform and every filesystem.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-scan");
    }
}
