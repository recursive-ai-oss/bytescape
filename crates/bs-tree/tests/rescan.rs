//! Replacing one folder's contents in a finished tree: totals, completeness,
//! delete marks, hard links, refusals and the tree's invariants throughout.
//! The property tests compare every rescan with a full scan of the same
//! files.

use bs_tree::{
    Aggregator, ApplyError, Batch, DeleteError, DeleteState, DirKey, FileKey, Meta, NodeFlags,
    NodeId, NodeKind, ReadErrorKind, ReadFailure, RescanError, Snapshot,
};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;

// ---- a small model of a file system -----------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Item {
    File {
        logical: u64,
        allocated: u64,
        link: Option<u128>,
    },
    Dir(BTreeMap<String, Item>),
}

fn file(logical: u64, allocated: u64) -> Item {
    Item::File {
        logical,
        allocated,
        link: None,
    }
}

fn linked(logical: u64, allocated: u64, id: u128) -> Item {
    Item::File {
        logical,
        allocated,
        link: Some(id),
    }
}

fn dir(items: Vec<(&str, Item)>) -> Item {
    Item::Dir(items.into_iter().map(|(n, i)| (n.to_owned(), i)).collect())
}

fn items_of(item: &Item) -> &BTreeMap<String, Item> {
    match item {
        Item::Dir(items) => items,
        Item::File { .. } => panic!("not a directory"),
    }
}

const DIR_BYTES: u64 = 4096;

fn dir_meta() -> Meta {
    Meta {
        allocated: DIR_BYTES,
        ..Meta::default()
    }
}

/// Emits `items` as the contents of directory `key`, parents before
/// children, each directory's listing closed after its entries.
fn emit(items: &BTreeMap<String, Item>, key: DirKey, next: &mut u32, batch: &mut Batch) {
    for (name, item) in items {
        match item {
            Item::File {
                logical,
                allocated,
                link,
            } => {
                let meta = Meta {
                    logical: *logical,
                    allocated: *allocated,
                    link: link.map(|id| FileKey { volume: 1, id }),
                    // A scanner that reads link counts (the Linux walker)
                    // flags every file with more than one name, whether or
                    // not the others are in the scan.
                    flags: if link.is_some() {
                        NodeFlags::HARD_LINK
                    } else {
                        NodeFlags::EMPTY
                    },
                    ..Meta::default()
                };
                batch
                    .file(NodeKind::File, key, name.as_bytes(), &meta)
                    .unwrap();
            }
            Item::Dir(children) => {
                let child = DirKey(*next);
                *next += 1;
                batch.dir(child, key, name.as_bytes(), &dir_meta()).unwrap();
                emit(children, child, next, batch);
            }
        }
    }
    batch.listing_done(key);
}

fn scan_batch(root_name: &str, root: &BTreeMap<String, Item>) -> Batch {
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, root_name.as_bytes(), &dir_meta())
        .unwrap();
    let mut next = 1;
    emit(root, DirKey(0), &mut next, &mut batch);
    batch
}

fn tree_of(root: &Item) -> Aggregator {
    let mut agg = Aggregator::new();
    let report = agg.apply(&scan_batch("/r", items_of(root)));
    assert_eq!(report.rejected, 0, "{:?}", report.first_errors);
    assert_eq!(agg.finish().unresolved, 0);
    agg.publish();
    agg
}

/// A copy of the model with the directory at `path` holding `items`.
fn replaced(root: &Item, path: &str, items: &BTreeMap<String, Item>) -> Item {
    fn go(at: &Item, parts: &[&str], items: &BTreeMap<String, Item>) -> Item {
        match parts.split_first() {
            None => Item::Dir(items.clone()),
            Some((first, rest)) => {
                let mut copy = items_of(at).clone();
                let child = copy.get(*first).expect("path exists").clone();
                copy.insert((*first).to_owned(), go(&child, rest, items));
                Item::Dir(copy)
            }
        }
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    go(root, &parts, items)
}

fn find(snap: &Snapshot, path: &str) -> NodeId {
    let mut at = snap.root().unwrap();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        at = at
            .children()
            .find(|c| c.name() == part.as_bytes())
            .unwrap_or_else(|| panic!("no {part} in {path}"));
    }
    at.id()
}

/// The path of `id` below the root's own name.
fn rel_path(snap: &Snapshot, id: NodeId) -> String {
    let full = snap.path(id, b'/').unwrap();
    let text = String::from_utf8(full).unwrap();
    text.strip_prefix("/r")
        .unwrap_or(&text)
        .trim_start_matches('/')
        .to_owned()
}

type View = BTreeMap<String, (NodeKind, u64, u64, u32, bool)>;

/// What a reader sees of the live tree: per path, kind, sizes, file count and
/// the complete flag.
fn view(snap: &Snapshot) -> View {
    let mut out = View::new();
    let mut stack = vec![snap.root().unwrap().id()];
    while let Some(id) = stack.pop() {
        let n = snap.node(id).unwrap();
        out.insert(
            rel_path(snap, id),
            (
                n.kind(),
                n.logical(),
                n.allocated(),
                n.file_count(),
                n.is_complete(),
            ),
        );
        stack.extend(n.children().map(|c| c.id()));
    }
    out
}

/// Rescans `path` of the tree in `agg` with `fresh` as the folder's contents.
fn rescan(agg: &mut Aggregator, path: &str, fresh: &BTreeMap<String, Item>) {
    let snap = agg.publish();
    let id = find(&snap, path);
    agg.begin_rescan(id, None).unwrap();
    let report = agg.apply(&scan_batch("whatever", fresh));
    assert_eq!(report.rejected, 0, "{:?}", report.first_errors);
    assert_eq!(agg.finish().unresolved, 0);
}

fn sample() -> Item {
    dir(vec![
        (
            "a",
            dir(vec![
                ("f1", file(100, 4096)),
                ("b", dir(vec![("f2", file(10, 8192))])),
                ("c", dir(vec![])),
            ]),
        ),
        ("g", file(5, 4096)),
        ("z", dir(vec![("big", file(900, 20480))])),
    ])
}

// ---- totals ---------------------------------------------------------------------

