//! One renderer type over both drawing backends, and the window surface that
//! goes with it.
//!
//! [`AnyRenderer`] has the calls the application makes and forwards them to
//! the software [`Renderer`] or the GPU [`GpuRenderer`]. [`Screen`] opens a
//! window surface with the backend the caller prefers, and when the GPU
//! reports an error it switches the renderer and the surface to software
//! drawing for the rest of the session, carrying the picture's state over.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::card::{Card, Popup};
use crate::chrome::Chrome;
use crate::geom::Rect;
use crate::gpu::{
    AdapterChoice, BackendPreference, Geometry, GpuContext, GpuError, GpuOptions, GpuPresenter,
    GpuRenderer, GpuWindow, PresentPacing, WindowBackend, choose_window, guard,
};
use crate::present::Presenter;
use crate::renderer::{PaintTimings, Painted, Renderer, ZoomDirection};
use crate::scene::{Item, Palette};
use crate::text::TextEngine;

/// A software or a GPU renderer behind the same calls.
pub enum AnyRenderer {
    Software(Box<Renderer>),
    Gpu(Box<GpuRenderer>),
}

macro_rules! both {
    ($self:ident, $r:ident => $e:expr) => {
        match $self {
            AnyRenderer::Software($r) => $e,
            AnyRenderer::Gpu($r) => $e,
        }
    };
}

/// The GPU shared by every offscreen renderer of this process, so a run that
/// makes many pictures starts the device once.
fn shared_gpu(options: &GpuOptions) -> Result<Arc<GpuContext>, GpuError> {
    type Slot = Option<(AdapterChoice, Arc<GpuContext>)>;
    static SHARED: Mutex<Slot> = Mutex::new(None);
    // A forced failure must reach the start-up step it names.
    if options.fail_at.is_some() {
        return guard(|| GpuContext::headless(options));
    }
    let mut slot = SHARED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, g)) = slot
        .as_ref()
        .filter(|(a, g)| *a == options.adapter && !g.has_failed())
    {
        return Ok(Arc::clone(g));
    }
    let g = guard(|| GpuContext::headless(options))?;
    *slot = Some((options.adapter, Arc::clone(&g)));
    Ok(g)
}

impl AnyRenderer {
    /// A renderer that draws into memory, for pictures and benchmarks. It uses
    /// the GPU only when `preference` is [`BackendPreference::Gpu`], so a
    /// picture made without asking is the same on every machine. Returns the
    /// renderer and, when the GPU was asked for and not used, why.
    pub fn offscreen(
        geometry: Geometry,
        palette: Palette,
        text: TextEngine,
        preference: BackendPreference,
    ) -> Option<(Self, Option<String>)> {
        Self::offscreen_with(geometry, palette, text, preference, &GpuOptions::from_env())
    }

    /// [`AnyRenderer::offscreen`] with explicit GPU options.
    pub fn offscreen_with(
        geometry: Geometry,
        palette: Palette,
        text: TextEngine,
        preference: BackendPreference,
        options: &GpuOptions,
    ) -> Option<(Self, Option<String>)> {
        let mut why = None;
        if preference == BackendPreference::Gpu {
            let made = guard(|| {
                let gpu = shared_gpu(options)?;
                GpuRenderer::new(gpu, geometry, palette.clone(), text.clone(), options)
            });
            match made {
                Ok(r) => return Some((Self::Gpu(Box::new(r)), None)),
                Err(e) => why = Some(e.to_string()),
            }
        }
        let r = Renderer::new(
            geometry.width,
            geometry.height,
            geometry.scale,
            geometry.font_px,
            palette,
            text,
        )?;
        Some((Self::Software(Box::new(r)), why))
    }

