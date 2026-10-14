//! Behavior of the layout on small, hand-made trees.

mod common;

use bs_layout::{
    BinName, CategoryId, GroupSpec, ItemKind, ItemRef, Metrics, Options, Rect, StateFlags,
    Thresholds, ViewTable, ViewTarget, content_rect, layout, layout_with, padding, title_height,
};
use bs_tree::{Aggregator, DeleteState, NodeId};
use common::*;
use std::sync::Arc;

fn m13(scale: f32) -> Metrics {
    Metrics::from_logical(13.0, scale)
}

const BIG: Rect = Rect::new(0.0, 0.0, 1000.0, 700.0);

fn folder(i: usize) -> ViewTarget {
    ViewTarget::Folder(id(i))
}

/// root / dir(1) / file(2): the directory is the root's only child, so it
/// fills the root's content area exactly.
fn dir_in_root() -> Spec {
    let mut s = Spec::new();
    let d = s.dir(0);
    s.file(d, 1 << 20);
    s
}

/// root / file(1)
fn file_in_root() -> Spec {
    let mut s = Spec::new();
    s.file(0, 1 << 20);
    s
}

#[test]
fn the_view_root_fills_the_viewport_and_comes_first() {
    let snap = build(&dir_in_root());
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let root = &l.items()[0];
    assert_eq!(root.rect, BIG);
    assert_eq!(root.kind, ItemKind::FolderTitled);
    assert_eq!(root.depth, 0);
    assert_eq!(root.id, 0);
    let label = root.label.as_ref().unwrap();
    assert_eq!(&*label.name, "/root");
    check_invariants(&snap, &l).unwrap();
}

/// The widths and heights to probe: both sides of every threshold.
fn probe_sizes(t: &Thresholds) -> Vec<i32> {
    let mut v = Vec::new();
    for edge in [t.nest_w, t.nest_h, t.label_w, t.label_h, t.min_tile] {
        for d in [-1i32, 0, 1] {
            v.push(edge.ceil() as i32 + d);
        }
    }
    v.retain(|&n| n >= 1);
    v.sort_unstable();
    v.dedup();
    v
}

#[test]
fn folder_thresholds_follow_the_font_at_every_scale() {
    let snap = build(&dir_in_root());
    for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
        let m = m13(scale);
        let t = Thresholds::new(m);
        for &w in &probe_sizes(&t) {
            for &h in &probe_sizes(&t) {
                let vp = Rect::new(7.0, 9.0, w as f32, h as f32);
                let l = layout(&snap, &folder(0), vp, m);
                let (wf, hf) = (w as f32, h as f32);
                if wf < 2.0 || hf < 2.0 {
                    assert!(l.items().is_empty(), "scale {scale}: {w}x{h} drawn");
                    continue;
                }
                let it = &l.items()[0];
                assert_eq!(it.rect, vp);
                let nest = wf >= 5.0 * m.em && hf >= 3.2 * m.em;
                let label = wf >= 3.5 * m.em && hf >= 1.6 * m.em;
                let want = if nest {
                    ItemKind::FolderTitled
                } else {
                    ItemKind::FolderBlock
                };
                assert_eq!(it.kind, want, "scale {scale}: {w}x{h}");
                assert_eq!(it.label.is_some(), nest || label, "scale {scale}: {w}x{h}");
                check_invariants(&snap, &l).unwrap();
            }
        }
    }
}