#[test]
fn a_rescan_replaces_the_contents_and_adjusts_every_ancestor() {
    let model = sample();
    let mut agg = tree_of(&model);
    let before = agg.publish();
    let a = find(&before, "a");
    let b = find(&before, "a/b");
    let z = find(&before, "z");
    // root, a, b, c, z: five directories of 4096; files 4096 + 8192 + 4096 +
    // 20480.
    assert_eq!(before.root().unwrap().allocated(), 5 * 4096 + 36_864);
    assert_eq!(before.node(a).unwrap().allocated(), 3 * 4096 + 4096 + 8192);

    // Under `a`: one file stays, one is new and big, `b` has other files and
    // `c` is gone.
    let fresh: BTreeMap<String, Item> = [
        ("f1".to_owned(), file(100, 4096)),
        ("new".to_owned(), file(77_000, 77_824)),
        (
            "b".to_owned(),
            dir(vec![("f3", file(1, 4096)), ("f4", file(2, 4096))]),
        ),
    ]
    .into_iter()
    .collect();
    rescan(&mut agg, "a", &fresh);
    let after = agg.publish();
    after.verify().unwrap();
    assert!(after.epoch() > before.epoch());

    // The same as scanning the changed files from scratch.
    let expected = replaced(&model, "a", &fresh);
    let from_scratch = tree_of(&expected).publish();
    assert_eq!(view(&after), view(&from_scratch));
    // Spelled out for the ancestors: the root gained and lost exactly what
    // changed under `a`.
    let old_inside = 4096 + 4096 + 8192 + 4096; // f1, b, f2, c
    let new_inside = 4096 + 77_824 + 4096 + 4096 + 4096; // f1, new, b, f3, f4
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated() - old_inside + new_inside
    );
    assert_eq!(after.node(a).unwrap().allocated(), 4096 + new_inside);
    assert_eq!(after.root().unwrap().file_count(), 2 + 4);

    // The folder, its parent and its sibling are the same nodes.
    assert_eq!(find(&after, "a"), a);
    assert_eq!(find(&after, "z"), z);
    assert_eq!(after.node(a).unwrap().name(), b"a");
    assert_eq!(after.node(a).unwrap().parent().unwrap().id(), NodeId::ROOT);
    // The old contents are cut off, the new ones are other nodes.
    assert!(after.node(b).unwrap().is_detached());
    assert!(!after.node(a).unwrap().is_detached());
    assert_ne!(find(&after, "a/b"), b);
    assert_eq!(after.detached_nodes(), 4);
    assert_eq!(after.len(), before.len() + 5);
    assert!(after.root().unwrap().is_complete());
    assert!(after.scan_complete());
    let report = agg.rescan_report().unwrap();
    assert!(report.replaced && !report.refused);
    assert_eq!(report.node, a);
    assert_eq!(report.detached_nodes, 4);
    assert_eq!(report.removed.allocated, old_inside);
    assert_eq!(report.removed.files, 2);
}

#[test]
fn a_rescan_of_a_folder_that_got_smaller_takes_the_difference_out_of_every_ancestor() {
    let model = dir(vec![(
        "top",
        dir(vec![(
            "mid",
            dir(vec![("deep", dir(vec![("f", file(1, 1_000_000))]))]),
        )]),
    )]);
    let mut agg = tree_of(&model);
    let before = agg.publish();
    rescan(&mut agg, "top/mid", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    for path in ["", "top", "top/mid"] {
        let id = find(&after, path);
        let was = before.node(id).unwrap().allocated();
        assert_eq!(after.node(id).unwrap().allocated(), was - 4096 - 1_000_000);
        assert_eq!(after.node(id).unwrap().file_count(), 0, "{path}");
    }
    assert!(after.scan_complete());
}

#[test]
fn the_folders_own_entry_is_replaced_by_the_new_scan() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    let a = find(&before, "a");
    agg.begin_rescan(a, None).unwrap();
    let mut batch = Batch::new();
    let meta = Meta {
        allocated: 12_288,
        mtime: 1_234_567,
        ..Meta::default()
    };
    batch.dir(DirKey(0), DirKey::NONE, b"a", &meta).unwrap();
    batch.listing_done(DirKey(0));
    assert_eq!(agg.apply(&batch).rejected, 0);
    agg.finish();
    let after = agg.publish();
    after.verify().unwrap();
    let node = after.node(a).unwrap();
    assert_eq!(node.allocated(), 12_288);
    assert_eq!(node.mtime(), 1_234_567);
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated() - before.node(a).unwrap().allocated() + 12_288
    );
    assert_eq!(after.root().unwrap().file_count(), 2);
}

#[test]
fn the_whole_tree_can_be_rescanned_from_its_root() {
    let model = sample();
    let mut agg = tree_of(&model);
    let fresh: BTreeMap<String, Item> = [("only".to_owned(), file(3, 4096))].into_iter().collect();
    rescan(&mut agg, "", &fresh);
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(
        view(&after),
        view(&tree_of(&Item::Dir(fresh.clone())).publish())
    );
    assert_eq!(after.root().unwrap().name(), b"/r");
    assert_eq!(after.root().unwrap().id(), NodeId::ROOT);
}

#[test]
fn rescanning_the_same_folder_again_and_a_folder_made_by_a_rescan_works() {
    let model = sample();
    let mut agg = tree_of(&model);
    let one: BTreeMap<String, Item> = [(
        "d".to_owned(),
        dir(vec![("x", file(1, 4096)), ("y", file(1, 8192))]),
    )]
    .into_iter()
    .collect();
    rescan(&mut agg, "a", &one);
    let two: BTreeMap<String, Item> = [("x".to_owned(), file(2, 4096))].into_iter().collect();
    rescan(&mut agg, "a/d", &two);
    let mid = agg.publish();
    mid.verify().unwrap();
    let three: BTreeMap<String, Item> = [("last".to_owned(), file(9, 4096))].into_iter().collect();
    rescan(&mut agg, "a", &three);
    let after = agg.publish();
    after.verify().unwrap();
    let expected = replaced(&model, "a", &three);
    assert_eq!(view(&after), view(&tree_of(&expected).publish()));
    // f1, b, f2, c; then x and y; then d and x. Every cut-off node counts.
    assert_eq!(after.detached_nodes(), 4 + 2 + 2);
}