    /// Which backend draws, in a few plain words.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Software(_) => "software",
            Self::Gpu(_) => "GPU",
        }
    }

    /// The GPU adapter's name, when drawing on the GPU.
    pub fn adapter_name(&self) -> Option<String> {
        match self {
            Self::Software(_) => None,
            Self::Gpu(g) => Some(g.gpu().adapter_info().name.clone()),
        }
    }

    pub fn set_items(&mut self, items: Arc<Vec<Item>>) {
        both!(self, r => r.set_items(items))
    }

    pub fn set_items_background(&mut self, items: Arc<Vec<Item>>) -> bool {
        both!(self, r => r.set_items_background(items))
    }

    pub fn poll_background(&mut self) -> bool {
        both!(self, r => r.poll_background())
    }

    pub fn background_pending(&self) -> bool {
        both!(self, r => r.background_pending())
    }

    pub fn set_chrome(&mut self, chrome: Chrome) -> bool {
        both!(self, r => r.set_chrome(chrome))
    }

    pub fn set_card(&mut self, card: Option<Card>) -> bool {
        both!(self, r => r.set_card(card))
    }

    pub fn set_toast(&mut self, toast: Option<Card>) -> bool {
        both!(self, r => r.set_toast(toast))
    }

    pub fn set_popup(&mut self, popup: Option<Popup>) -> bool {
        both!(self, r => r.set_popup(popup))
    }

    pub fn set_dim(&mut self, category: Option<u8>) {
        both!(self, r => r.set_dim(category))
    }

    pub fn set_pulse(&mut self, pulse: f32) {
        both!(self, r => r.set_pulse(pulse))
    }

    pub fn set_hover(&mut self, id: Option<u32>) {
        both!(self, r => r.set_hover(id))
    }

    pub fn set_selected(&mut self, id: Option<u32>) {
        both!(self, r => r.set_selected(id))
    }

    pub fn set_selection(&mut self, ids: &[u32]) {
        both!(self, r => r.set_selection(ids))
    }

    pub fn set_palette(&mut self, palette: Palette) {
        both!(self, r => r.set_palette(palette))
    }

    pub fn set_reduced_motion(&mut self, on: bool) {
        both!(self, r => r.set_reduced_motion(on))
    }

    pub fn begin_zoom(
        &mut self,
        direction: ZoomDirection,
        items: Arc<Vec<Item>>,
        child: Rect,
        now: Duration,
    ) {
        both!(self, r => r.begin_zoom(direction, items, child, now))
    }

    pub fn finish_animation(&mut self) {
        both!(self, r => r.finish_animation())
    }

    pub fn is_animating(&self) -> bool {
        both!(self, r => r.is_animating())
    }

    pub fn items(&self) -> &[Item] {
        both!(self, r => r.items())
    }

    pub fn item_at(&self, x: f32, y: f32) -> Option<&Item> {
        both!(self, r => r.item_at(x, y))
    }

    pub fn size(&self) -> (u32, u32) {
        both!(self, r => r.size())
    }

    pub fn scale(&self) -> f32 {
        both!(self, r => r.scale())
    }

    /// Threads a full repaint uses (the GPU renderer records on one).
    pub fn threads(&self) -> usize {
        match self {
            Self::Software(r) => r.threads(),
            Self::Gpu(_) => 1,
        }
    }

    pub fn timings(&self) -> PaintTimings {
        both!(self, r => r.timings())
    }

    pub fn palette(&self) -> &Palette {
        both!(self, r => r.palette())
    }

    pub fn text(&self) -> &TextEngine {
        both!(self, r => r.text())
    }

    pub fn chrome(&self) -> &Chrome {
        both!(self, r => r.chrome())
    }

    /// Draws the current picture into `target` (`width × height` native pixel
    /// words, `0xAARRGGBB`). The software renderer writes only what changed
    /// unless `whole` is set; the GPU renderer draws a whole picture and reads
    /// it back, which is for pictures and tests, not for a window.
    pub fn paint_words(&mut self, target: &mut [u32], now: Duration, whole: bool) -> Painted {
        match self {
            Self::Software(r) => r.paint(target, now, whole),
            Self::Gpu(g) => {
                let (w, h) = g.size();
                let n = w as usize * h as usize;
                let painted = g.paint_offscreen(now);
                let rgba = g.read_offscreen();
                let (Ok(painted), Ok(rgba)) = (painted, rgba) else {
                    return Painted {
                        damage: Vec::new(),
                        animating: false,
                    };
                };
                for (word, px) in target.iter_mut().take(n).zip(rgba.as_chunks::<4>().0) {
                    *word = 0xFF00_0000
                        | (u32::from(px[0]) << 16)
                        | (u32::from(px[1]) << 8)
                        | u32::from(px[2]);
                }
                painted
            }
        }
    }

    /// The current picture, fully drawn, as straight RGBA.
    pub fn snapshot_rgba(&mut self) -> Result<Vec<u8>, String> {
        match self {
            Self::Software(r) => Ok(r.snapshot_rgba()),
            Self::Gpu(g) => g.snapshot_rgba().map_err(|e| e.to_string()),
        }
    }

    pub fn save_png(&mut self, path: &std::path::Path) -> Result<(), String> {
        both!(self, r => r.save_png(path))
    }
}

