//! The size tree: arena storage, the string pool, size aggregation,
//! completeness tracking and snapshots for the layout and the renderer.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-tree");
    }
}