// ---- completeness ------------------------------------------------------------------

#[test]
fn the_folder_and_its_ancestors_are_incomplete_until_the_new_listing_ends() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    assert!(before.scan_complete());
    let a = find(&before, "a");
    agg.begin_rescan(a, None).unwrap();
    // Nothing happens until the scan's first record arrives.
    assert!(agg.is_rescanning());
    assert_eq!(agg.rescan_node(), Some(a));
    assert!(agg.publish().scan_complete());

    // The folder and a subfolder, neither listed yet.
    let mut first = Batch::new();
    first
        .dir(DirKey(0), DirKey::NONE, b"a", &dir_meta())
        .unwrap();
    first
        .dir(DirKey(1), DirKey(0), b"sub", &dir_meta())
        .unwrap();
    first
        .file(
            NodeKind::File,
            DirKey(1),
            b"f",
            &Meta {
                logical: 1,
                allocated: 4096,
                ..Meta::default()
            },
        )
        .unwrap();
    assert_eq!(agg.apply(&first).rejected, 0);
    let mid = agg.publish();
    mid.verify().unwrap();
    assert!(mid.node(a).unwrap().is_in_progress());
    assert!(mid.root().unwrap().is_in_progress());
    assert!(mid.root().unwrap().bytes_are_lower_bound());
    assert!(!mid.scan_complete());
    // What was not rescanned is still complete and exact.
    assert!(mid.node(find(&mid, "z")).unwrap().is_complete());
    // The old contents are already out of the totals.
    assert_eq!(mid.node(a).unwrap().allocated(), 4096 + 4096 + 4096);

    let mut rest = Batch::new();
    rest.listing_done(DirKey(1));
    assert_eq!(agg.apply(&rest).rejected, 0);
    let still = agg.publish();
    assert!(still.node(a).unwrap().is_in_progress());
    let mut last = Batch::new();
    last.listing_done(DirKey(0));
    assert_eq!(agg.apply(&last).rejected, 0);
    let done = agg.publish();
    done.verify().unwrap();
    assert!(done.node(a).unwrap().is_complete());
    assert!(done.root().unwrap().is_complete());
    assert!(done.scan_complete());
    assert!(done.epoch() > mid.epoch());
    agg.finish();
    assert!(!agg.is_rescanning());
}

#[test]
fn a_rescan_inside_an_incomplete_tree_does_not_complete_what_was_not_complete() {
    // The tree of a stopped scan: `a` is complete, the root is not.
    let mut agg = Aggregator::new();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"/r", &dir_meta())
        .unwrap();
    batch.dir(DirKey(1), DirKey(0), b"a", &dir_meta()).unwrap();
    batch.listing_done(DirKey(1));
    batch.dir(DirKey(2), DirKey(0), b"b", &dir_meta()).unwrap();
    agg.apply(&batch);
    agg.mark_stopped();
    agg.finish();
    let before = agg.publish();
    assert!(!before.root().unwrap().is_complete());
    let a = find(&before, "a");
    assert!(before.node(a).unwrap().is_complete());
    agg.begin_rescan(a, None).unwrap();
    let mut again = Batch::new();
    again
        .dir(DirKey(0), DirKey::NONE, b"a", &dir_meta())
        .unwrap();
    again.listing_done(DirKey(0));
    assert_eq!(agg.apply(&again).rejected, 0);
    agg.finish();
    let after = agg.publish();
    after.verify().unwrap();
    assert!(after.node(a).unwrap().is_complete());
    assert!(!after.root().unwrap().is_complete());
    assert!(!after.scan_complete());
    assert!(after.scan_stopped());
}

// ---- delete marks ---------------------------------------------------------------------

#[test]
fn delete_marks_outside_the_folder_survive_and_marks_inside_go_with_the_contents() {
    let model = sample();
    let mut agg = tree_of(&model);
    let snap = agg.publish();
    let b = find(&snap, "a/b");
    let big = find(&snap, "z/big");
    let c = find(&snap, "a/c");
    agg.mark_deleted(b, DeleteState::InBin).unwrap();
    agg.mark_deleted(c, DeleteState::Deleted).unwrap();
    agg.mark_deleted(big, DeleteState::InBin).unwrap();
    let marked = agg.publish();
    assert_eq!(
        marked.root().unwrap().delete_totals().in_bin.allocated,
        4096 + 8192 + 20480
    );

    let fresh: BTreeMap<String, Item> = [("f1".to_owned(), file(100, 4096))].into_iter().collect();
    rescan(&mut agg, "a", &fresh);
    let after = agg.publish();
    after.verify().unwrap();
    // `z/big` stays marked, in its own state, in every total above it.
    let z_big = after.node(big).unwrap();
    assert_eq!(z_big.delete_state(), DeleteState::InBin);
    assert_eq!(
        after.root().unwrap().delete_totals().in_bin.allocated,
        20480
    );
    assert_eq!(after.root().unwrap().delete_totals().deleted.allocated, 0);
    assert_eq!(
        after.node(find(&after, "a")).unwrap().delete_totals(),
        Default::default()
    );
    assert_eq!(agg.rescan_report().unwrap().marks_dropped, 2);
    // The old marks are gone with the nodes they were on.
    assert_eq!(
        agg.unmark_deleted(b).unwrap_err(),
        DeleteError::NotMarked(b)
    );
    assert_eq!(
        agg.unmark_deleted(c).unwrap_err(),
        DeleteError::NotMarked(c)
    );
    assert_eq!(
        agg.mark_deleted(b, DeleteState::InBin).unwrap_err(),
        DeleteError::UnknownNode(b)
    );
    // And the surviving mark undoes exactly.
    agg.unmark_deleted(big).unwrap();
    let undone = agg.publish();
    undone.verify().unwrap();
    assert_eq!(undone.root().unwrap().delete_totals(), Default::default());
    assert_eq!(
        undone.root().unwrap().current_allocated(),
        undone.root().unwrap().allocated()
    );
}