enum Surface<W: HasWindowHandle + HasDisplayHandle + Clone + Send + Sync + 'static> {
    Software(Presenter<W, W>),
    Gpu(Box<GpuPresenter>),
}

/// How the GPU is started for a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Startup {
    /// Wait for the GPU before the first frame.
    Wait,
    /// Draw the first frames in software while the GPU starts on another
    /// thread, and switch when it is ready. Starting a GPU can take longer
    /// than the time allowed for the first frame.
    Background,
}

/// Called from the GPU start-up thread when it finishes, so the caller can ask
/// for a repaint and the switch happens without waiting for other input.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

type GpuStart = Result<(Arc<GpuContext>, GpuPresenter), GpuError>;

/// A frame that takes the GPU path this long to present counts as slow. Some
/// displays (a remote X server without direct rendering, for one) make every
/// GPU frame slower than drawing on the CPU; after `SLOW_FRAMES` in a row the
/// session goes back to CPU drawing. Only a session that started on the CPU
/// watches, so a measurement that asked for the GPU is never switched.
const SLOW_FRAME: Duration = Duration::from_millis(100);
const SLOW_FRAMES: u32 = 5;

/// Counts GPU frames that took too long to present.
#[derive(Default)]
struct SlowWatch {
    enabled: bool,
    run: u32,
}

impl SlowWatch {
    /// Notes one presented frame (`drew` is false for a frame that drew
    /// nothing). Returns true when the last `SLOW_FRAMES` frames in a row were
    /// all slow.
    fn observe(&mut self, took: Duration, drew: bool) -> bool {
        if !self.enabled {
            return false;
        }
        if drew && took > SLOW_FRAME {
            self.run += 1;
        } else if drew {
            self.run = 0;
        }
        self.run >= SLOW_FRAMES
    }
}

/// What decides how a window is drawn.
#[derive(Clone)]
pub struct ScreenOptions {
    pub preference: BackendPreference,
    pub pacing: PresentPacing,
    pub startup: Startup,
    /// Called when the GPU finishes starting (see [`Wake`]).
    pub wake: Option<Wake>,
}

/// A window's drawing surface and what decides how it is drawn.
pub struct Screen<W: HasWindowHandle + HasDisplayHandle + Clone + Send + Sync + 'static> {
    window: W,
    surface: Surface<W>,
    size: (u32, u32),
    /// Why drawing is not on the GPU, when it was tried or asked for.
    reason: Option<String>,
    /// Set once, when a GPU error ended GPU drawing during the session.
    switched: bool,
    /// The GPU being started on another thread.
    starting: Option<Receiver<GpuStart>>,
    start_thread: Option<std::thread::JoinHandle<()>>,
    /// The GPU start-up waits until the first frame is on screen, so it
    /// cannot slow that frame down.
    start_after_first_frame: Option<PresentPacing>,
    wake: Option<Wake>,
    /// Frames in a row that took the GPU path too long to present, when the
    /// session watches for that (see `SLOW_FRAME`).
    watch: SlowWatch,
    options: GpuOptions,
}

