//! Behaviour of the retained renderer: dirty regions, tiling, and the zoom.

use std::sync::Arc;
use std::time::Duration;

use bs_render::{
    Flags, Item, ItemKind, Palette, Rect, Renderer, TextEngine, ZOOM_DURATION, ZoomDirection,
};

const W: u32 = 320;
const H: u32 = 200;

fn tile(id: u32, x: f32, y: f32, w: f32, h: f32, category: u8) -> Item {
    Item {
        rect: Rect::new(x, y, w, h),
        kind: ItemKind::FileTile,
        depth: 1,
        label: None,
        id,
        category,
        flags: Flags::default(),
    }
}

fn folder(id: u32, rect: Rect) -> Item {
    Item {
        rect,
        kind: ItemKind::FolderTitled,
        depth: 0,
        label: None,
        id,
        category: 0,
        flags: Flags::default(),
    }
}

fn view_a() -> Arc<Vec<Item>> {
    Arc::new(vec![
        folder(1, Rect::new(0.0, 0.0, W as f32, H as f32)),
        tile(2, 4.0, 24.0, 150.0, 80.0, 0),
        tile(3, 160.0, 24.0, 150.0, 80.0, 1),
        tile(4, 4.0, 110.0, 300.0, 80.0, 2),
    ])
}

fn view_b() -> Arc<Vec<Item>> {
    Arc::new(vec![
        folder(1, Rect::new(0.0, 0.0, W as f32, H as f32)),
        tile(2, 4.0, 24.0, 100.0, 160.0, 3),
        tile(3, 110.0, 24.0, 200.0, 160.0, 4),
    ])
}

fn renderer() -> Renderer {
    let mut r =
        Renderer::new(W, H, 1.0, 13.0, Palette::light(), TextEngine::new()).expect("renderer");
    r.set_threads(1);
    r
}

fn target() -> Vec<u32> {
    vec![0; (W * H) as usize]
}

#[test]
fn the_first_paint_covers_the_whole_window() {
    let mut r = renderer();
    r.set_items(view_a());
    let mut t = target();
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(painted.damage.len(), 1);
    assert_eq!(
        (painted.damage[0].w, painted.damage[0].h),
        (W as i32, H as i32)
    );
    assert!(!painted.animating);
    assert!(t.iter().all(|&p| p >> 24 == 0xFF), "every pixel is opaque");
}

#[test]
fn hover_repaints_only_the_edges_of_the_box() {
    let mut r = renderer();
    r.set_items(view_a());
    let mut t = target();
    r.paint(&mut t, Duration::ZERO, false);
    let before = t.clone();
    r.set_hover(Some(2));
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(painted.damage.len(), 1);
    let d = painted.damage[0];
    // The damage stays inside the hovered box and is not the whole window.
    assert!(
        d.x >= 0 && d.y >= 20 && d.right() <= 160 && d.bottom() <= 110,
        "{d:?}"
    );
    let changed: Vec<usize> = (0..t.len()).filter(|&i| t[i] != before[i]).collect();
    assert!(!changed.is_empty(), "an outline appears");
    for i in changed {
        let (x, y) = ((i as u32 % W) as i32, (i as u32 / W) as i32);
        assert!(x >= d.x && x < d.right() && y >= d.y && y < d.bottom());
    }
}

#[test]
fn moving_the_hover_matches_a_full_repaint() {
    let mut a = renderer();
    a.set_items(view_a());
    let mut t = target();
    a.paint(&mut t, Duration::ZERO, false);
    a.set_hover(Some(2));
    a.paint(&mut t, Duration::ZERO, false);
    a.set_hover(Some(3));
    a.set_selected(Some(4));
    a.paint(&mut t, Duration::ZERO, false);

    let mut b = renderer();
    b.set_items(view_a());
    b.set_hover(Some(3));
    b.set_selected(Some(4));
    let mut full = target();
    b.paint(&mut full, Duration::ZERO, false);
    assert_eq!(t, full, "incremental repaint equals a full one");
}

#[test]
fn tiling_does_not_change_the_picture() {
    let items = view_a();
    let mut single = renderer();
    single.set_items(items.clone());
    let mut wide = renderer();
    wide.set_threads(8);
    wide.set_items(items);
    assert_eq!(single.snapshot_rgba(), wide.snapshot_rgba());
}

