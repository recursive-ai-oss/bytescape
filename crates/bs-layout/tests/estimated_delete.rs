//! Delete texts when the bytes moved or deleted include an estimated size.

use bs_layout::{BinName, ItemKind, Layout, Metrics, Options, Rect, ViewTarget, layout_with};
use bs_tree::{Aggregator, Batch, DeleteState, DirKey, Meta, NodeFlags, NodeId, NodeKind};

const BIG: Rect = Rect::new(0.0, 0.0, 1000.0, 700.0);

fn m13() -> Metrics {
    Metrics::from_logical(13.0, 1.0)
}

fn opts() -> Options {
    Options {
        bin: BinName::Trash,
        ..Options::default()
    }
}

fn id(i: u32) -> NodeId {
    NodeId::from_index(i)
}

fn meta(bytes: u64, estimated: bool) -> Meta {
    Meta {
        logical: bytes,
        allocated: bytes,
        flags: if estimated {
            NodeFlags::ESTIMATED_SIZE
        } else {
            NodeFlags::EMPTY
        },
        ..Meta::default()
    }
}

/// ```text
/// 0 /root
///   1 d/
///     2 guess        40 MB, estimated
///     3 exact        10 MB
///   4 other          20 MB
///   5 solo           30 MB
/// ```
fn tree() -> Aggregator {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/root", &meta(4096, false))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"d", &meta(4096, false))
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"guess", &meta(40_000_000, true))
        .unwrap();
    b.file(
        NodeKind::File,
        DirKey(1),
        b"exact",
        &meta(10_000_000, false),
    )
    .unwrap();
    b.file(
        NodeKind::File,
        DirKey(0),
        b"other",
        &meta(20_000_000, false),
    )
    .unwrap();
    b.file(NodeKind::File, DirKey(0), b"solo", &meta(30_000_000, false))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg
}

fn text(l: &Layout, node: u32) -> String {
    l.item_for_node(id(node))
        .unwrap_or_else(|| panic!("node {node} is drawn"))
        .label
        .as_ref()
        .unwrap_or_else(|| panic!("node {node} has a label"))
        .size
        .to_string()
}

fn laid_out(agg: &mut Aggregator) -> Layout {
    layout_with(
        &agg.snapshot(),
        &ViewTarget::Folder(id(0)),
        BIG,
        m13(),
        opts(),
    )
}

#[test]
fn an_estimated_file_moved_to_the_bin_says_so_on_the_file_and_above() {
    let mut agg = tree();
    let mut l = laid_out(&mut agg);
    agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(text(&l, 2), "Moved to Trash \u{b7} \u{2248} 40.0 MB");
    assert_eq!(
        text(&l, 1),
        "\u{2248} 10.0 MB \u{b7} \u{2248} 40.0 MB moved to Trash"
    );
    assert_eq!(
        text(&l, 0),
        "\u{2248} 60.0 MB \u{b7} \u{2248} 40.0 MB moved to Trash"
    );
    // Sizes that are not involved carry no sign.
    assert_eq!(text(&l, 3), "10.0 MB");
    assert_eq!(text(&l, 4), "20.0 MB");
}

#[test]
fn an_exact_file_moved_to_the_bin_carries_no_sign_beside_an_estimate() {
    let mut agg = tree();
    let mut l = laid_out(&mut agg);
    agg.mark_deleted(id(3), DeleteState::InBin).unwrap();
    agg.mark_deleted(id(5), DeleteState::Deleted).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(text(&l, 3), "Moved to Trash \u{b7} 10.0 MB");
    // The folder still holds a guess among what remains, but the 10 MB that
    // left it were exact.
    assert_eq!(
        text(&l, 1),
        "\u{2248} 40.0 MB \u{b7} 10.0 MB moved to Trash"
    );
    // The root: what remains includes the guess; what was deleted is exact.
    assert_eq!(
        text(&l, 0),
        "\u{2248} 60.0 MB \u{b7} 10.0 MB moved to Trash \u{b7} 30.0 MB deleted"
    );
}

