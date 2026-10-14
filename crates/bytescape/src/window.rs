//! The window: the winit event loop, translated into the UI crate's own input
//! events, and the benchmark and screenshot entry points that share its
//! drawing path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bs_layout::Metrics;
use bs_render::present::Presenter;
use bs_render::{Flags, Item, ItemKind, ItemLabel, Painted, Palette, Rect, Renderer, TextEngine};
use bs_ui::tree::{TreeSource, scan_status};
use bs_ui::{App, Effect, InputEvent, Key, chrome};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey};
use winit::window::{Theme, Window, WindowId};

use crate::bench::{self, FrameSink, Offscreen, Script};
use crate::live::{ScanDriver, Tick};
use crate::synth::{LayoutParams, SynthSource, format_size};
use crate::{Mode, ShotOptions, ZoomOptions, font_px, real};

type Gfx = Presenter<Arc<Window>, Arc<Window>>;

pub fn run(mode: Mode, t0: Instant) -> Result<(), String> {
    match mode {
        Mode::FontInit => {
            font_init();
            Ok(())
        }
        Mode::Screenshot(opts) => screenshots(&opts),
        Mode::Zoom(opts) if !opts.windowed => {
            zoom_offscreen(&opts);
            Ok(())
        }
        mode => run_window(mode, t0),
    }
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

struct Scene {
    source: SynthSource,
    map: Rect,
}

fn build_scene(
    size: (u32, u32),
    scale: f32,
    boxes: Option<usize>,
) -> (Scene, Option<(f32, usize)>) {
    let (bars, map) = chrome::layout(size.0 as f32, size.1 as f32, scale, "", "");
    drop(bars);
    let params = LayoutParams {
        scale,
        em: font_px() * scale,
        min_area: 160.0 * scale * scale,
    };
    let mut source = SynthSource::new(0xB17E_5CA9E, params);
    let calibration = boxes.map(|n| source.calibrate(map, n));
    (Scene { source, map }, calibration)
}

fn font_init() {
    let engine = TextEngine::new();
    let t = Instant::now();
    engine.load_fonts();
    let init = t.elapsed();
    let style = bs_render::TextStyle {
        size: font_px(),
        scale: 1.0,
        color: bs_render::Color::rgb(0, 0, 0),
    };
    let t = Instant::now();
    let first = engine.label("Documents-123", style, 200.0).is_some();
    println!("font_init_ms={:.3}", init.as_secs_f64() * 1000.0);
    println!("first_label_ms={:.3}", t.elapsed().as_secs_f64() * 1000.0);
    println!("first_label_drawn={first}");
}

fn palette_for(dark: bool) -> Palette {
    Palette::for_scheme(dark)
}

pub(crate) fn make_renderer(
    size: (u32, u32),
    scale: f32,
    dark: bool,
    text: TextEngine,
) -> Result<Renderer, String> {
    let mut r = Renderer::new(size.0, size.1, scale, font_px(), palette_for(dark), text)
        .ok_or("window has no area")?;
    if std::env::var_os("BYTESCAPE_REDUCED_MOTION").is_some() {
        r.set_reduced_motion(true);
    }
    Ok(r)
}

pub(crate) fn print_env(label: &str, renderer: &Renderer, size: (u32, u32)) {
    println!("{label}.width={}", size.0);
    println!("{label}.height={}", size.1);
    println!("{label}.scale_factor={}", renderer.scale());
    println!("{label}.render_threads={}", renderer.threads());
}

// ---------------------------------------------------------------------------
// Zoom benchmark
// ---------------------------------------------------------------------------

/// The app a zoom measurement drives: the synthetic scene or a real scan.
enum BenchApp {
    Synth(App<SynthSource>),
    Real(App<TreeSource>),
}

fn prepare_zoom(
    opts: &ZoomOptions,
    size: (u32, u32),
    text: &TextEngine,
) -> Result<(BenchApp, Renderer, Script), String> {
    match &opts.path {
        Some(path) => {
            let (app, renderer, script) = real::prepare_zoom(opts, path, size, text)?;
            Ok((BenchApp::Real(app), renderer, script))
        }
        None => {
            let (app, renderer, script) = prepare_synthetic_zoom(opts, size, text)?;
            Ok((BenchApp::Synth(app), renderer, script))
        }
    }
}

/// The name of a measurement layer: the plain name for the synthetic scene,
/// prefixed with the tag for a real scan.
fn layer_name(opts: &ZoomOptions, base: &str) -> String {
    match &opts.path {
        Some(_) => format!("{}_{base}", opts.tag),
        None => base.to_string(),
    }
}

fn run_bench(
    app: &mut BenchApp,
    renderer: &mut Renderer,
    sink: &mut dyn FrameSink,
    script: &Script,
    layer: &str,
) {
    match app {
        BenchApp::Synth(a) => {
            bench::run(a, renderer, sink, script, layer);
        }
        BenchApp::Real(a) => {
            bench::run(a, renderer, sink, script, layer);
        }
    }
}

fn prepare_synthetic_zoom(
    opts: &ZoomOptions,
    size: (u32, u32),
    text: &TextEngine,
) -> Result<(App<SynthSource>, Renderer, Script), String> {
    let (mut scene, calibration) = build_scene(size, opts.scale, Some(opts.boxes));
    let (min_area, root_count) = calibration.unwrap_or((0.0, 0));
    let t = Instant::now();
    let child = scene.source.biggest_child(scene.map);
    let (item_id, child_view) = child.ok_or("scene has no folder to zoom into")?;
    let root_layout_ms = t.elapsed().as_secs_f64() * 1000.0;
    let child_items = {
        use bs_ui::ViewSource;
        scene.source.layout(child_view, scene.map).items.len()
    };
    println!("scene.seed=0xB17E5CA9E");
    println!("scene.kind=synthetic squarified tree, seeded");
    println!("scene.target_boxes={}", opts.boxes);
    println!("scene.min_area_px2={min_area:.1}");
    println!("scene.boxes_root_view={root_count}");
    println!("scene.boxes_child_view={child_items}");
    println!("scene.root_layout_ms={root_layout_ms:.1}");
    let aim = {
        use bs_ui::ViewSource;
        let layout = scene.source.layout(0, scene.map);
        let r = layout
            .items
            .iter()
            .find(|i| i.id == item_id)
            .map(|i| i.rect)
            .unwrap_or(scene.map);
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    };
    let renderer = {
        let mut r = make_renderer(size, opts.scale, opts.dark, text.clone())?;
        let (bars, _) = chrome::layout(size.0 as f32, size.1 as f32, opts.scale, "C:", "");
        r.set_bars(bars);
        r
    };
    // Folders to zoom into for the cold series: every nested folder of the
    // root view except the one the warm series uses, largest first.
    let cold_aims = {
        use bs_ui::ViewSource;
        let layout = scene.source.layout(0, scene.map);
        let mut folders: Vec<_> = layout
            .items
            .iter()
            .filter(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled && i.id != item_id)
            .collect();
        folders.sort_by(|a, b| (b.rect.w * b.rect.h).total_cmp(&(a.rect.w * a.rect.h)));
        folders
            .iter()
            .map(|i| (i.rect.x + i.rect.w / 2.0, i.rect.y + i.rect.h / 2.0))
            .collect::<Vec<_>>()
    };
    println!("scene.cold_folders_available={}", cold_aims.len());
    let map = scene.map;
    let app = App::new(scene.source, map);
    Ok((
        app,
        renderer,
        Script {
            cycles: opts.cycles,
            cold_cycles: opts.cold_cycles,
            hover_moves: opts.hover_moves,
            aim,
            cold_aims,
            paced: opts.paced,
        },
    ))
}

fn zoom_offscreen(opts: &ZoomOptions) {
    let text = TextEngine::new();
    text.load_fonts();
    let Ok((mut app, mut renderer, script)) = prepare_zoom(opts, opts.size, &text) else {
        eprintln!("bytescape: could not prepare the benchmark scene");
        return;
    };
    let layer = layer_name(
        opts,
        if opts.paced {
            "offscreen"
        } else {
            "offscreen_unpaced"
        },
    );
    print_env(&layer, &renderer, opts.size);
    let mut sink = Offscreen::new(opts.size.0, opts.size.1);
    run_bench(&mut app, &mut renderer, &mut sink, &script, &layer);
}

struct WindowSink<'a> {
    presenter: &'a mut Gfx,
}