#[test]
fn nested_marks_inside_the_folder_are_all_undone() {
    let model = dir(vec![(
        "a",
        dir(vec![(
            "b",
            dir(vec![
                ("c", dir(vec![("f", file(1, 4096))])),
                ("g", file(1, 8192)),
            ]),
        )]),
    )]);
    let mut agg = tree_of(&model);
    let snap = agg.publish();
    // The inner mark first, then one over it.
    let c = find(&snap, "a/b/c");
    let b = find(&snap, "a/b");
    agg.mark_deleted(c, DeleteState::InBin).unwrap();
    agg.mark_deleted(b, DeleteState::Deleted).unwrap();
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(agg.rescan_report().unwrap().marks_dropped, 2);
    for id in [NodeId::ROOT, find(&after, "a")] {
        assert_eq!(after.node(id).unwrap().delete_totals(), Default::default());
    }
}

#[test]
fn a_marked_folder_or_one_under_a_marked_folder_is_not_rescanned() {
    let mut agg = tree_of(&sample());
    let snap = agg.publish();
    let a = find(&snap, "a");
    let b = find(&snap, "a/b");
    agg.mark_deleted(a, DeleteState::InBin).unwrap();
    assert_eq!(agg.begin_rescan(a, None), Err(RescanError::Marked(a)));
    assert_eq!(agg.begin_rescan(b, None), Err(RescanError::Marked(b)));
    assert!(!agg.is_rescanning());
    agg.unmark_deleted(a).unwrap();
    agg.begin_rescan(b, None).unwrap();
}

// ---- hard links -------------------------------------------------------------------------

/// Three folders each holding a name of one file (id 7, 8192 bytes): the
/// first, in `a`, carries the bytes.
fn links() -> Item {
    dir(vec![
        ("a", dir(vec![("x", linked(100, 8192, 7))])),
        ("b", dir(vec![("y", linked(100, 8192, 7))])),
        ("c", dir(vec![("y2", linked(100, 8192, 7))])),
    ])
}

fn carrier(snap: &Snapshot) -> Vec<String> {
    let mut names = Vec::new();
    let mut stack = vec![snap.root().unwrap().id()];
    while let Some(id) = stack.pop() {
        let n = snap.node(id).unwrap();
        if n.kind() == NodeKind::File && n.allocated() > 0 {
            names.push(rel_path(snap, id));
        }
        stack.extend(n.children().map(|c| c.id()));
    }
    names.sort();
    names
}

#[test]
fn bytes_counted_in_the_folder_pass_to_a_name_that_survives_outside_it() {
    let mut agg = tree_of(&links());
    let before = agg.publish();
    assert_eq!(carrier(&before), vec!["a/x"]);
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    // The file is still on disk under two names: counted once, at the
    // first of them.
    assert_eq!(carrier(&after), vec!["b/y"]);
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated()
    );
    assert_eq!(
        after.root().unwrap().logical(),
        before.root().unwrap().logical()
    );
    let y = after.node(find(&after, "b/y")).unwrap();
    assert!(y.is_hard_link());
    assert!(!y.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
    assert_eq!(y.hard_link_origin(), None);
    let y2 = after.node(find(&after, "c/y2")).unwrap();
    assert!(y2.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
    assert_eq!(y2.hard_link_origin(), Some(y.id()));
    assert_eq!(agg.rescan_report().unwrap().link_carriers_moved, 1);
    // The bytes are in the names' folders, not only in the root.
    assert_eq!(
        after.node(find(&after, "b")).unwrap().allocated(),
        4096 + 8192
    );
    assert_eq!(after.node(find(&after, "a")).unwrap().allocated(), 4096);
}

#[test]
fn a_name_found_again_is_matched_with_the_file_counted_elsewhere() {
    let mut agg = tree_of(&links());
    let before = agg.publish();
    rescan(&mut agg, "a", &BTreeMap::new());
    // The file comes back under `a`, and a new name appears under `c`.
    let back: BTreeMap<String, Item> = [("x".to_owned(), linked(100, 8192, 7))]
        .into_iter()
        .collect();
    rescan(&mut agg, "a", &back);
    let more: BTreeMap<String, Item> = [
        ("y2".to_owned(), linked(100, 8192, 7)),
        ("extra".to_owned(), linked(100, 8192, 7)),
    ]
    .into_iter()
    .collect();
    rescan(&mut agg, "c", &more);
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(carrier(&after), vec!["b/y"]);
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated()
    );
    for path in ["a/x", "c/y2", "c/extra"] {
        let n = after.node(find(&after, path)).unwrap();
        assert_eq!(n.allocated(), 0, "{path}");
        assert!(n.flags().contains(NodeFlags::HARD_LINK_DUPLICATE), "{path}");
        assert_eq!(n.hard_link_origin(), Some(find(&after, "b/y")), "{path}");
    }
    assert_eq!(after.root().unwrap().file_count(), 4);
}

#[test]
fn a_name_in_the_folder_whose_file_is_counted_outside_is_just_forgotten() {
    let mut agg = tree_of(&links());
    let before = agg.publish();
    let old_name = find(&before, "b/y");
    assert!(before.node(old_name).unwrap().hard_link_origin().is_some());
    rescan(&mut agg, "b", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    // Nothing keeps pointing from the cut-off name.
    assert_eq!(after.node(old_name).unwrap().hard_link_origin(), None);
    assert_eq!(carrier(&after), vec!["a/x"]);
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated()
    );
    assert_eq!(after.root().unwrap().file_count(), 2);
    assert_eq!(agg.rescan_report().unwrap().link_carriers_moved, 0);
    assert_eq!(
        after.node(find(&after, "c/y2")).unwrap().hard_link_origin(),
        Some(find(&after, "a/x"))
    );
}

#[test]
fn a_name_that_is_marked_deleted_does_not_take_the_bytes_over() {
    let mut agg = tree_of(&links());
    let snap = agg.publish();
    // `b` is in the bin; `c` is not.
    agg.mark_deleted(find(&snap, "b"), DeleteState::InBin)
        .unwrap();
    let before = agg.publish();
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(carrier(&after), vec!["c/y2"]);
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated()
    );
    // The bin's share is what it was.
    assert_eq!(
        after.root().unwrap().delete_totals(),
        before.root().unwrap().delete_totals()
    );
    assert_eq!(
        after.node(find(&after, "b/y")).unwrap().hard_link_origin(),
        Some(find(&after, "c/y2"))
    );
    // Undo still restores the bin exactly.
    agg.unmark_deleted(find(&after, "b")).unwrap();
    let undone = agg.publish();
    undone.verify().unwrap();
    assert_eq!(undone.root().unwrap().delete_totals(), Default::default());
}

