//! The hover card on the retained renderer: it repaints its own rectangles
//! and nothing else, and what is underneath comes back when it goes.

use std::sync::Arc;
use std::time::Duration;

use bs_render::{
    Card, CardPart, CardRole, Flags, Item, ItemKind, Palette, Rect, Renderer, TextEngine,
    ZoomDirection,
};

const W: u32 = 400;
const H: u32 = 300;

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

fn view() -> Arc<Vec<Item>> {
    Arc::new(vec![
        Item {
            rect: Rect::new(0.0, 0.0, W as f32, H as f32),
            kind: ItemKind::FolderTitled,
            depth: 0,
            label: None,
            id: 1,
            category: 0,
            flags: Flags::default(),
        },
        tile(2, 4.0, 24.0, 190.0, 120.0, 0),
        tile(3, 200.0, 24.0, 190.0, 120.0, 1),
        tile(4, 4.0, 150.0, 386.0, 140.0, 2),
    ])
}

fn card(at: Rect, text: &str) -> Card {
    Card {
        rect: at,
        parts: vec![CardPart::Line {
            rect: Rect::new(at.x + 6.0, at.y + 6.0, at.w - 12.0, 20.0),
            text: text.into(),
            role: CardRole::Title,
        }],
    }
}

fn renderer() -> Renderer {
    let mut r =
        Renderer::new(W, H, 1.0, 13.0, Palette::light(), TextEngine::new()).expect("renderer");
    r.set_threads(1);
    r.set_items(view());
    r
}

fn settle(r: &mut Renderer, t: &mut [u32]) {
    r.paint(t, Duration::ZERO, true);
}

#[test]
fn showing_the_card_repaints_only_its_rectangle() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    settle(&mut r, &mut t);
    let before = t.clone();
    let at = Rect::new(100.0, 60.0, 160.0, 70.0);
    assert!(r.set_card(Some(card(at, "folder"))));
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(painted.damage.len(), 1);
    let d = painted.damage[0];
    assert!(d.w < W as i32 / 2 && d.h < H as i32 / 2, "{d:?}");
    // The card's area changed; far away pixels did not.
    let px = |v: &[u32], x: u32, y: u32| v[(y * W + x) as usize];
    assert_ne!(px(&t, 120, 80), px(&before, 120, 80));
    assert_eq!(px(&t, 5, 5), px(&before, 5, 5));
    assert_eq!(px(&t, 390, 290), px(&before, 390, 290));
    // The damage covers every pixel that differs.
    for y in 0..H {
        for x in 0..W {
            if px(&t, x, y) != px(&before, x, y) {
                assert!(
                    d.to_rect().contains(x as f32 + 0.5, y as f32 + 0.5),
                    "pixel {x},{y} changed outside {d:?}"
                );
            }
        }
    }
}

#[test]
fn removing_the_card_restores_what_was_under_it() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    settle(&mut r, &mut t);
    let before = t.clone();
    r.set_card(Some(card(Rect::new(100.0, 60.0, 160.0, 70.0), "folder")));
    r.paint(&mut t, Duration::ZERO, false);
    assert!(r.set_card(None));
    r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(t, before);
    assert!(r.card().is_none());
}

#[test]
fn an_unchanged_card_costs_nothing_and_a_moved_one_repaints_both_places() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    settle(&mut r, &mut t);
    let a = Rect::new(20.0, 40.0, 120.0, 60.0);
    r.set_card(Some(card(a, "x")));
    r.paint(&mut t, Duration::ZERO, false);
    assert!(!r.set_card(Some(card(a, "x"))));
    assert!(
        r.paint(&mut t, Duration::ZERO, false).damage.is_empty(),
        "nothing to repaint"
    );
    let b = Rect::new(240.0, 160.0, 120.0, 60.0);
    r.set_card(Some(card(b, "x")));
    let d = r.paint(&mut t, Duration::ZERO, false).damage[0];
    assert!(
        d.to_rect().contains(30.5, 50.5) && d.to_rect().contains(250.5, 170.5),
        "{d:?}"
    );
}

#[test]
fn hover_and_selection_outlines_do_not_paint_over_the_card() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    settle(&mut r, &mut t);
    // A card that overlaps the edge of tile 3.
    let at = Rect::new(180.0, 60.0, 100.0, 50.0);
    r.set_card(Some(card(at, "card")));
    r.paint(&mut t, Duration::ZERO, false);
    let inside = t[(80 * W + 200) as usize];
    r.set_hover(Some(3));
    r.paint(&mut t, Duration::ZERO, false);
    r.set_selected(Some(3));
    r.paint(&mut t, Duration::ZERO, false);
    assert_eq!(t[(80 * W + 200) as usize], inside, "the card stays on top");
}

#[test]
fn a_full_repaint_and_a_zoom_keep_the_card_consistent() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    settle(&mut r, &mut t);
    r.set_card(Some(card(Rect::new(100.0, 60.0, 160.0, 70.0), "folder")));
    r.paint(&mut t, Duration::ZERO, false);
    let with_card = t.clone();
    // A palette change repaints everything, card included.
    r.set_palette(Palette::light());
    r.paint(&mut t, Duration::ZERO, true);
    assert_eq!(t, with_card);
    // The card can be set and cleared while a zoom runs without panicking,
    // and the picture afterwards has no card.
    r.begin_zoom(
        ZoomDirection::In,
        view(),
        Rect::new(4.0, 24.0, 190.0, 120.0),
        Duration::ZERO,
    );
    r.set_card(Some(card(Rect::new(10.0, 10.0, 50.0, 30.0), "x")));
    r.set_card(None);
    r.finish_animation();
    r.paint(&mut t, Duration::from_secs(1), true);
    let mut plain = renderer();
    let mut p = vec![0u32; (W * H) as usize];
    plain.paint(&mut p, Duration::ZERO, true);
    assert_eq!(t, p);
}

#[test]
fn a_card_at_the_window_edge_is_clipped_not_a_crash() {
    let mut r = renderer();
    let mut t = vec![0u32; (W * H) as usize];
    settle(&mut r, &mut t);
    r.set_card(Some(card(Rect::new(300.0, 250.0, 200.0, 100.0), "edge")));
    let painted = r.paint(&mut t, Duration::ZERO, false);
    assert!(!painted.damage.is_empty());
    r.set_card(Some(card(Rect::new(-50.0, -20.0, 100.0, 60.0), "corner")));
    r.paint(&mut t, Duration::ZERO, false);
}
