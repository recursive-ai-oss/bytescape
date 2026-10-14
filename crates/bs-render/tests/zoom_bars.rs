//! A zoom moves the map and leaves the bars where they are.

use std::sync::Arc;
use std::time::Duration;

use bs_render::{
    Bar, Chrome, Flags, Item, ItemKind, Palette, Rect, Renderer, TextEngine, Widget, ZOOM_DURATION,
    ZoomDirection,
};

const W: u32 = 320;
const H: u32 = 240;
const TOP: f32 = 30.0;
const BOTTOM: f32 = 26.0;

fn map() -> Rect {
    Rect::new(0.0, TOP, W as f32, H as f32 - TOP - BOTTOM)
}

fn view(parts: u32) -> Arc<Vec<Item>> {
    let m = map();
    let mut items = vec![Item {
        rect: m,
        kind: ItemKind::FolderTitled,
        depth: 0,
        label: None,
        id: 1,
        category: 0,
        flags: Flags::default(),
    }];
    let w = (m.w - 8.0) / parts as f32;
    for i in 0..parts {
        items.push(Item {
            rect: Rect::new(m.x + 4.0 + i as f32 * w, m.y + 24.0, w - 2.0, m.h - 28.0),
            kind: ItemKind::FileTile,
            depth: 1,
            label: None,
            id: 2 + i,
            category: (i % 8) as u8,
            flags: Flags::default(),
        });
    }
    Arc::new(items)
}

/// Bars with one legend swatch in the top bar. A swatch draws without fonts,
/// so the test sees its change on any machine.
fn chrome(category: u8) -> Chrome {
    Chrome {
        bars: vec![
            Bar {
                rect: Rect::new(0.0, 0.0, W as f32, TOP),
                left: None,
                right: None,
                border_on_top: false,
            },
            Bar {
                rect: Rect::new(0.0, H as f32 - BOTTOM, W as f32, BOTTOM),
                left: None,
                right: None,
                border_on_top: true,
            },
        ],
        widgets: vec![Widget::Swatch {
            rect: Rect::new(8.0, 0.0, 40.0, TOP),
            category,
            name: None,
            hot: false,
        }],
    }
}

fn renderer() -> Renderer {
    let mut r =
        Renderer::new(W, H, 1.0, 13.0, Palette::light(), TextEngine::new()).expect("renderer");
    r.set_threads(1);
    r.set_chrome(chrome(1));
    r.set_items(view(3));
    r
}

fn rows(t: &[u32], from: u32, to: u32) -> Vec<u32> {
    t[(from * W) as usize..(to * W) as usize].to_vec()
}

#[test]
fn the_bars_stay_still_while_the_map_zooms_in_and_out() {
    for direction in [ZoomDirection::In, ZoomDirection::Out] {
        let mut r = renderer();
        let mut t = vec![0u32; (W * H) as usize];
        r.paint(&mut t, Duration::ZERO, true);
        let rest = t.clone();
        let child = Rect::new(4.0, TOP + 24.0, 100.0, 100.0);
        r.begin_zoom(direction, view(3), child, Duration::ZERO);
        let top = rows(&rest, 0, TOP as u32);
        let bottom = rows(&rest, H - BOTTOM as u32, H);
        for ms in [10, 40, 75, 110] {
            let p = r.paint(&mut t, Duration::from_millis(ms), false);
            assert!(p.animating, "{ms} ms");
            assert!(rows(&t, 0, TOP as u32) == top, "top bar moved at {ms} ms");
            assert!(
                rows(&t, H - BOTTOM as u32, H) == bottom,
                "bottom bar moved at {ms} ms"
            );
        }
        let p = r.paint(&mut t, ZOOM_DURATION + Duration::from_millis(5), false);
        assert!(!p.animating);
    }
}

#[test]
fn a_bar_that_changes_during_the_zoom_shows_the_change_in_the_next_frame() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    r.paint(&mut t, Duration::ZERO, true);
    r.begin_zoom(
        ZoomDirection::In,
        view(3),
        Rect::new(4.0, TOP + 24.0, 100.0, 100.0),
        Duration::ZERO,
    );
    r.paint(&mut t, Duration::from_millis(20), false);
    let before = rows(&t, 0, TOP as u32);
    assert!(r.set_chrome(chrome(4)));
    r.paint(&mut t, Duration::from_millis(40), false);
    let after = rows(&t, 0, TOP as u32);
    assert!(before != after, "the swatch in the top bar did not change");
    // Still the same after the zoom ends.
    r.paint(&mut t, ZOOM_DURATION + Duration::from_millis(5), false);
    r.paint(&mut t, ZOOM_DURATION + Duration::from_millis(40), true);
    assert!(rows(&t, 0, TOP as u32) == after, "the bar changed back");
}

#[test]
fn a_view_without_a_root_box_zooms_the_whole_window() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    r.paint(&mut t, Duration::ZERO, true);
    let items: Arc<Vec<Item>> = Arc::new(view(3).iter().skip(1).cloned().collect());
    r.begin_zoom(
        ZoomDirection::In,
        items,
        Rect::new(4.0, TOP + 24.0, 100.0, 100.0),
        Duration::ZERO,
    );
    let p = r.paint(&mut t, Duration::from_millis(40), false);
    assert!(p.animating);
}