impl FrameSink for WindowSink<'_> {
    fn paint(&mut self, renderer: &mut Renderer, now: Duration) -> Painted {
        let mut painted = Painted {
            damage: Vec::new(),
            animating: false,
        };
        let result = self.presenter.present_with(|buf, whole| {
            painted = renderer.paint(buf, now, whole);
            painted.damage.clone()
        });
        if let Err(e) = result {
            eprintln!("bytescape: present failed: {e}");
        }
        painted
    }
}

// ---------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------

/// A hand-built view showing the special blocks and states next to a map.
fn showcase(scene: &mut Scene, scale: f32, em: f32) -> (Vec<Item>, u32, u32) {
    use bs_ui::ViewSource;
    let map = scene.map;
    let left = Rect::new(map.x, map.y, (map.w * 0.62).round(), map.h);
    let mut items: Vec<Item> = scene.source.layout(0, left).items.as_ref().clone();
    // Mark the biggest few file tiles as deleted.
    let mut tiles: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].kind == ItemKind::FileTile && items[i].rect.w > 6.0 * em)
        .collect();
    tiles.sort_by(|&a, &b| {
        let area = |i: usize| items[i].rect.w * items[i].rect.h;
        area(b).total_cmp(&area(a))
    });
    let hover = tiles.get(3).map_or(0, |&i| items[i].id);
    let selected = tiles.get(5).map_or(0, |&i| items[i].id);
    for &i in tiles.iter().take(3) {
        items[i].flags = Flags::DELETED;
        items[i].label = Some(Box::new(ItemLabel {
            name: "Moved to Recycle Bin · 12.3 GB".into(),
            size: "".into(),
        }));
    }
    let col = Rect::new(
        left.right() + (8.0 * scale).round(),
        map.y,
        map.right() - left.right() - (8.0 * scale).round(),
        map.h,
    );
    let gap = (8.0 * scale).round();
    let h = ((col.h - 3.0 * gap) / 4.0).floor();
    let specials = [
        (ItemKind::Free, "Free · 112 GB"),
        (
            ItemKind::Unreadable,
            "Couldn't read: 23.4 GB · Grant access to see inside",
        ),
        (ItemKind::Reserved, "Reserved by filesystem · 18.0 GB"),
        (ItemKind::RecycleBin, "Recycle Bin · 14.1 GB"),
    ];
    for (n, (kind, text)) in specials.into_iter().enumerate() {
        items.push(Item {
            rect: Rect::new(col.x, col.y + n as f32 * (h + gap), col.w, h),
            kind,
            depth: 0,
            label: Some(Box::new(ItemLabel {
                name: text.into(),
                size: format_size(1).into(),
            })),
            id: 900_000 + n as u32,
            category: 0,
            flags: Flags::default(),
        });
    }
    (items, hover, selected)
}

