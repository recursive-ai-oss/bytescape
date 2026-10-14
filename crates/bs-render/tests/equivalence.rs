//! Tiled repaints and incremental repaints must produce exactly the picture a
//! single, unbanded pass produces. The scene mixes every kind of box, the
//! deleted and dimmed states, hatches, labels in several scripts and
//! fractional rectangles that straddle band edges, at several scales.

use std::sync::Arc;
use std::time::Duration;

use bs_render::scene::{SceneStyle, draw_bars, draw_items};
use bs_render::skia::{HatchTiles, SkiaCanvas};
use bs_render::{
    Bar, Canvas, Flags, Item, ItemKind, ItemLabel, Palette, Rect, Renderer, TextEngine, pixel,
};
use tiny_skia::Pixmap;

const FONT_PX: f32 = 13.0;

fn label(name: &str, size: &str) -> Option<Box<ItemLabel>> {
    Some(Box::new(ItemLabel {
        name: name.into(),
        size: size.into(),
    }))
}

/// A grid of boxes with fractional edges, one of every kind, cycling through
/// the state flags. Names mix Latin, Hebrew, Arabic, CJK and combining marks.
fn scene(w: f32, h: f32, scale: f32) -> Vec<Item> {
    let mut v = vec![Item {
        rect: Rect::new(0.0, 0.0, w, h),
        kind: ItemKind::FolderTitled,
        depth: 0,
        label: label("root-folder-with-a-rather-long-name-indeed", "2.0 TB"),
        id: 1,
        category: 0,
        flags: Flags::default(),
    }];
    let kinds = [
        ItemKind::FolderTitled,
        ItemKind::FolderBlock,
        ItemKind::FileTile,
        ItemKind::Group,
        ItemKind::Free,
        ItemKind::Unreadable,
        ItemKind::Reserved,
        ItemKind::RecycleBin,
    ];
    let flags = [
        Flags::default(),
        Flags::DELETED,
        Flags::DIMMED,
        Flags::DELETED | Flags::DIMMED,
    ];
    let names = [
        "item-{}-name.txt",
        "item-{}-\u{05e9}\u{05dc}\u{05d5}\u{05dd}.txt",
        "item-{}-\u{0645}\u{0631}\u{062d}\u{0628}\u{0627}",
        "item-{}-\u{5199}\u{771f}\u{30a2}\u{30eb}\u{30d0}\u{30e0}",
        "item-{}-e\u{0301}\u{0301}\u{0301}-combining",
        "item-{}-\u{202e}rtl\u{200b}zero-width",
    ];
    let (cols, rows) = (6usize, 7usize);
    let (cw, ch) = ((w - 8.0) / cols as f32, (h - 30.0) / rows as f32);
    let mut id = 2u32;
    for r in 0..rows {
        for c in 0..cols {
            let n = r * cols + c;
            // Edges land between pixels and between bands.
            let rect = Rect::new(
                4.0 + c as f32 * cw + 0.37,
                26.0 + r as f32 * ch + 0.61,
                cw - 1.3,
                ch - 1.7,
            );
            v.push(Item {
                rect,
                kind: kinds[n % kinds.len()],
                depth: 1 + (c % 4) as u8,
                label: label(
                    &names[n % names.len()].replace("{}", &id.to_string()),
                    "12.3 GB",
                ),
                id,
                category: (id % 11) as u8,
                flags: flags[(r + c) % flags.len()],
            });
            id += 1;
        }
    }
    // A box that starts above a band edge and ends below it, with a hatch.
    let tall = 3.0 * scale * 40.0;
    v.push(Item {
        rect: Rect::new(w - 90.0, 10.5, 61.7, tall.min(h - 12.0)),
        kind: ItemKind::Unreadable,
        depth: 1,
        label: label("tall-unreadable", "1 GB"),
        id,
        category: 0,
        flags: Flags::DELETED,
    });
    v
}

fn bars(w: f32, h: f32, scale: f32) -> Vec<Bar> {
    let bar_h = (26.0 * scale).round();
    vec![Bar {
        rect: Rect::new(0.0, h - bar_h, w, bar_h),
        left: Some("info · 41.2 GB · שלום".into()),
        right: Some("Find more free software at RecursiveAI.org".into()),
        border_on_top: true,
    }]
}

fn fonts() -> TextEngine {
    let text = TextEngine::new();
    text.load_fonts();
    assert!(
        text.measure("Sample", FONT_PX, 1.0).unwrap_or(0.0) > 0.0,
        "these tests need a system font; none was found"
    );
    text
}

fn renderer(w: u32, h: u32, scale: f32, threads: usize, text: &TextEngine) -> Renderer {
    let mut r = Renderer::new(w, h, scale, FONT_PX, Palette::dark(), text.clone())
        .expect("a renderer for a non-empty window");
    r.set_threads(threads);
    r.set_bars(bars(w as f32, h as f32, scale));
    let map_h = h as f32 - (26.0 * scale).round();
    r.set_items(Arc::new(scene(w as f32, map_h, scale)));
    r
}

