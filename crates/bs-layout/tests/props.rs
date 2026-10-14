//! Properties of the layout over random trees, viewports and fonts.

mod common;

use bs_layout::{ItemKind, Metrics, Rect, StateFlags, ViewTarget, layout};
use bs_tree::DeleteState;
use common::*;
use proptest::prelude::*;

/// A random tree: `raw[i]` describes node `i + 1`.
fn spec_from(raw: &[(u16, bool, u8, u8)]) -> Spec {
    let mut spec = Spec::new();
    let mut dirs = vec![0usize];
    for &(pick, is_dir, magnitude, category) in raw {
        let parent = dirs[pick as usize % dirs.len()];
        // Sizes spread over many orders of magnitude, with some zeros.
        let size = match magnitude % 8 {
            0 => 0,
            1 => 1,
            2 => 4096,
            3 => 4096 * 3,
            4 => 1 << 20,
            5 => 37 << 20,
            6 => 5 << 30,
            _ => 1 << 40,
        };
        let node = if is_dir {
            let mut n = SpecNode::dir(parent);
            n.size = size.min(1 << 20);
            n
        } else {
            SpecNode::file(parent, size).cat(category % 10)
        };
        let index = spec.add(node);
        if is_dir {
            dirs.push(index);
        }
    }
    spec
}

/// A denser random tree: sizes follow a smooth log-uniform spread, so many
/// boxes of every size are drawn.
fn smooth_spec(raw: &[(u16, bool, u16, u8)]) -> Spec {
    let mut spec = Spec::new();
    let mut dirs = vec![0usize];
    for &(pick, is_dir, magnitude, category) in raw {
        let parent = dirs[pick as usize % dirs.len()];
        let size = ((f64::from(magnitude) / 65535.0 * 15.0).exp() as u64 + 1) * 512;
        let node = if is_dir {
            SpecNode::dir(parent)
        } else {
            SpecNode::file(parent, size).cat(category % 10)
        };
        let index = spec.add(node);
        if is_dir {
            dirs.push(index);
        }
    }
    spec
}

fn tree_strategy(max: usize) -> impl Strategy<Value = Vec<(u16, bool, u8, u8)>> {
    prop::collection::vec(
        (any::<u16>(), any::<bool>(), any::<u8>(), any::<u8>()),
        0..max,
    )
}

fn viewport_strategy() -> impl Strategy<Value = (f32, f32, f32, f32)> {
    (
        -50.0f32..50.0,
        -50.0f32..50.0,
        1.0f32..2500.0,
        1.0f32..1800.0,
    )
}

