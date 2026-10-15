//! The window's state around the map: chrome, info bar, legend hover, the
//! capacity bar, the scan status and the look. It wraps the map's [`App`] and
//! turns events in window coordinates into effects for the host, with no
//! windowing code in it.

use std::sync::Arc;
use std::time::Duration;

use bs_render::scene::{CATEGORY_COUNT, CATEGORY_NAMES};
use bs_render::{Bar, Card, Chrome, Color, Glyph, Palette, Rect, TextEngine, TextRole, Widget};

use bs_render::Item;

#[path = "access.rs"]
pub mod access;
#[path = "actions.rs"]
pub mod actions;
#[path = "banner.rs"]
mod banner;

pub use actions::{
    ActOutcome, BinDrive, BinStyle, Capabilities, Disposer, Job, JobResult, Token, TreeChange,
};

use crate::action::{Action, reason};
use crate::blocks::Special;
use crate::capacity::{Breakdown, CapacityModel};
use crate::card::{self, CardData, CardHit, CardState};
use crate::chrome::{self, CrumbPart, Geometry, Hit, LegendMode, TextMeasure};
use crate::elevation::{self, Offer, UnreadableAs};
use crate::info::{self, InfoLine};
use crate::notice::{AboutInfo, CRASH_WORDS, CrashNotice};
use crate::picker::{PickTarget, Picker};
use crate::{App, Direction, Effect, InputEvent, Key, ViewId, ViewSource};

/// What the host should do after an event.
#[derive(Clone, Debug)]
pub enum ShellEffect {
    /// Apply to the renderer.
    Map(Effect),
    /// The legend is hovered: dim every category but this one (or stop).
    Dim(Option<u8>),
    /// Open this address in the user's browser.
    OpenLink(&'static str),
    /// Put this text on the clipboard.
    CopyText(String),
    /// The user asked for an action on boxes of a view. Actions the
    /// interface does not carry out itself are handled by whoever owns them.
    Action {
        action: Action,
        view: ViewId,
        items: Vec<u32>,
    },
    /// Stop the running scan.
    CancelScan,
    /// Scan the same place again from the start (the banner's button).
    Rescan,
    /// The user picked this drive: the index into the list the picker was
    /// made from.
    PickDrive(usize),
    /// "Scan a folder…": ask the system for a folder.
    ScanFolder,
    /// Go back from a drive's map to the drive picker.
    ShowPicker,
    /// The Unreadable block or the Full scan button was pressed: read the
    /// drive again with the administrator helper. The host runs it beside the
    /// ordinary scan (see [`crate::elevation::Race`]).
    FullScan,
    /// Work for the disk. Run it off the UI thread and hand the result to
    /// [`Shell::job_done`].
    Run(Job),
    /// Change delete marks in the size tree and lay the view out again with
    /// the new marks: the boxes stay where they are.
    Tree(Vec<TreeChange>),
    /// What the bin holds changed: measure it again.
    BinChanged,
    /// Open this address in the user's browser. Like [`ShellEffect::OpenLink`],
    /// for an address made at run time (the crash report's issue link).
    OpenUrl(String),
    /// The crash report was dealt with: record that it was seen, so it is not
    /// offered again.
    CrashSeen,
    /// The rectangle the map fills changed (a banner appeared or went), so
    /// whatever sits in it other than the map, such as the drive picker, must
    /// be laid out again.
    MapMoved,
}

/// What a running scan reads out in the progress overlay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanReadout {
    /// Share of the used bytes counted, when that figure means something.
    pub ratio: Option<f32>,
    pub files: u64,
    pub files_per_sec: f64,
    pub elapsed: Duration,
}

impl ScanReadout {
    /// "37% · 120k files/s · 0:12", or the file count in place of the
    /// percentage when the ratio means nothing.
    pub fn text(&self) -> String {
        let lead = match self.ratio {
            Some(r) => format!("{:.0}%", (r * 100.0).clamp(0.0, 99.0)),
            None => format!("{} files", bs_layout::format_count(self.files)),
        };
        let secs = self.elapsed.as_secs();
        format!(
            "{lead} · {}/s · {}:{:02}",
            rate_text(self.files_per_sec),
            secs / 60,
            secs % 60
        )
    }
}

fn rate_text(per_sec: f64) -> String {
    let n = per_sec.max(0.0);
    if n >= 1_000_000.0 {
        format!("{:.1}M files", n / 1_000_000.0)
    } else if n >= 1000.0 {
        format!("{:.0}k files", n / 1000.0)
    } else {
        format!("{n:.0} files")
    }
}

/// Words of the banner shown after a scan was stopped.
pub const STOPPED_BANNER: &str = "Scan stopped — results incomplete";

/// The buttons of the crash banner, in order.
pub const CRASH_BUTTONS: [&str; 3] = ["View report", "Report on GitHub", "Dismiss"];

/// The crash banner and what the pointer or keyboard is on.
struct CrashUi {
    notice: CrashNotice,
    hot: Option<usize>,
}

/// Where the parts of the crash banner are, in window coordinates.
struct CrashStrip {
    panel: Rect,
    text: Rect,
    buttons: [Rect; 3],
}

/// Which part of the overlay or banner the pointer is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayHit {
    Panel,
    Button,
}

/// The progress overlay or the stopped banner, in window coordinates.
struct Overlay {
    panel: Rect,
    text: Rect,
    button: Rect,
    words: String,
    button_text: &'static str,
    stopped: bool,
}

/// How long the pointer rests on a box before its card shows.
pub const CARD_REST: Duration = Duration::from_millis(450);
/// How long the card says "Copied".
/// What the map says in a folder that holds nothing.
pub const EMPTY_FOLDER: &str = "This folder is empty";

const COPIED_FOR: Duration = Duration::from_millis(1500);
/// How long a message stays in the info bar.
const NOTICE_FOR: Duration = Duration::from_secs(3);
/// A scan that has run this long is stopped only after a second Esc.
pub const CANCEL_CONFIRM_AFTER: Duration = Duration::from_secs(5);
/// How long the second Esc is waited for.
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// The box the pointer rests on, and since when.
#[derive(Clone, Copy, Debug)]
struct Rest {
    view: ViewId,
    id: u32,
    since: Duration,
    at: (f32, f32),
}

