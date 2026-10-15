//! A group view whose members are all too small to draw still says what the
//! group is, instead of showing an empty frame.

use std::sync::Arc;

use bs_layout::Metrics;
use bs_render::{ItemKind, Rect};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::ViewSource;
use bs_ui::tree::TreeSource;

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        ..Meta::default()
    }
}

/// `/data` with one big file and `small` tiny ones, fully counted.
fn tree(small: u32) -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"big.bin", &meta(100_000_000))
        .unwrap();
    for n in 0..small {
        b.file(
            NodeKind::File,
            DirKey(0),
            format!("s{n}").as_bytes(),
            &meta(1_000),
        )
        .unwrap();
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.publish()
}

fn group_view(source: &mut TreeSource, map: Rect) -> u64 {
    let layout = source.layout(source.root(), map);
    let (_, view) = layout
        .folders
        .iter()
        .copied()
        .find(|(id, _)| {
            layout
                .items
                .iter()
                .any(|i| i.id == *id && i.kind == ItemKind::Group)
        })
        .expect("a group of small files");
    view
}

#[test]
fn a_group_whose_members_are_all_under_a_pixel_shows_its_summary_not_an_empty_frame() {
    let mut source = TreeSource::new(tree(20_000), Metrics::from_logical(13.0, 1.0)).unwrap();
    let view = group_view(&mut source, Rect::new(0.0, 0.0, 1200.0, 800.0));
    // A window so small that 20,000 members cannot each get a pixel.
    let layout = source.layout(view, Rect::new(0.0, 0.0, 120.0, 70.0));
    assert_eq!(layout.items.len(), 1, "only the frame is drawn");
    let frame = &layout.items[0];
    assert_eq!(
        frame.kind,
        ItemKind::FolderBlock,
        "a labeled block, not a title bar over nothing"
    );
    let label = frame.label.as_ref().expect("the group's summary");
    assert_eq!(&*label.name, "20,000 small files");
    assert_eq!(&*label.size, "20.0 MB");
}

#[test]
fn a_group_view_with_members_that_fit_keeps_its_frame_and_members() {
    let mut source = TreeSource::new(tree(300), Metrics::from_logical(13.0, 1.0)).unwrap();
    let view = group_view(&mut source, Rect::new(0.0, 0.0, 1200.0, 800.0));
    let layout = source.layout(view, Rect::new(0.0, 0.0, 1200.0, 800.0));
    assert!(layout.items.len() > 10);
    assert_eq!(layout.items[0].kind, ItemKind::FolderTitled);
}