fn metrics_strategy() -> impl Strategy<Value = Metrics> {
    (
        4.0f32..40.0,
        prop::sample::select(vec![1.0f32, 1.25, 1.5, 2.0, 3.0]),
    )
        .prop_map(|(font, scale)| Metrics::from_logical(font, scale))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    /// Boxes are whole pixels, stay inside their parents, never overlap,
    /// follow the thresholds, and have areas in proportion to their bytes.
    #[test]
    fn the_layout_satisfies_its_invariants(
        raw in tree_strategy(250),
        (x, y, w, h) in viewport_strategy(),
        m in metrics_strategy(),
    ) {
        let snap = build(&spec_from(&raw));
        let vp = Rect::new(x, y, w, h);
        let l = layout(&snap, &ViewTarget::Folder(id(0)), vp, m);
        if let Err(e) = check_invariants(&snap, &l) {
            return Err(TestCaseError::fail(e));
        }
        // Every group can be entered and satisfies the same invariants.
        for (_, target) in l.enterable() {
            if matches!(target, ViewTarget::Group(_)) {
                let inside = layout(&snap, target, vp, m);
                if let Err(e) = check_invariants(&snap, &inside) {
                    return Err(TestCaseError::fail(format!("group view: {e}")));
                }
            }
        }
    }

    /// The same promises on dense trees, where hundreds of boxes are drawn,
    /// at fonts and scales from tiny to large.
    #[test]
    fn dense_trees_satisfy_the_invariants(
        raw in prop::collection::vec((any::<u16>(), any::<bool>(), any::<u16>(), any::<u8>()), 100..900),
        (x, y, w, h) in viewport_strategy(),
        m in metrics_strategy(),
    ) {
        let snap = build(&smooth_spec(&raw));
        let vp = Rect::new(x, y, w.max(200.0), h.max(150.0));
        let l = layout(&snap, &ViewTarget::Folder(id(0)), vp, m);
        if let Err(e) = check_invariants(&snap, &l) {
            return Err(TestCaseError::fail(e));
        }
        for (_, target) in l.enterable().iter().take(40) {
            let inside = layout(&snap, target, vp, m);
            if let Err(e) = check_invariants(&snap, &inside) {
                return Err(TestCaseError::fail(format!("inner view: {e}")));
            }
        }
    }

    /// Any folder of the tree is a valid view.
    #[test]
    fn every_folder_is_a_valid_view(
        raw in tree_strategy(120),
        pick in any::<u16>(),
        m in metrics_strategy(),
    ) {
        let spec = spec_from(&raw);
        let dirs: Vec<usize> = (0..spec.nodes.len()).filter(|&i| spec.nodes[i].is_dir).collect();
        let folder = dirs[pick as usize % dirs.len()];
        let snap = build(&spec);
        let l = layout(&snap, &ViewTarget::Folder(id(folder)), Rect::new(0.0, 0.0, 900.0, 600.0), m);
        if let Err(e) = check_invariants(&snap, &l) {
            return Err(TestCaseError::fail(e));
        }
        prop_assert!(l.items().first().is_none_or(|i| i.id == folder as u32));
    }

    /// The same inputs give the same list, item for item.
    #[test]
    fn the_layout_is_deterministic(
        raw in tree_strategy(200),
        (x, y, w, h) in viewport_strategy(),
        m in metrics_strategy(),
    ) {
        let spec = spec_from(&raw);
        let vp = Rect::new(x, y, w, h);
        let a = layout(&build(&spec), &ViewTarget::Folder(id(0)), vp, m);
        let b = layout(&build(&spec), &ViewTarget::Folder(id(0)), vp, m);
        prop_assert_eq!(a.items(), b.items());
        prop_assert_eq!(a.enterable(), b.enterable());
    }

    /// Deleting, refreshing and undoing never move a box, and undo
    /// restores every item exactly.
    #[test]
    fn deletes_never_move_boxes_and_undo_is_exact(
        raw in tree_strategy(150),
        picks in prop::collection::vec(any::<u16>(), 1..6),
        m in metrics_strategy(),
    ) {
        let spec = spec_from(&raw);
        let mut agg = aggregate(&spec, Listing::Complete);
        let vp = Rect::new(0.0, 0.0, 1100.0, 750.0);
        let mut l = layout(&agg.snapshot(), &ViewTarget::Folder(id(0)), vp, m);
        let original = l.items().to_vec();
        let mut marked = Vec::new();
        for p in picks {
            let node = 1 + p as usize % (spec.nodes.len().max(2) - 1);
            if node >= spec.nodes.len() { continue; }
            let state = if p % 2 == 0 { DeleteState::InBin } else { DeleteState::Deleted };
            if agg.mark_deleted(id(node), state).is_ok() {
                marked.push(node);
            }
        }
        l.refresh(&agg.snapshot());
        prop_assert_eq!(l.items().len(), original.len());
        for (now, was) in l.items().iter().zip(&original) {
            prop_assert_eq!(now.rect, was.rect);
            prop_assert_eq!(now.kind, was.kind);
            prop_assert_eq!(now.id, was.id);
            prop_assert_eq!(now.depth, was.depth);
            prop_assert_eq!(now.bytes, was.bytes);
            prop_assert_eq!(now.label.is_some(), was.label.is_some());
        }
        for &node in &marked {
            if let Some(it) = l.item_for_node(id(node)) {
                prop_assert!(it.flags.contains(StateFlags::DELETED));
            }
        }
        for &node in marked.iter().rev() {
            agg.unmark_deleted(id(node)).unwrap();
        }
        l.refresh(&agg.snapshot());
        prop_assert_eq!(l.items(), &original[..]);
    }

    /// A layout made from a snapshot that already holds deletes and unfinished
    /// folders agrees with what a refresh from the same snapshot computes:
    /// the quick summary made while grouping and the full one never differ.
    #[test]
    fn refresh_from_the_same_snapshot_changes_nothing(
        raw in prop::collection::vec((any::<u16>(), any::<bool>(), any::<u16>(), any::<u8>()), 50..600),
        picks in prop::collection::vec(any::<u16>(), 0..12),
        finish_listings in any::<bool>(),
        m in metrics_strategy(),
    ) {
        let spec = smooth_spec(&raw);
        let mut agg = aggregate(
            &spec,
            if finish_listings { Listing::Complete } else { Listing::None },
        );
        if finish_listings {
            for p in picks {
                let node = 1 + p as usize % (spec.nodes.len().max(2) - 1);
                if node < spec.nodes.len() {
                    let state = if p % 2 == 0 { DeleteState::InBin } else { DeleteState::Deleted };
                    let _ = agg.mark_deleted(id(node), state);
                }
            }
        }
        let snap = agg.snapshot();
        let vp = Rect::new(0.0, 0.0, 1400.0, 900.0);
        let l = layout(&snap, &ViewTarget::Folder(id(0)), vp, m);
        let mut refreshed = l.clone();
        refreshed.refresh(&snap);
        prop_assert_eq!(l.items(), refreshed.items());
        for (_, target) in l.enterable().iter().take(20) {
            let inside = layout(&snap, target, vp, m);
            let mut again = inside.clone();
            again.refresh(&snap);
            prop_assert_eq!(inside.items(), again.items());
        }
    }

    /// A different epoch's snapshot with the same tree changes nothing in the
    /// geometry, and a relayout at the same size reproduces the list.
    #[test]
    fn relayout_at_the_same_size_reproduces_the_list(
        raw in tree_strategy(150),
        (x, y, w, h) in viewport_strategy(),
        m in metrics_strategy(),
    ) {
        let snap = build(&spec_from(&raw));
        let vp = Rect::new(x, y, w, h);
        let l = layout(&snap, &ViewTarget::Folder(id(0)), vp, m);
        let again = l.relayout(&snap, vp, m);
        prop_assert_eq!(l.items(), again.items());
    }
}

/// A single directory with a very large number of children, each a different
/// size: the layout stays valid and finishes.
#[test]
fn a_huge_fan_out_in_one_directory_is_valid() {
    let mut spec = Spec::new();
    let mut rng = Rng(99);
    for _ in 0..200_000 {
        let size = ((rng.unit() * 12.0).exp() as u64).max(1) * 512;
        spec.file(0, size);
    }
    let snap = build(&spec);
    let m = Metrics::from_logical(13.0, 1.0);
    let l = layout(
        &snap,
        &ViewTarget::Folder(id(0)),
        Rect::new(0.0, 0.0, 1920.0, 1080.0),
        m,
    );
    check_invariants(&snap, &l).unwrap();
    assert!(l.items().iter().any(|i| i.kind == ItemKind::Group));
}