#[test]
fn mixed_deletes_sign_only_the_amount_that_includes_a_guess() {
    let mut agg = tree();
    let mut l = laid_out(&mut agg);
    agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
    agg.mark_deleted(id(5), DeleteState::Deleted).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(
        text(&l, 0),
        "\u{2248} 30.0 MB \u{b7} \u{2248} 40.0 MB moved to Trash \u{b7} 30.0 MB deleted"
    );
    // Moving the whole folder takes the guess along, whatever else is in it.
    agg.mark_deleted(id(1), DeleteState::Deleted).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(text(&l, 1), "Deleted \u{b7} \u{2248} 50.0 MB");
    // Undo in reverse: the signs go with the guess.
    agg.unmark_deleted(id(1)).unwrap();
    agg.unmark_deleted(id(5)).unwrap();
    agg.unmark_deleted(id(2)).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(text(&l, 0), "\u{2248} 100.0 MB");
    assert_eq!(text(&l, 1), "\u{2248} 50.0 MB");
}

#[test]
fn a_lower_bound_and_an_estimate_in_the_moved_bytes_say_both() {
    // The folder `d` could not be read, so what is moved out of it is a lower
    // bound as well as an estimate.
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/root", &meta(4096, false))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"d", &meta(4096, false))
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"guess", &meta(40_000_000, true))
        .unwrap();
    b.file(
        NodeKind::File,
        DirKey(0),
        b"other",
        &meta(20_000_000, false),
    )
    .unwrap();
    b.unreadable(
        DirKey(1),
        bs_tree::ReadFailure {
            kind: bs_tree::ReadErrorKind::PermissionDenied,
            code: 13,
        },
    );
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let mut l = laid_out(&mut agg);
    agg.mark_deleted(id(1), DeleteState::InBin).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(
        text(&l, 0),
        "\u{2265} \u{2248} 20.0 MB \u{b7} \u{2265} \u{2248} 40.0 MB moved to Trash"
    );
}

/// A big file and small files that make a group; `guess` marks which of them
/// are estimates. Node 1 is the big file, nodes 2.. are the small ones.
fn grouped(small: usize, guess: impl Fn(usize) -> bool) -> Aggregator {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/root", &meta(4096, false))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"big", &meta(80_000_000, false))
        .unwrap();
    for i in 0..small {
        b.file(
            NodeKind::File,
            DirKey(0),
            format!("s{i}").as_bytes(),
            &meta(20_000, guess(i)),
        )
        .unwrap();
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg
}

fn group_text(l: &Layout) -> String {
    l.items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .expect("a group")
        .label
        .as_ref()
        .expect("a labeled group")
        .size
        .to_string()
}

#[test]
fn a_group_signs_the_moved_amount_only_when_a_moved_member_was_a_guess() {
    // Member 2 is a guess; members 3.. are exact.
    let mut agg = grouped(300, |i| i == 0);
    let mut l = laid_out(&mut agg);
    assert!(
        group_text(&l).starts_with("\u{2248} "),
        "{}",
        group_text(&l)
    );

    // An exact member moves: the amount left still includes the guess.
    agg.mark_deleted(id(3), DeleteState::InBin).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(
        group_text(&l),
        "\u{2248} 6.0 MB \u{b7} 20.0 kB moved to Trash"
    );

    // The guess moves too: now the moved amount includes it.
    agg.mark_deleted(id(2), DeleteState::Deleted).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(
        group_text(&l),
        "6.0 MB \u{b7} 20.0 kB moved to Trash \u{b7} \u{2248} 20.0 kB deleted"
    );
}

#[test]
fn a_group_of_exact_members_never_shows_the_sign() {
    let mut agg = grouped(300, |_| false);
    let mut l = laid_out(&mut agg);
    agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
    l.refresh(&agg.snapshot());
    assert!(!group_text(&l).contains('\u{2248}'), "{}", group_text(&l));
}

#[test]
fn a_group_that_is_all_moved_signs_it_when_any_member_was_a_guess() {
    let mut agg = grouped(300, |i| i == 299);
    let mut l = laid_out(&mut agg);
    for n in 2..=301 {
        agg.mark_deleted(id(n), DeleteState::InBin).unwrap();
    }
    l.refresh(&agg.snapshot());
    assert_eq!(group_text(&l), "Moved to Trash \u{b7} \u{2248} 6.0 MB");

    let mut agg = grouped(300, |_| false);
    let mut l = laid_out(&mut agg);
    for n in 2..=301 {
        agg.mark_deleted(id(n), DeleteState::InBin).unwrap();
    }
    l.refresh(&agg.snapshot());
    assert_eq!(group_text(&l), "Moved to Trash \u{b7} 6.0 MB");
}