/// The picture from one canvas over the whole pixmap, with no bands.
fn reference(w: u32, h: u32, scale: f32, text: &TextEngine) -> Vec<u8> {
    let pal = Palette::dark();
    let style = SceneStyle::new(scale, FONT_PX, true);
    let mut pm = Pixmap::new(w, h).expect("pixmap");
    {
        let mut canvas = SkiaCanvas::new(pm.as_mut(), text.clone(), HatchTiles::default());
        canvas.fill_rect(Rect::new(0.0, 0.0, w as f32, h as f32), pal.bg);
        let map_h = h as f32 - (26.0 * scale).round();
        draw_items(
            &mut canvas,
            &scene(w as f32, map_h, scale),
            &pal,
            &style,
            None,
        );
        draw_bars(
            &mut canvas,
            &bars(w as f32, h as f32, scale),
            &pal,
            &style,
            None,
        );
    }
    pixel::to_png_rgba(pm.data())
}

/// Sizes chosen so band heights do not divide the window height.
const CASES: [(u32, u32, f32); 3] = [(640, 487, 1.0), (733, 517, 1.5), (1000, 633, 2.0)];

fn differing(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| x != y)
        .count()
        + a.len().abs_diff(b.len())
}

#[test]
fn tiled_repaint_equals_one_unbanded_pass() {
    let text = fonts();
    for (w, h, s) in CASES {
        let want = reference(w, h, s, &text);
        for threads in [1, 2, 7, 16] {
            let got = renderer(w, h, s, threads, &text).snapshot_rgba();
            assert_eq!(
                differing(&got, &want),
                0,
                "{w}x{h} at scale {s} with {threads} threads differs from the unbanded picture"
            );
        }
    }
}

#[test]
fn single_thread_equals_many_threads() {
    let text = fonts();
    for (w, h, s) in CASES {
        let one = renderer(w, h, s, 1, &text).snapshot_rgba();
        for threads in [2, 7, 16] {
            let many = renderer(w, h, s, threads, &text).snapshot_rgba();
            assert_eq!(differing(&one, &many), 0, "{w}x{h}@{s} threads {threads}");
        }
    }
}

#[test]
fn the_test_scene_really_draws_labels_and_hatches() {
    // A guard against the scene silently turning into solid fills: the picture
    // must differ when labels or hatches are removed.
    let text = fonts();
    let (w, h, s) = CASES[0];
    let with_labels = reference(w, h, s, &text);
    let no_fonts = {
        let blank = TextEngine::new();
        let pal = Palette::dark();
        let style = SceneStyle::new(s, FONT_PX, true);
        let mut pm = Pixmap::new(w, h).expect("pixmap");
        {
            let mut canvas = SkiaCanvas::new(pm.as_mut(), blank, HatchTiles::default());
            canvas.fill_rect(Rect::new(0.0, 0.0, w as f32, h as f32), pal.bg);
            let map_h = h as f32 - (26.0 * s).round();
            draw_items(&mut canvas, &scene(w as f32, map_h, s), &pal, &style, None);
        }
        pixel::to_png_rgba(pm.data())
    };
    assert!(
        differing(&with_labels, &no_fonts) > 2_000,
        "labels are drawn"
    );
    let hatch_pixels = with_labels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] != p[1] || p[1] != p[2])
        .count();
    assert!(
        hatch_pixels > 500,
        "coloured pixels from categories and hatches"
    );
}

#[test]
fn incremental_repaint_equals_full_repaint() {
    let text = fonts();
    // Hover and selection moves across items of every kind and state.
    let sequence: [(Option<u32>, Option<u32>); 8] = [
        (Some(3), None),
        (Some(9), Some(9)),
        (Some(1), Some(14)),
        (Some(20), Some(21)),
        (None, Some(7)),
        (Some(7), Some(1)),
        (Some(26), Some(26)),
        (Some(43), Some(30)),
    ];
    for (w, h, s) in CASES {
        for threads in [1, 4] {
            let mut inc = renderer(w, h, s, threads, &text);
            let mut shown = vec![0u32; (w * h) as usize];
            inc.paint(&mut shown, Duration::ZERO, true);
            for (hover, selected) in sequence {
                inc.set_hover(hover);
                inc.set_selected(selected);
                inc.paint(&mut shown, Duration::ZERO, false);
            }
            let (last_hover, last_selected) = sequence[sequence.len() - 1];
            let mut full = renderer(w, h, s, 1, &text);
            full.set_hover(last_hover);
            full.set_selected(last_selected);
            let mut want = vec![0u32; (w * h) as usize];
            full.paint(&mut want, Duration::ZERO, true);
            let diff = shown.iter().zip(&want).filter(|(a, b)| a != b).count();
            assert_eq!(
                diff, 0,
                "{w}x{h}@{s} threads {threads}: {diff} pixels differ after incremental repaints"
            );
        }
    }
}
