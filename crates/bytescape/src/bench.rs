//! Benchmark paths: the scripted zoom, hover and selection redraws, and the
//! numbers they print. The same input events a user's wheel produces drive the
//! zoom, through the same state machine.
//!
//! Two series are measured. The warm series zooms the same pair of views over
//! and over after one untimed pass, so layout and label caches are hot. The
//! cold series zooms into a different folder each time, so every target view
//! is laid out and its labels shaped for the first time; its first frame also
//! carries the layout time, because a real zoom would wait for it.

use std::time::{Duration, Instant};

use bs_render::{Painted, Renderer};
use bs_ui::{App, Effect, InputEvent};

use crate::synth::SynthSource;

/// Where finished frames go: a window or a plain buffer.
pub trait FrameSink {
    fn paint(&mut self, renderer: &mut Renderer, now: Duration) -> Painted;
}

/// Renders into memory; measures drawing without presenting.
pub struct Offscreen {
    buffer: Vec<u32>,
    first: bool,
}

impl Offscreen {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            buffer: vec![0; (w * h) as usize],
            first: true,
        }
    }
}

impl FrameSink for Offscreen {
    fn paint(&mut self, renderer: &mut Renderer, now: Duration) -> Painted {
        let whole = std::mem::take(&mut self.first);
        renderer.paint(&mut self.buffer, now, whole)
    }
}

/// Frame times in microseconds, by kind of frame.
#[derive(Default)]
pub struct Series {
    pub first: Vec<u64>,
    pub anim: Vec<u64>,
    pub final_frame: Vec<u64>,
    pub all: Vec<u64>,
    pub final_render: Vec<u64>,
    pub final_copy: Vec<u64>,
    pub layout: Vec<u64>,
}

#[derive(Default)]
pub struct Samples {
    pub warm: Series,
    pub cold: Series,
    pub hover: Vec<u64>,
    pub select: Vec<u64>,
}

pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub over_budget: usize,
    pub over_double_budget: usize,
}

const BUDGET_MICROS: u64 = 16_700;
const DOUBLE_BUDGET_MICROS: u64 = 33_300;

pub fn stats(v: &[u64]) -> Option<Stats> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let at = |q: f64| s[(((s.len() - 1) as f64) * q).round() as usize] as f64 / 1000.0;
    Some(Stats {
        n: s.len(),
        min: s[0] as f64 / 1000.0,
        p50: at(0.5),
        p95: at(0.95),
        p99: at(0.99),
        max: *s.last().unwrap_or(&0) as f64 / 1000.0,
        over_budget: s.iter().filter(|&&x| x > BUDGET_MICROS).count(),
        over_double_budget: s.iter().filter(|&&x| x > DOUBLE_BUDGET_MICROS).count(),
    })
}

/// Prints `prefix.name=value` lines for one distribution.
pub fn print_stats(prefix: &str, v: &[u64]) {
    if let Some(s) = stats(v) {
        println!("{prefix}.n={}", s.n);
        println!("{prefix}.min_ms={:.3}", s.min);
        println!("{prefix}.p50_ms={:.3}", s.p50);
        println!("{prefix}.p95_ms={:.3}", s.p95);
        println!("{prefix}.p99_ms={:.3}", s.p99);
        println!("{prefix}.max_ms={:.3}", s.max);
        println!("{prefix}.over_16_7ms={}", s.over_budget);
        println!("{prefix}.over_33_3ms={}", s.over_double_budget);
    }
}

/// A virtual 60 Hz clock: frames are stepped 1/60 s apart so the animation
/// has a fixed number of frames, while each frame's real cost is measured.
const FRAME: Duration = Duration::from_micros(16_667);

pub struct Script {
    /// Zoom pairs in the warm series.
    pub cycles: usize,
    /// Zoom-ins in the cold series, each into a folder not visited before.
    pub cold_cycles: usize,
    pub hover_moves: usize,
    /// A point over the biggest folder: the warm series' wheel goes there.
    pub aim: (f32, f32),
    /// Points over other folders, largest first, for the cold series.
    pub cold_aims: Vec<(f32, f32)>,
    /// Sleep until each frame's real-time slot before drawing it.
    pub paced: bool,
}

fn micros(t: Instant) -> u64 {
    t.elapsed().as_micros() as u64
}

fn apply(renderer: &mut Renderer, fx: Vec<Effect>, now: Duration) {
    for e in fx {
        match e {
            Effect::SetHover(id) => renderer.set_hover(id),
            Effect::SetSelected(id) => renderer.set_selected(id),
            Effect::SetView(items) => renderer.set_items(items),
            Effect::Zoom {
                direction,
                items,
                child,
            } => renderer.begin_zoom(direction, items, child, now),
            Effect::SkipAnimation => renderer.finish_animation(),
            Effect::ViewChanged(_) => {}
        }
    }
}

/// What a zoom step runs against.
struct Rig<'a> {
    app: &'a mut App<SynthSource>,
    renderer: &'a mut Renderer,
    sink: &'a mut dyn FrameSink,
    clock: Duration,
    paced: bool,
}

