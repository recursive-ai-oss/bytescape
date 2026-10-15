//! The GPU backend draws the same pictures as the software backend, within a
//! small stated tolerance.
//!
//! These tests need a Vulkan adapter. They use a software one (llvmpipe or
//! lavapipe) so results do not depend on a graphics card. Without any adapter
//! each test prints `NOT RUN` and returns; set `BYTESCAPE_REQUIRE_GPU=1` to make
//! a missing adapter a failure. `BYTESCAPE_TEST_ADAPTER=default` uses the
//! system's default adapter instead.

use std::sync::Arc;
use std::time::Duration;

use bs_render::gpu::{
    AdapterChoice, BackendPreference, FailStage, Geometry, GpuContext, GpuError, GpuOptions,
    GpuRenderer, OffscreenBackend, choose_offscreen,
};
use bs_render::skia::{HatchTiles, SkiaCanvas};
use bs_render::{
    Bar, Canvas, Chrome, Flags, Hatch, HatchAngle, Item, ItemKind, ItemLabel, Palette, Rect,
    Renderer, TextEngine, ZOOM_DURATION, ZoomDirection, pixel,
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

fn options() -> GpuOptions {
    let adapter = match std::env::var("BYTESCAPE_TEST_ADAPTER").as_deref() {
        Ok("default") => AdapterChoice::Default,
        _ => AdapterChoice::Software,
    };
    GpuOptions {
        adapter,
        fail_at: None,
    }
}

/// The GPU for these tests, or `None` after printing why the test did not run.
fn gpu(test: &str) -> Option<Arc<GpuContext>> {
    match GpuContext::headless(&options()) {
        Ok(g) => {
            eprintln!("{test}: adapter {:?}", g.adapter_info().name);
            Some(g)
        }
        Err(e) => {
            assert!(
                std::env::var_os("BYTESCAPE_REQUIRE_GPU").is_none(),
                "{test}: a graphics adapter is required: {e}"
            );
            eprintln!("{test}: NOT RUN (no graphics adapter: {e})");
            None
        }
    }
}

fn gpu_renderer(gpu: &Arc<GpuContext>, w: u32, h: u32, s: f32, text: &TextEngine) -> GpuRenderer {
    let mut r = GpuRenderer::new(
        gpu.clone(),
        Geometry {
            width: w,
            height: h,
            scale: s,
            font_px: FONT_PX,
        },
        Palette::dark(),
        text.clone(),
        &options(),
    )
    .expect("a GPU renderer");
    r.set_bars(bars(w as f32, h as f32, s));
    let map_h = h as f32 - (26.0 * s).round();
    r.set_items(Arc::new(scene(w as f32, map_h, s)));
    r
}

fn sw_renderer(w: u32, h: u32, s: f32, text: &TextEngine) -> Renderer {
    let mut r = Renderer::new(w, h, s, FONT_PX, Palette::dark(), text.clone()).expect("renderer");
    r.set_threads(2);
    r.set_bars(bars(w as f32, h as f32, s));
    let map_h = h as f32 - (26.0 * s).round();
    r.set_items(Arc::new(scene(w as f32, map_h, s)));
    r
}

/// How far two pictures are apart.
#[derive(Debug)]
struct Diff {
    /// Pixels where any channel differs by more than `TOLERANCE`.
    over: usize,
    pixels: usize,
    max: u8,
}

impl Diff {
    fn fraction(&self) -> f64 {
        self.over as f64 / self.pixels as f64
    }
}

/// Channel difference allowed per pixel: premultiplied blending rounds
/// differently on the two backends.
const TOLERANCE: u8 = 3;

/// Channel difference allowed on the anti-aliased edge of a hatch line: the
/// software hatch is a rasterized tile and the GPU one is computed per pixel.
const EDGE_TOLERANCE: u8 = 16;

fn diff(a: &[u8], b: &[u8]) -> Diff {
    diff_within(a, b, TOLERANCE)
}

fn diff_within(a: &[u8], b: &[u8], tolerance: u8) -> Diff {
    assert_eq!(a.len(), b.len(), "picture sizes differ");
    let mut d = Diff {
        over: 0,
        pixels: a.len() / 4,
        max: 0,
    };
    for (x, y) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let m = (0..3).map(|i| x[i].abs_diff(y[i])).max().unwrap_or(0);
        d.max = d.max.max(m);
        if m > tolerance {
            d.over += 1;
        }
    }
    d
}