impl<W: HasWindowHandle + HasDisplayHandle + Clone + Send + Sync + 'static> Screen<W> {
    /// Opens the surface for `window` and a renderer for it. The GPU is tried
    /// unless `preference` says software; a failure at any step, even a panic
    /// inside the driver stack, gives software drawing and the reason.
    pub fn open(
        window: W,
        geometry: Geometry,
        palette: Palette,
        text: TextEngine,
        screen: ScreenOptions,
    ) -> Result<(Self, AnyRenderer), String> {
        let ScreenOptions {
            preference,
            pacing,
            startup,
            wake,
        } = screen;
        let size = (geometry.width, geometry.height);
        let options = GpuOptions::from_env();
        let background = startup == Startup::Background && preference == BackendPreference::Auto;
        let preference = if background {
            BackendPreference::Software
        } else {
            preference
        };
        let chosen = choose_window(
            window.clone(),
            size,
            geometry.scale,
            geometry.font_px,
            palette.clone(),
            text.clone(),
            preference,
            pacing,
            &options,
        );
        match chosen {
            WindowBackend::Gpu(g) => {
                let GpuWindow {
                    renderer,
                    presenter,
                } = *g;
                Ok((
                    Self {
                        window,
                        surface: Surface::Gpu(Box::new(presenter)),
                        size,
                        reason: None,
                        switched: false,
                        starting: None,
                        start_thread: None,
                        start_after_first_frame: None,
                        wake: None,
                        watch: SlowWatch::default(),
                        options,
                    },
                    AnyRenderer::Gpu(Box::new(renderer)),
                ))
            }
            WindowBackend::Software { reason } => {
                let presenter = Presenter::new(window.clone(), window.clone(), size.0, size.1)
                    .map_err(|e| e.to_string())?;
                let renderer = Renderer::new(
                    geometry.width,
                    geometry.height,
                    geometry.scale,
                    geometry.font_px,
                    palette,
                    text,
                )
                .ok_or("the window has no area")?;
                let reason = (preference != BackendPreference::Software).then_some(reason);
                Ok((
                    Self {
                        window,
                        surface: Surface::Software(presenter),
                        size,
                        reason,
                        switched: false,
                        starting: None,
                        start_thread: None,
                        start_after_first_frame: background.then_some(pacing),
                        wake,
                        watch: SlowWatch::default(),
                        options,
                    },
                    AnyRenderer::Software(Box::new(renderer)),
                ))
            }
        }
    }

    /// Starts the GPU on another thread.
    fn start_gpu(&mut self, pacing: PresentPacing) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (window, size, options) = (self.window.clone(), self.size, self.options);
        let wake = self.wake.clone();
        let spawned = std::thread::Builder::new()
            .name("gpu start".into())
            .spawn(move || {
                let _ = tx.send(guard(|| GpuPresenter::open(window, size, pacing, &options)));
                if let Some(wake) = wake {
                    wake();
                }
            });
        if let Ok(handle) = spawned {
            self.starting = Some(rx);
            self.start_thread = Some(handle);
        }
    }

    /// Takes over with the GPU once it has started, carrying the picture over.
    fn adopt_started_gpu(&mut self, renderer: &mut AnyRenderer) {
        let Some(rx) = &self.starting else {
            return;
        };
        let started = match rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                Err(GpuError::Lost("the GPU start-up ended early".into()))
            }
            Ok(r) => r,
        };
        self.starting = None;
        if let Some(handle) = self.start_thread.take() {
            let _ = handle.join();
        }
        let outcome = started.and_then(|(gpu, mut presenter)| {
            let AnyRenderer::Software(software) = &*renderer else {
                return Err(GpuError::Lost("already drawing on the GPU".into()));
            };
            let gpu_renderer = guard(|| GpuRenderer::from_software(gpu, software, &self.options))?;
            presenter.resize(self.size.0, self.size.1);
            Ok((presenter, gpu_renderer))
        });
        match outcome {
            Ok((presenter, gpu_renderer)) => {
                *renderer = AnyRenderer::Gpu(Box::new(gpu_renderer));
                self.surface = Surface::Gpu(Box::new(presenter));
                self.reason = None;
                self.watch.enabled = true;
            }
            Err(e) => {
                eprintln!("bytescape: GPU drawing is not available ({e}); using software drawing");
                self.reason = Some(e.to_string());
            }
        }
    }

    /// Why the GPU is not drawing, if it is not and that is not by choice.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// True after a GPU error moved drawing to software during the session.
    pub fn switched_to_software(&self) -> bool {
        self.switched
    }

    /// The backend in use, in plain words: "GPU (name)", "software", or
    /// "software (reason the GPU was not used)".
    pub fn description(&self, renderer: &AnyRenderer) -> String {
        if self.starting.is_some() {
            return "Drawing: software while the GPU starts".into();
        }
        drawing_line(renderer.adapter_name().as_deref(), self.reason.as_deref())
    }

    /// Moves drawing to software, carrying the picture's state over.
    fn fall_back(&mut self, renderer: &mut AnyRenderer, why: String) -> Result<(), String> {
        let AnyRenderer::Gpu(g) = renderer else {
            return Ok(());
        };
        let software = g
            .to_software()
            .ok_or_else(|| "the window has no area".to_string())?;
        let presenter = Presenter::new(
            self.window.clone(),
            self.window.clone(),
            self.size.0,
            self.size.1,
        )
        .map_err(|e| e.to_string())?;
        *renderer = AnyRenderer::Software(Box::new(software));
        self.surface = Surface::Software(presenter);
        self.reason = Some(why);
        self.switched = true;
        Ok(())
    }

    /// Draws the current picture into the window and shows it. A GPU error
    /// ends GPU drawing for the session and the same frame is drawn in
    /// software.
    pub fn present(&mut self, renderer: &mut AnyRenderer, now: Duration) -> Painted {
        self.adopt_started_gpu(renderer);
        let outcome = match (&mut self.surface, &mut *renderer) {
            (Surface::Gpu(p), AnyRenderer::Gpu(g)) => {
                let started = std::time::Instant::now();
                let result = guard(|| p.present(g, now));
                Some((result, started.elapsed()))
            }
            _ => None,
        };
        let mut gpu_failed = None;
        if let Some((result, took)) = outcome {
            match result {
                Ok(mut painted) => {
                    if !self.watch.observe(took, !painted.damage.is_empty()) {
                        return painted;
                    }
                    let why = format!(
                        "GPU drawing took over {} ms a frame on this display",
                        SLOW_FRAME.as_millis()
                    );
                    eprintln!("bytescape: {why}; using software drawing");
                    if self.fall_back(renderer, why).is_ok() {
                        // Draw the next frame at once, in software.
                        painted.animating = true;
                    }
                    return painted;
                }
                Err(e) => gpu_failed = Some(e.to_string()),
            }
        }
        if let Some(why) = gpu_failed {
            eprintln!("bytescape: GPU drawing stopped ({why}); using software drawing");
            if let Err(e) = self.fall_back(renderer, why) {
                eprintln!("bytescape: could not switch to software drawing: {e}");
                return Painted {
                    damage: Vec::new(),
                    animating: false,
                };
            }
        }
        let (Surface::Software(p), AnyRenderer::Software(r)) = (&mut self.surface, renderer) else {
            return Painted {
                damage: Vec::new(),
                animating: false,
            };
        };
        let mut painted = Painted {
            damage: Vec::new(),
            animating: false,
        };
        let result = p.present_with(|buf, whole| {
            painted = r.paint(buf, now, whole);
            painted.damage.clone()
        });
        if let Err(e) = result {
            eprintln!("bytescape: present failed: {e}");
        }
        if let Some(pacing) = self.start_after_first_frame.take() {
            self.start_gpu(pacing);
        }
        painted
    }

    /// Resizes the surface and the renderer (size and scale factor together).
    /// A GPU error moves drawing to software; the picture is then drawn at the
    /// new size by the software renderer.
    pub fn resize(
        &mut self,
        renderer: &mut AnyRenderer,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let failed = match (&mut self.surface, &mut *renderer) {
            (Surface::Gpu(p), AnyRenderer::Gpu(g)) => {
                p.resize(width, height);
                match g.resize(width, height, scale) {
                    Ok(()) => None,
                    Err(e) => Some(e.to_string()),
                }
            }
            (Surface::Software(p), AnyRenderer::Software(r)) => {
                p.resize(width, height).map_err(|e| e.to_string())?;
                r.resize(width, height, scale);
                None
            }
            _ => None,
        };
        self.size = (width, height);
        if let Some(why) = failed {
            eprintln!("bytescape: GPU drawing stopped ({why}); using software drawing");
            self.fall_back(renderer, why)?;
            if let AnyRenderer::Software(r) = renderer {
                r.resize(width, height, scale);
            }
        }
        Ok(())
    }
}

