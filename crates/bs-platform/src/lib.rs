//! Platform services: the Recycle Bin and Trash, revealing files in the file
//! manager, terminals, drive lists, installed-program and package lookup,
//! launching the elevated helper and the light and dark setting.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-platform");
    }
}
