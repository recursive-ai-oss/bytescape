//! Entries a reader made up, and entries that share data with others: what
//! the tree records, what it answers, and how both cross the wire.

use bs_tree::{
    Aggregator, Batch, DecodeError, DeleteState, DirKey, FileKey, Meta, NodeFlags, NodeId,
    NodeKind, Record, Snapshot,
};

fn dir(b: &mut Batch, key: u32, parent: DirKey, name: &[u8], flags: NodeFlags) {
    let meta = Meta {
        flags,
        ..Meta::default()
    };
    b.dir(DirKey(key), parent, name, &meta).unwrap();
}

fn file(b: &mut Batch, parent: u32, name: &[u8], allocated: u64, shared: Option<u64>) {
    let meta = Meta {
        logical: allocated,
        allocated,
        flags: if shared.is_some() {
            NodeFlags::SHARED
        } else {
            NodeFlags::EMPTY
        },
        exclusive: shared.unwrap_or(0),
        ..Meta::default()
    };
    b.file(NodeKind::File, DirKey(parent), name, &meta).unwrap();
}

fn find(snap: &Snapshot, name: &[u8]) -> NodeId {
    (0..snap.len())
        .map(NodeId::from_index)
        .find(|&id| snap.node(id).unwrap().name() == name)
        .unwrap()
}

fn apply(batch: &Batch) -> Snapshot {
    let mut agg = Aggregator::new();
    assert_eq!(agg.apply(batch).rejected, 0);
    agg.finish();
    agg.snapshot()
}

/// Root with a synthetic folder `fake` holding `inner/deep`, and a real
/// folder `real`.
fn made_up_folder() -> Batch {
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    dir(&mut b, 1, DirKey(0), b"fake", NodeFlags::SYNTHETIC);
    dir(&mut b, 2, DirKey(1), b"inner", NodeFlags::EMPTY);
    dir(&mut b, 3, DirKey(0), b"real", NodeFlags::EMPTY);
    file(&mut b, 2, b"deep", 4096, None);
    file(&mut b, 1, b"top", 4096, None);
    file(&mut b, 3, b"ok", 4096, None);
    for k in 0..4 {
        b.listing_done(DirKey(k));
    }
    b
}

#[test]
fn a_made_up_folder_and_everything_below_it_says_so() {
    let snap = apply(&made_up_folder());
    snap.verify().unwrap();
    let node = |name: &[u8]| snap.node(find(&snap, name)).unwrap();
    assert!(node(b"fake").is_synthetic());
    assert!(node(b"fake").is_under_synthetic());
    for below in [&b"inner"[..], b"deep", b"top"] {
        assert!(!node(below).is_synthetic(), "{below:?}");
        assert!(node(below).is_under_synthetic(), "{below:?}");
    }
    for real in [&b"root"[..], b"real", b"ok"] {
        assert!(!node(real).is_synthetic(), "{real:?}");
        assert!(!node(real).is_under_synthetic(), "{real:?}");
    }
}

#[test]
fn the_synthetic_mark_is_not_a_stored_flag_and_does_not_disturb_the_others() {
    let snap = apply(&made_up_folder());
    let fake = snap.node(find(&snap, b"fake")).unwrap();
    // The flag column has no room for it; the answer comes from the side
    // table, and the column must still hold the delete state and the kind.
    assert!(!fake.flags().contains(NodeFlags::SYNTHETIC));
    assert_eq!(fake.kind(), NodeKind::Dir);
    assert_eq!(fake.delete_state(), DeleteState::None);
    assert!(fake.is_complete());
}

#[test]
fn a_synthetic_flag_on_a_name_with_a_delete_mark_keeps_both() {
    let mut agg = Aggregator::new();
    agg.apply(&made_up_folder());
    agg.finish();
    let snap = agg.snapshot();
    let deep = find(&snap, b"deep");
    agg.mark_deleted(deep, DeleteState::InBin).unwrap();
    let snap = agg.snapshot();
    let node = snap.node(deep).unwrap();
    assert_eq!(node.delete_state(), DeleteState::InBin);
    assert!(node.is_under_synthetic());
    snap.verify().unwrap();
}

