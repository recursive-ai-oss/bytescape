//! Times the zoom animation through the GPU drawing backend and prints the
//! same statistics as the software zoom benchmark of the `bytescape` binary,
//! as `key=value` lines under a `wgpu_windowed` or `wgpu_offscreen` layer.
//!
//! ```text
//! gpu_zoom [--windowed] [--paced] [--vsync] [--software-adapter]
//!          [--size WxH] [--scale F] [--boxes N] [--dark]
//!          [--cycles N] [--cold-cycles N] [--hover-moves N]
//! ```
//!
//! The scene is the seeded synthetic tree the software benchmark uses, so the
//! two backends draw the same boxes. A windowed run must be started inside a
//! virtual display (`xvfb-run`), never on a desktop session. Every run prints
//! the adapter it used: a software adapter (llvmpipe) says what the CPU does
//! when asked to act as a GPU, and is not a verdict on any graphics card.

#![allow(clippy::too_many_arguments)]

#[allow(dead_code)]
#[path = "../../../bytescape/src/synth.rs"]
mod synth;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bs_render::gpu::{
    AdapterChoice, BackendPreference, FailStage, Geometry, GpuContext, GpuError, GpuOptions,
    GpuRenderer, GpuWindow, PresentPacing, WindowBackend, choose_window,
};
use bs_render::{Bar, Painted, Palette, Rect, TextEngine};
use bs_ui::{App, Effect, InputEvent, ViewSource};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use synth::{LayoutParams, SynthSource};

const FONT_PX: f32 = 13.0;
const BUDGET_MICROS: u64 = 16_700;
const DOUBLE_BUDGET_MICROS: u64 = 33_300;
/// A virtual 60 Hz clock: frames are stepped 1/60 s apart so the animation
/// has a fixed number of frames, while each frame's real cost is measured.
const FRAME: Duration = Duration::from_micros(16_667);