/// Saves a picture as a PNG in `$BYTESCAPE_TEST_DUMP`, when that is set, so a
/// failing comparison can be looked at.
fn dump(name: &str, rgba: &[u8], w: u32, h: u32) {
    let Some(dir) = std::env::var_os("BYTESCAPE_TEST_DUMP") else {
        return;
    };
    let Some(size) = tiny_skia::IntSize::from_wh(w, h) else {
        return;
    };
    if let Some(pm) = Pixmap::from_vec(rgba.to_vec(), size) {
        let _ = pm.save_png(std::path::Path::new(&dir).join(format!("{name}.png")));
    }
}

/// Software words (`0xAARRGGBB`) as straight RGBA.
fn words_rgba(words: &[u32]) -> Vec<u8> {
    words
        .iter()
        .flat_map(|&w| [(w >> 16) as u8, (w >> 8) as u8, w as u8, 255])
        .collect()
}

const CASES: [(u32, u32, f32); 3] = [(640, 487, 1.0), (733, 517, 1.5), (1000, 633, 2.0)];

/// At most this fraction of pixels may differ by more than the edge tolerance.
/// The software hatch tile doubles a pixel where a one-pixel line wraps around
/// the tile seam; the GPU draws that line evenly, so the scene's fine hatches
/// differ there.
const MAX_FRACTION: f64 = 0.02;

/// The same bound for a picture that is one hatch only.
const MAX_HATCH_FRACTION: f64 = 0.04;

#[test]
fn the_scene_matches_the_software_picture() {
    let Some(g) = gpu("scene") else { return };
    let text = fonts();
    for (w, h, s) in CASES {
        let want = sw_renderer(w, h, s, &text).snapshot_rgba();
        let got = gpu_renderer(&g, w, h, s, &text)
            .snapshot_rgba()
            .expect("readback");
        dump(&format!("scene-{w}-gpu"), &got, w, h);
        dump(&format!("scene-{w}-sw"), &want, w, h);
        let d = diff_within(&got, &want, EDGE_TOLERANCE);
        eprintln!("scene {w}x{h}@{s}: {d:?} ({:.4}%)", d.fraction() * 100.0);
        assert!(
            d.fraction() <= MAX_FRACTION,
            "{w}x{h}@{s}: {:.3}% of pixels differ by more than {EDGE_TOLERANCE}",
            d.fraction() * 100.0
        );
    }
}

#[test]
fn the_scene_really_draws_labels_hatches_and_states() {
    // A guard against both backends drawing nothing alike: the picture must
    // change when the labels are removed.
    let Some(g) = gpu("guard") else { return };
    let text = fonts();
    let (w, h, s) = CASES[0];
    let with = gpu_renderer(&g, w, h, s, &text)
        .snapshot_rgba()
        .expect("readback");
    let none = gpu_renderer(&g, w, h, s, &TextEngine::new())
        .snapshot_rgba()
        .expect("readback");
    assert!(diff(&with, &none).over > 2_000, "labels are drawn");
    let coloured = with
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] != p[1] || p[1] != p[2])
        .count();
    assert!(coloured > 500, "categories and hatches are coloured");
}