#[test]
fn when_every_surviving_name_is_deleted_the_bytes_leave_the_count_with_the_folder() {
    let mut agg = tree_of(&links());
    let snap = agg.publish();
    agg.mark_deleted(find(&snap, "b"), DeleteState::InBin)
        .unwrap();
    agg.mark_deleted(find(&snap, "c"), DeleteState::Deleted)
        .unwrap();
    let before = agg.publish();
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated() - 8192
    );
    assert!(carrier(&after).is_empty());
    // The surviving names still agree on which of them is the first.
    let y = after.node(find(&after, "b/y")).unwrap();
    let y2 = after.node(find(&after, "c/y2")).unwrap();
    assert_eq!(y.hard_link_origin(), None);
    assert_eq!(y2.hard_link_origin(), Some(y.id()));
}

#[test]
fn files_with_names_outside_the_scan_stay_known_across_rescans() {
    // The scanner flagged the file as having several names, but only one of
    // them is in the tree.
    let mut agg = Aggregator::new();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"/r", &dir_meta())
        .unwrap();
    batch.dir(DirKey(1), DirKey(0), b"a", &dir_meta()).unwrap();
    batch.dir(DirKey(2), DirKey(0), b"b", &dir_meta()).unwrap();
    let single = Meta {
        logical: 10,
        allocated: 4096,
        link: Some(FileKey { volume: 1, id: 5 }),
        flags: NodeFlags::HARD_LINK,
        ..Meta::default()
    };
    batch
        .file(NodeKind::File, DirKey(1), b"only", &single)
        .unwrap();
    for k in 0..3 {
        batch.listing_done(DirKey(k));
    }
    agg.apply(&batch);
    agg.finish();
    let before = agg.publish();
    // A name for the same file appears in `b`, where it was not at first.
    let id = find(&before, "b");
    agg.begin_rescan(id, None).unwrap();
    let mut again = Batch::new();
    again
        .dir(DirKey(0), DirKey::NONE, b"b", &dir_meta())
        .unwrap();
    again
        .file(NodeKind::File, DirKey(0), b"new-name", &single)
        .unwrap();
    again.listing_done(DirKey(0));
    assert_eq!(agg.apply(&again).rejected, 0);
    agg.finish();
    let after = agg.publish();
    after.verify().unwrap();
    // Counted once, at the name in `a`.
    assert_eq!(
        after.root().unwrap().allocated(),
        before.root().unwrap().allocated()
    );
    let n = after.node(find(&after, "b/new-name")).unwrap();
    assert!(n.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
    assert_eq!(n.hard_link_origin(), Some(find(&after, "a/only")));
}

#[test]
fn shared_and_estimated_bytes_follow_a_carrier() {
    let mut agg = Aggregator::new();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"/r", &dir_meta())
        .unwrap();
    batch.dir(DirKey(1), DirKey(0), b"a", &dir_meta()).unwrap();
    batch.dir(DirKey(2), DirKey(0), b"b", &dir_meta()).unwrap();
    let meta = Meta {
        logical: 10,
        allocated: 8192,
        link: Some(FileKey { volume: 1, id: 5 }),
        flags: NodeFlags::SHARED | NodeFlags::ESTIMATED_SIZE,
        exclusive: 4096,
        ..Meta::default()
    };
    batch.file(NodeKind::File, DirKey(1), b"x", &meta).unwrap();
    batch.file(NodeKind::File, DirKey(2), b"y", &meta).unwrap();
    for k in 0..3 {
        batch.listing_done(DirKey(k));
    }
    assert_eq!(agg.apply(&batch).rejected, 0);
    agg.finish();
    let before = agg.publish();
    before.verify().unwrap();
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    let y = after.node(find(&after, "b/y")).unwrap();
    assert_eq!(y.allocated(), 8192);
    assert!(y.is_shared());
    assert_eq!(y.exclusive_allocated(), Some(4096));
    assert!(y.size_is_estimated());
    assert!(after.node(find(&after, "b")).unwrap().size_is_estimated());
    assert!(after.root().unwrap().size_is_estimated());
}

// ---- unreadable and failed -----------------------------------------------------------------

/// root / a / { bad (unreadable) }, z / { worse (unreadable) } built by hand.
fn tree_with_unreadable(z_too: bool) -> Aggregator {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/r", &dir_meta()).unwrap();
    b.dir(DirKey(1), DirKey(0), b"a", &dir_meta()).unwrap();
    b.dir(DirKey(2), DirKey(1), b"bad", &dir_meta()).unwrap();
    b.dir(DirKey(3), DirKey(0), b"z", &dir_meta()).unwrap();
    let denied = ReadFailure {
        kind: ReadErrorKind::PermissionDenied,
        code: 13,
    };
    b.unreadable(DirKey(2), denied);
    if z_too {
        b.dir(DirKey(4), DirKey(3), b"worse", &dir_meta()).unwrap();
        b.unreadable(DirKey(4), denied);
    }
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(3));
    b.listing_done(DirKey(0));
    assert_eq!(agg.apply(&b).rejected, 0);
    agg.finish();
    agg.publish();
    agg
}

#[test]
fn unreadable_below_is_worked_out_again_for_the_folders_above() {
    let mut agg = tree_with_unreadable(false);
    let before = agg.publish();
    assert!(before.root().unwrap().has_unreadable_below());
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    assert!(!after.root().unwrap().has_unreadable_below());
    assert!(
        !after
            .node(find(&after, "a"))
            .unwrap()
            .has_unreadable_below()
    );
    assert!(!after.root().unwrap().bytes_are_lower_bound());

    // With another unreadable folder elsewhere the root keeps the flag.
    let mut agg = tree_with_unreadable(true);
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    assert!(after.root().unwrap().has_unreadable_below());
    assert!(after.root().unwrap().bytes_are_lower_bound());
    assert!(
        !after
            .node(find(&after, "a"))
            .unwrap()
            .has_unreadable_below()
    );
}

