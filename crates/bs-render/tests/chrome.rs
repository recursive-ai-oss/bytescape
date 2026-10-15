//! Chrome widgets, legend dimming, the title pulse and multi-selection go
//! through the dirty-region path: what is repainted is small, and the picture
//! that results equals a full repaint.

use std::sync::Arc;
use std::time::Duration;

use bs_render::{
    Chrome, Flags, Glyph, Item, ItemKind, Palette, Rect, Renderer, SegmentKind, TextEngine,
    TextRole, Widget,
};

const W: u32 = 400;
const H: u32 = 260;

fn item(id: u32, kind: ItemKind, depth: u8, rect: Rect, category: u8, flags: Flags) -> Item {
    Item {
        rect,
        kind,
        depth,
        label: None,
        id,
        category,
        flags,
    }
}

fn view() -> Arc<Vec<Item>> {
    Arc::new(vec![
        item(
            1,
            ItemKind::FolderTitled,
            0,
            Rect::new(0.0, 40.0, W as f32, 190.0),
            0,
            Flags::default(),
        ),
        item(
            2,
            ItemKind::FileTile,
            1,
            Rect::new(4.0, 64.0, 150.0, 80.0),
            0,
            Flags::default(),
        ),
        item(
            3,
            ItemKind::FileTile,
            1,
            Rect::new(160.0, 64.0, 150.0, 80.0),
            1,
            Flags::default(),
        ),
        item(
            4,
            ItemKind::FolderTitled,
            1,
            Rect::new(4.0, 150.0, 300.0, 70.0),
            0,
            Flags::IN_PROGRESS,
        ),
    ])
}

fn chrome(text: &str) -> Chrome {
    Chrome {
        bars: Vec::new(),
        widgets: vec![
            Widget::Button {
                rect: Rect::new(4.0, 2.0, 28.0, 28.0),
                glyph: Glyph::Back,
                enabled: true,
                hot: false,
            },
            Widget::Text {
                rect: Rect::new(10.0, 232.0, 300.0, 24.0),
                text: text.into(),
                role: TextRole::Primary,
                hot: false,
            },
            Widget::Capacity {
                rect: Rect::new(0.0, 30.0, W as f32, 10.0),
                parts: vec![(SegmentKind::Used, 0.5), (SegmentKind::Free, 0.5)],
            },
        ],
    }
}

fn renderer() -> Renderer {
    let mut r =
        Renderer::new(W, H, 1.0, 13.0, Palette::light(), TextEngine::new()).expect("renderer");
    r.set_threads(1);
    r.set_items(view());
    r.set_chrome(chrome("one"));
    r
}

fn settled() -> (Renderer, Vec<u32>) {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    r.paint(&mut t, Duration::ZERO, false);
    (r, t)
}

fn full(r: &mut Renderer) -> Vec<u32> {
    let mut t = vec![0u32; (W * H) as usize];
    r.paint(&mut t, Duration::ZERO, true);
    t
}

#[test]
fn changing_one_widget_repaints_only_its_rectangle() {
    let (mut r, mut t) = settled();
    let before = t.clone();
    let mut next = chrome("two");
    // A different capacity too, to see both regions reported.
    if let Widget::Capacity { parts, .. } = &mut next.widgets[2] {
        parts[0].1 = 0.25;
        parts[1].1 = 0.75;
    }
    r.set_chrome(next);
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(painted.damage.len(), 1);
    let d = painted.damage[0];
    assert!(d.h < H as i32, "not the whole window: {d:?}");
    // The back button, which did not change, stays out of the damage.
    assert!(d.y >= 30, "{d:?}");
    for i in (0..t.len()).filter(|&i| t[i] != before[i]) {
        let (x, y) = ((i as u32 % W) as i32, (i as u32 / W) as i32);
        assert!(x >= d.x && x < d.right() && y >= d.y && y < d.bottom());
    }
    assert_ne!(t, before, "the capacity bar moved");
    // The same chrome again changes nothing.
    r.set_chrome(chrome_with_quarter());
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert!(painted.damage.is_empty());
}

fn chrome_with_quarter() -> Chrome {
    let mut c = chrome("two");
    if let Widget::Capacity { parts, .. } = &mut c.widgets[2] {
        parts[0].1 = 0.25;
        parts[1].1 = 0.75;
    }
    c
}

#[test]
fn a_hot_button_repaints_the_button_alone() {
    let (mut r, mut t) = settled();
    let mut next = chrome("one");
    if let Widget::Button { hot, .. } = &mut next.widgets[0] {
        *hot = true;
    }
    r.set_chrome(next);
    let painted = r.paint(&mut t, Duration::ZERO, false);
    let d = painted.damage[0];
    assert!(d.right() <= 40 && d.bottom() <= 40, "{d:?}");
}

