//! The window's state around the map: chrome, info bar, legend hover, the
//! capacity bar, the scan status and the look. It wraps the map's [`App`] and
//! turns events in window coordinates into effects for the host, with no
//! windowing code in it.

use std::time::Duration;

use bs_render::scene::{CATEGORY_COUNT, CATEGORY_NAMES};
use bs_render::{Bar, Chrome, Color, Glyph, Palette, Rect, TextEngine, TextRole, Widget};

use crate::capacity::{Breakdown, CapacityModel};
use crate::chrome::{self, Geometry, Hit, LegendMode, TextMeasure};
use crate::info::{self, InfoLine};
use crate::{App, Effect, InputEvent, Key, ViewSource};

/// What the host should do after an event.
#[derive(Clone, Debug)]
pub enum ShellEffect {
    /// Apply to the renderer.
    Map(Effect),
    /// The legend is hovered: dim every category but this one (or stop).
    Dim(Option<u8>),
    /// Open this address in the user's browser.
    OpenLink(&'static str),
}

/// A [`TextMeasure`] on the renderer's text engine.
pub struct EngineMeasure {
    engine: TextEngine,
    size: f32,
    scale: f32,
}

impl EngineMeasure {
    /// Measures text at `size` logical pixels and `scale`.
    pub fn new(engine: TextEngine, size: f32, scale: f32) -> Self {
        Self {
            engine,
            size,
            scale,
        }
    }
}

impl TextMeasure for EngineMeasure {
    fn width(&self, text: &str) -> Option<f32> {
        self.engine.measure(text, self.size, self.scale)
    }
}

/// How the window looks, from the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    pub dark: bool,
    pub accent: Option<Color>,
}

/// The pulse of "Scanning…" in the title text: opacity between these.
const PULSE_LOW: f32 = 0.55;
/// The pulse changes in steps of this size, so label caching stays small.
const PULSE_STEP: f32 = 0.05;

/// All state of the window except the pixels.
pub struct Shell<S: ViewSource> {
    app: Option<App<S>>,
    width: u32,
    height: u32,
    scale: f32,
    font_px: f32,
    text: TextEngine,
    measure: Box<dyn TextMeasure>,
    geom: Geometry,
    geom_key: (u32, u32, u32, bool),
    crumb: String,
    status: String,
    scanning: bool,
    capacity: CapacityModel,
    hot: Option<Hit>,
    dim: Option<u8>,
    look: Look,
    reduced_motion: bool,
    drive_total: Option<u64>,
}

fn measure_for(text: &TextEngine, font_px: f32, scale: f32) -> Box<dyn TextMeasure> {
    Box::new(EngineMeasure::new(text.clone(), font_px, scale))
}

impl<S: ViewSource> Shell<S> {
    /// A window of `width × height` device pixels.
    pub fn new(
        size: (u32, u32),
        scale: f32,
        font_px: f32,
        text: TextEngine,
        look: Look,
    ) -> Shell<S> {
        let measure = measure_for(&text, font_px, scale);
        let geom = Self::compute(size, scale, font_px, measure.as_ref());
        Shell {
            app: None,
            width: size.0,
            height: size.1,
            scale,
            font_px,
            text,
            measure,
            geom,
            geom_key: (size.0, size.1, scale.to_bits(), false),
            crumb: String::new(),
            status: String::new(),
            scanning: false,
            capacity: CapacityModel::default(),
            hot: None,
            dim: None,
            look,
            reduced_motion: false,
            drive_total: None,
        }
    }

    /// Replaces how text is measured (tests use a fixed width).
    pub fn with_measure(mut self, measure: Box<dyn TextMeasure>) -> Self {
        self.measure = measure;
        self.geom_key.3 = !self.geom_key.3;
        self
    }

    fn compute(size: (u32, u32), scale: f32, font_px: f32, m: &dyn TextMeasure) -> Geometry {
        chrome::geometry(
            size.0 as f32,
            size.1 as f32,
            scale,
            font_px * scale,
            &chrome::LEGEND_NAMES,
            m,
        )
    }

    /// The chrome geometry, laid out again when the window or the fonts
    /// changed.
    pub fn geometry(&mut self) -> &Geometry {
        let key = (
            self.width,
            self.height,
            self.scale.to_bits(),
            self.text.is_ready(),
        );
        if key != self.geom_key {
            self.geom_key = key;
            self.geom = Self::compute(
                (self.width, self.height),
                self.scale,
                self.font_px,
                self.measure.as_ref(),
            );
        }
        &self.geom
    }