/// The card on screen.
struct Shown {
    view: ViewId,
    id: u32,
    generation: u64,
    data: CardData,
    pointer: (f32, f32),
    state: CardState,
    built: card::Built,
}

#[derive(Default)]
struct CardUi {
    rest: Option<Rest>,
    shown: Option<Shown>,
    copied_until: Option<Duration>,
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
    /// The geometry has room for the Full scan button.
    geom_full_scan: bool,
    /// Whether and how administrator rights are offered.
    offer: Offer,
    crumb: String,
    status: String,
    scanning: bool,
    capacity: CapacityModel,
    hot: Option<Hit>,
    dim: Option<u8>,
    look: Look,
    reduced_motion: bool,
    drive_total: Option<u64>,
    card: CardUi,
    /// The time of the latest event, for texts that expire.
    clock: Duration,
    notice: Option<(String, Duration)>,
    esc_armed: Option<Duration>,
    scan_since: Option<Duration>,
    picker: Option<Picker>,
    picker_hot: Option<PickTarget>,
    picker_sel: Option<usize>,
    picker_return: bool,
    readout: Option<ScanReadout>,
    stopped: bool,
    overlay_hot: Option<OverlayHit>,
    acts: actions::ActionsUi,
    /// Effects produced while time passed, for the host to collect.
    pending_fx: Vec<ShellEffect>,
    /// The note about a crash last time, until it is dismissed.
    crash: Option<CrashUi>,
    /// The banner button the keyboard is on.
    crash_focus: Option<usize>,
    about: AboutInfo,
    unix_clock: Box<dyn Fn() -> i64>,
}