#[test]
fn hatches_in_both_directions_match_the_software_tiles() {
    let Some(g) = gpu("hatch") else { return };
    // The hatches the scene paints at scales 1, 1.5 and 2, plus others.
    let cases = [
        (HatchAngle::Rising, 14.0, 5.0, 140u8),
        (HatchAngle::Rising, 21.0, 7.5, 140),
        (HatchAngle::Falling, 6.0, 1.0, 204),
        (HatchAngle::Falling, 9.0, 1.5, 204),
        (HatchAngle::Falling, 12.0, 2.0, 204),
        (HatchAngle::Rising, 8.0, 2.0, 255),
        (HatchAngle::Falling, 5.0, 1.0, 255),
        (HatchAngle::Rising, 12.0, 3.0, 255),
    ];
    let mut bad = Vec::new();
    for (angle, spacing, width, alpha) in cases {
        {
            let hatch = Hatch {
                angle,
                spacing,
                line_width: width,
                color: bs_render::Color::rgba(20, 40, 60, alpha),
            };
            let rect = Rect::new(3.0, 5.0, 90.0, 70.0);
            let mut pm = Pixmap::new(96, 80).expect("pixmap");
            {
                let mut c = SkiaCanvas::new(pm.as_mut(), TextEngine::new(), HatchTiles::default());
                c.fill_rect(
                    Rect::new(0.0, 0.0, 96.0, 80.0),
                    bs_render::Color::rgb(250, 240, 200),
                );
                c.fill_pattern(rect, hatch);
            }
            let want = pixel::to_png_rgba(pm.data());
            let mut r = GpuRenderer::new(
                g.clone(),
                Geometry {
                    width: 96,
                    height: 80,
                    scale: 1.0,
                    font_px: FONT_PX,
                },
                Palette::dark(),
                TextEngine::new(),
                &options(),
            )
            .expect("renderer");
            let got = r.render_with(|c| {
                c.fill_rect(
                    Rect::new(0.0, 0.0, 96.0, 80.0),
                    bs_render::Color::rgb(250, 240, 200),
                );
                c.fill_pattern(rect, hatch);
            });
            let got = got.expect("readback");
            dump(
                &format!("hatch-{angle:?}-{spacing}-{alpha}-gpu"),
                &got,
                96,
                80,
            );
            dump(
                &format!("hatch-{angle:?}-{spacing}-{alpha}-sw"),
                &want,
                96,
                80,
            );
            let d = diff_within(&got, &want, EDGE_TOLERANCE);
            eprintln!("hatch {angle:?} {spacing}/{width}: {d:?}");
            if d.fraction() > MAX_HATCH_FRACTION {
                bad.push(format!("{angle:?} {spacing}/{width}/{alpha}: {d:?}"));
            }
        }
    }
    assert!(bad.is_empty(), "hatches differ: {bad:#?}");
}

#[test]
fn incremental_repaints_equal_a_full_repaint() {
    let Some(g) = gpu("incremental") else { return };
    let text = fonts();
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
        let mut inc = gpu_renderer(&g, w, h, s, &text);
        inc.paint_offscreen(Duration::ZERO).expect("first frame");
        for (hover, selected) in sequence {
            inc.set_hover(hover);
            inc.set_selected(selected);
            inc.paint_offscreen(Duration::ZERO).expect("frame");
        }
        let shown = inc.read_offscreen().expect("readback");
        let (last_hover, last_selected) = sequence[sequence.len() - 1];
        let mut full = gpu_renderer(&g, w, h, s, &text);
        full.set_hover(last_hover);
        full.set_selected(last_selected);
        full.paint_offscreen(Duration::ZERO).expect("frame");
        let want = full.read_offscreen().expect("readback");
        let d = diff(&shown, &want);
        assert_eq!(
            d.over, 0,
            "{w}x{h}@{s}: incremental differs from full: {d:?}"
        );
    }
}