impl<W: HasWindowHandle + HasDisplayHandle + Clone + Send + Sync + 'static> Drop for Screen<W> {
    fn drop(&mut self) {
        // Leaving while a graphics driver is still starting up on its thread
        // makes some drivers abort the whole program as it exits, so wait for
        // the start-up to finish first.
        if let Some(handle) = self.start_thread.take() {
            let _ = handle.join();
        }
    }
}

/// The line the About box shows for the drawing backend: "Drawing: GPU
/// (adapter)", "Drawing: software", or "Drawing: software, because ..." when
/// the GPU was not used for a reason other than the user's choice.
pub fn drawing_line(adapter: Option<&str>, software_reason: Option<&str>) -> String {
    match (adapter, software_reason) {
        (Some(name), _) => format!("Drawing: GPU ({name})"),
        (None, Some(why)) => format!("Drawing: software, because {}", short(why)),
        (None, None) => "Drawing: software".to_string(),
    }
}

/// The first line of `reason`, cut to a length that fits one line of the
/// About box.
fn short(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or("");
    let line = line
        .trim_start_matches("graphics device: ")
        .trim_start_matches("no usable graphics adapter: ");
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_run_of_slow_frames_trips_the_watch() {
        let slow = SLOW_FRAME + Duration::from_millis(1);
        let fast = Duration::from_millis(5);
        let mut w = SlowWatch {
            enabled: true,
            run: 0,
        };
        for _ in 0..SLOW_FRAMES - 1 {
            assert!(!w.observe(slow, true));
        }
        // A frame that draws nothing does not count either way.
        assert!(!w.observe(slow, false) || SLOW_FRAMES == 1);
        assert!(w.observe(slow, true));
        // One quick frame starts the count again.
        let mut w = SlowWatch {
            enabled: true,
            run: 0,
        };
        for _ in 0..SLOW_FRAMES - 1 {
            w.observe(slow, true);
        }
        assert!(!w.observe(fast, true));
        for _ in 0..SLOW_FRAMES - 1 {
            assert!(!w.observe(slow, true));
        }
    }

    #[test]
    fn a_session_that_asked_for_the_gpu_is_never_switched() {
        let mut w = SlowWatch::default();
        for _ in 0..100 {
            assert!(!w.observe(Duration::from_secs(1), true));
        }
    }
}
