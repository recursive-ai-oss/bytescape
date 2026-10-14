//! A directory replaced by a link between being listed and being opened must
//! not be followed. The file system is changed at exactly that moment, through
//! a hook, so the test does not depend on winning a race. One test per file
//! because the hook is process-wide.
#![cfg(target_os = "linux")]

mod support;

use bs_tree::{NodeFlags, NodeKind, ReadErrorKind};
use std::os::unix::fs::symlink;
use support::*;

#[test]
fn a_directory_swapped_for_a_link_before_it_is_opened_is_refused() {
    let fx = Fixture::new("swap-exact");
    let outside = Fixture::new("swap-exact-outside");
    // Following the link would add a terabyte (logical).
    std::fs::File::create(outside.path("trap.bin"))
        .unwrap()
        .set_len(1 << 40)
        .unwrap();
    fx.file("victim/inner.txt", 100);
    fx.file("bystander/ok.txt", 200);
    fx.settle();

    let root = fx.root.clone();
    let target = outside.root.clone();
    bs_scan::set_before_open_hook(Some(Box::new(move |name| {
        if name == b"victim" {
            std::fs::rename(root.join("victim"), root.join("victim.old")).unwrap();
            symlink(&target, root.join("victim")).unwrap();
        }
    })));
    let result = scan(&fx.root);
    bs_scan::set_before_open_hook(None);

    result.snapshot.verify().unwrap();
    let snap = &result.snapshot;
    assert!(snap.scan_complete());
    assert!(
        snap.root().unwrap().logical() < (1 << 30),
        "the link was followed"
    );
    // It was listed as a directory, could not be opened as one, and says so.
    let victim = node_at(snap, "victim");
    assert_eq!(victim.kind(), NodeKind::Dir);
    assert!(victim.is_unreadable());
    assert_eq!(victim.read_failure().unwrap().kind, ReadErrorKind::NotFound);
    assert_eq!(victim.children().count(), 0);
    assert!(snap.root().unwrap().bytes_are_lower_bound());
    // The renamed original is simply a new directory, found in this listing
    // or not, and the rest of the tree is intact.
    assert_eq!(node_at(snap, "bystander/ok.txt").logical(), 200);
    assert!(
        !node_at(snap, "bystander")
            .flags()
            .contains(NodeFlags::NOT_FOLLOWED)
    );
}