#[test]
fn nothing_is_synthetic_unless_a_reader_says_so() {
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    file(&mut b, 0, b"a", 512, None);
    b.listing_done(DirKey(0));
    let snap = apply(&b);
    for id in 0..snap.len() {
        let node = snap.node(NodeId::from_index(id)).unwrap();
        assert!(!node.is_synthetic() && !node.is_under_synthetic());
    }
}

#[test]
fn the_synthetic_mark_crosses_the_wire() {
    let mut bytes = Vec::new();
    made_up_folder().encode(&mut bytes).unwrap();
    let back = Batch::decode(&bytes).unwrap();
    let snap = apply(&back);
    assert!(snap.node(find(&snap, b"fake")).unwrap().is_synthetic());
    assert!(
        snap.node(find(&snap, b"deep"))
            .unwrap()
            .is_under_synthetic()
    );
}

/// root/{a (shared, 8192 of which 4096 are its own), b (shared, 8192, none
/// its own), c (plain, 4096), sub/d (shared, 4096, all its own)}.
fn shared_files() -> Batch {
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    dir(&mut b, 1, DirKey(0), b"sub", NodeFlags::EMPTY);
    file(&mut b, 0, b"a", 8192, Some(4096));
    file(&mut b, 0, b"b", 8192, Some(0));
    file(&mut b, 0, b"c", 4096, None);
    file(&mut b, 1, b"d", 4096, Some(4096));
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    b
}

#[test]
fn a_shared_file_keeps_the_bytes_that_are_only_its_own() {
    let snap = apply(&shared_files());
    snap.verify().unwrap();
    let node = |name: &[u8]| snap.node(find(&snap, name)).unwrap();
    assert!(node(b"a").is_shared());
    assert_eq!(node(b"a").exclusive_allocated(), Some(4096));
    assert_eq!(node(b"b").exclusive_allocated(), Some(0));
    assert_eq!(node(b"d").exclusive_allocated(), Some(4096));
    assert_eq!(node(b"c").exclusive_allocated(), None);
    assert!(!node(b"c").is_shared());
}

#[test]
fn deleting_frees_between_what_is_only_yours_and_everything_counted() {
    let snap = apply(&shared_files());
    let est = |name: &[u8]| snap.delete_would_free(find(&snap, name)).unwrap();
    // One shared file: half of it stays on disk for the other holder.
    assert_eq!((est(b"a").at_least, est(b"a").at_most), (4096, 8192));
    assert_eq!((est(b"b").at_least, est(b"b").at_most), (0, 8192));
    // A plain file frees exactly what it holds.
    assert!(est(b"c").is_exact());
    assert_eq!(est(b"c").at_most, 4096);
    // A folder adds up its shared files' own parts and its plain files.
    let root = snap.delete_would_free(NodeId::ROOT).unwrap();
    assert_eq!(root.at_most, 8192 + 8192 + 4096 + 4096);
    assert_eq!(root.at_least, 4096 + 4096 + 4096);
    let sub = est(b"sub");
    assert!(sub.is_exact());
    assert_eq!(sub.at_most, 4096);
    assert!(snap.delete_would_free(NodeId::from_index(9999)).is_none());
}

#[test]
fn a_shared_file_that_is_already_gone_is_not_counted_twice() {
    let mut agg = Aggregator::new();
    agg.apply(&shared_files());
    agg.finish();
    let snap = agg.snapshot();
    let a = find(&snap, b"a");
    agg.mark_deleted(a, DeleteState::InBin).unwrap();
    let snap = agg.snapshot();
    let root = snap.delete_would_free(NodeId::ROOT).unwrap();
    // `a` left the "still on disk" figure, so it must leave the shared part
    // too, or `at_least` would drop below what is really certain.
    assert_eq!(root.at_most, 8192 + 4096 + 4096);
    assert_eq!(root.at_least, 4096 + 4096);
    assert!(root.at_least <= root.at_most);
    snap.verify().unwrap();
}