#[test]
fn a_folder_that_was_unreadable_and_is_read_now_is_a_normal_folder() {
    let mut agg = tree_with_unreadable(false);
    let before = agg.publish();
    let bad = find(&before, "a/bad");
    assert!(before.node(bad).unwrap().is_unreadable());
    rescan(
        &mut agg,
        "a/bad",
        &[("f".to_owned(), file(1, 4096))].into_iter().collect(),
    );
    let after = agg.publish();
    after.verify().unwrap();
    let n = after.node(bad).unwrap();
    assert!(!n.is_unreadable());
    assert_eq!(n.read_failure(), None);
    assert!(n.is_complete());
    assert!(!after.root().unwrap().has_unreadable_below());
}

#[test]
fn a_folder_that_cannot_be_read_now_is_unreadable_with_nothing_in_it() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    let a = find(&before, "a");
    agg.begin_rescan(a, None).unwrap();
    let mut batch = Batch::new();
    let denied = ReadFailure {
        kind: ReadErrorKind::PermissionDenied,
        code: 13,
    };
    let meta = Meta {
        failure: Some(denied),
        allocated: 4096,
        ..Meta::default()
    };
    batch.dir(DirKey(0), DirKey::NONE, b"a", &meta).unwrap();
    assert_eq!(agg.apply(&batch).rejected, 0);
    agg.finish();
    let after = agg.publish();
    after.verify().unwrap();
    let n = after.node(a).unwrap();
    assert!(n.is_unreadable());
    assert_eq!(n.read_failure(), Some(denied));
    assert_eq!(n.children().count(), 0);
    assert_eq!(n.allocated(), 4096);
    assert!(n.bytes_are_lower_bound());
    assert!(after.root().unwrap().has_unreadable_below());
    assert!(after.root().unwrap().bytes_are_lower_bound());
    // Counted as listed: the rest of the tree is complete around it.
    assert!(after.scan_complete());
}

#[test]
fn a_scan_that_stops_after_the_folder_leaves_it_incomplete_and_can_mark_it_unreadable() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    let a = find(&before, "a");
    agg.begin_rescan(a, None).unwrap();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"a", &dir_meta())
        .unwrap();
    batch
        .dir(DirKey(1), DirKey(0), b"half", &dir_meta())
        .unwrap();
    assert_eq!(agg.apply(&batch).rejected, 0);
    // The scan ends here, with an error.
    agg.finish();
    let io = ReadFailure {
        kind: ReadErrorKind::Io,
        code: 5,
    };
    assert!(agg.mark_rescan_failed(io));
    // Once.
    assert!(!agg.mark_rescan_failed(io));
    let after = agg.publish();
    after.verify().unwrap();
    let n = after.node(a).unwrap();
    assert!(n.is_unreadable() && !n.is_complete());
    assert_eq!(n.read_failure(), Some(io));
    assert!(n.bytes_are_lower_bound());
    assert!(!after.root().unwrap().is_complete());
    assert!(!after.scan_complete());
    assert!(after.root().unwrap().bytes_are_lower_bound());
    // The rest of the tree is untouched and still complete.
    assert!(after.node(find(&after, "z")).unwrap().is_complete());
}

#[test]
fn a_scan_that_never_delivers_the_folder_changes_nothing() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    let a = find(&before, "a");
    agg.begin_rescan(a, None).unwrap();
    agg.finish();
    let report = agg.rescan_report().unwrap();
    assert!(!report.replaced && !report.refused);
    assert!(!agg.mark_rescan_failed(ReadFailure {
        kind: ReadErrorKind::Io,
        code: 5
    }));
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(view(&before), view(&after));
    assert_eq!(after.detached_nodes(), 0);
    assert!(after.scan_complete());
    // And the folder can be rescanned afterwards.
    rescan(&mut agg, "a", &BTreeMap::new());
    agg.publish().verify().unwrap();
}

#[test]
fn records_that_never_find_their_parent_are_lost_in_the_folder_not_the_root() {
    let mut agg = tree_of(&sample());
    let snap = agg.publish();
    let a = find(&snap, "a");
    agg.begin_rescan(a, None).unwrap();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"a", &dir_meta())
        .unwrap();
    // A file whose directory never arrives.
    batch
        .file(NodeKind::File, DirKey(99), b"orphan", &Meta::default())
        .unwrap();
    batch.listing_done(DirKey(0));
    let report = agg.apply(&batch);
    assert_eq!((report.applied, report.parked), (2, 1));
    assert_eq!(agg.finish().unresolved, 1);
    let after = agg.publish();
    after.verify().unwrap();
    assert!(after.node(a).unwrap().has_lost_records_below());
    assert!(after.root().unwrap().has_lost_records_below());
    assert!(
        !after
            .node(find(&after, "z"))
            .unwrap()
            .has_lost_records_below()
    );
    assert_eq!(after.lost_records(), 1);
}

// ---- refusals ---------------------------------------------------------------------------------

