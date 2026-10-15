//! Choosing a backend for a renderer, falling back, and carrying a picture's
//! state from one backend to the other.
//!
//! The tests that need a GPU use a software adapter (llvmpipe). Without any
//! adapter they print `NOT RUN` and return; `BYTESCAPE_REQUIRE_GPU=1` makes a
//! missing adapter a failure.

use std::sync::Arc;

use bs_render::gpu::{
    AdapterChoice, BackendPreference, FailStage, Geometry, GpuContext, GpuOptions, GpuRenderer,
};
use bs_render::{
    AnyRenderer, Card, Flags, Item, ItemKind, Palette, Rect, Renderer, TextEngine, drawing_line,
};

const FONT_PX: f32 = 13.0;

fn options() -> GpuOptions {
    GpuOptions {
        adapter: AdapterChoice::Software,
        fail_at: None,
    }
}

fn geometry() -> Geometry {
    Geometry {
        width: 300,
        height: 200,
        scale: 1.0,
        font_px: FONT_PX,
    }
}

fn items() -> Arc<Vec<Item>> {
    let mut v = vec![Item {
        rect: Rect::new(0.0, 0.0, 300.0, 200.0),
        kind: ItemKind::FolderTitled,
        depth: 0,
        label: None,
        id: 1,
        category: 0,
        flags: Flags::default(),
    }];
    for i in 0..6u32 {
        v.push(Item {
            rect: Rect::new(6.0 + i as f32 * 48.0, 30.0, 44.0, 120.0),
            kind: if i == 4 {
                ItemKind::Unreadable
            } else {
                ItemKind::FileTile
            },
            depth: 1,
            label: None,
            id: 2 + i,
            category: (i % 8) as u8,
            flags: if i == 2 {
                Flags::DELETED
            } else {
                Flags::default()
            },
        });
    }
    Arc::new(v)
}

/// A GPU, or `None` after printing why a test did not run.
fn gpu(test: &str) -> Option<Arc<GpuContext>> {
    match GpuContext::headless(&options()) {
        Ok(g) => Some(g),
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

#[test]
fn software_preference_gives_a_software_renderer_without_a_reason() {
    let (r, why) = AnyRenderer::offscreen_with(
        geometry(),
        Palette::light(),
        TextEngine::new(),
        BackendPreference::Software,
        &options(),
    )
    .expect("a renderer");
    assert!(matches!(r, AnyRenderer::Software(_)));
    assert_eq!(why, None);
    assert_eq!(r.backend_name(), "software");
}

#[test]
fn automatic_preference_draws_offscreen_pictures_in_software() {
    // A picture made without asking for the GPU is the same on every machine.
    let (r, why) = AnyRenderer::offscreen_with(
        geometry(),
        Palette::light(),
        TextEngine::new(),
        BackendPreference::Auto,
        &options(),
    )
    .expect("a renderer");
    assert!(matches!(r, AnyRenderer::Software(_)));
    assert_eq!(why, None);
}

#[test]
fn a_forced_failure_at_each_stage_gives_software_and_a_reason() {
    for stage in [
        FailStage::Instance,
        FailStage::Adapter,
        FailStage::Device,
        FailStage::Pipeline,
        FailStage::Renderer,
    ] {
        let o = GpuOptions {
            fail_at: Some(stage),
            ..options()
        };
        let (r, why) = AnyRenderer::offscreen_with(
            geometry(),
            Palette::light(),
            TextEngine::new(),
            BackendPreference::Gpu,
            &o,
        )
        .expect("a renderer even when the GPU fails");
        assert!(matches!(r, AnyRenderer::Software(_)), "{stage:?}");
        let why = why.expect("a reason");
        if why.contains("forced failure") {
            continue;
        }
        // Without an adapter the real lookup fails before the later stages.
        assert!(
            !matches!(stage, FailStage::Instance | FailStage::Adapter),
            "{stage:?}: {why}"
        );
        assert!(
            why.contains("no usable graphics adapter"),
            "{stage:?}: {why}"
        );
        assert!(
            std::env::var_os("BYTESCAPE_REQUIRE_GPU").is_none(),
            "{stage:?}: a graphics adapter is required: {why}"
        );
        eprintln!("{stage:?}: NOT RUN (no graphics adapter)");
    }
}

#[test]
fn the_gpu_is_used_when_asked_for_and_available() {
    let Some(_g) = gpu("gpu preference") else {
        return;
    };
    let (r, why) = AnyRenderer::offscreen_with(
        geometry(),
        Palette::light(),
        TextEngine::new(),
        BackendPreference::Gpu,
        &options(),
    )
    .expect("a renderer");
    assert!(matches!(r, AnyRenderer::Gpu(_)), "{why:?}");
    assert!(r.adapter_name().is_some());
}

#[test]
fn drawing_is_described_in_plain_words() {
    assert_eq!(drawing_line(None, None), "Drawing: software");
    assert_eq!(
        drawing_line(Some("Example GPU"), None),
        "Drawing: GPU (Example GPU)"
    );
    let line = drawing_line(None, Some("forced failure at Device"));
    assert_eq!(line, "Drawing: software, because forced failure at Device");
    // A long reason is cut to one line, and only its first line is used.
    let long = format!(
        "graphics device: first line\nsecond line {}",
        "x".repeat(200)
    );
    let line = drawing_line(None, Some(&long));
    assert!(line.starts_with("Drawing: software, because first line"));
    assert!(!line.contains("second line"));
    for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
        assert!(!line.to_lowercase().contains(banned));
    }
}

#[test]
fn a_gpu_renderer_hands_its_state_to_a_software_one() {
    let Some(g) = gpu("to software") else { return };
    let mut gr = GpuRenderer::new(
        g,
        geometry(),
        Palette::dark(),
        TextEngine::new(),
        &options(),
    )
    .expect("renderer");
    gr.set_items(items());
    gr.set_selection(&[3, 5]);
    gr.set_hover(Some(4));
    gr.set_dim(Some(2));
    gr.set_reduced_motion(true);
    let sw = gr.to_software().expect("a software renderer");
    assert_eq!(sw.items().len(), 7);
    assert_eq!(sw.selection(), &[3, 5]);
    assert_eq!(sw.hover(), Some(4));
    assert_eq!(sw.dim(), Some(2));
    assert!(sw.reduced_motion());
    assert_eq!(sw.size(), (300, 200));
}

#[test]
fn a_software_renderer_hands_its_state_to_a_gpu_one_and_the_pictures_agree() {
    let Some(g) = gpu("from software") else {
        return;
    };
    let mut sw = Renderer::new(300, 200, 1.0, FONT_PX, Palette::dark(), TextEngine::new())
        .expect("renderer");
    sw.set_items(items());
    sw.set_selection(&[3]);
    sw.set_hover(Some(5));
    sw.set_card(None::<Card>);
    let want = sw.snapshot_rgba();
    let mut gr = GpuRenderer::from_software(g, &sw, &options()).expect("gpu renderer");
    assert_eq!(gr.items().len(), 7);
    let got = gr.snapshot_rgba().expect("readback");
    assert_eq!(got.len(), want.len());
    // Fills and outlines are exact; only the hatch of the unreadable box
    // differs, at its anti-aliased edges.
    let over = got
        .as_chunks::<4>()
        .0
        .iter()
        .zip(want.as_chunks::<4>().0)
        .filter(|(a, b)| (0..3).any(|i| a[i].abs_diff(b[i]) > 16))
        .count();
    assert!(
        over * 50 <= got.len() / 4,
        "{over} pixels differ after the hand-over"
    );
}