    /// The rectangle the map fills.
    pub fn map_rect(&self) -> Rect {
        chrome::map_rect(self.width as f32, self.height as f32, self.scale)
    }

    pub fn app(&self) -> Option<&App<S>> {
        self.app.as_ref()
    }

    pub fn app_mut(&mut self) -> Option<&mut App<S>> {
        self.app.as_mut()
    }

    /// Installs the map once the first data is there.
    pub fn set_app(&mut self, app: App<S>) {
        self.app = Some(app);
    }

    /// Reduced motion replaces the capacity glide and the title pulse with
    /// instant changes.
    pub fn set_reduced_motion(&mut self, on: bool) {
        self.reduced_motion = on;
    }

    pub fn reduced_motion(&self) -> bool {
        self.reduced_motion
    }

    /// Sets the breadcrumb shown before a map exists (for example the folder
    /// being scanned).
    pub fn set_crumb(&mut self, crumb: String) {
        self.crumb = crumb;
    }

    /// Sets the status text shown when nothing is hovered or selected, and
    /// whether a scan is running.
    pub fn set_status(&mut self, status: String, scanning: bool) {
        self.status = status;
        self.scanning = scanning;
    }

    /// Whether the pointer is on something that can be clicked, so the window
    /// shows a pointing hand.
    pub fn wants_pointer_cursor(&self) -> bool {
        match self.hot {
            Some(Hit::Link | Hit::Swatch(_) | Hit::Refresh | Hit::Overflow) => true,
            Some(Hit::Back) => self.can_go_back(),
            Some(Hit::Forward) => self.can_go_forward(),
            Some(Hit::Up) => self.can_go_up(),
            _ => false,
        }
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    /// Sets the size of the drive, for "of drive" in the info bar.
    pub fn set_drive_total(&mut self, total: Option<u64>) {
        self.drive_total = total;
    }

    /// Feeds the capacity bar.
    pub fn set_capacity(&mut self, breakdown: Breakdown, now: Duration) {
        self.drive_total = Some(breakdown.total);
        self.capacity.set(breakdown, now, self.reduced_motion);
    }

    /// Whether the capacity bar is still moving.
    pub fn capacity_changing(&self, now: Duration) -> bool {
        self.capacity.is_changing(now)
    }

    /// The window changed size or scale. Returns the effects of laying the
    /// map out again for the new size.
    pub fn resize(&mut self, size: (u32, u32), scale: f32, now: Duration) -> Vec<ShellEffect> {
        self.width = size.0;
        self.height = size.1;
        self.scale = scale;
        self.measure = measure_for(&self.text, self.font_px, scale);
        self.geom_key.3 = !self.text.is_ready();
        self.hot = None;
        self.dim = None;
        let map = self.map_rect();
        self.map_event(InputEvent::Resized { map }, now)
    }

    /// The palette for the current look.
    pub fn palette(&self) -> Palette {
        let pal = Palette::for_scheme(self.look.dark);
        match self.look.accent {
            Some(a) => pal.with_accent(a),
            None => pal,
        }
    }

    pub fn look(&self) -> Look {
        self.look
    }

    /// Takes a new look from the system. Returns whether it changed.
    pub fn set_look(&mut self, look: Look) -> bool {
        let changed = self.look != look;
        self.look = look;
        changed
    }

    /// The opacity of the title text of folders still being counted: a soft
    /// pulse once a second while a scan runs, otherwise solid.
    pub fn pulse(&self, now: Duration) -> f32 {
        if !self.scanning || self.reduced_motion {
            return 1.0;
        }
        let phase = now.as_secs_f32().fract();
        let wave = 0.5 + 0.5 * (phase * std::f32::consts::TAU).cos();
        let v = PULSE_LOW + (1.0 - PULSE_LOW) * wave;
        (v / PULSE_STEP).round() * PULSE_STEP
    }

    /// Whether the pulse needs frames.
    pub fn pulsing(&self) -> bool {
        self.scanning && !self.reduced_motion
    }

    fn map_event(&mut self, event: InputEvent, now: Duration) -> Vec<ShellEffect> {
        match self.app.as_mut() {
            Some(app) => app
                .update(event, now)
                .into_iter()
                .map(ShellEffect::Map)
                .collect(),
            None => Vec::new(),
        }
    }

    fn set_hot(&mut self, hit: Option<Hit>) -> Vec<ShellEffect> {
        if hit == self.hot {
            return Vec::new();
        }
        self.hot = hit;
        let dim = match hit {
            Some(Hit::Swatch(c)) => Some(c),
            _ => None,
        };
        if dim == self.dim {
            return Vec::new();
        }
        self.dim = dim;
        vec![ShellEffect::Dim(dim)]
    }

    /// Handles one event in window coordinates.
    pub fn update(&mut self, event: InputEvent, now: Duration) -> Vec<ShellEffect> {
        let point = match event {
            InputEvent::PointerMoved { x, y }
            | InputEvent::Click { x, y }
            | InputEvent::ClickWith { x, y, .. }
            | InputEvent::DoubleClick { x, y }
            | InputEvent::Wheel { x, y, .. } => Some((x, y)),
            _ => None,
        };
        let Some((x, y)) = point else {
            if matches!(event, InputEvent::PointerLeft) {
                let mut fx = self.set_hot(None);
                fx.extend(self.map_event(event, now));
                return fx;
            }
            return self.map_event(event, now);
        };
        let hit = {
            let g = self.geometry();
            g.in_chrome(x, y).then(|| g.hit(x, y))
        };
        let Some(hit) = hit else {
            // Over the map.
            let mut fx = self.set_hot(None);
            fx.extend(self.map_event(event, now));
            return fx;
        };
        match event {
            InputEvent::PointerMoved { .. } => {
                let mut fx = self.set_hot(hit);
                // The pointer left the map for the chrome.
                fx.extend(self.map_event(InputEvent::PointerLeft, now));
                fx
            }
            InputEvent::Click { .. } | InputEvent::ClickWith { .. } => self.chrome_click(hit, now),
            _ => Vec::new(),
        }
    }

    fn chrome_click(&mut self, hit: Option<Hit>, now: Duration) -> Vec<ShellEffect> {
        let Some(hit) = hit else {
            return Vec::new();
        };
        let key = match hit {
            Hit::Back if self.can_go_back() => Some(Key::AltLeft),
            Hit::Forward if self.can_go_forward() => Some(Key::AltRight),
            Hit::Up if self.can_go_up() => Some(Key::AltUp),
            Hit::Refresh => Some(Key::Refresh),
            Hit::Link => return vec![ShellEffect::OpenLink(chrome::LINK_URL)],
            _ => None,
        };
        match key {
            Some(k) => self.map_event(InputEvent::Key(k), now),
            None => Vec::new(),
        }
    }

    fn can_go_back(&self) -> bool {
        self.app.as_ref().is_some_and(App::can_go_back)
    }

    fn can_go_forward(&self) -> bool {
        self.app.as_ref().is_some_and(App::can_go_forward)
    }

    fn can_go_up(&self) -> bool {
        self.app.as_ref().is_some_and(App::can_go_up)
    }

    /// The text a chrome control shows in the info bar while the pointer is on
    /// it. A disabled control says why.
    fn tooltip(&self, hit: Hit) -> Option<InfoLine> {
        let line = |s: &str| InfoLine {
            name: s.to_string(),
            details: String::new(),
        };
        Some(match hit {
            Hit::Back if self.can_go_back() => line("Back (Alt+Left)"),
            Hit::Back => line("Back — nothing to go back to"),
            Hit::Forward if self.can_go_forward() => line("Forward (Alt+Right)"),
            Hit::Forward => line("Forward — nothing to go forward to"),
            Hit::Up if self.can_go_up() => line("Up one level (Backspace)"),
            Hit::Up => line("Up — already at the top"),
            Hit::Refresh => line("Refresh the layout (F5)"),
            Hit::Link => line("Opens RecursiveAI.org in your browser"),
            Hit::Swatch(c) => line(&format!(
                "{} — other categories dimmed",
                CATEGORY_NAMES.get(c as usize).copied().unwrap_or("Other")
            )),
            Hit::Overflow => {
                let hidden: Vec<&str> = CATEGORY_NAMES
                    .iter()
                    .copied()
                    .skip(self.geom.swatches.len())
                    .collect();
                line(&format!("More categories: {}", hidden.join(", ")))
            }
            Hit::Capacity => line(&self.capacity.breakdown()?.describe()),
            Hit::Crumb | Hit::Info => return None,
        })
    }

    /// The info bar's text now: the control under the pointer, else the box
    /// under the pointer, else the selection, else the scan status.
    pub fn info_line(&self) -> InfoLine {
        if let Some(t) = self.hot.and_then(|h| self.tooltip(h)) {
            return t;
        }
        if let Some(app) = &self.app
            && let Some(line) = app
                .info()
                .and_then(|infos| info::describe(&infos, self.drive_total))
        {
            return line;
        }
        InfoLine {
            name: self.status.clone(),
            details: String::new(),
        }
    }

    /// The chrome to draw at `now`.
    pub fn chrome(&mut self, now: Duration) -> Chrome {
        let info = self.info_line();
        let breadcrumb = match &self.app {
            Some(app) => {
                let c = app.source().breadcrumb(app.current_view());
                if c.is_empty() { self.crumb.clone() } else { c }
            }
            None => self.crumb.clone(),
        };
        let (back, forward, up) = (self.can_go_back(), self.can_go_forward(), self.can_go_up());
        let hot = self.hot;
        let parts = self.capacity.parts_at(now).segments();
        let has_capacity = self.capacity.breakdown().is_some();
        let g = self.geometry().clone();
        let em = self.font_px * self.scale;
        let name_gap = (em * 0.6).round();
        let measure = |s: &str| {
            self.measure
                .width(s)
                .unwrap_or(s.chars().count() as f32 * em * 0.56)
        };

        let mut widgets = Vec::new();
        for (rect, glyph, enabled, h) in [
            (g.back, Glyph::Back, back, Hit::Back),
            (g.forward, Glyph::Forward, forward, Hit::Forward),
            (g.up, Glyph::Up, up, Hit::Up),
            (g.refresh, Glyph::Refresh, true, Hit::Refresh),
        ] {
            widgets.push(Widget::Button {
                rect,
                glyph,
                enabled,
                hot: hot == Some(h),
            });
        }
        widgets.push(Widget::Text {
            rect: g.crumb,
            text: breadcrumb.into(),
            role: TextRole::Primary,
            hot: false,
        });
        if has_capacity {
            widgets.push(Widget::Capacity {
                rect: g.capacity,
                parts,
            });
        }
        // The info bar: the name shortens, the facts stay whole while they
        // fit.
        if info.details.is_empty() {
            widgets.push(Widget::Text {
                rect: g.info,
                text: info.name.into(),
                role: TextRole::Primary,
                hot: false,
            });
        } else {
            let details_w = measure(&info.details).ceil() + 2.0;
            let details_w = details_w.min((g.info.w - 8.0 * em).max(0.0));
            let name_w = (g.info.w - details_w - name_gap).max(0.0);
            widgets.push(Widget::Text {
                rect: Rect::new(g.info.x, g.info.y, name_w, g.info.h),
                text: info.name.into(),
                role: TextRole::Primary,
                hot: false,
            });
            widgets.push(Widget::Text {
                rect: Rect::new(g.info.x + name_w + name_gap, g.info.y, details_w, g.info.h),
                text: info.details.into(),
                role: TextRole::Secondary,
                hot: false,
            });
        }
        for s in &g.swatches {
            let name = s
                .named
                .then(|| chrome::LEGEND_NAMES[s.category as usize % CATEGORY_COUNT].into());
            widgets.push(Widget::Swatch {
                rect: s.rect,
                category: s.category,
                name,
                hot: hot == Some(Hit::Swatch(s.category)),
            });
        }
        if let Some(rect) = g.overflow {
            widgets.push(Widget::Overflow {
                rect,
                hot: hot == Some(Hit::Overflow),
            });
        }
        widgets.push(Widget::Text {
            rect: g.link,
            text: g.link_text.into(),
            role: TextRole::Link,
            hot: hot == Some(Hit::Link),
        });
        Chrome {
            bars: vec![
                Bar {
                    rect: g.nav,
                    left: None,
                    right: None,
                    border_on_top: false,
                },
                Bar {
                    rect: g.status,
                    left: None,
                    right: None,
                    border_on_top: true,
                },
            ],
            widgets,
        }
    }

    /// How the legend currently fits, for tests and pictures.
    pub fn legend_mode(&mut self) -> LegendMode {
        self.geometry().legend
    }
}