/// A grid of `n` small tiles in folders, like the synthetic gate scene at a
/// small size.
fn tile_scene(w: f32, h: f32, n: usize) -> Vec<Item> {
    let mut v = vec![Item {
        rect: Rect::new(0.0, 0.0, w, h),
        kind: ItemKind::FolderTitled,
        depth: 0,
        label: None,
        id: 1,
        category: 0,
        flags: Flags::default(),
    }];
    let cols = (n as f32 * w / h).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let (cw, ch) = (w / cols as f32, h / rows as f32);
    for i in 0..n {
        let (c, r) = (i % cols, i / cols);
        v.push(Item {
            rect: Rect::new(c as f32 * cw + 0.2, r as f32 * ch + 0.3, cw - 0.9, ch - 0.8),
            kind: if i % 97 == 0 {
                ItemKind::Unreadable
            } else {
                ItemKind::FileTile
            },
            depth: 1,
            label: None,
            id: 2 + i as u32,
            category: (i % 9) as u8,
            flags: if i % 53 == 0 {
                Flags::DELETED
            } else {
                Flags::default()
            },
        });
    }
    v
}

#[test]
fn twenty_thousand_tiles_match_the_software_picture() {
    let Some(g) = gpu("tiles") else { return };
    let (w, h) = (1280u32, 800u32);
    let items = Arc::new(tile_scene(w as f32, h as f32, 20_000));
    let text = TextEngine::new();
    let mut sw =
        Renderer::new(w, h, 1.0, FONT_PX, Palette::light(), text.clone()).expect("renderer");
    sw.set_items(items.clone());
    let want = sw.snapshot_rgba();
    let mut r = GpuRenderer::new(
        g,
        Geometry {
            width: w,
            height: h,
            scale: 1.0,
            font_px: FONT_PX,
        },
        Palette::light(),
        text,
        &options(),
    )
    .expect("renderer");
    r.set_items(items);
    let got = r.snapshot_rgba().expect("readback");
    let d = diff_within(&got, &want, EDGE_TOLERANCE);
    eprintln!("20k tiles: {d:?}");
    assert!(d.fraction() <= MAX_FRACTION, "{d:?}");
}