#[test]
fn a_rescan_is_refused_when_it_cannot_begin() {
    // While the first scan runs.
    let mut running = Aggregator::new();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"/r", &dir_meta())
        .unwrap();
    running.apply(&batch);
    assert_eq!(
        running.begin_rescan(NodeId::ROOT, None),
        Err(RescanError::ScanRunning)
    );

    let mut agg = tree_of(&sample());
    let snap = agg.publish();
    let f1 = find(&snap, "a/f1");
    let b = find(&snap, "a/b");
    let nowhere = NodeId::from_index(9999);
    assert_eq!(
        agg.begin_rescan(nowhere, None),
        Err(RescanError::UnknownNode(nowhere))
    );
    assert_eq!(
        agg.begin_rescan(f1, None),
        Err(RescanError::NotADirectory(f1))
    );
    assert!(!agg.is_rescanning());
    agg.begin_rescan(b, None).unwrap();
    assert_eq!(
        agg.begin_rescan(find(&snap, "z"), None),
        Err(RescanError::Busy)
    );
    agg.finish();
    // A node an earlier rescan cut off.
    rescan(&mut agg, "a", &BTreeMap::new());
    assert_eq!(agg.begin_rescan(b, None), Err(RescanError::Detached(b)));
    for e in [
        RescanError::Busy,
        RescanError::ScanRunning,
        RescanError::Detached(b),
        RescanError::Marked(b),
        RescanError::NotADirectory(b),
        RescanError::UnknownNode(b),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn a_scan_of_another_folder_is_ignored_whole_and_costs_nothing() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    let a = find(&before, "a");
    agg.begin_rescan(a, Some(b"/r/a")).unwrap();
    // A link has replaced `/r` since: the scanner reports another root.
    let mut batch = scan_batch(
        "/elsewhere/a",
        &[("evil".to_owned(), file(1, 4096))].into_iter().collect(),
    );
    batch.listing_done(DirKey(7));
    let report = agg.apply(&batch);
    assert!(report.rejected >= 3, "{report:?}");
    assert_eq!(report.applied, 0);
    assert_eq!(report.first_errors[0].1, ApplyError::RescanRootMismatch);
    // Later batches of the same scan are ignored too.
    let more = agg.apply(&scan_batch("/elsewhere/a", &BTreeMap::new()));
    assert_eq!(more.applied, 0);
    assert_eq!(agg.finish().unresolved, 0);
    let report = agg.rescan_report().unwrap();
    assert!(report.refused && !report.replaced);
    let after = agg.publish();
    after.verify().unwrap();
    assert_eq!(view(&before), view(&after));
    // Nothing was lost, so the scan still reads as complete and exact.
    assert_eq!(after.lost_records(), 0);
    assert!(after.scan_complete());
    assert!(!after.root().unwrap().has_lost_records_below());
    assert_eq!(after.detached_nodes(), 0);

    // The right name is accepted.
    agg.begin_rescan(a, Some(b"/r/a")).unwrap();
    let fresh: BTreeMap<String, Item> = BTreeMap::new();
    assert_eq!(agg.apply(&scan_batch("/r/a", &fresh)).rejected, 0);
    agg.finish();
    assert!(agg.rescan_report().unwrap().replaced);
}

#[test]
fn only_one_root_record_is_taken_and_it_must_be_a_directory() {
    let mut agg = tree_of(&sample());
    let snap = agg.publish();
    agg.begin_rescan(find(&snap, "a"), None).unwrap();
    let mut file_root = Batch::new();
    file_root
        .file(NodeKind::File, DirKey::NONE, b"a", &Meta::default())
        .unwrap();
    let report = agg.apply(&file_root);
    assert_eq!(report.first_errors[0].1, ApplyError::RootMustBeDirectory);
    let mut two = Batch::new();
    two.dir(DirKey(0), DirKey::NONE, b"a", &dir_meta()).unwrap();
    two.dir(DirKey(1), DirKey::NONE, b"a", &dir_meta()).unwrap();
    let report = agg.apply(&two);
    assert_eq!(report.first_errors[0].1, ApplyError::RootAlreadyPresent);
}

// ---- the arena -----------------------------------------------------------------------------------

#[test]
fn cut_off_nodes_are_out_of_the_tree_but_still_in_the_arena() {
    let mut agg = tree_of(&sample());
    let before = agg.publish();
    let f2 = find(&before, "a/b/f2");
    let b = find(&before, "a/b");
    rescan(&mut agg, "a", &BTreeMap::new());
    let after = agg.publish();
    after.verify().unwrap();
    // Still readable by id, as they were.
    let node = after.node(f2).unwrap();
    assert_eq!(node.name(), b"f2");
    assert_eq!(node.allocated(), 8192);
    assert!(node.is_detached());
    assert!(after.node(b).unwrap().is_detached());
    // Not part of the tree: no way to reach them from the root.
    assert_eq!(after.node(b).unwrap().parent().map(|p| p.id()), None);
    assert!(
        after
            .node(find(&after, "a"))
            .unwrap()
            .children()
            .next()
            .is_none()
    );
    assert_eq!(view(&after).len(), 5);
    // The snapshot taken before is unchanged.
    assert!(!before.node(f2).unwrap().is_detached());
    assert_eq!(before.node(b).unwrap().parent().unwrap().name(), b"a");
    // The root is never detached, and neither is anything in the tree.
    assert!(!after.root().unwrap().is_detached());
    assert!(!after.node(find(&after, "z/big")).unwrap().is_detached());
    // A delete mark on a cut-off node is refused.
    assert_eq!(
        agg.mark_deleted(f2, DeleteState::InBin).unwrap_err(),
        DeleteError::UnknownNode(f2)
    );
    // Questions about the tree do not see them.
    assert_eq!(
        after
            .large_dirs(NodeId::ROOT, 0, 10, 10)
            .unwrap()
            .dirs
            .len(),
        2
    );
    assert_eq!(after.large_dirs(b, 0, 10, 10), None);
}

#[test]
fn old_snapshots_stay_valid_while_a_rescan_runs() {
    let mut agg = tree_of(&sample());
    let reader = agg.reader();
    let before = reader.load();
    let a = find(&before, "a");
    agg.begin_rescan(a, None).unwrap();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"a", &dir_meta())
        .unwrap();
    batch.listing_done(DirKey(0));
    agg.apply(&batch);
    agg.publish();
    // A reader holding the old snapshot sees the old tree exactly.
    before.verify().unwrap();
    assert_eq!(before.node(a).unwrap().children().count(), 3);
    assert!(Arc::strong_count(&before) >= 1);
    let now = reader.load();
    assert_eq!(now.node(a).unwrap().children().count(), 0);
    assert!(now.epoch() > before.epoch());
}

// ---- properties -----------------------------------------------------------------------------------

/// A model from raw numbers: `raw[i]` is node `i + 1`, a child of one of the
/// directories made so far. Files may carry a link id; the size of a linked
/// file depends on its id alone, as it does on disk.
fn model_from_raw(raw: &[(u8, bool, u8, u8, u8)]) -> Item {
    fn insert(root: &mut BTreeMap<String, Item>, path: &[String], name: String, item: Item) {
        match path.split_first() {
            None => {
                root.insert(name, item);
            }
            Some((first, rest)) => {
                if let Some(Item::Dir(children)) = root.get_mut(first) {
                    insert(children, rest, name, item);
                }
            }
        }
    }
    let mut root = BTreeMap::new();
    let mut dirs: Vec<Vec<String>> = vec![Vec::new()];
    for (i, &(parent, is_dir, size, blocks, link)) in raw.iter().enumerate() {
        let path = dirs[usize::from(parent) % dirs.len()].clone();
        let name = format!("n{i}");
        if is_dir {
            insert(&mut root, &path, name.clone(), Item::Dir(BTreeMap::new()));
            let mut inner = path;
            inner.push(name);
            dirs.push(inner);
        } else if link % 4 == 0 {
            let id = u128::from(link / 4 % 5);
            let alloc = (id as u64 + 1) * 4096;
            insert(&mut root, &path, name, linked(alloc / 2, alloc, id));
        } else {
            insert(
                &mut root,
                &path,
                name,
                file(u64::from(size), u64::from(blocks) * 512),
            );
        }
    }
    Item::Dir(root)
}