/// Runs one wheel step and every frame of its animation. When
/// `layout_in_first_frame` is set, the time the app spent laying out the new
/// view is added to the first frame.
fn zoom_step(rig: &mut Rig<'_>, out: &mut Series, event: InputEvent, layout_in_first_frame: bool) {
    rig.app.set_animating(rig.renderer.is_animating());
    let t = Instant::now();
    let fx = rig.app.update(event, rig.clock);
    let layout_us = micros(t);
    out.layout.push(layout_us);
    let start = rig.clock;
    let began = Instant::now();
    let mut i = 0u32;
    loop {
        let now = start + FRAME * i;
        if rig.paced {
            // Waiting for the frame slot is not part of the frame's cost.
            let slot = began + FRAME * i;
            if let Some(wait) = slot.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        let t = Instant::now();
        if i == 0 {
            // The target view is rendered once, off-screen, when the zoom starts.
            apply(rig.renderer, fx.clone(), now);
        }
        let painted = rig.sink.paint(rig.renderer, now);
        let mut us = micros(t);
        if i == 0 && layout_in_first_frame {
            us += layout_us;
        }
        out.all.push(us);
        if i == 0 {
            out.first.push(us);
        } else if painted.animating {
            out.anim.push(us);
        } else {
            out.final_frame.push(us);
            let t = rig.renderer.timings();
            out.final_render.push(t.render_micros);
            out.final_copy.push(t.copy_micros);
        }
        i += 1;
        if !painted.animating || i > 200 {
            break;
        }
    }
    rig.clock = start + FRAME * i + FRAME;
    if rig.paced {
        std::thread::sleep(FRAME);
    }
}

fn wheel(aim: (f32, f32), steps: f32) -> InputEvent {
    InputEvent::Wheel {
        x: aim.0,
        y: aim.1,
        steps,
    }
}

fn print_series(layer: &str, name: &str, s: &Series) {
    let p = format!("{layer}.zoom.{name}");
    print_stats(&format!("{p}.all_frames"), &s.all);
    print_stats(&format!("{p}.anim_frames"), &s.anim);
    print_stats(&format!("{p}.first_frames"), &s.first);
    print_stats(&format!("{p}.final_frames"), &s.final_frame);
    print_stats(&format!("{p}.final_render"), &s.final_render);
    print_stats(&format!("{p}.final_copy"), &s.final_copy);
    print_stats(&format!("{p}.layout"), &s.layout);
}

/// Runs the full script and prints results as `key=value` lines.
pub fn run(
    app: &mut App<SynthSource>,
    renderer: &mut Renderer,
    sink: &mut dyn FrameSink,
    script: &Script,
    layer: &str,
) -> Samples {
    let mut out = Samples::default();
    // Settle on the first view (not part of the zoom numbers).
    renderer.set_items(app.items());
    sink.paint(renderer, Duration::ZERO);
    let mut rig = Rig {
        app,
        renderer,
        sink,
        clock: Duration::ZERO,
        paced: script.paced,
    };

    // Cold series first, while no target view has been laid out or shaped.
    // The way back is not part of it: the parent view is warm by then.
    let mut way_back = Series::default();
    let mut cold_run = 0usize;
    for aim in script.cold_aims.iter().take(script.cold_cycles) {
        zoom_step(&mut rig, &mut out.cold, wheel(*aim, 1.0), true);
        zoom_step(&mut rig, &mut way_back, wheel(*aim, -1.0), false);
        cold_run += 1;
    }

    // Warm series: one untimed pass over the pair, then the timed cycles.
    let mut untimed = Series::default();
    for steps in [1.0, -1.0] {
        zoom_step(&mut rig, &mut untimed, wheel(script.aim, steps), false);
    }
    for _ in 0..script.cycles {
        for steps in [1.0, -1.0] {
            zoom_step(&mut rig, &mut out.warm, wheel(script.aim, steps), false);
        }
    }
    let clock = rig.clock;

    // Hover and selection on the settled view.
    let items = app.items();
    let mut rng = crate::synth::Rng::new(99);
    let (w, h) = renderer.size();
    let top = items.first().map(|i| i.rect).unwrap_or_default();
    let mut moves = 0usize;
    while moves < script.hover_moves {
        let px = top.x + rng.unit() * (top.w.min(w as f32) - 1.0);
        let py = top.y + rng.unit() * (top.h.min(h as f32) - 1.0);
        let t = Instant::now();
        let fx = app.update(InputEvent::PointerMoved { x: px, y: py }, clock);
        if fx.is_empty() {
            continue;
        }
        apply(renderer, fx, clock);
        sink.paint(renderer, clock);
        out.hover.push(micros(t));
        moves += 1;
        if moves.is_multiple_of(8) {
            let t = Instant::now();
            let fx = app.update(InputEvent::Click { x: px, y: py }, clock);
            apply(renderer, fx, clock);
            sink.paint(renderer, clock);
            out.select.push(micros(t));
        }
    }
    println!("{layer}.zoom.warm.cycles={}", script.cycles);
    println!("{layer}.zoom.cold.cycles={cold_run}");
    print_series(layer, "warm", &out.warm);
    print_series(layer, "cold", &out.cold);
    print_stats(&format!("{layer}.hover"), &out.hover);
    print_stats(&format!("{layer}.select"), &out.select);
    out
}