#[test]
fn a_later_name_of_a_shared_hard_link_frees_nothing() {
    let key = bs_tree::FileKey { volume: 1, id: 7 };
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    for name in [&b"first"[..], b"second"] {
        let meta = Meta {
            logical: 4096,
            allocated: 4096,
            flags: NodeFlags::SHARED,
            exclusive: 4096,
            link: Some(key),
            ..Meta::default()
        };
        b.file(NodeKind::File, DirKey(0), name, &meta).unwrap();
    }
    b.listing_done(DirKey(0));
    let snap = apply(&b);
    snap.verify().unwrap();
    let second = snap.node(find(&snap, b"second")).unwrap();
    assert_eq!(second.allocated(), 0);
    assert_eq!(second.exclusive_allocated(), Some(0));
    let first = snap.node(find(&snap, b"first")).unwrap();
    assert_eq!(first.exclusive_allocated(), Some(4096));
}

#[test]
fn an_exclusive_figure_beyond_the_allocation_is_cut_to_it() {
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    file(&mut b, 0, b"liar", 4096, Some(u64::MAX));
    b.listing_done(DirKey(0));
    let snap = apply(&b);
    snap.verify().unwrap();
    let liar = snap.node(find(&snap, b"liar")).unwrap();
    assert_eq!(liar.exclusive_allocated(), Some(4096));
}

#[test]
fn the_exclusive_figure_crosses_the_wire_for_shared_entries_only() {
    let mut bytes = Vec::new();
    shared_files().encode(&mut bytes).unwrap();
    let back = Batch::decode(&bytes).unwrap();
    let snap = apply(&back);
    let node = |name: &[u8]| snap.node(find(&snap, name)).unwrap();
    assert_eq!(node(b"a").exclusive_allocated(), Some(4096));
    assert_eq!(node(b"c").exclusive_allocated(), None);
    // An entry that is not shared costs no bytes for the figure.
    let mut plain = Vec::new();
    let mut b = Batch::new();
    file(&mut b, 0, b"c", 4096, None);
    b.encode(&mut plain).unwrap();
    let mut shared = Vec::new();
    let mut b = Batch::new();
    file(&mut b, 0, b"c", 4096, Some(1));
    b.encode(&mut shared).unwrap();
    assert_eq!(shared.len(), plain.len() + 8);
}

#[test]
fn a_figure_without_the_flag_on_the_wire_is_ignored() {
    // A hostile peer sets the option bit but not the flag: the value is read
    // (so the record stays well formed) and then dropped.
    let mut b = Batch::new();
    file(&mut b, 0, b"c", 4096, Some(77));
    let mut bytes = Vec::new();
    b.encode(&mut bytes).unwrap();
    // Header 16, names 1, tag, kind, flags (2).
    let flags_at = 16 + 1 + 1 + 1;
    let mut flags = u16::from_le_bytes([bytes[flags_at], bytes[flags_at + 1]]);
    flags &= !NodeFlags::SHARED.bits();
    bytes[flags_at..flags_at + 2].copy_from_slice(&flags.to_le_bytes());
    let back = Batch::decode(&bytes).unwrap();
    let Record::Entry(e) = back.records()[0] else {
        panic!("entry expected");
    };
    assert_eq!(e.meta.exclusive, 0);
    assert!(!e.meta.flags.contains(NodeFlags::SHARED));
}

#[test]
fn unknown_option_bits_are_still_refused() {
    let mut b = Batch::new();
    file(&mut b, 0, b"c", 4096, None);
    let mut bytes = Vec::new();
    b.encode(&mut bytes).unwrap();
    // Header 16, names 1, tag, kind, flags (2), category, options.
    let opt_at = 16 + 1 + 1 + 1 + 2 + 1;
    bytes[opt_at] = 8;
    assert_eq!(Batch::decode(&bytes), Err(DecodeError::BadValue));
}

