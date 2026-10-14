//! Looking items up by id, and moving layouts between threads.

mod common;

use bs_layout::{Metrics, Rect, ViewTarget, layout};
use common::*;
use std::collections::HashSet;
use std::sync::Arc;

const BIG: Rect = Rect::new(0.0, 0.0, 1600.0, 1000.0);

fn m13() -> Metrics {
    Metrics::from_logical(13.0, 1.0)
}

fn assert_lookups_agree_with_a_scan(l: &bs_layout::Layout, extra_ids: &[u32]) {
    let items = l.items();
    assert!(!items.is_empty());
    for it in items {
        let by_scan = items.iter().find(|i| i.id == it.id);
        assert_eq!(l.item(it.id), by_scan, "id {}", it.id);
        assert_eq!(l.rect_of(it.id), Some(it.rect), "id {}", it.id);
    }
    // Ids that are not drawn: neighbours of drawn ones, and the extremes.
    let drawn: HashSet<u32> = items.iter().map(|i| i.id).collect();
    let mut probes: Vec<u32> = vec![0, 1, u32::MAX, u32::MAX - 1, u32::MAX / 2];
    probes.extend_from_slice(extra_ids);
    probes.extend(
        items
            .iter()
            .flat_map(|i| [i.id.wrapping_add(1), i.id.wrapping_sub(1)]),
    );
    for p in probes {
        if !drawn.contains(&p) {
            assert!(l.item(p).is_none(), "id {p} is not drawn");
            assert_eq!(l.rect_of(p), None, "id {p}");
        }
    }
}

#[test]
fn lookup_by_id_agrees_with_a_scan_for_drawn_and_undrawn_ids() {
    let spec = synthetic(4000, 7);
    let snap = build(&spec);
    // A small window, so that many nodes are grouped or too small to draw.
    let l = layout(
        &snap,
        &ViewTarget::Folder(id(0)),
        Rect::new(0.0, 0.0, 700.0, 500.0),
        m13(),
    );
    assert!(l.items().len() > 100, "{}", l.items().len());
    assert_lookups_agree_with_a_scan(&l, &[snap.len(), snap.len() + 1]);
    // Zero-byte and tiny nodes are not drawn; they must come back as `None`.
    let undrawn = (0..snap.len()).filter(|&n| !l.items().iter().any(|i| i.id == n));
    assert!(undrawn.clone().count() > 0, "the tree has undrawn nodes");
    for n in undrawn.take(200) {
        assert!(l.item(n).is_none());
    }
}

#[test]
fn lookup_works_in_a_group_view_on_clones_and_after_refresh() {
    // Many small siblings, so there are groups and a group view.
    let mut s = Spec::new();
    s.file(0, 50 << 20);
    for _ in 0..300 {
        s.file(0, 4096);
    }
    let snap = build(&s);
    let l = layout(&snap, &ViewTarget::Folder(id(0)), BIG, m13());
    let group = l
        .items()
        .iter()
        .find(|i| i.kind == bs_layout::ItemKind::Group)
        .expect("a group");
    // A group's id is past the node ids.
    assert_eq!(l.item(group.id).map(|i| i.id), Some(group.id));
    assert_eq!(l.rect_of(group.id), Some(group.rect));
    let (_, target) = l
        .enterable()
        .iter()
        .find(|(item, _)| *item == group.id)
        .unwrap();
    let inside = layout(&snap, target, BIG, m13());
    assert_lookups_agree_with_a_scan(&inside, &[]);

    let mut copy = l.clone();
    copy.refresh(&snap);
    assert_lookups_agree_with_a_scan(&copy, &[]);
    let relaid = l.relayout(&snap, Rect::new(0.0, 0.0, 900.0, 600.0), m13());
    assert_lookups_agree_with_a_scan(&relaid, &[]);
}

#[test]
fn an_empty_layout_finds_nothing() {
    let snap = build(&Spec::new());
    for viewport in [
        BIG,
        Rect::new(0.0, 0.0, 0.0, 0.0),
        Rect::new(0.0, 0.0, 1.0, 1.0),
    ] {
        let l = layout(&snap, &ViewTarget::Folder(id(0)), viewport, m13());
        for probe in [0, 1, 7, u32::MAX] {
            if l.items().iter().all(|i| i.id != probe) {
                assert!(l.item(probe).is_none());
                assert_eq!(l.rect_of(probe), None);
            }
        }
    }
    let unknown = layout(&snap, &ViewTarget::Folder(id(99)), BIG, m13());
    assert!(unknown.items().is_empty());
    assert!(unknown.item(0).is_none());
    assert_eq!(unknown.rect_of(0), None);
}

#[test]
fn a_layout_made_on_another_thread_can_be_used_on_this_one() {
    // The interface thread asks a worker for a layout and receives it. The
    // snapshot and the view cross threads on the way in, the layout on the
    // way out; this is the shape of that hand-over.
    let snap = Arc::new(build(&synthetic(2000, 3)));
    let target = ViewTarget::Folder(id(0));
    let made = {
        let snap = Arc::clone(&snap);
        let target = target.clone();
        std::thread::spawn(move || layout(&snap, &target, BIG, m13()))
            .join()
            .unwrap()
    };
    let here = layout(&snap, &target, BIG, m13());
    assert_eq!(made.items(), here.items());
    assert_eq!(made.items().len(), here.items().len());

    // Shared between threads at once, too.
    let shared = Arc::new(made);
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                shared
                    .items()
                    .iter()
                    .filter(|i| shared.item(i.id).is_some())
                    .count()
            })
        })
        .collect();
    for r in readers {
        assert_eq!(r.join().unwrap(), shared.items().len());
    }
}