#[test]
fn a_different_number_of_widgets_repaints_everything() {
    let (mut r, mut t) = settled();
    let mut next = chrome("one");
    next.widgets.pop();
    r.set_chrome(next);
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(
        (painted.damage[0].w, painted.damage[0].h),
        (W as i32, H as i32)
    );
}

#[test]
fn dimming_repaints_the_map_in_place_and_matches_a_full_repaint() {
    let (mut r, mut t) = settled();
    let before = t.clone();
    r.set_dim(Some(1));
    let painted = r.paint(&mut t, Duration::ZERO, false);
    let d = painted.damage[0];
    // The chrome above and below the map is not touched.
    assert!(d.y >= 40 && d.bottom() <= 230, "{d:?}");
    assert_ne!(t, before);
    assert_eq!(t, full(&mut r), "incremental dimming equals a full repaint");

    // Category 1 (tile 3) keeps its color; tile 2 (category 0) loses it.
    let pal = Palette::light();
    let px = |t: &[u32], x: u32, y: u32| t[(y * W + x) as usize] & 0x00FF_FFFF;
    let argb = |c: bs_render::Color| u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b);
    assert_eq!(px(&t, 200, 100), argb(pal.category_fill(1)));
    assert_ne!(px(&t, 50, 100), argb(pal.category_fill(0)));

    // Clearing restores the original picture.
    r.set_dim(None);
    r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(t, before);
}

#[test]
fn dimming_does_not_replace_the_items() {
    let (mut r, _) = settled();
    let ptr = r.items().as_ptr();
    r.set_dim(Some(2));
    r.set_dim(None);
    assert_eq!(r.items().as_ptr(), ptr, "no new layout was handed in");
}

#[test]
fn the_title_pulse_repaints_title_bars_of_unfinished_folders_only() {
    let mut r = renderer();
    let mut item4 = view().as_ref().clone();
    item4[3].label = Some(Box::new(bs_render::ItemLabel {
        name: "Scanning".into(),
        size: "≥ 1 MB".into(),
    }));
    r.set_items(Arc::new(item4));
    let mut t = vec![0u32; (W * H) as usize];
    r.paint(&mut t, Duration::ZERO, false);
    r.set_pulse(0.6);
    let painted = r.paint(&mut t, Duration::ZERO, false);
    let d = painted.damage[0];
    // Only the title bar of item 4 (y 150..) can be in the damage.
    assert!(d.y >= 150 && d.bottom() <= 150 + 30, "{d:?}");
    assert_eq!(t, full(&mut r));
    // The same value again is free.
    r.set_pulse(0.6);
    assert!(r.paint(&mut t, Duration::ZERO, false).damage.is_empty());
}

#[test]
fn several_boxes_can_be_selected_and_the_picture_matches_a_full_repaint() {
    let (mut r, mut t) = settled();
    r.set_selection(&[2, 3]);
    r.paint(&mut t, Duration::ZERO, false);
    r.set_selection(&[3]);
    r.paint(&mut t, Duration::ZERO, false);
    r.set_hover(Some(2));
    r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(t, full(&mut r));

    let mut fresh = renderer();
    fresh.set_selection(&[3]);
    fresh.set_hover(Some(2));
    assert_eq!(full(&mut fresh), t);
}

#[test]
fn the_selection_of_one_box_is_the_same_as_set_selected() {
    let mut a = renderer();
    a.set_selected(Some(3));
    let mut b = renderer();
    b.set_selection(&[3]);
    assert_eq!(a.snapshot_rgba(), b.snapshot_rgba());
}

#[test]
fn panels_and_pills_are_drawn_and_tracked_as_changes() {
    use bs_render::{Chrome, Widget};
    let a = Chrome {
        bars: Vec::new(),
        widgets: vec![
            Widget::Panel {
                rect: Rect::new(0.0, 0.0, 100.0, 24.0),
            },
            Widget::Pill {
                rect: Rect::new(60.0, 2.0, 36.0, 20.0),
                text: "Rescan".into(),
                accent: true,
                hot: false,
            },
        ],
    };
    let mut b = a.clone();
    if let Widget::Pill { hot, .. } = &mut b.widgets[1] {
        *hot = true;
    }
    let regions = a.changed_regions(&b).expect("same shape");
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0], Rect::new(60.0, 2.0, 36.0, 20.0));
}