#[test]
fn a_zoom_animates_then_lands_on_the_full_quality_view() {
    let mut r = renderer();
    r.set_items(view_a());
    let mut t = target();
    r.paint(&mut t, Duration::ZERO, false);

    let child = Rect::new(4.0, 24.0, 150.0, 80.0);
    r.begin_zoom(ZoomDirection::In, view_b(), child, Duration::ZERO);
    assert!(r.is_animating());

    let mid = r.paint(&mut t, ZOOM_DURATION / 2, false);
    assert!(mid.animating);
    let at_start = {
        let mut r2 = renderer();
        r2.set_items(view_a());
        let mut t2 = target();
        r2.paint(&mut t2, Duration::ZERO, false);
        t2
    };
    assert_ne!(
        t, at_start,
        "the middle of a zoom differs from where it began"
    );

    let end = r.paint(&mut t, ZOOM_DURATION, false);
    assert!(!end.animating);
    assert!(!r.is_animating());
    let mut fresh = renderer();
    fresh.set_items(view_b());
    let mut want = target();
    fresh.paint(&mut want, Duration::ZERO, false);
    assert_eq!(t, want, "the end of a zoom is exactly the new view");
}

#[test]
fn a_zoom_out_ends_on_the_parent_view() {
    let mut r = renderer();
    r.set_items(view_b());
    let mut t = target();
    r.paint(&mut t, Duration::ZERO, false);
    r.begin_zoom(
        ZoomDirection::Out,
        view_a(),
        Rect::new(4.0, 24.0, 150.0, 80.0),
        Duration::ZERO,
    );
    assert!(r.paint(&mut t, Duration::from_millis(10), false).animating);
    r.paint(&mut t, ZOOM_DURATION * 2, false);
    let mut fresh = renderer();
    fresh.set_items(view_a());
    let mut want = target();
    fresh.paint(&mut want, Duration::ZERO, false);
    assert_eq!(t, want);
}

#[test]
fn reduced_motion_replaces_the_animation_with_an_instant_change() {
    let mut r = renderer();
    r.set_reduced_motion(true);
    r.set_items(view_a());
    let mut t = target();
    r.paint(&mut t, Duration::ZERO, false);
    r.begin_zoom(
        ZoomDirection::In,
        view_b(),
        Rect::new(4.0, 24.0, 150.0, 80.0),
        Duration::ZERO,
    );
    assert!(!r.is_animating());
    let painted = r.paint(&mut t, Duration::from_millis(1), false);
    assert!(!painted.animating);
}

#[test]
fn skipping_an_animation_jumps_to_the_end() {
    let mut r = renderer();
    r.set_items(view_a());
    let mut t = target();
    r.paint(&mut t, Duration::ZERO, false);
    r.begin_zoom(
        ZoomDirection::In,
        view_b(),
        Rect::new(4.0, 24.0, 150.0, 80.0),
        Duration::ZERO,
    );
    r.finish_animation();
    assert!(!r.is_animating());
    assert_eq!(r.items().len(), 3);
}

#[test]
fn resizing_drops_the_old_view_and_paints_at_the_new_size() {
    let mut r = renderer();
    r.set_items(view_a());
    r.resize(640, 400, 2.0);
    assert_eq!(r.size(), (640, 400));
    assert_eq!(r.scale(), 2.0);
    assert!(r.items().is_empty());
    let mut t = vec![0u32; 640 * 400];
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(painted.damage[0].w, 640);
}

#[test]
fn labels_appear_once_fonts_are_ready_without_a_new_view() {
    let text = TextEngine::new();
    let mut r = Renderer::new(W, H, 1.0, 13.0, Palette::light(), text.clone()).expect("renderer");
    r.set_threads(1);
    let mut items = (*view_a()).clone();
    items[0].label = Some(Box::new(bs_render::ItemLabel {
        name: "Documents".into(),
        size: "1.2 GB".into(),
    }));
    r.set_items(Arc::new(items));
    let mut t = target();
    r.paint(&mut t, Duration::ZERO, false);
    let without = t.clone();
    text.load_fonts();
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert!(!painted.damage.is_empty());
    assert_ne!(t, without, "title text is drawn after the fonts arrive");
}

#[test]
fn hit_testing_returns_the_topmost_box() {
    let mut r = renderer();
    r.set_items(view_a());
    assert_eq!(r.item_at(10.0, 30.0).map(|i| i.id), Some(2));
    assert_eq!(r.item_at(2.0, 2.0).map(|i| i.id), Some(1));
    assert!(r.item_at(-5.0, 2.0).is_none());
}
