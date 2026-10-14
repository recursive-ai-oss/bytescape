//! The color of a group of small siblings: the dominant category's, but only
//! when it holds more than 60% of the group's bytes.

mod common;

use bs_layout::{CategoryId, ItemKind, Metrics, Rect, ViewTarget, layout};
use common::*;

fn m13() -> Metrics {
    Metrics::from_logical(13.0, 1.0)
}

/// Big enough for the group to carry a label.
const LABELED: Rect = Rect::new(0.0, 0.0, 1000.0, 700.0);
/// So small that the group is drawn without a label.
const UNLABELED: Rect = Rect::new(0.0, 0.0, 100.0, 60.0);

/// How many small files each `(bytes, category)` below is spread over, so
/// that every file is small next to the folder even in the large viewport.
const PARTS: u64 = 80;

/// One big file and small files of the given `(bytes, tree category)`. Each
/// entry is spread over `PARTS` files that add up to exactly its bytes. The big
/// file is nine times the small ones together, so the group is a tenth of the
/// folder.
fn folder_with(members: &[(u64, u8)]) -> Spec {
    let total: u64 = members.iter().map(|m| m.0).sum();
    let mut s = Spec::new();
    s.file(0, total * 9);
    for &(size, category) in members {
        assert!(size >= 2 * PARTS, "parts must not be empty");
        let part = size / PARTS;
        for _ in 1..PARTS {
            s.add(SpecNode::file(0, part).cat(category));
        }
        s.add(SpecNode::file(0, size - part * (PARTS - 1)).cat(category));
    }
    s
}

/// The category of the one group in the folder's layout, in a viewport where
/// it is (not) labeled, and again after a refresh, which reads the group from
/// the snapshot instead of from what gathering saw.
fn group_category(spec: &Spec, viewport: Rect, labeled: bool) -> CategoryId {
    let snap = build(spec);
    let mut l = layout(&snap, &ViewTarget::Folder(id(0)), viewport, m13());
    let group = l
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap_or_else(|| panic!("a group is drawn"));
    assert_eq!(group.label.is_some(), labeled);
    let first = group.category;
    l.refresh(&snap);
    let again = l
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap()
        .category;
    assert_eq!(first, again, "a refresh changes no color");
    first
}

fn both_ways(spec: &Spec) -> CategoryId {
    let a = group_category(spec, LABELED, true);
    let b = group_category(spec, UNLABELED, false);
    assert_eq!(a, b, "labeled and unlabeled groups are colored alike");
    a
}

#[test]
fn exactly_sixty_percent_of_the_bytes_is_not_enough() {
    // 6000 of 10000 bytes in category 1.
    let spec = folder_with(&[(3000, 1), (3000, 1), (2000, 2), (2000, 3)]);
    assert_eq!(both_ways(&spec), CategoryId::NEUTRAL);
}

#[test]
fn one_byte_over_sixty_percent_takes_the_color() {
    // 6001 of 10000 bytes in category 1 (palette entry 0).
    let spec = folder_with(&[(3000, 1), (3001, 1), (1999, 2), (2000, 3)]);
    assert_eq!(both_ways(&spec), CategoryId(0));
    // The same holds for another category, and whatever the member order.
    let spec = folder_with(&[(1999, 1), (2000, 3), (3000, 8), (3001, 8)]);
    assert_eq!(both_ways(&spec), CategoryId(7));
}

#[test]
fn the_share_is_of_bytes_not_of_files() {
    // One huge file of category 4 beats many small ones of category 2.
    let mut members = vec![(14_000, 4)];
    members.extend(std::iter::repeat_n((200, 2), 30));
    assert_eq!(both_ways(&folder_with(&members)), CategoryId(3));
    // And three small ones in category 4 lose to a big one in category 2.
    let members = vec![(2000, 4), (2000, 4), (2000, 4), (10_000, 2)];
    assert_eq!(both_ways(&folder_with(&members)), CategoryId(1));
    // Not when the many together are still short of 60%.
    let members = vec![(2000, 4), (2000, 4), (2000, 4), (9000, 2)];
    assert_eq!(both_ways(&folder_with(&members)), CategoryId::NEUTRAL);
}

#[test]
fn the_largest_category_alone_is_not_enough_without_the_sixty_percent() {
    // 50% / 30% / 20%: a plurality, but not more than 60%.
    let spec = folder_with(&[(2500, 1), (2500, 1), (1500, 2), (1500, 2), (2000, 3)]);
    assert_eq!(both_ways(&spec), CategoryId::NEUTRAL);
    // Two categories tied at 50% each.
    let spec = folder_with(&[(2500, 1), (2500, 1), (2500, 2), (2500, 2)]);
    assert_eq!(both_ways(&spec), CategoryId::NEUTRAL);
}

#[test]
fn unclassified_bytes_count_as_other_and_can_dominate() {
    // Mostly unclassified: the group is drawn as Other, not neutral.
    let spec = folder_with(&[(3000, 0), (3100, 0), (2000, 1), (1900, 2)]);
    assert_eq!(both_ways(&spec), CategoryId::OTHER);
    // Unknown category numbers are Other too.
    let spec = folder_with(&[(3000, 0), (3100, 77), (2000, 1), (1900, 2)]);
    assert_eq!(both_ways(&spec), CategoryId::OTHER);
    // Other at exactly 60% is neutral like any category.
    let spec = folder_with(&[(3000, 0), (3000, 200), (2000, 1), (2000, 2)]);
    assert_eq!(both_ways(&spec), CategoryId::NEUTRAL);
}

#[test]
fn folders_among_the_members_count_as_other() {
    // Two empty folders of 4096 bytes each among small files.
    // `PARTS` empty folders of 4096 bytes each among files of category 1, which
    // together hold `files` bytes. Category 1 has more than 60% of the group's
    // bytes when files * 10 > (files + folders) * 6, that is above 1.5 times
    // the bytes of the folders.
    let folders = PARTS * 4096;
    let build = |files: u64| {
        let mut s = Spec::new();
        s.file(0, 10_000_000);
        for _ in 0..PARTS {
            s.dir(0);
        }
        let part = files / PARTS;
        for _ in 1..PARTS {
            s.add(SpecNode::file(0, part).cat(1));
        }
        s.add(SpecNode::file(0, files - part * (PARTS - 1)).cat(1));
        s
    };
    let sixty = folders * 3 / 2;
    assert_eq!(
        group_category(&build(sixty), LABELED, true),
        CategoryId::NEUTRAL
    );
    assert_eq!(
        group_category(&build(sixty + 1), LABELED, true),
        CategoryId(0)
    );
}

#[test]
fn the_members_keep_their_own_colors_when_the_group_has_none() {
    let spec = folder_with(&[(2500, 1), (2500, 1), (2500, 2), (2500, 2)]);
    let snap = build(&spec);
    let l = layout(&snap, &ViewTarget::Folder(id(0)), LABELED, m13());
    let group = l
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Group)
        .unwrap();
    assert_eq!(group.category, CategoryId::NEUTRAL);
    let (_, target) = l
        .enterable()
        .iter()
        .find(|(item, _)| *item == group.id)
        .unwrap();
    let inside = layout(&snap, target, LABELED, m13());
    let count = |category: CategoryId| {
        inside
            .items()
            .iter()
            .filter(|i| i.kind == ItemKind::FileTile && i.category == category)
            .count()
    };
    assert_eq!(count(CategoryId(0)), 2 * PARTS as usize);
    assert_eq!(count(CategoryId(1)), 2 * PARTS as usize);
}