fn zoom_view(parts: u32, w: f32, h: f32) -> Arc<Vec<Item>> {
    let m = Rect::new(0.0, 30.0, w, h - 56.0);
    let mut items = vec![Item {
        rect: m,
        kind: ItemKind::FolderTitled,
        depth: 0,
        label: None,
        id: 1,
        category: 0,
        flags: Flags::default(),
    }];
    let pw = (m.w - 8.0) / parts as f32;
    for i in 0..parts {
        items.push(Item {
            rect: Rect::new(m.x + 4.0 + i as f32 * pw, m.y + 24.0, pw - 2.0, m.h - 28.0),
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

#[test]
fn a_zoom_starts_and_ends_on_the_same_pictures_and_keeps_the_bars_still() {
    let Some(g) = gpu("zoom") else { return };
    let (w, h) = (320u32, 240u32);
    let chrome = Chrome {
        bars: vec![
            Bar {
                rect: Rect::new(0.0, 0.0, w as f32, 30.0),
                left: None,
                right: None,
                border_on_top: false,
            },
            Bar {
                rect: Rect::new(0.0, h as f32 - 26.0, w as f32, 26.0),
                left: None,
                right: None,
                border_on_top: true,
            },
        ],
        widgets: Vec::new(),
    };
    for direction in [ZoomDirection::In, ZoomDirection::Out] {
        let text = TextEngine::new();
        let mut sw =
            Renderer::new(w, h, 1.0, FONT_PX, Palette::light(), text.clone()).expect("renderer");
        sw.set_threads(1);
        sw.set_chrome(chrome.clone());
        sw.set_items(zoom_view(3, w as f32, h as f32));
        let mut words = vec![0u32; (w * h) as usize];
        sw.paint(&mut words, Duration::ZERO, true);
        let mut r = GpuRenderer::new(
            g.clone(),
            Geometry {
                width: w,
                height: h,
                scale: 1.0,
                font_px: FONT_PX,
            },
            Palette::light(),
            text,
            &options(),
        )
        .expect("renderer");
        r.set_chrome(chrome.clone());
        r.set_items(zoom_view(3, w as f32, h as f32));
        r.paint_offscreen(Duration::ZERO).expect("frame");
        let rest = r.read_offscreen().expect("readback");
        let child = Rect::new(4.0, 54.0, 100.0, 100.0);
        let target = zoom_view(5, w as f32, h as f32);
        sw.begin_zoom(direction, target.clone(), child, Duration::ZERO);
        r.begin_zoom(direction, target, child, Duration::ZERO);
        assert!(r.is_animating());
        let row = (w * 4) as usize;
        for ms in [0u64, 40, 75, 110] {
            let now = Duration::from_millis(ms);
            let p = sw.paint(&mut words, now, false);
            let q = r.paint_offscreen(now).expect("zoom frame");
            assert_eq!(p.animating, q.animating, "{ms} ms");
            let got = r.read_offscreen().expect("readback");
            // Bars do not move.
            assert_eq!(got[..30 * row], rest[..30 * row], "top bar at {ms} ms");
            let from = (h as usize - 26) * row;
            assert_eq!(got[from..], rest[from..], "bottom bar at {ms} ms");
            let d = diff_within(&got, &words_rgba(&words), EDGE_TOLERANCE);
            eprintln!(
                "zoom {direction:?} {ms} ms: {d:?} ({:.2}%)",
                d.fraction() * 100.0
            );
            // The software frame samples the nearest pixel and the GPU one
            // blends neighbours, so only the edges of the scaled boxes differ.
            let allowed = if ms == 0 { 0.0 } else { 0.05 };
            assert!(d.fraction() <= allowed, "{direction:?} {ms} ms: {d:?}");
        }
        let end = ZOOM_DURATION + Duration::from_millis(5);
        let p = sw.paint(&mut words, end, false);
        let q = r.paint_offscreen(end).expect("final frame");
        assert!(!p.animating && !q.animating);
        let d = diff(&r.read_offscreen().expect("readback"), &words_rgba(&words));
        assert_eq!(d.over, 0, "{direction:?} final frame: {d:?}");
    }
}

#[test]
fn reduced_motion_replaces_the_zoom_with_an_instant_change() {
    let Some(g) = gpu("reduced") else { return };
    let (w, h) = (320u32, 240u32);
    let mut r = GpuRenderer::new(
        g,
        Geometry {
            width: w,
            height: h,
            scale: 1.0,
            font_px: FONT_PX,
        },
        Palette::light(),
        TextEngine::new(),
        &options(),
    )
    .expect("renderer");
    r.set_reduced_motion(true);
    r.set_items(zoom_view(3, w as f32, h as f32));
    r.paint_offscreen(Duration::ZERO).expect("frame");
    r.begin_zoom(
        ZoomDirection::In,
        zoom_view(5, w as f32, h as f32),
        Rect::new(4.0, 54.0, 100.0, 100.0),
        Duration::ZERO,
    );
    assert!(!r.is_animating());
}

#[test]
fn resize_gives_a_picture_of_the_new_size() {
    let Some(g) = gpu("resize") else { return };
    let text = fonts();
    let mut r = gpu_renderer(&g, 400, 300, 1.0, &text);
    r.snapshot_rgba().expect("first");
    r.resize(500, 350, 1.5).expect("resize");
    r.set_bars(bars(500.0, 350.0, 1.5));
    r.set_items(Arc::new(scene(500.0, 320.0, 1.5)));
    assert_eq!(r.snapshot_rgba().expect("second").len(), 500 * 350 * 4);
    assert!(matches!(r.resize(0, 10, 1.0), Err(GpuError::Size { .. })));
}

#[test]
fn a_forced_failure_at_any_stage_falls_back_to_software() {
    for stage in [
        FailStage::Instance,
        FailStage::Adapter,
        FailStage::Device,
        FailStage::Pipeline,
    ] {
        let o = GpuOptions {
            fail_at: Some(stage),
            ..options()
        };
        match choose_offscreen(BackendPreference::Auto, &o) {
            OffscreenBackend::Software { reason } => {
                if reason.contains("forced failure") {
                    continue;
                }
                // Instance and Adapter fail before any adapter is looked up.
                // Later stages are only reached on a machine with an adapter;
                // without one the real lookup fails first, which is still a
                // fall back to software.
                assert!(
                    !matches!(stage, FailStage::Instance | FailStage::Adapter),
                    "{stage:?}: {reason}"
                );
                assert!(
                    reason.contains("no usable graphics adapter"),
                    "{stage:?}: {reason}"
                );
                assert!(
                    std::env::var_os("BYTESCAPE_REQUIRE_GPU").is_none(),
                    "{stage:?}: a graphics adapter is required: {reason}"
                );
                eprintln!("{stage:?}: NOT RUN (no graphics adapter: {reason})");
            }
            OffscreenBackend::Gpu(_) => panic!("{stage:?} did not fall back"),
        }
    }
}

#[test]
fn a_forced_renderer_failure_is_an_error_not_a_panic() {
    let Some(g) = gpu("renderer failure") else {
        return;
    };
    let o = GpuOptions {
        fail_at: Some(FailStage::Renderer),
        ..options()
    };
    let r = GpuRenderer::new(
        g,
        Geometry {
            width: 100,
            height: 100,
            scale: 1.0,
            font_px: FONT_PX,
        },
        Palette::light(),
        TextEngine::new(),
        &o,
    );
    assert!(matches!(r, Err(GpuError::Injected(FailStage::Renderer))));
}

#[test]
fn software_preference_never_touches_the_gpu() {
    match choose_offscreen(BackendPreference::Software, &options()) {
        OffscreenBackend::Software { reason } => assert!(reason.contains("requested")),
        OffscreenBackend::Gpu(_) => panic!("software was requested"),
    }
}

#[test]
fn a_panic_inside_the_graphics_stack_becomes_an_error() {
    let r: Result<(), GpuError> = bs_render::gpu::guard(|| panic!("driver exploded"));
    assert!(matches!(r, Err(GpuError::Panicked(m)) if m.contains("driver exploded")));
}

#[test]
fn thousands_of_labels_spill_into_more_atlas_pages_and_still_match() {
    let Some(g) = gpu("labels") else { return };
    let text = fonts();
    let (w, h) = (3000u32, 2000u32);
    let style = bs_render::TextStyle {
        size: FONT_PX,
        scale: 1.0,
        color: bs_render::Color::rgb(230, 230, 235),
    };
    let bg = bs_render::Color::rgb(30, 30, 34);
    let draw = |c: &mut dyn Canvas| {
        c.fill_rect(Rect::new(0.0, 0.0, w as f32, h as f32), bg);
        // The clip cuts labels at its right edge and below its bottom edge.
        c.push_clip(Rect::new(0.0, 0.0, 2990.5, 1500.0));
        for i in 0..5_600u32 {
            let (col, row) = (i % 50, i / 50);
            let name = format!("item-{i}-name");
            c.draw_text(
                &name,
                style,
                col as f32 * 60.0 + 1.3,
                row as f32 * 18.0 + 2.0,
                58.0,
            );
        }
        c.pop_clip();
    };
    let mut pm = Pixmap::new(w, h).expect("pixmap");
    {
        let mut c = SkiaCanvas::new(pm.as_mut(), text.clone(), HatchTiles::default());
        draw(&mut c);
    }
    let want = pixel::to_png_rgba(pm.data());
    let mut r = GpuRenderer::new(
        g,
        Geometry {
            width: w,
            height: h,
            scale: 1.0,
            font_px: FONT_PX,
        },
        Palette::dark(),
        text,
        &options(),
    )
    .expect("renderer");
    let got = r.render_with(|c| draw(c)).expect("readback");
    let d = diff(&got, &want);
    eprintln!("5600 labels: {d:?}");
    assert_eq!(d.over, 0, "{d:?}");
    let text_pixels = got.as_chunks::<4>().0.iter().filter(|p| p[0] > 100).count();
    assert!(text_pixels > 20_000, "labels were drawn: {text_pixels}");
}