#[test]
fn file_thresholds_follow_the_font_at_every_scale() {
    let snap = build(&file_in_root());
    for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
        let m = m13(scale);
        let t = Thresholds::new(m);
        for &w in &probe_sizes(&t) {
            for &h in &probe_sizes(&t) {
                let vp = Rect::new(0.0, 0.0, w as f32, h as f32);
                // A file as the view's own box is a plain tile.
                let l = layout(&snap, &folder(1), vp, m);
                let (wf, hf) = (w as f32, h as f32);
                if wf < 2.0 || hf < 2.0 {
                    assert!(l.items().is_empty());
                    continue;
                }
                let it = &l.items()[0];
                assert_eq!(it.kind, ItemKind::FileTile);
                assert_eq!(
                    it.label.is_some(),
                    wf >= 3.5 * m.em && hf >= 1.6 * m.em,
                    "scale {scale}: {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn the_title_band_and_padding_frame_the_content_area() {
    let snap = build(&dir_in_root());
    for scale in [1.0f32, 1.5, 2.0] {
        let m = m13(scale);
        let l = layout(&snap, &folder(0), BIG, m);
        let root = l.items()[0].rect;
        let c = content_rect(root, m);
        let pad = padding(m);
        assert_eq!(c.x, root.x + 1.0 + pad);
        assert_eq!(c.y, root.y + 1.0 + title_height(m) + pad);
        assert_eq!(c.right(), root.right() - 1.0 - pad);
        assert_eq!(c.bottom(), root.bottom() - 1.0 - pad);
        // The only child fills it.
        assert_eq!(l.items()[1].rect, c);
    }
    // Too small for any content: an empty rectangle, not a negative one.
    let c = content_rect(Rect::new(0.0, 0.0, 10.0, 10.0), m13(1.0));
    assert!(c.w == 0.0 && c.h == 0.0);
}

#[test]
fn thresholds_in_em_scale_with_the_font() {
    let t13 = Thresholds::new(Metrics {
        em: 13.0,
        scale: 1.0,
    });
    let t26 = Thresholds::new(Metrics {
        em: 26.0,
        scale: 2.0,
    });
    assert_eq!(t13.nest_w, 65.0);
    assert_eq!(t26.nest_w, 130.0);
    assert_eq!(t13.group_side, 32.5);
    assert_eq!(t26.min_tile, 2.0);
    assert_eq!(
        title_height(Metrics {
            em: 13.0,
            scale: 1.0
        }),
        21.0
    );
    assert_eq!(
        padding(Metrics {
            em: 13.0,
            scale: 1.0
        }),
        2.0
    );
    assert_eq!(
        padding(Metrics {
            em: 26.0,
            scale: 2.0
        }),
        4.0
    );
}

#[test]
fn equal_siblings_tile_their_parent_exactly() {
    let mut s = Spec::new();
    for _ in 0..6 {
        s.file(0, 4096);
    }
    let snap = build(&s);
    let m = m13(1.0);
    let l = layout(&snap, &folder(0), BIG, m);
    let content = content_rect(l.items()[0].rect, m);
    assert_eq!(l.items().len(), 7);
    let covered: f64 = l.items()[1..].iter().map(|i| i.rect.area()).sum();
    assert_eq!(covered, content.area());
    check_invariants(&snap, &l).unwrap();
}

#[test]
fn bigger_siblings_come_first_and_ties_follow_node_ids() {
    let mut s = Spec::new();
    s.file(0, 4096); // 1
    s.file(0, 8192); // 2
    s.file(0, 4096); // 3
    s.file(0, 1 << 20); // 4
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let order: Vec<u32> = l.items()[1..].iter().map(|i| i.id).collect();
    assert_eq!(order, vec![4, 2, 1, 3]);
}

#[test]
fn a_zero_byte_entry_has_no_box_and_no_effect_on_the_others() {
    let mut with = Spec::new();
    let mut without = Spec::new();
    for s in [&mut with, &mut without] {
        s.file(0, 4096);
        s.file(0, 8192);
    }
    with.file(0, 0);
    with.dir(0); // an empty directory still has its own 4096 bytes
    let a = layout(&build(&with), &folder(0), BIG, m13(1.0));
    let b = layout(&build(&without), &folder(0), BIG, m13(1.0));
    assert!(a.item_for_node(id(3)).is_none());
    assert!(a.item_for_node(id(4)).is_some());
    assert!(b.items().len() < a.items().len());
}

#[test]
fn few_small_siblings_are_drawn_one_by_one() {
    let mut s = Spec::new();
    s.file(0, 100 << 20);
    s.file(0, 4096);
    s.file(0, 4096);
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    assert!(l.items().iter().all(|i| i.kind != ItemKind::Group));
}

/// A big file and `members` equal small files, sized so the small ones are
/// 10% of the bytes in total and each is far below the group threshold.
fn big_and_small(members: usize, cat: impl Fn(usize) -> u8) -> Spec {
    let mut s = Spec::new();
    s.file(0, members as u64 * 4096 * 9);
    for i in 0..members {
        s.add(SpecNode::file(0, 4096).cat(cat(i)));
    }
    s
}

#[test]
fn small_siblings_merge_into_a_group_with_addressable_members() {
    let snap = build(&big_and_small(100, |_| 3));
    let m = m13(1.0);
    let l = layout(&snap, &folder(0), BIG, m);
    let groups: Vec<_> = l
        .items()
        .iter()
        .filter(|i| i.kind == ItemKind::Group)
        .collect();
    assert_eq!(groups.len(), 1);
    let g = groups[0];
    // The big file and the group; none of the hundred files on its own.
    assert_eq!(l.items().len(), 3);
    assert_eq!(g.bytes, 100 * 4096);
    assert!(g.id >= snap.len());
    let Some(ItemRef::Group(spec)) = l.item_ref(g.id) else {
        panic!("group id does not resolve");
    };
    assert_eq!(spec.folder, id(0));
    assert_eq!(spec.members.len(), 100);
    // Dominant category: the tree's 3 is palette entry 2.
    assert_eq!(g.category, CategoryId(2));
    // Zooming into the group lays out the members.
    let (_, target) = l
        .enterable()
        .iter()
        .find(|(item, _)| *item == g.id)
        .expect("the group is enterable");
    let inside = layout(&snap, target, BIG, m);
    assert_eq!(inside.items()[0].kind, ItemKind::FolderTitled);
    assert_eq!(inside.items()[0].id, snap.len());
    assert_eq!(inside.items().len(), 101);
    assert!(
        inside.items()[1..]
            .iter()
            .all(|i| i.kind == ItemKind::FileTile)
    );
    check_invariants(&snap, &inside).unwrap();
}

#[test]
fn group_labels_count_members_and_say_files_or_items() {
    let snap = build(&big_and_small(1200, |_| 0));
    let big = Rect::new(0.0, 0.0, 1600.0, 1200.0);
    let l = layout(&snap, &folder(0), big, m13(1.0));
    let g = l
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap();
    let label = g.label.as_ref().unwrap();
    assert_eq!(&*label.name, "1,200 small files");
    assert_eq!(&*label.size, "4.9 MB");

    // With a folder among the members the group says "items".
    let mut s2 = big_and_small(199, |_| 0);
    s2.add(SpecNode::dir(0));
    let l2 = layout(&build(&s2), &folder(0), big, m13(1.0));
    let g2 = l2
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap();
    assert_eq!(&*g2.label.as_ref().unwrap().name, "200 small items");
}

#[test]
fn a_group_takes_a_category_only_when_it_holds_more_than_sixty_percent() {
    let dominant = |first: usize, a: u8, b: u8| {
        let snap = build(&big_and_small(100, |i| if i < first { a } else { b }));
        let l = layout(&snap, &folder(0), BIG, m13(1.0));
        l.items()
            .iter()
            .find(|i| i.kind == ItemKind::Group)
            .map(|g| g.category)
            .unwrap()
    };
    // Exactly 60% is not more than 60%.
    assert_eq!(dominant(60, 1, 2), CategoryId::NEUTRAL);
    assert_eq!(dominant(61, 1, 2), CategoryId(0));
    assert_eq!(dominant(100, 0, 0), CategoryId::OTHER);
    // Unknown tree categories count as Other.
    assert_eq!(dominant(100, 200, 200), CategoryId::OTHER);
}

#[test]
fn a_group_view_does_not_regroup_its_own_members() {
    let mut s = Spec::new();
    for _ in 0..100 {
        s.file(0, 4096);
    }
    let snap = build(&s);
    let m = m13(1.0);
    let small = Rect::new(0.0, 0.0, 80.0, 80.0);
    // In the folder view all one hundred are small: one group fills the box.
    let l = layout(&snap, &folder(0), small, m);
    assert_eq!(
        l.items()
            .iter()
            .filter(|i| i.kind == ItemKind::Group)
            .count(),
        1
    );
    // Entering shows the members themselves, in the same size of window.
    let (_, target) = l.enterable().first().unwrap();
    let inside = layout(&snap, target, small, m);
    assert!(inside.items().iter().all(|i| i.kind != ItemKind::Group));
    assert!(inside.items().len() > 50);
    check_invariants(&snap, &inside).unwrap();
}

#[test]
fn many_equal_tiny_boxes_stop_at_the_item_limit() {
    let mut s = Spec::new();
    for _ in 0..4000 {
        s.file(0, 4096);
    }
    let snap = build(&s);
    let m = m13(1.0);
    let small = Rect::new(0.0, 0.0, 400.0, 400.0);
    let l = layout(&snap, &folder(0), small, m);
    let (_, target) = l.enterable().first().unwrap();
    let opts = Options {
        max_items: 50,
        ..Options::default()
    };
    let inside = layout_with(&snap, target, Rect::new(0.0, 0.0, 800.0, 800.0), m, opts);
    assert!(inside.is_truncated());
    assert_eq!(inside.items().len(), 50);
    let full = layout(&snap, target, Rect::new(0.0, 0.0, 800.0, 800.0), m);
    assert!(!full.is_truncated());
}

#[test]
fn nested_folders_nest_until_the_box_is_too_small() {
    let mut s = Spec::new();
    let mut parent = 0;
    for _ in 0..4 {
        parent = s.dir(parent);
    }
    s.file(parent, 1 << 20);
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let kinds: Vec<ItemKind> = l.items().iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ItemKind::FolderTitled,
            ItemKind::FolderTitled,
            ItemKind::FolderTitled,
            ItemKind::FolderTitled,
            ItemKind::FolderTitled,
            ItemKind::FileTile
        ]
    );
    let depths: Vec<u8> = l.items().iter().map(|i| i.depth).collect();
    assert_eq!(depths, vec![0, 1, 2, 3, 4, 5]);
    // A narrower window stops nesting and draws a block.
    let narrow = Rect::new(0.0, 0.0, 120.0, 120.0);
    let l = layout(&snap, &folder(0), narrow, m13(1.0));
    assert!(l.items().iter().any(|i| i.kind == ItemKind::FolderBlock));
    check_invariants(&snap, &l).unwrap();
}

#[test]
fn a_very_deep_tree_lays_out_without_recursion_limits() {
    let mut s = Spec::new();
    let mut parent = 0;
    for _ in 0..3000 {
        parent = s.dir(parent);
    }
    s.file(parent, 1 << 20);
    let snap = build(&s);
    let m = Metrics {
        em: 1.0,
        scale: 1.0,
    };
    let vp = Rect::new(0.0, 0.0, 4000.0, 4000.0);
    let l = layout(&snap, &folder(0), vp, m);
    assert!(l.items().len() >= 250);
    assert!(l.items().len() < 3002);
    check_invariants(&snap, &l).unwrap();
}

#[test]
fn the_nesting_depth_is_capped_so_depth_never_wraps() {
    let mut s = Spec::new();
    let mut parent = 0;
    for _ in 0..600 {
        parent = s.dir(parent);
    }
    s.file(parent, 1 << 20);
    let snap = build(&s);
    let m = Metrics {
        em: 0.5,
        scale: 1.0,
    };
    let vp = Rect::new(0.0, 0.0, 6000.0, 6000.0);
    let l = layout(&snap, &folder(0), vp, m);
    let max_depth = l.items().iter().map(|i| u32::from(i.depth)).max().unwrap();
    assert!(max_depth <= 251, "depth {max_depth}");
    let depths: Vec<u8> = l.items().iter().map(|i| i.depth).collect();
    assert!(depths.windows(2).all(|w| w[1] <= w[0] + 1));
}

#[test]
fn unusable_viewports_and_metrics_give_an_empty_or_sane_layout() {
    let snap = build(&dir_in_root());
    let nan = f32::NAN;
    for vp in [
        Rect::new(0.0, 0.0, 0.0, 0.0),
        Rect::new(0.0, 0.0, -50.0, 50.0),
        Rect::new(nan, 0.0, 50.0, 50.0),
        Rect::new(0.0, 0.0, f32::INFINITY, 50.0),
        Rect::new(0.0, 0.0, 1.0, 1.0),
        Rect::new(0.0, 0.0, 1.4, 900.0),
        Rect::new(-30.0, -30.0, 3.0, 3.0),
        Rect::new(0.0, 0.0, 2.0, 2.0),
    ] {
        let l = layout(&snap, &folder(0), vp, m13(1.0));
        check_invariants(&snap, &l).unwrap();
    }
    for m in [
        Metrics {
            em: 0.0,
            scale: 0.0,
        },
        Metrics {
            em: nan,
            scale: nan,
        },
        Metrics {
            em: -5.0,
            scale: 1.0,
        },
        Metrics {
            em: f32::INFINITY,
            scale: 1.0,
        },
        Metrics {
            em: 1e9,
            scale: 1e9,
        },
        Metrics {
            em: 0.001,
            scale: 0.001,
        },
    ] {
        let l = layout(&snap, &folder(0), BIG, m);
        check_invariants(&snap, &l).unwrap();
        assert!(!l.items().is_empty());
    }
    // Nothing smaller than 2 px is drawn, root included.
    assert!(
        layout(&snap, &folder(0), Rect::new(0.0, 0.0, 1.0, 1.0), m13(1.0))
            .items()
            .is_empty()
    );
}

#[test]
fn an_empty_tree_and_unknown_views_give_empty_layouts() {
    let empty = Aggregator::new().snapshot();
    assert!(layout(&empty, &folder(0), BIG, m13(1.0)).items().is_empty());
    let snap = build(&dir_in_root());
    assert!(
        layout(&snap, &folder(999), BIG, m13(1.0))
            .items()
            .is_empty()
    );
}

#[test]
fn a_tree_with_only_zero_byte_content_still_shows_its_folder() {
    let mut s = Spec::new();
    s.nodes[0].size = 0;
    s.file(0, 0);
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    assert_eq!(l.items().len(), 1);
}

#[test]
fn names_are_one_printable_line() {
    let mut s = Spec::new();
    s.add(SpecNode::file(0, 1 << 20).named(b"line\nbreak\xFF.txt"));
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let name = &l.item_for_node(id(1)).unwrap().label.as_ref().unwrap().name;
    assert_eq!(&**name, "line\u{FFFD}break\u{FFFD}.txt");
}

#[test]
fn the_same_input_gives_the_same_layout() {
    let spec = synthetic(3000, 7);
    let a = layout(&build(&spec), &folder(0), BIG, m13(1.0));
    let b = layout(&build(&spec), &folder(0), BIG, m13(1.0));
    assert_eq!(a.items(), b.items());
    assert_eq!(a.enterable(), b.enterable());
    assert!(a.items().len() > 100);
    check_invariants(&build(&spec), &a).unwrap();
}

#[test]
fn a_synthetic_disk_satisfies_the_invariants_at_several_scales() {
    let snap = build(&synthetic(6000, 11));
    for scale in [1.0f32, 1.5, 2.0] {
        let l = layout(
            &snap,
            &folder(0),
            Rect::new(0.0, 0.0, 1600.0 * scale, 900.0 * scale),
            m13(scale),
        );
        check_invariants(&snap, &l).unwrap();
        // Every folder or group box can be entered.
        let enterable = l
            .items()
            .iter()
            .filter(|i| {
                matches!(
                    i.kind,
                    ItemKind::FolderBlock | ItemKind::Group | ItemKind::FolderTitled
                ) && i.depth > 0
            })
            .count();
        assert_eq!(l.enterable().len(), enterable);
    }
}

// ---- Deletes, undo and progress -----------------------------------------

/// root / a(1) / { f(2) 40 MB, g(3) 10 MB }, h(4) 20 MB
fn deletable() -> Spec {
    let mut s = Spec::new();
    let a = s.dir(0);
    s.file(a, 40_000_000);
    s.file(a, 10_000_000);
    s.file(0, 20_000_000);
    s
}

fn opts(bin: BinName) -> Options {
    Options {
        bin,
        ..Options::default()
    }
}

fn comparable(l: &bs_layout::Layout) -> Vec<(u32, Rect, ItemKind, u8, u64)> {
    l.items()
        .iter()
        .map(|i| (i.id, i.rect, i.kind, i.depth, i.bytes))
        .collect()
}

#[test]
fn a_delete_changes_states_and_texts_but_no_rectangle() {
    let mut agg = aggregate(&deletable(), Listing::Complete);
    let before = agg.snapshot();
    let mut l = layout_with(
        &before,
        &folder(0),
        BIG,
        m13(1.0),
        opts(BinName::RecycleBin),
    );
    let original = l.clone();
    agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
    let after = agg.snapshot();
    l.refresh(&after);

    assert_eq!(comparable(&l), comparable(&original));
    let f = l.item_for_node(id(2)).unwrap();
    assert!(f.flags.contains(StateFlags::DELETED));
    let text = &f.label.as_ref().unwrap().size;
    assert_eq!(&**text, "Moved to Recycle Bin · 40.0 MB");
    // Its folder shows what is left and what moved.
    let a = l.item_for_node(id(1)).unwrap();
    assert!(!a.flags.contains(StateFlags::DELETED));
    assert_eq!(
        &*a.label.as_ref().unwrap().size,
        "10.0 MB · 40.0 MB moved to Recycle Bin"
    );
    // The sibling outside is untouched.
    let h = l.item_for_node(id(4)).unwrap();
    assert!(!h.flags.contains(StateFlags::DELETED));
    check_invariants(&after, &l).unwrap();
}

#[test]
fn undo_restores_the_items_exactly() {
    let mut agg = aggregate(&deletable(), Listing::Complete);
    let mut l = layout_with(
        &agg.snapshot(),
        &folder(0),
        BIG,
        m13(1.0),
        opts(BinName::RecycleBin),
    );
    let original = l.items().to_vec();
    agg.mark_deleted(id(1), DeleteState::Deleted).unwrap();
    l.refresh(&agg.snapshot());
    // Everything inside the deleted folder inherits the state.
    for n in [1, 2, 3] {
        let it = l.item_for_node(id(n)).unwrap();
        assert!(it.flags.contains(StateFlags::DELETED), "node {n}");
    }
    assert_eq!(
        &*l.item_for_node(id(1)).unwrap().label.as_ref().unwrap().size,
        "Deleted · 50.0 MB"
    );
    assert_ne!(l.items(), &original[..]);
    agg.unmark_deleted(id(1)).unwrap();
    l.refresh(&agg.snapshot());
    assert_eq!(l.items(), &original[..]);
}

#[test]
fn deleting_inside_a_group_updates_the_group_without_moving_it() {
    let s = big_and_small(300, |_| 0);
    let mut agg = aggregate(&s, Listing::Complete);
    let mut l = layout_with(
        &agg.snapshot(),
        &folder(0),
        BIG,
        m13(1.0),
        opts(BinName::RecycleBin),
    );
    let group = |l: &bs_layout::Layout| {
        l.items()
            .iter()
            .find(|i| i.kind == ItemKind::Group)
            .unwrap()
            .clone()
    };
    let before = group(&l);
    assert_eq!(&*before.label.as_ref().unwrap().size, "1.2 MB");
    agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
    l.refresh(&agg.snapshot());
    let g = group(&l);
    assert_eq!(g.rect, before.rect);
    assert!(!g.flags.contains(StateFlags::DELETED));
    assert_eq!(
        &*g.label.as_ref().unwrap().size,
        "1.2 MB \u{b7} 4.1 kB moved to Recycle Bin"
    );
    // When every member is gone the whole group reads as deleted.
    for n in 3..=301 {
        agg.mark_deleted(id(n), DeleteState::InBin).unwrap();
    }
    l.refresh(&agg.snapshot());
    let g = group(&l);
    assert_eq!(g.rect, before.rect);
    assert!(g.flags.contains(StateFlags::DELETED));
    assert_eq!(
        &*g.label.as_ref().unwrap().size,
        "Moved to Recycle Bin \u{b7} 1.2 MB"
    );
}

#[test]
fn unfinished_directories_are_marked_in_progress_with_a_lower_bound() {
    let mut agg = aggregate(&deletable(), Listing::None);
    let snap = agg.snapshot();
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let a = l.item_for_node(id(1)).unwrap();
    assert!(a.flags.contains(StateFlags::IN_PROGRESS));
    assert!(a.label.as_ref().unwrap().size.starts_with("≥ "));
    // A file is complete as soon as it is counted.
    let f = l.item_for_node(id(2)).unwrap();
    assert!(!f.flags.contains(StateFlags::IN_PROGRESS));
    assert!(!f.label.as_ref().unwrap().size.starts_with('≥'));

    // Finishing the listings and refreshing clears it without moving boxes.
    let done = aggregate(&deletable(), Listing::Complete).snapshot();
    let mut l2 = l.clone();
    l2.refresh(&done);
    assert!(
        !l2.item_for_node(id(1))
            .unwrap()
            .flags
            .contains(StateFlags::IN_PROGRESS)
    );
    assert_eq!(comparable(&l2), comparable(&l));
}

#[test]
fn a_relayout_is_the_only_thing_that_moves_boxes() {
    let snap = build(&synthetic(1500, 5));
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let wider = l.relayout(&snap, Rect::new(0.0, 0.0, 1400.0, 700.0), m13(1.0));
    assert_ne!(comparable(&l), comparable(&wider));
    let same = l.relayout(&snap, BIG, m13(1.0));
    assert_eq!(same.items(), l.items());
    // A new em moves them too.
    let bigger_font = l.relayout(&snap, BIG, m13(2.0));
    assert_ne!(comparable(&l), comparable(&bigger_font));
}

// ---- Navigation -----------------------------------------------------------

#[test]
fn view_ids_are_stable_and_remember_where_they_were_entered_from() {
    let snap = build(&synthetic(1500, 9));
    let m = m13(1.0);
    let mut table = ViewTable::new();
    let root = table.root(folder(0));
    assert_eq!(root, 0);
    let l = layout(&snap, &folder(0), BIG, m);
    let first = table.register(root, &l);
    assert_eq!(first.len(), l.enterable().len());
    let ids: std::collections::HashSet<_> = first.iter().map(|(_, v)| *v).collect();
    assert_eq!(
        ids.len(),
        first.len(),
        "every enterable box has its own view"
    );
    for (_, v) in &first {
        assert_eq!(table.parent(*v), Some(root));
    }
    // Laying the same view out again hands out the same ids.
    let again = table.register(root, &l.relayout(&snap, BIG, m));
    assert_eq!(first, again);
    assert_eq!(table.len(), 1 + first.len());
    // Folder views resolve back to their target.
    let (item, view) = first[0];
    assert_eq!(table.target(view), Some(&l.enterable()[0].1));
    assert_eq!(l.enterable()[0].0, item);
    assert!(table.target(u64::MAX).is_none());
    assert!(table.parent(u64::MAX).is_none());
    assert!(table.enter(u64::MAX).is_none());
    // A view can be entered and laid out.
    let target = table.enter(view).unwrap();
    let inside = layout(&snap, &target, BIG, m);
    check_invariants(&snap, &inside).unwrap();
}

/// A folder of small files that keeps growing, laid out after every batch
/// the way a running scan does.
fn growing_folder(small: usize) -> bs_tree::Aggregator {
    let mut s = Spec::new();
    // Nine times the small files, so they stay small and their group is wide.
    s.file(0, small as u64 * 4096 * 9);
    for _ in 0..small {
        s.file(0, 4096);
    }
    aggregate(&s, Listing::None)
}

fn add_files(agg: &mut bs_tree::Aggregator, round: usize, count: usize) {
    let mut b = bs_tree::Batch::new();
    for i in 0..count {
        b.file(
            bs_tree::NodeKind::File,
            bs_tree::DirKey(0),
            format!("r{round}-{i}").as_bytes(),
            &bs_tree::Meta {
                logical: 4096,
                allocated: 4096,
                ..bs_tree::Meta::default()
            },
        )
        .unwrap();
    }
    agg.apply(&b);
}

#[test]
fn repeated_relayouts_do_not_grow_the_view_table() {
    let mut agg = growing_folder(20_000);
    let m = m13(1.0);
    let mut table = ViewTable::new();
    let root = table.root(folder(0));
    let mut members_in_latest = 0;
    for round in 0..15 {
        let l = layout(&agg.snapshot(), &folder(0), BIG, m);
        members_in_latest = l
            .enterable()
            .iter()
            .map(|(_, t)| match t {
                ViewTarget::Group(g) => g.members.len(),
                ViewTarget::Folder(_) => 0,
            })
            .sum::<usize>();
        assert!(members_in_latest >= 20_000);
        table.register(root, &l);
        // The root, and what the latest layout offers: nothing older.
        assert_eq!(table.len(), 1 + l.enterable().len(), "round {round}");
        assert_eq!(table.held_member_ids(), members_in_latest, "round {round}");
        add_files(&mut agg, round, 1000);
    }
    assert!(members_in_latest >= 34_000);
}

#[test]
fn entered_views_survive_relayouts_until_history_lets_go() {
    let mut agg = growing_folder(5000);
    let m = m13(1.0);
    let mut table = ViewTable::new();
    let root = table.root(folder(0));
    let l = layout(&agg.snapshot(), &folder(0), BIG, m);
    let offered = table.register(root, &l);
    let (_, group_view) = *offered
        .iter()
        .find(|(_, v)| matches!(table.target(*v), Some(ViewTarget::Group(_))))
        .expect("the folder offers a group");
    let entered = table.enter(group_view).unwrap();
    let ViewTarget::Group(spec) = &entered else {
        panic!("not a group");
    };
    let members = spec.members.len();

    // The scan goes on and the folder is laid out again and again.
    for round in 0..5 {
        add_files(&mut agg, round, 500);
        let l = layout(&agg.snapshot(), &folder(0), BIG, m);
        table.register(root, &l);
    }
    // The entered group is still there with its members and its parent; the
    // table holds it, the root and the latest offer, and nothing in between.
    assert_eq!(table.target(group_view), Some(&entered));
    assert_eq!(table.parent(group_view), Some(root));
    assert_eq!(table.len(), 3);
    assert!(table.held_member_ids() < members + 2 * 8000);

    // History moves on: only the root and what it offers remain.
    table.prune(&[root]);
    assert!(table.target(group_view).is_none());
    assert_eq!(table.len(), 2);
    assert_eq!(table.target(root), Some(&folder(0)));

    // Pruning with a view in history keeps it and its ancestors.
    let l = layout(&agg.snapshot(), &folder(0), BIG, m);
    let offered = table.register(root, &l);
    let (_, g) = offered[0];
    table.enter(g).unwrap();
    table.prune(&[g]);
    assert!(table.target(g).is_some());
    assert_eq!(table.parent(g), Some(root));
}

#[test]
fn views_outside_a_layout_find_their_tree_parent() {
    let snap = build(&dir_in_root());
    let up = ViewTable::tree_parent(&snap, &folder(1));
    assert_eq!(up, Some(folder(0)));
    assert_eq!(ViewTable::tree_parent(&snap, &folder(0)), None);
    assert_eq!(ViewTable::tree_parent(&snap, &folder(999)), None);
}

#[test]
fn a_node_added_after_the_layout_has_no_item_even_if_its_number_is_a_groups() {
    let mut agg = growing_folder(300);
    let snap = agg.snapshot();
    let before = snap.len();
    let mut l = layout(&snap, &folder(0), BIG, m13(1.0));
    let group = l
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .expect("the small files are grouped")
        .id;
    assert_eq!(group, before, "the first group takes the first free number");

    // The scan goes on: the next node gets the group's number.
    add_files(&mut agg, 0, 1);
    let after = agg.snapshot();
    let late = NodeId::from_index(before);
    assert!(after.node(late).is_some());
    l.refresh(&after);
    assert!(l.item_for_node(late).is_none());
    assert!(l.item_for_node(NodeId::from_index(u32::MAX)).is_none());
    // The group is still reachable by its item id, and nodes that were there
    // still resolve.
    assert!(l.item(group).is_some());
    assert!(l.item_for_node(id(1)).is_some());
    // A fresh layout counts the late file, and the ids are consistent again.
    let fresh = l.relayout(&after, BIG, m13(1.0));
    check_invariants(&after, &fresh).unwrap();
}

#[test]
fn item_ids_resolve_to_nodes_and_groups() {
    let mut s = Spec::new();
    s.file(0, 100 << 20);
    for _ in 0..3 {
        s.file(0, 4096);
    }
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    assert!(matches!(l.item_ref(1), Some(ItemRef::Node(n)) if n == NodeId::from_index(1)));
    assert!(matches!(l.item_ref(snap.len()), Some(ItemRef::Group(_))));
    assert!(l.item_ref(snap.len() + 1).is_none());
    assert!(l.item(u32::MAX).is_none());
}

// ---- Scale ------------------------------------------------------------------

#[test]
fn a_million_siblings_become_one_group_in_a_single_pass() {
    let mut s = Spec::new();
    s.file(0, 1 << 30);
    for _ in 0..1_000_000 {
        s.file(0, 4096);
    }
    let snap = build(&s);
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    assert_eq!(l.items().len(), 3);
    let g = l
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap();
    assert_eq!(&*g.label.as_ref().unwrap().name, "1,000,000 small files");
    check_invariants(&snap, &l).unwrap();
    // Entering: a million equal members get under a pixel of area each, so
    // nothing inside can be drawn. The view is its frame, titled with the
    // group's total, and that is not a truncation.
    let (_, target) = l.enterable().iter().find(|(i, _)| *i == g.id).unwrap();
    let inside = layout(&snap, target, BIG, m13(1.0));
    assert_eq!(inside.items().len(), 1);
    let frame = &inside.items()[0];
    assert_eq!(frame.kind, ItemKind::FolderTitled);
    assert_eq!(frame.bytes, 1_000_000 * 4096);
    assert_eq!(&*frame.label.as_ref().unwrap().size, "4.1 GB");
    assert!(!inside.is_truncated());
}

#[test]
fn texts_use_the_name_the_platform_gives_the_bin() {
    assert_eq!(Options::default().bin, BinName::platform());
    assert_eq!(
        BinName::platform(),
        if cfg!(windows) {
            BinName::RecycleBin
        } else {
            BinName::Trash
        }
    );
    for (bin, name) in [
        (BinName::RecycleBin, "Recycle Bin"),
        (BinName::Trash, "Trash"),
    ] {
        assert_eq!(bin.as_str(), name);
        // A file, and the folder above it.
        let mut agg = aggregate(&deletable(), Listing::Complete);
        let mut l = layout_with(&agg.snapshot(), &folder(0), BIG, m13(1.0), opts(bin));
        agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
        l.refresh(&agg.snapshot());
        let text = |l: &bs_layout::Layout, n: usize| {
            l.item_for_node(id(n))
                .unwrap()
                .label
                .as_ref()
                .unwrap()
                .size
                .to_string()
        };
        assert_eq!(text(&l, 2), format!("Moved to {name} \u{b7} 40.0 MB"));
        assert_eq!(
            text(&l, 1),
            format!("10.0 MB \u{b7} 40.0 MB moved to {name}")
        );
        // A new layout keeps the name.
        let again = l.relayout(&agg.snapshot(), BIG, m13(1.0));
        assert_eq!(text(&again, 2), text(&l, 2));

        // A group: partly moved, then moved and deleted, then all moved.
        let group = |l: &bs_layout::Layout| {
            l.items()
                .iter()
                .find(|i| i.kind == ItemKind::Group)
                .unwrap()
                .label
                .as_ref()
                .unwrap()
                .size
                .to_string()
        };
        let mut agg = aggregate(&big_and_small(300, |_| 0), Listing::Complete);
        let mut l = layout_with(&agg.snapshot(), &folder(0), BIG, m13(1.0), opts(bin));
        agg.mark_deleted(id(2), DeleteState::InBin).unwrap();
        l.refresh(&agg.snapshot());
        assert_eq!(group(&l), format!("1.2 MB \u{b7} 4.1 kB moved to {name}"));
        for n in 3..=150 {
            agg.mark_deleted(id(n), DeleteState::InBin).unwrap();
        }
        for n in 151..=301 {
            agg.mark_deleted(id(n), DeleteState::Deleted).unwrap();
        }
        l.refresh(&agg.snapshot());
        assert_eq!(
            group(&l),
            format!("610.3 kB moved to {name} \u{b7} 618.5 kB deleted")
        );
        let mut all = aggregate(&big_and_small(300, |_| 0), Listing::Complete);
        let mut l = layout_with(&all.snapshot(), &folder(0), BIG, m13(1.0), opts(bin));
        for n in 2..=301 {
            all.mark_deleted(id(n), DeleteState::InBin).unwrap();
        }
        l.refresh(&all.snapshot());
        assert_eq!(group(&l), format!("Moved to {name} \u{b7} 1.2 MB"));
    }
}

#[test]
fn a_group_view_groups_its_own_small_tail() {
    let mut s = Spec::new();
    s.file(0, 1 << 40); // 1: the reason the members are small in the folder
    for _ in 0..5 {
        s.file(0, 300_000); // 2..=6: big enough to stand alone in the group
    }
    for _ in 0..300 {
        s.file(0, 4096); // 7..=306: a tail that is small even there
    }
    let snap = build(&s);
    let members: Vec<NodeId> = (2..=306).map(id).collect();
    let target = ViewTarget::Group(Arc::new(GroupSpec {
        folder: id(0),
        members,
    }));
    let inside = layout(&snap, &target, BIG, m13(1.0));
    check_invariants(&snap, &inside).unwrap();
    let kinds = |k: ItemKind| inside.items().iter().filter(|i| i.kind == k).count();
    assert_eq!(kinds(ItemKind::FolderTitled), 1);
    assert_eq!(kinds(ItemKind::FileTile), 5);
    assert_eq!(kinds(ItemKind::Group), 1);
    assert_eq!(inside.items().len(), 7);
    let nested = inside
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap();
    assert_eq!(nested.bytes, 300 * 4096);
    // The view's own group is number 0, the nested one comes next.
    assert_eq!(inside.items()[0].id, snap.len());
    assert_eq!(nested.id, snap.len() + 1);
    let Some(ItemRef::Group(spec)) = inside.item_ref(nested.id) else {
        panic!("nested group does not resolve");
    };
    assert_eq!(spec.members.len(), 300);
    assert_eq!(spec.folder, id(0));
}

/// A big file and `k` files of `s` bytes in a window of known size: how many
/// group boxes does the layout draw?
fn groups_drawn(scale: f32, big: u64, k: usize, s: u64) -> usize {
    let mut spec = Spec::new();
    spec.file(0, big);
    for _ in 0..k {
        spec.file(0, s);
    }
    let snap = build(&spec);
    let l = layout(&snap, &folder(0), window(scale), m13(scale));
    l.items()
        .iter()
        .filter(|i| i.kind == ItemKind::Group)
        .count()
}

fn window(scale: f32) -> Rect {
    Rect::new(0.0, 0.0, (1000.0 * scale).round(), (700.0 * scale).round())
}

#[test]
fn the_grouping_limit_is_a_2_5_em_square_of_the_parents_content_area() {
    for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
        let m = m13(scale);
        let side = f64::from(2.5 * m.em);
        let square = side * side;
        let area = content_rect(window(scale), m).area();
        for k in [2usize, 3, 4, 10] {
            // A member is small when its share of the content area is under
            // the square: s * area < square * (big + k * s). Size the big
            // file so that the boundary lands near 50,000 bytes, then find
            // it exactly.
            let kk = k as f64;
            let big = (50_000.0 * (area - square * kk) / square).ceil() as u64;
            let small = |s: u64| (s as f64) * area < square * (big as f64 + kk * s as f64);
            let (mut lo, mut hi) = (1u64, 500_000u64);
            assert!(small(lo) && !small(hi));
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if small(mid) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            // `lo` is the biggest size still under the square, `hi` the
            // smallest that reaches it. Three members make a group, two do
            // not.
            let (below, above) = (lo, hi);
            assert_eq!(
                groups_drawn(scale, big, k, below),
                usize::from(k >= 3),
                "scale {scale}: {k} members of {below} bytes"
            );
            assert_eq!(
                groups_drawn(scale, big, k, above),
                0,
                "scale {scale}: {k} members of {above} bytes"
            );
        }
    }
}