fn dir_paths(item: &Item) -> Vec<String> {
    fn go(item: &Item, prefix: &str, out: &mut Vec<String>) {
        out.push(prefix.to_owned());
        for (name, child) in items_of(item) {
            if matches!(child, Item::Dir(_)) {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                go(child, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    go(item, "", &mut out);
    out
}

/// What the tree must count: every directory, every plain file, every linked
/// file once. And how many names there are.
fn oracle(item: &Item) -> (u64, u64, u64) {
    fn go(
        items: &BTreeMap<String, Item>,
        links: &mut BTreeMap<u128, (u64, u64)>,
        acc: &mut (u64, u64, u64),
    ) {
        for child in items.values() {
            match child {
                Item::Dir(inner) => {
                    acc.1 += DIR_BYTES;
                    go(inner, links, acc);
                }
                Item::File {
                    logical,
                    allocated,
                    link: None,
                } => {
                    acc.0 += logical;
                    acc.1 += allocated;
                    acc.2 += 1;
                }
                Item::File {
                    logical,
                    allocated,
                    link: Some(id),
                } => {
                    links.insert(*id, (*logical, *allocated));
                    acc.2 += 1;
                }
            }
        }
    }
    let mut acc = (0, DIR_BYTES, 0);
    let mut links = BTreeMap::new();
    go(items_of(item), &mut links, &mut acc);
    for (l, a) in links.values() {
        acc.0 += l;
        acc.1 += a;
    }
    acc
}

fn raw() -> impl Strategy<Value = Vec<(u8, bool, u8, u8, u8)>> {
    proptest::collection::vec(
        (
            any::<u8>(),
            proptest::bool::weighted(0.3),
            any::<u8>(),
            0u8..40,
            any::<u8>(),
        ),
        0..40,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    /// A rescan gives the tree a full scan of the same files would give, and
    /// every total is what an independent count says.
    #[test]
    fn a_rescan_equals_scanning_from_scratch(
        before in raw(),
        after in raw(),
        pick in any::<prop::sample::Index>(),
    ) {
        let model = model_from_raw(&before);
        let replacement = model_from_raw(&after);
        let paths = dir_paths(&model);
        let path = &paths[pick.index(paths.len())];
        let fresh = items_of(&replacement).clone();
        let mut agg = tree_of(&model);
        let first = agg.publish();
        let folder = find(&first, path);
        rescan(&mut agg, path, &fresh);
        let got = agg.publish();
        got.verify().unwrap();
        prop_assert_eq!(got.node(folder).unwrap().name(), first.node(folder).unwrap().name());
        let expected = replaced(&model, path, &fresh);
        let truth = oracle(&expected);
        let root = got.root().unwrap();
        prop_assert_eq!((root.logical(), root.allocated(), u64::from(root.file_count())), truth);
        prop_assert!(got.scan_complete());
        // Without hard links the trees agree path by path.
        if !format!("{model:?}{expected:?}").contains("link: Some") {
            let fresh_tree = tree_of(&expected).publish();
            prop_assert_eq!(view(&got), view(&fresh_tree));
        }
        // Whatever was cut off is not in the view.
        prop_assert_eq!(view(&got).len() as u64 + got.detached_nodes(), u64::from(got.len()));
    }

    /// Several rescans in a row keep the tree equal to a scan of the final
    /// files, and undo of marks made along the way stays exact.
    #[test]
    fn rescans_in_a_row_keep_the_tree_exact(
        start in raw(),
        steps in proptest::collection::vec((raw(), any::<prop::sample::Index>()), 1..4),
        mark in any::<prop::sample::Index>(),
    ) {
        let mut model = model_from_raw(&start);
        let mut agg = tree_of(&model);
        for (raw, pick) in steps {
            let paths = dir_paths(&model);
            let path = paths[pick.index(paths.len())].clone();
            // A mark somewhere that is not inside the folder: kept through.
            let snap = agg.publish();
            let outside: Vec<NodeId> = {
                let mut out = Vec::new();
                let mut stack = vec![snap.root().unwrap().id()];
                while let Some(id) = stack.pop() {
                    let n = snap.node(id).unwrap();
                    let p = rel_path(&snap, id);
                    let inside = path.is_empty() || p == path || p.starts_with(&format!("{path}/"));
                    let above = path == p || path.starts_with(&format!("{p}/")) || p.is_empty();
                    if !inside && !above && n.kind() == NodeKind::File && !n.is_hard_link() {
                        out.push(id);
                    }
                    stack.extend(n.children().map(|c| c.id()));
                }
                out
            };
            let marked = if outside.is_empty() { None } else {
                let id = outside[mark.index(outside.len())];
                agg.mark_deleted(id, DeleteState::InBin).ok().map(|m| (id, m))
            };
            let fresh = items_of(&model_from_raw(&raw)).clone();
            rescan(&mut agg, &path, &fresh);
            model = replaced(&model, &path, &fresh);
            let got = agg.publish();
            got.verify().unwrap();
            if let Some((id, moved)) = marked {
                prop_assert_eq!(got.root().unwrap().delete_totals().in_bin, moved.bytes);
                agg.unmark_deleted(id).unwrap();
            }
            let got = agg.publish();
            got.verify().unwrap();
            prop_assert_eq!(got.root().unwrap().delete_totals(), Default::default());
            let root = got.root().unwrap();
            prop_assert_eq!(
                (root.logical(), root.allocated(), u64::from(root.file_count())),
                oracle(&model)
            );
        }
    }
}