/// Seconds since 1970 from the system clock; 0 if the clock is before it.
fn system_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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
            geom_full_scan: false,
            offer: Offer::Available,
            crumb: String::new(),
            status: String::new(),
            scanning: false,
            capacity: CapacityModel::default(),
            hot: None,
            dim: None,
            look,
            reduced_motion: false,
            drive_total: None,
            card: CardUi::default(),
            clock: Duration::ZERO,
            notice: None,
            esc_armed: None,
            scan_since: None,
            picker: None,
            picker_hot: None,
            picker_sel: None,
            picker_return: false,
            readout: None,
            stopped: false,
            overlay_hot: None,
            acts: actions::ActionsUi::default(),
            pending_fx: Vec::new(),
            crash: None,
            crash_focus: None,
            about: AboutInfo::new(env!("CARGO_PKG_VERSION"), "unknown", ""),
            unix_clock: Box::new(system_unix_seconds),
        }
    }

    /// Effects that came from time passing (a hold that completed), to be
    /// carried out like the ones [`Shell::update`] returns.
    pub fn take_effects(&mut self) -> Vec<ShellEffect> {
        std::mem::take(&mut self.pending_fx)
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
        let full_scan = self.offer.shows_button();
        if key != self.geom_key || full_scan != self.geom_full_scan {
            self.geom_key = key;
            self.geom_full_scan = full_scan;
            self.geom = Self::compute(
                (self.width, self.height),
                self.scale,
                self.font_px,
                self.measure.as_ref(),
            );
            if full_scan {
                let em = self.font_px * self.scale;
                let w = self
                    .measure
                    .width(elevation::FULL_SCAN_LABEL)
                    .unwrap_or(elevation::FULL_SCAN_LABEL.chars().count() as f32 * em * 0.56);
                self.geom.reserve_full_scan(w, self.scale, em);
            }
        }
        &self.geom
    }

    /// The rectangle the map fills. While the banner of a stopped scan
    /// shows, it takes a strip off the top, so nothing is covered.
    pub fn map_rect(&self) -> Rect {
        let full = chrome::map_rect(self.width as f32, self.height as f32, self.scale);
        let strip = (self.banner_height() + self.crash_height()).min(full.h);
        Rect::new(full.x, full.y + strip, full.w, full.h - strip)
    }

    fn banner_height(&self) -> f32 {
        if self.stopped && !self.scanning && self.app.is_some() {
            (self.font_px * self.scale * 2.4).round()
        } else {
            0.0
        }
    }

    pub fn app(&self) -> Option<&App<S>> {
        self.app.as_ref()
    }

    pub fn app_mut(&mut self) -> Option<&mut App<S>> {
        self.app.as_mut()
    }

    /// Installs the map once the first data is there.
    pub fn set_app(&mut self, app: App<S>) {
        self.new_scene();
        self.card = CardUi::default();
        self.picker = None;
        self.picker_hot = None;
        self.picker_sel = None;
        self.app = Some(app);
    }

    /// Removes the map (a new scan is about to replace it).
    pub fn clear_app(&mut self) {
        self.new_scene();
        self.card = CardUi::default();
        self.app = None;
        self.readout = None;
        self.stopped = false;
    }

    /// Takes the capacity bar away (no drive is open).
    pub fn clear_capacity(&mut self) {
        self.capacity = CapacityModel::default();
        self.drive_total = None;
    }

    /// Shows the drive picker in place of a map.
    pub fn set_picker(&mut self, picker: Picker) {
        self.new_scene();
        self.card = CardUi::default();
        self.app = None;
        self.picker = Some(picker);
        self.picker_hot = None;
        self.picker_sel = None;
        self.readout = None;
        self.stopped = false;
    }

    /// The picker, while it is showing.
    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    pub fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }

    /// The boxes of the picker's cards, for the renderer. Make them again
    /// after a resize.
    pub fn picker_items(&self) -> Option<Arc<Vec<Item>>> {
        let layout = self.picker_layout()?;
        Some(Arc::new(layout.items))
    }

    fn picker_layout(&self) -> Option<crate::picker::PickerLayout> {
        let picker = self.picker.as_ref()?;
        Some(picker.layout(
            self.map_rect(),
            self.font_px * self.scale,
            self.scale,
            self.measure.as_ref(),
        ))
    }

    /// Whether a drive's map can go back to the picker (the user came from
    /// it).
    pub fn set_picker_return(&mut self, on: bool) {
        self.picker_return = on;
    }

    /// Whether the open map was reached from the picker.
    pub fn picker_return(&self) -> bool {
        self.picker_return
    }

    /// The progress overlay's figures, or `None` to take it away.
    pub fn set_readout(&mut self, readout: Option<ScanReadout>) {
        self.readout = readout;
    }

    /// Whether the scan was stopped before it finished, so the banner shows.
    pub fn set_stopped(&mut self, stopped: bool) -> bool {
        let before = self.map_rect();
        self.stopped = stopped;
        self.map_rect() != before
    }

    fn overlay(&self) -> Option<Overlay> {
        let (words, button_text, stopped) = if self.banner_height() > 0.0 {
            (STOPPED_BANNER.to_string(), "Rescan", true)
        } else if self.scanning && self.app.is_some() {
            (self.readout?.text(), "Cancel", false)
        } else {
            return None;
        };
        let em = self.font_px * self.scale;
        let gap = (10.0 * self.scale).round();
        let pad = (em * 0.7).round();
        let m = |t: &str| {
            self.measure
                .width(t)
                .unwrap_or(t.chars().count() as f32 * em * 0.56)
                .ceil()
        };
        let full = chrome::map_rect(self.width as f32, self.height as f32, self.scale);
        let btn_w = (m(button_text) + em * 1.6).round();
        let bh = (em * 1.7).round();
        if stopped {
            // A strip across the top of the map, with the button after the
            // words.
            let h = self.banner_height();
            let panel = Rect::new(full.x, full.y + self.crash_height(), full.w, h);
            let text_w = m(&words);
            let text = Rect::new(panel.x + pad, panel.y, text_w + 2.0, h);
            let button = Rect::new(
                text.right() + pad,
                panel.y + ((h - bh) / 2.0).round(),
                btn_w,
                bh,
            );
            return Some(Overlay {
                panel,
                text,
                button,
                words,
                button_text,
                stopped,
            });
        }
        // The progress readout sits at the right end of the navigation bar,
        // which has nothing there, so it covers no part of the map.
        let nav_h = (chrome::NAV_HEIGHT * self.scale).round();
        let h = (em * 2.0).round().min(nav_h);
        let text_w = m(&words);
        // The Full scan button keeps the far end; the readout sits before it.
        let right = self
            .geom
            .full_scan
            .map_or(self.width as f32 - gap, |r| r.x - gap);
        let w = (pad + text_w + pad + btn_w + (em * 0.3).round())
            .min((right - gap).max(0.0))
            .round();
        let panel = Rect::new(right - w, ((nav_h - h) / 2.0).round(), w, h);
        let bh = bh.min(h - 2.0);
        let button = Rect::new(
            panel.right() - btn_w - (em * 0.3).round(),
            panel.y + ((h - bh) / 2.0).round(),
            btn_w,
            bh,
        );
        let text = Rect::new(
            panel.x + pad,
            panel.y,
            (button.x - panel.x - 2.0 * pad).max(0.0),
            h,
        );
        Some(Overlay {
            panel,
            text,
            button,
            words,
            button_text,
            stopped,
        })
    }

    fn overlay_at(&self, x: f32, y: f32) -> Option<OverlayHit> {
        let o = self.overlay()?;
        if o.button.contains(x, y) {
            Some(OverlayHit::Button)
        } else if o.panel.contains(x, y) {
            Some(OverlayHit::Panel)
        } else {
            None
        }
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
        if !scanning {
            self.scan_since = None;
            self.esc_armed = None;
        }
    }

    /// Whether something is being scanned, without touching the status text:
    /// a full scan that runs after the ordinary one has finished.
    pub fn set_scanning(&mut self, scanning: bool, now: Duration) {
        if scanning == self.scanning {
            return;
        }
        self.scanning = scanning;
        if scanning {
            self.scan_since = Some(now);
        } else {
            self.scan_since = None;
            self.esc_armed = None;
        }
    }

    /// A scan started at `now`. Esc stops a scan that is younger than a few
    /// seconds at once and asks for a second Esc after that.
    pub fn begin_scan(&mut self, now: Duration) {
        self.scan_since = Some(now);
    }

    /// Whether the pointer is on something that can be clicked, so the window
    /// shows a pointing hand.
    pub fn wants_pointer_cursor(&self) -> bool {
        if self
            .card
            .shown
            .as_ref()
            .is_some_and(|s| s.state.hot.is_some())
        {
            return true;
        }
        if self.picker_hot.is_some() && self.app.is_none() {
            return true;
        }
        match self.hot {
            Some(Hit::Link | Hit::Swatch(_) | Hit::Refresh | Hit::Overflow | Hit::CrumbSeg(_)) => {
                true
            }
            Some(Hit::FullScan) => self.offer.can_start(),
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
        self.card = CardUi::default();
        self.close_menu_if_open();
        self.rebuild_modal(now);
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

    // ---- the picker ------------------------------------------------------

    /// Hover and clicks while the picker shows.
    fn picker_event(&mut self, event: InputEvent) -> Vec<ShellEffect> {
        let Some(layout) = self.picker_layout() else {
            return Vec::new();
        };
        match event {
            InputEvent::PointerMoved { x, y } => {
                let target = layout.target_at(x, y);
                if target == self.picker_hot {
                    return Vec::new();
                }
                self.picker_hot = target;
                let id = target.and_then(crate::picker::PickerLayout::item_id);
                vec![ShellEffect::Map(Effect::SetHover(id))]
            }
            InputEvent::PointerLeft => {
                self.picker_hot = None;
                vec![ShellEffect::Map(Effect::SetHover(None))]
            }
            InputEvent::Click { x, y } | InputEvent::DoubleClick { x, y } => {
                match layout.target_at(x, y) {
                    Some(t) => self.picker_choose(t),
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn picker_choose(&mut self, target: PickTarget) -> Vec<ShellEffect> {
        match target {
            PickTarget::Folder => vec![ShellEffect::ScanFolder],
            PickTarget::Card(i) => match self.picker.as_ref().and_then(|p| p.cards().get(i)) {
                Some(card) => vec![ShellEffect::PickDrive(card.drive_index)],
                None => Vec::new(),
            },
        }
    }

    /// Arrows move between cards and Enter picks, while the picker shows.
    fn picker_key(&mut self, key: Key) -> Vec<ShellEffect> {
        let Some(picker) = self.picker.as_ref() else {
            return Vec::new();
        };
        let n = picker.cards().len();
        let current = match self.picker_hot {
            Some(PickTarget::Card(i)) => Some(i),
            _ => self.picker_sel,
        };
        match key {
            Key::Enter => match current {
                Some(i) => self.picker_choose(PickTarget::Card(i)),
                None => Vec::new(),
            },
            Key::Arrow(d) if n > 0 => {
                let step = matches!(d, Direction::Right | Direction::Down);
                let next = match current {
                    None => 0,
                    Some(i) if step => (i + 1) % n,
                    Some(i) => (i + n - 1) % n,
                };
                self.picker_sel = Some(next);
                self.picker_hot = Some(PickTarget::Card(next));
                vec![ShellEffect::Map(Effect::SetHover(Some(next as u32 + 1)))]
            }
            _ => Vec::new(),
        }
    }

    fn map_event(&mut self, event: InputEvent, now: Duration) -> Vec<ShellEffect> {
        let fx: Vec<ShellEffect> = match self.app.as_mut() {
            Some(app) => app
                .update(event, now)
                .into_iter()
                .map(ShellEffect::Map)
                .collect(),
            None => Vec::new(),
        };
        // A double click or Enter on a file asks for its card at once.
        let request = self.app.as_mut().and_then(App::take_card_request);
        if let Some(id) = request {
            let pointer = self.pointer_for(id);
            self.show_card(id, pointer);
        }
        self.sync_card();
        self.note_selection();
        fx
    }

    /// Where to anchor a card asked for without a pointer: the middle of the
    /// box.
    fn pointer_for(&self, id: u32) -> (f32, f32) {
        self.app
            .as_ref()
            .and_then(|a| a.item(id))
            .map_or((0.0, 0.0), |i| {
                (i.rect.x + i.rect.w / 2.0, i.rect.y + i.rect.h / 2.0)
            })
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

    // ---- the hover card -------------------------------------------------

    fn card_metrics(&self) -> (f32, f32) {
        (self.font_px * self.scale, self.scale)
    }

    /// Builds and shows the card of box `id` of the current view.
    fn show_card(&mut self, id: u32, pointer: (f32, f32)) {
        let Some(app) = self.app.as_ref() else {
            return;
        };
        let view = app.current_view();
        let generation = app.generation();
        let Some(data) = self.card_data(view, id, true) else {
            return;
        };
        self.card.shown = Some(Shown {
            view,
            id,
            generation,
            data,
            pointer,
            state: CardState::default(),
            built: card::Built {
                card: Card {
                    rect: Rect::default(),
                    parts: Vec::new(),
                },
                hits: Vec::new(),
                anchor: Rect::default(),
            },
        });
        self.rebuild_card();
    }

    /// Builds the card again from what it was made from: after new data, a
    /// hovered button or the "Copied" confirmation.
    fn rebuild_card(&mut self) {
        let (em, scale) = self.card_metrics();
        // The card's soft edge reaches a little past its panel, so the panel
        // keeps that far from the capacity bar above and the status bar below.
        let map = {
            let map = self.map_rect();
            let edge = (bs_render::card::CARD_EDGE * scale).ceil();
            Rect::new(
                map.x + edge,
                map.y + edge,
                (map.w - 2.0 * edge).max(0.0),
                (map.h - 2.0 * edge).max(0.0),
            )
        };
        let drive_total = self.drive_total;
        let anchor = match (self.card.shown.as_ref(), self.app.as_ref()) {
            (Some(s), Some(app)) => app.item(s.id).map(|i| i.rect),
            _ => None,
        };
        let copied = self.card.copied_until.is_some();
        let (Some(shown), Some(anchor)) = (self.card.shown.as_mut(), anchor) else {
            self.card.shown = None;
            return;
        };
        shown.state.copied = copied;
        shown.built = card::build(
            &shown.data,
            drive_total,
            anchor,
            shown.pointer,
            map,
            card::Metrics {
                em,
                scale,
                measure: self.measure.as_ref(),
            },
            shown.state,
        );
    }

    fn hide_card(&mut self) {
        self.card.shown = None;
    }

    /// Keeps the card in step with the map: gone when its box is, built from
    /// the new numbers when the layout was replaced.
    fn sync_card(&mut self) {
        let Some(app) = self.app.as_ref() else {
            self.card = CardUi::default();
            return;
        };
        let (view, generation) = (app.current_view(), app.generation());
        let Some(shown) = self.card.shown.as_mut() else {
            return;
        };
        if shown.view != view {
            self.card.shown = None;
            return;
        }
        if shown.generation == generation {
            return;
        }
        shown.generation = generation;
        let (id, view) = (shown.id, shown.view);
        let exists = app.item(id).is_some();
        match self.card_data(view, id, true) {
            Some(data) if exists => {
                if let Some(s) = self.card.shown.as_mut() {
                    s.data = data;
                }
                self.rebuild_card();
            }
            _ => self.card.shown = None,
        }
        if let Some(r) = self.card.rest.as_mut() {
            r.view = view;
        }
    }

    /// The card to draw, if one is up.
    pub fn card(&self) -> Option<&Card> {
        self.card.shown.as_ref().map(|s| &s.built.card)
    }

    /// Whether the pointer is over the card or the gap beside it, where the
    /// card stays up and the map below does not see the pointer.
    fn over_card(&self, x: f32, y: f32) -> bool {
        self.card.shown.as_ref().is_some_and(|s| {
            let gap = (8.0 * self.scale).round();
            s.built.card.rect.inflate(gap).contains(x, y)
        })
    }

    fn card_pointer(&mut self, x: f32, y: f32) {
        let Some(shown) = self.card.shown.as_mut() else {
            return;
        };
        let hot = shown.built.hit(x, y);
        if hot != shown.state.hot {
            shown.state.hot = hot;
            self.rebuild_card();
        }
    }

    fn card_click(&mut self, x: f32, y: f32, now: Duration) -> Vec<ShellEffect> {
        let Some(shown) = self.card.shown.as_ref() else {
            return Vec::new();
        };
        let (view, id, path) = (shown.view, shown.id, shown.data.path.clone());
        match shown.built.hit(x, y) {
            Some(CardHit::Path) => {
                self.card.copied_until = Some(now + COPIED_FOR);
                self.rebuild_card();
                vec![ShellEffect::CopyText(path)]
            }
            Some(CardHit::Button(action)) => {
                self.hide_card();
                self.card.rest = None;
                self.run_action(action, view, vec![id], now)
            }
            None => Vec::new(),
        }
    }

    /// Notes where the pointer rests on the map, so the card can show once it
    /// has stayed put long enough.
    fn track_rest(&mut self, x: f32, y: f32, now: Duration) {
        let Some(app) = self.app.as_ref() else {
            return;
        };
        let (view, hover) = (app.current_view(), app.hover());
        if let Some(s) = &self.card.shown
            && (hover != Some(s.id) || s.view != view)
        {
            self.card.shown = None;
        }
        let Some(id) = hover else {
            self.card.rest = None;
            return;
        };
        if self.card.shown.is_some() {
            return;
        }
        let jitter = 3.0 * self.scale;
        match self.card.rest.as_mut() {
            Some(r) if r.id == id && r.view == view => {
                // Moving restarts the rest; staying within a few pixels
                // does not.
                if (r.at.0 - x).abs() > jitter || (r.at.1 - y).abs() > jitter {
                    r.since = now;
                    r.at = (x, y);
                }
            }
            _ => {
                self.card.rest = Some(Rest {
                    view,
                    id,
                    since: now,
                    at: (x, y),
                });
            }
        }
    }

    /// Advances texts that expire and shows the card once the pointer has
    /// rested. Returns whether anything visible changed.
    pub fn tick(&mut self, now: Duration) -> bool {
        self.clock = now;
        let mut changed = false;
        if self.card.copied_until.is_some_and(|t| now >= t) {
            self.card.copied_until = None;
            if self.card.shown.is_some() {
                self.rebuild_card();
            }
            changed = true;
        }
        if self.notice.as_ref().is_some_and(|(_, t)| now >= *t) {
            self.notice = None;
            changed = true;
        }
        if self.esc_armed.is_some_and(|t| now >= t) {
            self.esc_armed = None;
        }
        changed |= self.actions_tick(now);
        if self.card.shown.is_none()
            && let Some(r) = self.card.rest
            && now.saturating_sub(r.since) >= CARD_REST
        {
            self.card.rest = None;
            self.show_card(r.id, r.at);
            changed |= self.card.shown.is_some();
        }
        changed
    }

    /// When [`Shell::tick`] next has something to do, if ever.
    pub fn next_deadline(&self, now: Duration) -> Option<Duration> {
        let mut soonest: Option<Duration> = None;
        let mut at = |t: Duration| {
            let d = t.saturating_sub(now);
            soonest = Some(soonest.map_or(d, |s| s.min(d)));
        };
        if self.card.shown.is_none()
            && let Some(r) = self.card.rest
        {
            at(r.since + CARD_REST);
        }
        if let Some(t) = self.card.copied_until {
            at(t);
        }
        if let Some((_, t)) = &self.notice {
            at(*t);
        }
        if let Some(d) = self.actions_deadline(now) {
            at(now + d);
        }
        soonest
    }

    // ---- keys and actions ----------------------------------------------

    /// Shows a short message in the info bar for a few seconds.
    pub fn notify(&mut self, text: impl Into<String>, now: Duration) {
        self.say(text, now);
    }

    fn say(&mut self, text: impl Into<String>, now: Duration) {
        self.notice = Some((text.into(), now + NOTICE_FOR));
    }

    /// The selected boxes' view and ids.
    fn selection(&self) -> (ViewId, Vec<u32>) {
        match self.app.as_ref() {
            Some(app) => (app.current_view(), app.selection().to_vec()),
            None => (0, Vec::new()),
        }
    }

    /// Why `action` cannot run on the selection, if it cannot.
    fn refusal(&self, action: Action, view: ViewId, items: &[u32]) -> Option<&'static str> {
        let app = self.app.as_ref()?;
        if items.is_empty() && !matches!(action, Action::Undo | Action::ContextMenu) {
            return Some(reason::NOTHING_SELECTED);
        }
        if items
            .iter()
            .any(|&id| app.source().special(view, id).is_some())
        {
            return Some(reason::NOT_ON_DISK);
        }
        items.iter().find_map(|&id| {
            let data = self.card_data(view, id, false)?;
            card::why_not(&data, action).or(match action {
                Action::ZoomInto | Action::Rescan if !data.is_folder => Some(reason::NOT_A_FOLDER),
                // Reading a folder again needs the finished tree to itself.
                Action::Rescan if self.scanning || !data.complete => Some(reason::SCANNING),
                _ => None,
            })
        })
    }

    fn run_action(
        &mut self,
        action: Action,
        view: ViewId,
        items: Vec<u32>,
        now: Duration,
    ) -> Vec<ShellEffect> {
        // Undo is about the last delete, not about what is selected now.
        if action == Action::Undo {
            return self.undo_last(now);
        }
        // The About box and the releases page are not about any box.
        if let Some(fx) = self.app_action(action) {
            return fx;
        }
        if let Some(why) = self.refusal(action, view, &items) {
            self.say(format!("{}: {why}", action.label()), now);
            return Vec::new();
        }
        match action {
            Action::CopyPath => {
                let paths: Vec<String> = items
                    .iter()
                    .filter_map(|&id| self.app.as_ref()?.source().path(view, id))
                    .collect();
                if paths.is_empty() {
                    return Vec::new();
                }
                self.say(
                    if paths.len() == 1 {
                        "Copied the path".to_string()
                    } else {
                        format!("Copied {} paths", paths.len())
                    },
                    now,
                );
                return vec![ShellEffect::CopyText(paths.join("\n"))];
            }
            Action::ZoomInto => {
                if let Some(app) = self.app.as_mut()
                    && let Some(&id) = items.last()
                {
                    return app.open(id).into_iter().map(ShellEffect::Map).collect();
                }
                return Vec::new();
            }
            Action::CopyRemovalCommand => {
                let command =
                    items
                        .first()
                        .and_then(|&id| match self.card_data(view, id, false)?.owner {
                            crate::owners::Ownership::Package(p) => p.removal_command,
                            _ => None,
                        });
                return match command {
                    Some(text) => {
                        self.say("Copied the removal command", now);
                        vec![ShellEffect::CopyText(text)]
                    }
                    None => Vec::new(),
                };
            }
            Action::Delete => return self.request_delete(false, view, items, now),
            Action::DeletePermanently => return self.request_delete(true, view, items, now),
            Action::Undo => return self.undo_last(now),
            Action::ContextMenu => return self.open_menu_for_selection(now),
            _ => {}
        }
        vec![ShellEffect::Action {
            action,
            view,
            items,
        }]
    }

    fn key(&mut self, key: Key, now: Duration) -> Vec<ShellEffect> {
        if self.modal_open() {
            return self.modal_input(InputEvent::Key(key), now);
        }
        if let Some(fx) = self.crash_key(key, now) {
            return fx;
        }
        if self.picker.is_some() && self.app.is_none() {
            return match key {
                Key::Escape => Vec::new(),
                Key::About => self.open_about(),
                other => self.picker_key(other),
            };
        }
        // The toast's Undo button is the first stop of Tab; Enter or Space
        // presses it, anything else lets go of it.
        if matches!(key, Key::Tab | Key::BackTab) && self.toast_has_button() {
            self.acts.toast_focus = !self.acts.toast_focus;
            return Vec::new();
        }
        // The keyboard can only be on a button that is on screen.
        if self.acts.toast_focus && !self.toast_has_button() {
            self.acts.toast_focus = false;
        }
        if self.acts.toast_focus {
            self.acts.toast_focus = false;
            match key {
                Key::Enter | Key::Space => return self.undo_last(now),
                Key::Escape => return Vec::new(),
                _ => {}
            }
        }
        if self.picker_return
            && matches!(key, Key::Backspace | Key::AltUp)
            && !self.can_go_up()
            && self.app.is_some()
        {
            return vec![ShellEffect::ShowPicker];
        }
        let action = match key {
            Key::Escape => return self.escape(now),
            Key::CopyPath => Action::CopyPath,
            Key::Delete => Action::Delete,
            Key::DeletePermanently => Action::DeletePermanently,
            Key::Undo => Action::Undo,
            Key::Rescan => Action::Rescan,
            Key::ContextMenu => Action::ContextMenu,
            Key::About => Action::About,
            other => {
                self.hide_card();
                self.card.rest = None;
                return self.map_event(InputEvent::Key(other), now);
            }
        };
        let (view, items) = self.selection();
        self.run_action(action, view, items, now)
    }

    /// Esc peels one thing at a time: the card, then the selection, then the
    /// scan. A scan that has run for more than a few seconds needs a second
    /// Esc, so a stray key press cannot throw away minutes of work.
    fn escape(&mut self, now: Duration) -> Vec<ShellEffect> {
        if self.card.shown.take().is_some() {
            self.card.rest = None;
            return Vec::new();
        }
        if self.app.as_ref().is_some_and(|a| !a.selection().is_empty()) {
            return self.map_event(InputEvent::Key(Key::Escape), now);
        }
        if !self.scanning {
            return Vec::new();
        }
        let ran = now.saturating_sub(self.scan_since.unwrap_or(now));
        if ran <= CANCEL_CONFIRM_AFTER || self.esc_armed.is_some_and(|t| now < t) {
            self.esc_armed = None;
            return vec![ShellEffect::CancelScan];
        }
        self.esc_armed = Some(now + CONFIRM_WINDOW);
        self.say("Press Esc again to stop the scan", now);
        Vec::new()
    }

    /// Handles one event in window coordinates.
    pub fn update(&mut self, event: InputEvent, now: Duration) -> Vec<ShellEffect> {
        self.clock = now;
        if matches!(event, InputEvent::FocusLost) {
            self.focus_lost();
            return Vec::new();
        }
        if let InputEvent::Key(key) = event {
            return self.key(key, now);
        }
        if self.modal_open() {
            return self.modal_input(event, now);
        }
        if let Some(fx) = self.toast_input(event, now) {
            return fx;
        }
        match event {
            InputEvent::RightClick { x, y } => return self.right_click(x, y, now),
            // Only dialogs use the button going down and coming up.
            InputEvent::PointerDown { .. }
            | InputEvent::PointerUp { .. }
            | InputEvent::KeyReleased(_) => return Vec::new(),
            _ => {}
        }
        let point = match event {
            InputEvent::PointerMoved { x, y }
            | InputEvent::Click { x, y }
            | InputEvent::ClickWith { x, y, .. }
            | InputEvent::DoubleClick { x, y }
            | InputEvent::Wheel { x, y, .. }
            | InputEvent::Touchpad { x, y, .. }
            | InputEvent::Pinch { x, y, .. } => Some((x, y)),
            _ => None,
        };
        let Some((x, y)) = point else {
            if matches!(event, InputEvent::PointerLeft) {
                self.hide_card();
                self.card.rest = None;
                let mut fx = self.set_hot(None);
                fx.extend(self.picker_event(event));
                fx.extend(self.map_event(event, now));
                return fx;
            }
            return self.map_event(event, now);
        };
        if let Some(hit) = self.crash_at(x, y) {
            return self.crash_event(event, hit, now);
        }
        if let Some(c) = self.crash.as_mut() {
            c.hot = None;
        }
        if let Some(hit) = self.overlay_at(x, y) {
            return self.overlay_event(event, hit);
        }
        // The pointer is not on the overlay (any more).
        self.overlay_hot = None;
        // The card and the gap beside it belong to the card: the map below
        // does not see the pointer, so the box the card describes stays
        // hovered while the pointer travels onto it.
        if self.over_card(x, y) {
            match event {
                InputEvent::PointerMoved { .. } => {
                    self.card_pointer(x, y);
                    return self.set_hot(None);
                }
                InputEvent::Click { .. } | InputEvent::ClickWith { .. } => {
                    return self.card_click(x, y, now);
                }
                _ => return Vec::new(),
            }
        }
        let hit = {
            let g = self.geometry();
            g.in_chrome(x, y).then(|| g.hit(x, y))
        };
        let Some(hit) = hit else {
            // Over the map.
            let mut fx = self.set_hot(None);
            if self.picker.is_some() && self.app.is_none() {
                fx.extend(self.picker_event(event));
                return fx;
            }
            match event {
                InputEvent::PointerMoved { .. } => {
                    fx.extend(self.map_event(event, now));
                    self.track_rest(x, y, now);
                }
                InputEvent::Click { .. } | InputEvent::ClickWith { .. } => {
                    // Clicking is activity: the card goes and the rest starts
                    // over.
                    self.hide_card();
                    self.card.rest = None;
                    fx.extend(self.map_event(event, now));
                    self.track_rest(x, y, now);
                    if matches!(event, InputEvent::Click { .. }) {
                        fx.extend(self.block_click(now));
                    }
                }
                _ => {
                    self.hide_card();
                    self.card.rest = None;
                    fx.extend(self.map_event(event, now));
                }
            }
            return fx;
        };
        let hit = match hit {
            Some(Hit::Crumb) => self.crumb_at(x).map_or(hit, |i| Some(Hit::CrumbSeg(i))),
            other => other,
        };
        match event {
            InputEvent::PointerMoved { .. } => {
                let mut fx = self.set_hot(hit);
                // The pointer left the map for the chrome.
                self.hide_card();
                self.card.rest = None;
                fx.extend(self.map_event(InputEvent::PointerLeft, now));
                fx
            }
            InputEvent::Click { x, .. } if hit == Some(Hit::Capacity) => {
                self.capacity_click(x, now)
            }
            InputEvent::Click { .. } | InputEvent::ClickWith { .. } => self.chrome_click(hit, now),
            _ => Vec::new(),
        }
    }

    /// Whether and how administrator rights are offered.
    pub fn offer(&self) -> &Offer {
        &self.offer
    }

    /// How the Unreadable block words itself now.
    pub fn unreadable_as(&self) -> UnreadableAs {
        self.offer.unreadable_as()
    }

    /// Sets whether and how administrator rights are offered. Returns whether
    /// the Unreadable block must be worded again (the host lays the drive view
    /// out once more with [`UnreadableAs`] in its blocks).
    pub fn set_offer(&mut self, offer: Offer) -> bool {
        let reword = self.offer.unreadable_as() != offer.unreadable_as();
        self.offer = offer;
        reword
    }

    /// The button or the Unreadable block was pressed.
    fn full_scan_requested(&mut self, now: Duration) -> Vec<ShellEffect> {
        match self.offer.clone() {
            Offer::Available if self.busy() => {
                // A delete or a restore that is still under way would finish
                // into a map that the full result is about to replace.
                self.say("Full scan: wait for the last action to finish", now);
                Vec::new()
            }
            Offer::Available => {
                self.say("Starting the administrator helper…", now);
                vec![ShellEffect::FullScan]
            }
            Offer::Running => {
                self.say("The full scan is already running", now);
                Vec::new()
            }
            Offer::Done => {
                self.say(
                    "This map was already read with administrator rights; what is left is the filesystem's own overhead",
                    now,
                );
                Vec::new()
            }
            Offer::Explained(text) => {
                self.say(text, now);
                vec![ShellEffect::OpenUrl(
                    crate::notice::RELEASES_URL.to_string(),
                )]
            }
            Offer::Unavailable(text) => {
                self.say(text, now);
                Vec::new()
            }
            Offer::DeletesOnly => {
                self.say(
                    "A full scan reads a whole drive; open a drive to use it",
                    now,
                );
                Vec::new()
            }
            Offer::Hidden => {
                self.say("Administrator scans aren't available on this computer", now);
                Vec::new()
            }
        }
    }

    /// A click that selected one of the drive-level blocks.
    fn block_click(&mut self, now: Duration) -> Vec<ShellEffect> {
        let Some(app) = self.app.as_ref() else {
            return Vec::new();
        };
        let [id] = app.selection() else {
            return Vec::new();
        };
        match app.source().special(app.current_view(), *id) {
            Some(Special::Unreadable) => self.full_scan_requested(now),
            Some(Special::RecycleBin) => self.request_empty(now),
            _ => Vec::new(),
        }
    }

    /// The pointer is over the progress overlay or the banner.
    fn overlay_event(&mut self, event: InputEvent, hit: OverlayHit) -> Vec<ShellEffect> {
        self.hide_card();
        self.card.rest = None;
        let mut fx = self.set_hot(None);
        match event {
            InputEvent::PointerMoved { .. } => {
                self.overlay_hot = Some(hit);
                fx.extend(self.map_event(InputEvent::PointerLeft, self.clock));
            }
            InputEvent::Click { .. } if hit == OverlayHit::Button => {
                let stopped = self.overlay().is_some_and(|o| o.stopped);
                fx.push(if stopped {
                    ShellEffect::Rescan
                } else {
                    ShellEffect::CancelScan
                });
            }
            _ => {}
        }
        fx
    }

    fn chrome_click(&mut self, hit: Option<Hit>, now: Duration) -> Vec<ShellEffect> {
        let Some(hit) = hit else {
            return Vec::new();
        };
        let key = match hit {
            Hit::Back if self.can_go_back() => Some(Key::AltLeft),
            Hit::Forward if self.can_go_forward() => Some(Key::AltRight),
            Hit::Up if self.can_go_up() => Some(Key::AltUp),
            Hit::Up if self.picker_return && self.app.is_some() => {
                return vec![ShellEffect::ShowPicker];
            }
            Hit::Refresh => Some(Key::Refresh),
            Hit::FullScan => return self.full_scan_requested(now),
            Hit::Link => return vec![ShellEffect::OpenLink(chrome::LINK_URL)],
            Hit::CrumbSeg(index) => {
                self.hide_card();
                self.card.rest = None;
                return self.map_event(InputEvent::CrumbClick { index }, now);
            }
            _ => None,
        };
        match key {
            Some(k) => self.key(k, now),
            None => Vec::new(),
        }
    }

    /// The breadcrumb as laid out now.
    fn crumb_parts(&mut self) -> Vec<CrumbPart> {
        let mut area = self.geometry().crumb;
        if let Some(o) = self.overlay().filter(|o| !o.stopped) {
            area.w = (o.panel.x - area.x - (8.0 * self.scale).round()).max(0.0);
        }
        let em = self.font_px * self.scale;
        let segs = self.crumb_segments();
        chrome::layout_crumbs(area, em, &segs, self.measure.as_ref())
    }

    /// The names the breadcrumb shows: the view's trail, else its text, else
    /// what was set before a map existed.
    /// Where the words for an empty folder go: centred in the map, when the
    /// view on screen is a folder that holds nothing.
    fn empty_label_rect(&self, measure: &dyn Fn(&str) -> f32) -> Option<Rect> {
        let app = self.app.as_ref()?;
        if app.items().len() > 1 || !app.source().view_is_empty(app.current_view()) {
            return None;
        }
        let map = self.map_rect();
        let em = self.font_px * self.scale;
        let w = measure(EMPTY_FOLDER).ceil().min(map.w);
        let h = (em * 1.4).round();
        Some(Rect::new(
            (map.x + (map.w - w) / 2.0).round(),
            (map.y + (map.h - h) / 2.0).round(),
            w,
            h,
        ))
    }

    fn crumb_segments(&self) -> Vec<String> {
        if let Some(app) = &self.app {
            let view = app.current_view();
            let trail = app.source().trail(view);
            if !trail.is_empty() {
                return trail;
            }
            let text = app.source().breadcrumb(view);
            if !text.is_empty() {
                return vec![text];
            }
        }
        if self.crumb.is_empty() {
            Vec::new()
        } else {
            vec![self.crumb.clone()]
        }
    }

    /// The clickable breadcrumb segment under `x`.
    fn crumb_at(&mut self, x: f32) -> Option<usize> {
        self.crumb_parts()
            .iter()
            .find(|p| p.target.is_some() && x >= p.rect.x && x < p.rect.right())
            .and_then(|p| p.target)
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
            Hit::Up if self.picker_return => line("Back to the drives (Backspace)"),
            Hit::Up => line("Up — already at the top"),
            Hit::Refresh => line("Refresh the layout (F5)"),
            Hit::FullScan if self.offer.can_start() => {
                line("Read the whole drive with administrator rights")
            }
            Hit::FullScan => line("A full scan is running"),
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
            Hit::Capacity => line(
                &self
                    .capacity
                    .breakdown()?
                    .describe_as(self.offer == Offer::Done),
            ),
            Hit::CrumbSeg(i) => {
                let segs = self.crumb_segments();
                line(&format!("Go to {}", segs.get(i)?))
            }
            Hit::Crumb | Hit::Info => return None,
        })
    }

    /// The info bar's text now: the control under the pointer, else the box
    /// under the pointer, else the selection, else the scan status.
    pub fn info_line(&self) -> InfoLine {
        if let Some(t) = self.hot.and_then(|h| self.tooltip(h)) {
            return t;
        }
        if let Some((text, until)) = &self.notice
            && self.clock < *until
        {
            return InfoLine {
                name: text.clone(),
                details: String::new(),
            };
        }
        if self.app.is_none()
            && let Some(picker) = &self.picker
        {
            let card = match self.picker_hot {
                Some(PickTarget::Card(i)) => Some(i),
                _ => self.picker_sel,
            };
            if let Some((name, details)) = card.and_then(|i| picker.describe(i)) {
                return InfoLine { name, details };
            }
            if self.picker_hot == Some(PickTarget::Folder) {
                return InfoLine {
                    name: "Choose a folder to scan with the system's folder dialog".into(),
                    details: String::new(),
                };
            }
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
        self.clock = self.clock.max(now);
        let crumbs = self.crumb_parts();
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
            (g.up, Glyph::Up, up || self.picker_return, Hit::Up),
            (g.refresh, Glyph::Refresh, true, Hit::Refresh),
        ] {
            widgets.push(Widget::Button {
                rect,
                glyph,
                enabled,
                hot: hot == Some(h),
            });
        }
        if let Some(rect) = g.full_scan {
            widgets.push(Widget::Text {
                rect,
                text: elevation::FULL_SCAN_LABEL.into(),
                role: if self.offer.can_start() {
                    TextRole::Link
                } else {
                    TextRole::Secondary
                },
                hot: hot == Some(Hit::FullScan) && self.offer.can_start(),
            });
        }
        for part in crumbs {
            let role = match (part.current, part.target) {
                (true, _) => TextRole::Primary,
                (false, Some(_)) => TextRole::Link,
                (false, None) => TextRole::Secondary,
            };
            widgets.push(Widget::Text {
                rect: part.rect,
                text: part.text.into(),
                role,
                hot: part.target.is_some_and(|i| hot == Some(Hit::CrumbSeg(i))),
            });
        }
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
        if let Some(layout) = self.picker_layout() {
            let hot_folder = self.picker_hot == Some(PickTarget::Folder);
            for mut w in layout.widgets {
                if let Widget::Pill { hot, .. } = &mut w {
                    *hot = hot_folder;
                }
                widgets.push(w);
            }
        }
        if let Some(rect) = self.empty_label_rect(&measure) {
            widgets.push(Widget::Text {
                rect,
                text: EMPTY_FOLDER.into(),
                role: TextRole::Secondary,
                hot: false,
            });
        }
        widgets.extend(self.crash_widgets());
        if let Some(o) = self.overlay() {
            widgets.push(Widget::Panel { rect: o.panel });
            widgets.push(Widget::Text {
                rect: o.text,
                text: o.words.into(),
                role: TextRole::Primary,
                hot: false,
            });
            widgets.push(Widget::Pill {
                rect: o.button,
                text: o.button_text.into(),
                accent: o.stopped,
                hot: self.overlay_hot == Some(OverlayHit::Button),
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