/// root / d(1) / { guess(2): estimated size, exact(3) }, other(4)
fn with_an_estimate() -> bs_tree::Snapshot {
    use bs_tree::{Batch, DirKey, Meta, NodeFlags, NodeKind};
    let sized = |allocated: u64, flags: NodeFlags| Meta {
        logical: allocated,
        allocated,
        flags,
        ..Meta::default()
    };
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(
        DirKey(0),
        DirKey::NONE,
        b"/root",
        &sized(4096, NodeFlags::EMPTY),
    )
    .unwrap();
    b.dir(DirKey(1), DirKey(0), b"d", &sized(4096, NodeFlags::EMPTY))
        .unwrap();
    b.file(
        NodeKind::File,
        DirKey(1),
        b"guess",
        &sized(40_000_000, NodeFlags::ESTIMATED_SIZE),
    )
    .unwrap();
    b.file(
        NodeKind::File,
        DirKey(1),
        b"exact",
        &sized(10_000_000, NodeFlags::EMPTY),
    )
    .unwrap();
    b.file(
        NodeKind::File,
        DirKey(0),
        b"other",
        &sized(20_000_000, NodeFlags::EMPTY),
    )
    .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.snapshot()
}

#[test]
fn an_estimated_size_is_shown_with_an_approximation_sign_and_so_is_what_contains_it() {
    let snap = with_an_estimate();
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let size = |n: usize| {
        l.item_for_node(id(n))
            .unwrap_or_else(|| panic!("node {n} is drawn"))
            .label
            .as_ref()
            .unwrap()
            .size
            .to_string()
    };
    // The estimate and the folder holding it carry the sign; the rest do not.
    assert!(size(2).starts_with("≈ "), "{}", size(2));
    assert!(size(1).starts_with("≈ "), "{}", size(1));
    assert!(l.items()[0].label.as_ref().unwrap().size.starts_with("≈ "));
    assert!(!size(3).contains('≈'), "{}", size(3));
    assert!(!size(4).contains('≈'), "{}", size(4));
    // The tree says the same, and does not call the estimate a lower bound.
    assert!(snap.node(id(1)).unwrap().size_is_estimated());
    assert!(!snap.node(id(1)).unwrap().bytes_are_lower_bound());
    assert!(!size(2).contains('≥'));
}

#[test]
fn a_size_that_is_both_a_lower_bound_and_an_estimate_says_both() {
    // The same tree with the root's listing left open.
    use bs_tree::{Batch, DirKey, Meta, NodeFlags, NodeKind};
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/root", &Meta::default())
        .unwrap();
    b.file(
        NodeKind::File,
        DirKey(0),
        b"guess",
        &Meta {
            logical: 30_000_000,
            allocated: 30_000_000,
            flags: NodeFlags::ESTIMATED_SIZE,
            ..Meta::default()
        },
    )
    .unwrap();
    agg.apply(&b);
    let snap = agg.snapshot();
    let l = layout(&snap, &folder(0), BIG, m13(1.0));
    let root = l.items()[0].label.as_ref().unwrap().size.to_string();
    assert!(root.starts_with("≥ ≈ "), "{root}");
}