#[derive(Clone)]
struct Options {
    windowed: bool,
    paced: bool,
    vsync: bool,
    software_adapter: bool,
    size: (u32, u32),
    scale: f32,
    boxes: usize,
    dark: bool,
    cycles: usize,
    cold_cycles: usize,
    hover_moves: usize,
    /// Keep the final picture on screen this long, for a screenshot.
    hold_ms: u64,
    /// Makes a start-up step fail, to show the fallback.
    fail_at: Option<FailStage>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        windowed: false,
        paced: false,
        vsync: false,
        software_adapter: false,
        size: (3840, 2160),
        scale: 1.0,
        boxes: 20_000,
        dark: false,
        cycles: 30,
        cold_cycles: 24,
        hover_moves: 300,
        hold_ms: 0,
        fail_at: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--windowed" => o.windowed = true,
            "--paced" => o.paced = true,
            "--vsync" => o.vsync = true,
            "--software-adapter" => o.software_adapter = true,
            "--dark" => o.dark = true,
            "--size" => {
                let v = value("--size")?;
                let (w, h) = v.split_once('x').ok_or("--size is WxH")?;
                o.size = (
                    w.parse().map_err(|_| "bad width")?,
                    h.parse().map_err(|_| "bad height")?,
                );
            }
            "--scale" => o.scale = value("--scale")?.parse().map_err(|_| "bad scale")?,
            "--boxes" => o.boxes = value("--boxes")?.parse().map_err(|_| "bad box count")?,
            "--cycles" => o.cycles = value("--cycles")?.parse().map_err(|_| "bad cycles")?,
            "--cold-cycles" => {
                o.cold_cycles = value("--cold-cycles")?
                    .parse()
                    .map_err(|_| "bad cold cycles")?;
            }
            "--hover-moves" => {
                o.hover_moves = value("--hover-moves")?.parse().map_err(|_| "bad moves")?;
            }
            "--hold-ms" => o.hold_ms = value("--hold-ms")?.parse().map_err(|_| "bad hold")?,
            "--fail-at" => {
                o.fail_at = Some(match value("--fail-at")?.as_str() {
                    "instance" => FailStage::Instance,
                    "adapter" => FailStage::Adapter,
                    "device" => FailStage::Device,
                    "pipeline" => FailStage::Pipeline,
                    "surface" => FailStage::Surface,
                    "renderer" => FailStage::Renderer,
                    "first-frame" => FailStage::FirstFrame,
                    other => return Err(format!("unknown stage {other}")),
                });
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(o)
}

// ---------------------------------------------------------------------------
// Statistics, in the software benchmark's format
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Series {
    first: Vec<u64>,
    anim: Vec<u64>,
    final_frame: Vec<u64>,
    all: Vec<u64>,
    final_render: Vec<u64>,
    final_copy: Vec<u64>,
    layout: Vec<u64>,
}

fn print_stats(prefix: &str, v: &[u64]) {
    if v.is_empty() {
        return;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let at = |q: f64| s[(((s.len() - 1) as f64) * q).round() as usize] as f64 / 1000.0;
    println!("{prefix}.n={}", s.len());
    println!("{prefix}.min_ms={:.3}", s[0] as f64 / 1000.0);
    println!("{prefix}.p50_ms={:.3}", at(0.5));
    println!("{prefix}.p95_ms={:.3}", at(0.95));
    println!("{prefix}.p99_ms={:.3}", at(0.99));
    println!(
        "{prefix}.max_ms={:.3}",
        *s.last().unwrap_or(&0) as f64 / 1000.0
    );
    println!(
        "{prefix}.over_16_7ms={}",
        s.iter().filter(|&&x| x > BUDGET_MICROS).count()
    );
    println!(
        "{prefix}.over_33_3ms={}",
        s.iter().filter(|&&x| x > DOUBLE_BUDGET_MICROS).count()
    );
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

fn micros(t: Instant) -> u64 {
    t.elapsed().as_micros() as u64
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Where finished frames go.
trait Sink {
    fn paint(&mut self, r: &mut GpuRenderer, now: Duration) -> Result<Painted, GpuError>;
}

/// Draws into a texture and waits for the GPU to finish, so the time covers
/// the drawing itself and not just handing it over.
struct Offscreen;

impl Sink for Offscreen {
    fn paint(&mut self, r: &mut GpuRenderer, now: Duration) -> Result<Painted, GpuError> {
        let p = r.paint_offscreen(now)?;
        r.gpu().wait_idle();
        Ok(p)
    }
}

fn wheel(aim: (f32, f32), steps: f32) -> InputEvent {
    InputEvent::Wheel {
        x: aim.0,
        y: aim.1,
        steps,
    }
}

fn apply(r: &mut GpuRenderer, fx: Vec<Effect>, now: Duration) {
    for e in fx {
        match e {
            Effect::SetHover(id) => r.set_hover(id),
            Effect::SetSelected(id) => r.set_selected(id),
            Effect::SetSelection(ids) => r.set_selection(&ids),
            Effect::SetView(items) => r.set_items(items),
            Effect::Zoom {
                direction,
                items,
                child,
            } => r.begin_zoom(direction, items, child, now),
            Effect::SkipAnimation => r.finish_animation(),
            _ => {}
        }
    }
}

struct Rig<'a> {
    app: &'a mut App<SynthSource>,
    renderer: &'a mut GpuRenderer,
    sink: &'a mut dyn Sink,
    clock: Duration,
    paced: bool,
}

fn zoom_step(
    rig: &mut Rig<'_>,
    out: &mut Series,
    event: InputEvent,
    layout_in_first_frame: bool,
) -> Result<(), GpuError> {
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
            let slot = began + FRAME * i;
            if let Some(wait) = slot.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        let t = Instant::now();
        if i == 0 {
            apply(rig.renderer, fx.clone(), now);
        }
        let painted = rig.sink.paint(rig.renderer, now)?;
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
    Ok(())
}

struct Scene {
    app: App<SynthSource>,
    aim: (f32, f32),
    cold_aims: Vec<(f32, f32)>,
}

fn build_scene(o: &Options) -> Result<Scene, String> {
    let (w, h) = (o.size.0 as f32, o.size.1 as f32);
    let map = bs_ui::chrome::map_rect(w, h, o.scale);
    let params = LayoutParams {
        scale: o.scale,
        em: FONT_PX * o.scale,
        min_area: 160.0 * o.scale * o.scale,
    };
    let mut source = SynthSource::new(0xB17E_5CA9E, params);
    let (min_area, root_count) = source.calibrate(map, o.boxes);
    let (item_id, child_view) = source
        .biggest_child(map)
        .ok_or("the scene has no folder to zoom into")?;
    let child_items = source.layout(child_view, map).items.len();
    println!("wgpu_scene.seed=0xB17E5CA9E");
    println!(
        "wgpu_scene.kind=synthetic squarified tree, seeded (shared with the software benchmark)"
    );
    println!("wgpu_scene.target_boxes={}", o.boxes);
    println!("wgpu_scene.min_area_px2={min_area:.1}");
    println!("wgpu_scene.boxes_root_view={root_count}");
    println!("wgpu_scene.boxes_child_view={child_items}");
    let root = source.layout(0, map);
    let centre = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let aim = root
        .items
        .iter()
        .find(|i| i.id == item_id)
        .map_or(centre(map), |i| centre(i.rect));
    let mut folders: Vec<_> = root
        .items
        .iter()
        .filter(|i| i.depth == 1 && i.kind == bs_render::ItemKind::FolderTitled && i.id != item_id)
        .collect();
    folders.sort_by(|a, b| (b.rect.w * b.rect.h).total_cmp(&(a.rect.w * a.rect.h)));
    let cold_aims: Vec<_> = folders.iter().map(|i| centre(i.rect)).collect();
    println!("wgpu_scene.cold_folders_available={}", cold_aims.len());
    Ok(Scene {
        app: App::new(source, map),
        aim,
        cold_aims,
    })
}

fn bars(o: &Options) -> Vec<Bar> {
    let (w, h) = (o.size.0 as f32, o.size.1 as f32);
    let map = bs_ui::chrome::map_rect(w, h, o.scale);
    vec![
        Bar {
            rect: Rect::new(0.0, 0.0, w, map.y),
            left: Some("C: \u{203a} Users \u{203a} synthetic".into()),
            right: None,
            border_on_top: false,
        },
        Bar {
            rect: Rect::new(0.0, map.bottom(), w, h - map.bottom()),
            left: Some("synthetic tree \u{b7} 41.2 GB".into()),
            right: Some("Find more free software at RecursiveAI.org".into()),
            border_on_top: true,
        },
    ]
}

fn run_script(
    o: &Options,
    scene: &mut Scene,
    renderer: &mut GpuRenderer,
    sink: &mut dyn Sink,
    layer: &str,
) -> Result<(), GpuError> {
    renderer.set_items(scene.app.items());
    sink.paint(renderer, Duration::ZERO)?;
    let mut out_warm = Series::default();
    let mut out_cold = Series::default();
    let mut out_return = Series::default();
    let (aim, cold_aims) = (scene.aim, scene.cold_aims.clone());
    let mut rig = Rig {
        app: &mut scene.app,
        renderer,
        sink,
        clock: Duration::ZERO,
        paced: o.paced,
    };
    let mut cold_run = 0usize;
    for a in cold_aims.iter().take(o.cold_cycles) {
        zoom_step(&mut rig, &mut out_cold, wheel(*a, 1.0), true)?;
        zoom_step(&mut rig, &mut out_return, wheel(*a, -1.0), false)?;
        cold_run += 1;
    }
    let mut untimed = Series::default();
    for steps in [1.0, -1.0] {
        zoom_step(&mut rig, &mut untimed, wheel(aim, steps), false)?;
    }
    for _ in 0..o.cycles {
        for steps in [1.0, -1.0] {
            zoom_step(&mut rig, &mut out_warm, wheel(aim, steps), false)?;
        }
    }
    let clock = rig.clock;
    let (app, renderer, sink) = (rig.app, rig.renderer, rig.sink);

    // Hover and selection on the settled view.
    let items = app.items();
    let mut rng = synth::Rng::new(99);
    let (w, h) = renderer.size();
    let top = items.first().map(|i| i.rect).unwrap_or_default();
    let (mut hover, mut select) = (Vec::new(), Vec::new());
    let mut moves = 0usize;
    while moves < o.hover_moves {
        let px = top.x + rng.unit() * (top.w.min(w as f32) - 1.0);
        let py = top.y + rng.unit() * (top.h.min(h as f32) - 1.0);
        let t = Instant::now();
        let fx = app.update(InputEvent::PointerMoved { x: px, y: py }, clock);
        if fx.is_empty() {
            continue;
        }
        apply(renderer, fx, clock);
        sink.paint(renderer, clock)?;
        hover.push(micros(t));
        moves += 1;
        if moves.is_multiple_of(8) {
            let t = Instant::now();
            let fx = app.update(InputEvent::Click { x: px, y: py }, clock);
            apply(renderer, fx, clock);
            sink.paint(renderer, clock)?;
            select.push(micros(t));
        }
    }
    println!("{layer}.zoom.warm.cycles={}", o.cycles);
    println!("{layer}.zoom.cold.cycles={cold_run}");
    print_series(layer, "warm", &out_warm);
    print_series(layer, "cold", &out_cold);
    print_series(layer, "cold_return", &out_return);
    print_stats(&format!("{layer}.hover"), &hover);
    print_stats(&format!("{layer}.select"), &select);
    Ok(())
}

fn print_env(layer: &str, gpu: &GpuContext, r: &GpuRenderer, o: &Options, mode: &str) {
    let info = gpu.adapter_info();
    println!("{layer}.width={}", o.size.0);
    println!("{layer}.height={}", o.size.1);
    println!("{layer}.scale_factor={}", r.scale());
    println!("{layer}.adapter={}", info.name);
    println!("{layer}.adapter_type={:?}", info.device_type);
    println!("{layer}.adapter_backend={:?}", info.backend);
    println!(
        "{layer}.adapter_driver={} {}",
        info.driver, info.driver_info
    );
    println!("{layer}.software_adapter={}", gpu.is_software());
    println!("{layer}.present_mode={mode}");
    if gpu.is_software() {
        println!("{layer}.note=software adapter, not a GPU verdict");
    }
}

fn gpu_options(o: &Options) -> GpuOptions {
    GpuOptions {
        adapter: if o.software_adapter {
            AdapterChoice::Software
        } else {
            AdapterChoice::Default
        },
        fail_at: o.fail_at,
    }
}

fn text_engine() -> TextEngine {
    let text = TextEngine::new();
    text.load_fonts();
    text
}

fn palette(o: &Options) -> Palette {
    Palette::for_scheme(o.dark)
}

fn offscreen(o: &Options) -> Result<(), String> {
    let mut scene = build_scene(o)?;
    let gpu = GpuContext::headless(&gpu_options(o)).map_err(|e| e.to_string())?;
    let text = text_engine();
    let mut r = GpuRenderer::new(
        gpu.clone(),
        Geometry {
            width: o.size.0,
            height: o.size.1,
            scale: o.scale,
            font_px: FONT_PX,
        },
        palette(o),
        text,
        &gpu_options(o),
    )
    .map_err(|e| e.to_string())?;
    r.set_bars(bars(o));
    let layer = if o.paced {
        "wgpu_offscreen"
    } else {
        "wgpu_offscreen_unpaced"
    };
    print_env(layer, &gpu, &r, o, "none (offscreen, waits for the GPU)");
    run_script(o, &mut scene, &mut r, &mut Offscreen, layer).map_err(|e| e.to_string())
}

struct Host {
    o: Options,
    window: Option<Arc<Window>>,
    ran: bool,
    result: Result<(), String>,
}

impl ApplicationHandler for Host {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Bytescape GPU benchmark")
            .with_inner_size(PhysicalSize::new(self.o.size.0, self.o.size.1))
            .with_resizable(false);
        match el.create_window(attrs) {
            Ok(w) => {
                w.request_redraw();
                self.window = Some(Arc::new(w));
            }
            Err(e) => {
                self.result = Err(format!("window: {e}"));
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if !matches!(event, WindowEvent::RedrawRequested) || self.ran {
            return;
        }
        self.ran = true;
        if let Some(w) = self.window.clone() {
            self.result = windowed(&self.o, w);
        }
        el.exit();
    }
}

fn windowed(o: &Options, window: Arc<Window>) -> Result<(), String> {
    let size = window.inner_size();
    let o = Options {
        size: (size.width, size.height),
        scale: window.scale_factor() as f32,
        ..o.clone()
    };
    let mut scene = build_scene(&o)?;
    let pacing = if o.vsync {
        PresentPacing::Vsync
    } else {
        PresentPacing::Immediate
    };
    let backend = choose_window(
        window,
        o.size,
        o.scale,
        FONT_PX,
        palette(&o),
        text_engine(),
        BackendPreference::Gpu,
        pacing,
        &gpu_options(&o),
    );
    let mut gw = match backend {
        WindowBackend::Gpu(g) => *g,
        WindowBackend::Software { reason } => {
            return Err(format!("the GPU backend was not available: {reason}"));
        }
    };
    gw.renderer.set_bars(bars(&o));
    let layer = if o.paced {
        "wgpu_windowed"
    } else {
        "wgpu_windowed_unpaced"
    };
    let mode = format!("{:?}", gw.presenter.present_mode());
    print_env(layer, gw.renderer.gpu(), &gw.renderer, &o, &mode);
    // The sink borrows the presenter while the script borrows the renderer, so
    // the pair is split.
    let GpuWindow {
        mut renderer,
        presenter,
    } = gw;
    let mut sink = WindowedPresenter { presenter };
    run_script(&o, &mut scene, &mut renderer, &mut sink, layer).map_err(|e| e.to_string())?;
    if o.hold_ms > 0 {
        let shown = sink
            .paint(&mut renderer, Duration::ZERO)
            .map_err(|e| e.to_string())?;
        println!("{layer}.hold.animating={}", shown.animating);
        std::thread::sleep(Duration::from_millis(o.hold_ms));
    }
    Ok(())
}

struct WindowedPresenter {
    presenter: bs_render::gpu::GpuPresenter,
}

impl Sink for WindowedPresenter {
    fn paint(&mut self, r: &mut GpuRenderer, now: Duration) -> Result<Painted, GpuError> {
        self.presenter.present(r, now)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let o = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gpu_zoom: {e}");
            std::process::exit(2);
        }
    };
    let result = if o.windowed {
        match EventLoop::new() {
            Ok(el) => {
                let mut host = Host {
                    o: o.clone(),
                    window: None,
                    ran: false,
                    result: Err("the window never asked for a frame".into()),
                };
                match el.run_app(&mut host) {
                    Ok(()) => host.result,
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(format!("event loop: {e}")),
        }
    } else {
        offscreen(&o)
    };
    if let Err(e) = result {
        println!("wgpu.error={e}");
        eprintln!("gpu_zoom: {e}");
        std::process::exit(3);
    }
}