fn screenshots(opts: &ShotOptions) -> Result<(), String> {
    std::fs::create_dir_all(&opts.dir).map_err(|e| e.to_string())?;
    let text = TextEngine::new();
    text.load_fonts();
    let states: Vec<String> = match (opts.states.is_empty(), &opts.path) {
        (true, Some(_)) => vec!["midscan".into(), "complete".into()],
        (true, None) => vec!["map20k".into(), "showcase".into()],
        (false, _) => opts.states.clone(),
    };
    let (real_states, states): (Vec<String>, Vec<String>) = states
        .into_iter()
        .partition(|s| s == "midscan" || s == "complete");
    if !real_states.is_empty() {
        real::screenshots(opts, &real_states, &text)?;
    }
    let scale = opts.scale;
    let scale_name = if scale.fract() == 0.0 {
        format!("{}", scale as u32)
    } else {
        format!("{scale}")
    };
    for state in &states {
        for dark in [false, true] {
            let boxes = (state == "map20k").then_some(opts.boxes);
            let (mut scene, cal) = build_scene(opts.size, scale, boxes);
            if state == "showcase" {
                scene.source.set_min_area(900.0 * scale * scale);
            }
            let mut renderer = make_renderer(opts.size, scale, dark, text.clone())?;
            let (bars, _) = chrome::layout(
                opts.size.0 as f32,
                opts.size.1 as f32,
                scale,
                "C:  ›  Users  ›  alex  ›  Videos",
                "Videos · 41.2 GB · 12% of drive",
            );
            renderer.set_bars(bars);
            match state.as_str() {
                "showcase" => {
                    let (items, hover, selected) = showcase(&mut scene, scale, font_px() * scale);
                    renderer.set_items(Arc::new(items));
                    renderer.set_hover(Some(hover));
                    renderer.set_selected(Some(selected));
                }
                _ => {
                    use bs_ui::ViewSource;
                    let layout = scene.source.layout(0, scene.map);
                    if let Some((min_area, n)) = cal {
                        println!("{state}: {n} boxes at min area {min_area:.0} px²");
                    }
                    renderer.set_items(layout.items);
                }
            }
            let name = format!(
                "{state}-{}-{scale_name}x.png",
                if dark { "dark" } else { "light" }
            );
            let path = Path::new(&opts.dir).join(&name);
            renderer.save_png(&path)?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

struct Live {
    window: Arc<Window>,
    presenter: Gfx,
    renderer: Renderer,
    /// The map of a real scan, once the first snapshot has arrived.
    app: Option<App<TreeSource>>,
    /// The scripted run of a zoom measurement.
    bench: Option<(BenchApp, Script)>,
    scan: Option<ScanDriver>,
    crumb: String,
    status: String,
}

/// What the live map cost while a scan ran, for the `--bench-live` report.
#[derive(Default)]
struct LiveStats {
    layouts_us: Vec<u64>,
    layout_boxes: Vec<u64>,
    /// Everything the window did for one data update: layout, conversion and
    /// handing the picture to the renderer. This is the longest the event loop
    /// stayed busy for new data.
    updates_us: Vec<u64>,
    paints_us: Vec<u64>,
    /// The part of a paint spent redrawing the picture, and the part spent
    /// copying it into the window buffer; the rest is presenting.
    paint_render_us: Vec<u64>,
    paint_copy_us: Vec<u64>,
    first_map_ms: Option<f64>,
    scan_done_ms: Option<f64>,
}

struct WinApp {
    mode: Mode,
    t0: Instant,
    text: TextEngine,
    live: Option<Live>,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    last_click: Option<(Instant, (f32, f32))>,
    fonts_announced: bool,
    bench_done: bool,
    error: Option<String>,
    timeline: Vec<(&'static str, f64)>,
    scan_path: Option<PathBuf>,
    frames: u32,
    /// A layout with contents (more than the root box) has been produced.
    map_seen: bool,
    /// That layout was handed to the renderer and no frame has shown it yet.
    map_pending: bool,
    /// The scan ended and no frame has shown its final layout yet.
    finish_pending: bool,
    stats: LiveStats,
}

/// Where a bare launch scans until the drive picker exists: the home folder.
fn default_scan_root() -> PathBuf {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(home)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }))
}

fn run_window(mode: Mode, t0: Instant) -> Result<(), String> {
    let text = TextEngine::new();
    match &mode {
        // Benchmarks measure a warm font cache and a settled picture; the first
        // frame run discovers fonts on a worker thread, as the program does.
        Mode::Zoom(_) => text.load_fonts(),
        _ => text.load_fonts_in_background(),
    }
    let scan_path = match &mode {
        Mode::Interactive { path } => Some(path.clone().unwrap_or_else(default_scan_root)),
        Mode::FirstFrame { path } => path.clone(),
        Mode::Live { path, .. } => Some(path.clone()),
        _ => None,
    };
    let loop_started = t0.elapsed().as_secs_f64() * 1000.0;
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    let loop_built = t0.elapsed().as_secs_f64() * 1000.0;
    let mut app = WinApp {
        mode,
        t0,
        text,
        live: None,
        modifiers: ModifiersState::default(),
        cursor: (0.0, 0.0),
        last_click: None,
        fonts_announced: false,
        bench_done: false,
        error: None,
        timeline: vec![
            ("event_loop_build_start", loop_started),
            ("event_loop_built", loop_built),
        ],
        scan_path,
        frames: 0,
        map_seen: false,
        map_pending: false,
        finish_pending: false,
        stats: LiveStats::default(),
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Applies the app's effects to the renderer. Returns whether the view
/// changed, so the breadcrumb needs rebuilding.
fn apply(renderer: &mut Renderer, fx: Vec<Effect>, now: Duration) -> bool {
    let mut view_changed = false;
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
            Effect::ViewChanged(_) => view_changed = true,
        }
    }
    view_changed
}

/// Rebuilds the navigation and status bars from the current view and scan
/// status, and returns the map rectangle they leave.
fn refresh_bars(live: &mut Live) -> Rect {
    let (w, h) = live.renderer.size();
    let scale = live.renderer.scale();
    if let Some(app) = &live.app {
        live.crumb = app.source().breadcrumb(app.current_view());
    }
    let (bars, map) = chrome::layout(w as f32, h as f32, scale, &live.crumb, &live.status);
    live.renderer.set_bars(bars);
    map
}

impl WinApp {
    fn dark_theme(window: &Window) -> bool {
        match std::env::var("BYTESCAPE_THEME").as_deref() {
            Ok("dark") => true,
            Ok("light") => false,
            _ => window.theme() == Some(Theme::Dark),
        }
    }

    fn ms(&self) -> f64 {
        self.t0.elapsed().as_secs_f64() * 1000.0
    }

    fn create(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        let at = self.ms();
        self.timeline.push(("event_loop_running", at));
        let mut attrs = Window::default_attributes().with_title("Bytescape");
        let fixed = match &self.mode {
            Mode::Zoom(z) => Some(z.size),
            Mode::Live { size, .. } => Some(*size),
            _ => None,
        };
        attrs = match fixed {
            Some(s) => attrs
                .with_inner_size(PhysicalSize::new(s.0, s.1))
                .with_resizable(false),
            None => attrs.with_inner_size(LogicalSize::new(1280.0, 800.0)),
        };
        let window = Arc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        let at = self.ms();
        self.timeline.push(("window_created", at));
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        let presenter = Presenter::new(window.clone(), window.clone(), size.width, size.height)
            .map_err(|e| e.to_string())?;
        let at = self.ms();
        self.timeline.push(("surface_created", at));
        let dark = match &self.mode {
            Mode::Zoom(z) => z.dark,
            _ => Self::dark_theme(&window),
        };
        let mut renderer =
            make_renderer((size.width, size.height), scale, dark, self.text.clone())?;
        let crumb = self
            .scan_path
            .as_ref()
            .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let status = if self.scan_path.is_some() {
            "Scanning…".to_string()
        } else {
            String::new()
        };
        let (bars, map) = chrome::layout(
            size.width as f32,
            size.height as f32,
            scale,
            &crumb,
            &status,
        );
        renderer.set_bars(bars);
        let at = self.ms();
        self.timeline.push(("renderer_created", at));
        let mut bench = None;
        if let Mode::Zoom(z) = &self.mode {
            let opts = z.clone();
            let (a, r, sc) = prepare_zoom(&opts, (size.width, size.height), &self.text)?;
            renderer = r;
            bench = Some((a, sc));
        } else if self.scan_path.is_some() {
            // One empty block stands in for the map until the first snapshot.
            renderer.set_items(Arc::new(vec![scan_placeholder(map)]));
        } else {
            renderer.set_items(Arc::new(placeholders(map, scale)));
        }
        self.live = Some(Live {
            window: window.clone(),
            presenter,
            renderer,
            app: None,
            bench,
            scan: None,
            crumb,
            status,
        });
        window.request_redraw();
        Ok(())
    }

    fn redraw(&mut self, el: &ActiveEventLoop) {
        let t0 = self.t0;
        let now = t0.elapsed();
        let Some(live) = self.live.as_mut() else {
            return;
        };
        if let Mode::Zoom(z) = &self.mode {
            if self.bench_done {
                return;
            }
            let Some((mut app, script)) = live.bench.take() else {
                return;
            };
            let layer = layer_name(
                z,
                if z.paced {
                    "windowed"
                } else {
                    "windowed_unpaced"
                },
            );
            print_env(&layer, &live.renderer, live.renderer.size());
            let mut sink = WindowSink {
                presenter: &mut live.presenter,
            };
            run_bench(&mut app, &mut live.renderer, &mut sink, &script, &layer);
            self.bench_done = true;
            el.exit();
            return;
        }
        let started = Instant::now();
        let mut sink = WindowSink {
            presenter: &mut live.presenter,
        };
        let painted = sink.paint(&mut live.renderer, now);
        if self.scan_path.is_some() {
            self.stats
                .paints_us
                .push(started.elapsed().as_micros() as u64);
            let t = live.renderer.timings();
            self.stats.paint_render_us.push(t.render_micros);
            self.stats.paint_copy_us.push(t.copy_micros);
        }
        if painted.animating {
            live.window.request_redraw();
        }
        self.frames += 1;
        if self.frames == 1 {
            self.timeline
                .push(("first_frame_presented", t0.elapsed().as_secs_f64() * 1000.0));
            if let Mode::FirstFrame { path } = &self.mode {
                println!("presented");
                for (name, ms) in &self.timeline {
                    println!("internal.{name}_ms={ms:.3}");
                }
                println!("internal.fonts_ready={}", self.text.is_ready());
                flush_stdout();
                if path.is_none() {
                    el.exit();
                    return;
                }
            }
            // The scan starts after the first frame is on screen, so nothing
            // it does can delay that frame.
            if let Some(path) = &self.scan_path {
                live.scan = Some(ScanDriver::start(path.clone()));
            }
        }
        if self.map_pending && !painted.damage.is_empty() {
            self.map_pending = false;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.stats.first_map_ms = Some(ms);
            if matches!(self.mode, Mode::FirstFrame { .. }) {
                let boxes = live.renderer.items().len();
                println!("internal.first_map_frame_ms={ms:.3}");
                println!("internal.first_map_frame_boxes={boxes}");
                flush_stdout();
                if let Some(scan) = &live.scan {
                    scan.cancel();
                }
                el.exit();
                return;
            }
        }
        if self.finish_pending && !painted.damage.is_empty() && !live.renderer.is_animating() {
            self.finish_pending = false;
            if matches!(self.mode, Mode::Live { .. }) {
                self.print_live_report();
                el.exit();
            }
        }
    }

    fn print_live_report(&self) {
        let scan = self.live.as_ref().and_then(|l| l.scan.as_ref());
        let stats = scan.and_then(|s| s.stats());
        let entries = stats.map_or(0, |s| s.records_applied);
        let files = stats.map_or(0, |s| s.progress.files);
        let threads = stats.map_or(0, |s| s.threads);
        let scan_ms = stats.map_or(0.0, |s| s.elapsed.as_secs_f64() * 1000.0);
        let s = &self.stats;
        let (w, h) = self.live.as_ref().map_or((0, 0), |l| l.renderer.size());
        println!("live.window={w}x{h}");
        println!("live.scan_ms={scan_ms:.0}");
        println!("live.records_applied={entries}");
        println!("live.files={files}");
        println!("live.scan_threads={threads}");
        println!("live.font_px={}", font_px());
        println!(
            "live.first_map_frame_ms={:.1}",
            s.first_map_ms.unwrap_or(f64::NAN)
        );
        println!(
            "live.scan_done_ms={:.1}",
            s.scan_done_ms.unwrap_or(f64::NAN)
        );
        println!("live.relayouts={}", s.layouts_us.len());
        let (hits, misses) = self.text.cache_stats();
        println!("live.label_cache_hits={hits}");
        println!("live.label_cache_misses={misses}");
        bench::print_stats("live.layout", &s.layouts_us);
        bench::print_stats("live.update", &s.updates_us);
        bench::print_stats("live.paint", &s.paints_us);
        bench::print_stats("live.paint_render", &s.paint_render_us);
        bench::print_stats("live.paint_copy", &s.paint_copy_us);
        for (i, n) in s.layout_boxes.iter().enumerate() {
            println!("live.layout_boxes.{i}={n}");
        }
        flush_stdout();
    }

    /// Takes new data from a running scan, if the throttle allows, and lays
    /// the current view out again.
    fn pump_scan(&mut self) {
        let t0 = self.t0;
        let Some(live) = self.live.as_mut() else {
            return;
        };
        if live.scan.is_none() {
            return;
        }
        // A zoom in flight finishes first; the data stays published.
        if live.renderer.is_animating() {
            return;
        }
        let tick = match live.scan.as_mut() {
            Some(scan) => scan.tick(Instant::now()),
            None => return,
        };
        let (snapshot, done, problem) = match tick {
            Tick::Idle => return,
            Tick::Data(s) => (s, false, None),
            Tick::Finished(s, p) => (s, true, p),
        };
        let began = Instant::now();
        let now = t0.elapsed();
        let metrics = Metrics::from_logical(font_px(), live.renderer.scale());
        let mut boxes = 0usize;
        let mut layout_us = 0u64;
        match live.app.as_mut() {
            Some(app) => {
                app.source_mut().set_snapshot(Arc::clone(&snapshot));
                let fx = app.update(InputEvent::DataChanged, now);
                let stats = app.source().last_layout();
                boxes = stats.items;
                layout_us = stats.took.as_micros() as u64;
                apply(&mut live.renderer, fx, now);
            }
            None => {
                if let Some(source) = TreeSource::new(Arc::clone(&snapshot), metrics) {
                    let (w, h) = live.renderer.size();
                    let (_, map) =
                        chrome::layout(w as f32, h as f32, live.renderer.scale(), "", "");
                    let app = App::new(source, map);
                    let stats = app.source().last_layout();
                    boxes = stats.items;
                    layout_us = stats.took.as_micros() as u64;
                    live.renderer.set_items(app.items());
                    live.app = Some(app);
                }
            }
        }
        // The first snapshot is often just the root, which lays out as one
        // empty box. The first frame that shows the folder's contents is the
        // one worth timing.
        if boxes >= 2 && !self.map_seen {
            self.map_seen = true;
            self.map_pending = true;
        }
        live.status = match problem {
            Some(p) => p,
            None => scan_status(&snapshot),
        };
        refresh_bars(live);
        let cost = began.elapsed();
        if let Some(scan) = live.scan.as_mut() {
            scan.note_layout_cost(cost);
        }
        self.stats.layouts_us.push(layout_us);
        self.stats.layout_boxes.push(boxes as u64);
        self.stats.updates_us.push(cost.as_micros() as u64);
        if done {
            self.stats.scan_done_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
            self.finish_pending = true;
        }
        live.window.request_redraw();
    }

    fn feed(&mut self, event: InputEvent) {
        let now = self.t0.elapsed();
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let Some(app) = live.app.as_mut() else {
            return;
        };
        app.set_animating(live.renderer.is_animating());
        let fx = app.update(event, now);
        if fx.is_empty() {
            return;
        }
        if apply(&mut live.renderer, fx, now) {
            refresh_bars(live);
        }
        live.window.request_redraw();
    }

    fn resize(&mut self) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let size = live.window.inner_size();
        let scale = live.window.scale_factor() as f32;
        if size.width == 0 || size.height == 0 {
            return;
        }
        if live.presenter.resize(size.width, size.height).is_err() {
            return;
        }
        live.renderer.resize(size.width, size.height, scale);
        let map = refresh_bars(live);
        match live.app.as_mut() {
            Some(app) => {
                app.source_mut()
                    .set_metrics(Metrics::from_logical(font_px(), scale));
                let fx = app.update(InputEvent::Resized { map }, Duration::ZERO);
                apply(&mut live.renderer, fx, Duration::ZERO);
            }
            None if self.scan_path.is_some() => live
                .renderer
                .set_items(Arc::new(vec![scan_placeholder(map)])),
            None => live.renderer.set_items(Arc::new(placeholders(map, scale))),
        }
        live.window.request_redraw();
    }
}

fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// The empty block shown for the map until the first snapshot of a scan.
fn scan_placeholder(map: Rect) -> Item {
    Item {
        rect: map,
        kind: ItemKind::FolderBlock,
        depth: 0,
        label: None,
        id: 1,
        category: 0,
        flags: Flags::default(),
    }
}

/// Drive-card placeholders shown when there is nothing to scan yet.
fn placeholders(map: Rect, scale: f32) -> Vec<Item> {
    let gap = (12.0 * scale).round();
    let w = ((map.w - 3.0 * gap) / 2.0).floor();
    let h = ((map.h - 2.0 * gap) * 0.5).floor();
    let mk = |id, x, y, w, h| Item {
        rect: Rect::new(x, y, w, h),
        kind: ItemKind::FolderBlock,
        depth: 0,
        label: None,
        id,
        category: 0,
        flags: Flags::default(),
    };
    vec![
        mk(1, map.x + gap, map.y + gap, w, h),
        mk(2, map.x + 2.0 * gap + w, map.y + gap, w, h),
        mk(
            3,
            map.x + gap,
            map.y + 2.0 * gap + h,
            w * 2.0 + gap,
            (h * 0.5).floor(),
        ),
    ]
}

impl ApplicationHandler for WinApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.live.is_none()
            && let Err(e) = self.create(el)
        {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.redraw(el),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.resize(),
            WindowEvent::ThemeChanged(theme) => {
                if let Some(live) = self.live.as_mut() {
                    live.renderer.set_palette(palette_for(theme == Theme::Dark));
                    live.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let PhysicalPosition { x, y } = position;
                self.cursor = (x as f32, y as f32);
                self.feed(InputEvent::PointerMoved {
                    x: x as f32,
                    y: y as f32,
                });
            }
            WindowEvent::CursorLeft { .. } => self.feed(InputEvent::PointerLeft),
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0) as f32,
                };
                let (x, y) = self.cursor;
                self.feed(InputEvent::Wheel { x, y, steps });
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                let (x, y) = self.cursor;
                match button {
                    MouseButton::Left => {
                        let now = Instant::now();
                        let double = self.last_click.is_some_and(|(t, (lx, ly))| {
                            now.duration_since(t) < Duration::from_millis(400)
                                && (lx - x).abs() < 5.0
                                && (ly - y).abs() < 5.0
                        });
                        self.last_click = if double { None } else { Some((now, (x, y))) };
                        if double {
                            self.feed(InputEvent::DoubleClick { x, y });
                        } else {
                            self.feed(InputEvent::Click { x, y });
                        }
                    }
                    MouseButton::Back => self.feed(InputEvent::Key(Key::AltLeft)),
                    MouseButton::Forward => self.feed(InputEvent::Key(Key::AltRight)),
                    _ => {}
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let alt = self.modifiers.alt_key();
                let key = match event.logical_key {
                    WKey::Named(NamedKey::Enter) => Some(Key::Enter),
                    WKey::Named(NamedKey::Backspace) => Some(Key::Backspace),
                    WKey::Named(NamedKey::Home) => Some(Key::Home),
                    WKey::Named(NamedKey::Escape) => Some(Key::Escape),
                    WKey::Named(NamedKey::ArrowLeft) if alt => Some(Key::AltLeft),
                    WKey::Named(NamedKey::ArrowRight) if alt => Some(Key::AltRight),
                    WKey::Named(NamedKey::ArrowUp) if alt => Some(Key::AltUp),
                    _ => None,
                };
                if let Some(key) = key {
                    self.feed(InputEvent::Key(key));
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.pump_scan();
        if matches!(self.mode, Mode::Zoom(_)) {
            return;
        }
        // Fonts load on a worker thread; redraw once when they arrive so labels
        // appear, polling lightly until then. A running scan is polled for new
        // data at a steady, light rate; nothing else keeps the loop awake.
        let mut poll: Option<Duration> = None;
        if !self.fonts_announced {
            if self.text.is_ready() {
                self.fonts_announced = true;
                if let Some(live) = self.live.as_ref() {
                    live.window.request_redraw();
                }
            } else {
                poll = Some(Duration::from_millis(16));
            }
        }
        let scanning = self
            .live
            .as_ref()
            .is_some_and(|l| l.scan.as_ref().is_some_and(ScanDriver::running));
        // The scan starts after the first present, so until then the loop
        // must come back by itself too.
        if scanning || (self.scan_path.is_some() && self.frames == 0) {
            let scan_poll = Duration::from_millis(25);
            poll = Some(poll.map_or(scan_poll, |p| p.min(scan_poll)));
        }
        match poll {
            Some(d) => el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + d)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}