#[test]
fn every_truncation_of_a_shared_record_is_refused() {
    let mut bytes = Vec::new();
    shared_files().encode(&mut bytes).unwrap();
    for cut in 0..bytes.len() {
        assert!(Batch::decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
}

/// A file with two names (a keyed pair) and one flagged by the scanner whose
/// other names lie outside the scanned area, next to a plain file.
fn hard_links() -> Batch {
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    dir(&mut b, 1, DirKey(0), b"inside", NodeFlags::EMPTY);
    let linked = |id: u128, flags: NodeFlags| Meta {
        logical: 1 << 20,
        allocated: 1 << 20,
        flags,
        link: Some(FileKey { volume: 1, id }),
        ..Meta::default()
    };
    b.file(
        NodeKind::File,
        DirKey(0),
        b"a",
        &linked(42, NodeFlags::EMPTY),
    )
    .unwrap();
    b.file(
        NodeKind::File,
        DirKey(0),
        b"b",
        &linked(42, NodeFlags::EMPTY),
    )
    .unwrap();
    // Seen once, but the scanner knows it has a name elsewhere.
    b.file(
        NodeKind::File,
        DirKey(1),
        b"c",
        &linked(43, NodeFlags::HARD_LINK),
    )
    .unwrap();
    file(&mut b, 1, b"plain", 4096, None);
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    b
}

#[test]
fn a_hard_linked_file_is_not_counted_as_certainly_freed() {
    let snap = apply(&hard_links());
    snap.verify().unwrap();
    let est = |name: &[u8]| snap.delete_would_free(find(&snap, name)).unwrap();
    // Two names of one file: deleting one name frees nothing, whichever
    // of the two carries the bytes.
    assert_eq!((est(b"a").at_least, est(b"a").at_most), (0, 1 << 20));
    assert_eq!((est(b"b").at_least, est(b"b").at_most), (0, 0));
    // One seen name that the scanner flagged: the others are outside.
    assert_eq!((est(b"c").at_least, est(b"c").at_most), (0, 1 << 20));
    // A folder frees its plain files for certain and may free the linked one.
    let inside = est(b"inside");
    assert_eq!((inside.at_least, inside.at_most), (4096, 4096 + (1 << 20)));
    let root = snap.delete_would_free(NodeId::ROOT).unwrap();
    assert_eq!(root.at_most, 2 * (1 << 20) + 4096);
    assert_eq!(root.at_least, 4096);
}

#[test]
fn a_hard_linked_file_in_the_bin_is_not_subtracted_again() {
    let mut agg = Aggregator::new();
    agg.apply(&hard_links());
    agg.finish();
    let a = find(&agg.snapshot(), b"a");
    agg.mark_deleted(a, DeleteState::InBin).unwrap();
    let snap = agg.snapshot();
    let root = snap.delete_would_free(NodeId::ROOT).unwrap();
    assert_eq!(root.at_most, (1 << 20) + 4096);
    assert_eq!(root.at_least, 4096);
    snap.verify().unwrap();
}

#[test]
fn a_file_both_shared_and_hard_linked_is_left_out_once() {
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"root", NodeFlags::EMPTY);
    let m = Meta {
        logical: 8192,
        allocated: 8192,
        flags: NodeFlags::SHARED | NodeFlags::HARD_LINK,
        exclusive: 4096,
        link: Some(FileKey { volume: 1, id: 7 }),
        ..Meta::default()
    };
    b.file(NodeKind::File, DirKey(0), b"x", &m).unwrap();
    file(&mut b, 0, b"y", 4096, None);
    b.listing_done(DirKey(0));
    let snap = apply(&b);
    let root = snap.delete_would_free(NodeId::ROOT).unwrap();
    assert_eq!(root.at_most, 8192 + 4096);
    assert_eq!(root.at_least, 4096);
}
