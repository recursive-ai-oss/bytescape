//! The software renderer: a retained picture of the current view, dirty-region
//! repainting for hover and selection, tiled full repaints and the cached-image
//! zoom.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tiny_skia::{Pixmap, PixmapMut};

use crate::canvas::Canvas;
use crate::card::{Card, draw_card};
use crate::chrome::{Chrome, Widget, draw_chrome};
use crate::geom::{IRect, Rect};
use crate::pixel;
use crate::scene::{
    Bar, Flags, Item, ItemKind, Palette, SceneStyle, draw_bars, draw_items, draw_overlay,
    title_band,
};
use crate::skia::{HatchTiles, SkiaCanvas};
use crate::text::TextEngine;
use crate::zoom::{ZoomFrame, compose_zoom, parallel_bands, zoom_geometry};

/// Length of a zoom animation.
pub const ZOOM_DURATION: Duration = Duration::from_millis(150);

/// Which way a zoom goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomDirection {
    /// Into a child folder: the child's box grows to fill the window.
    In,
    /// Out to the parent: the window shrinks to the folder's box.
    Out,
}

struct Anim {
    direction: ZoomDirection,
    /// The large image (the parent view) and the small one (the child view).
    big: Pixmap,
    small: Pixmap,
    child: Rect,
    /// The part of the window that zooms: the map. The bars stay put.
    region: IRect,
    start: Duration,
}

/// What `paint` changed in the target buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Painted {
    /// Regions of the target that were written. The whole target counts as one
    /// region after a zoom frame or a full repaint.
    pub damage: Vec<IRect>,
    /// True while a zoom is still running, so the caller schedules the next
    /// frame immediately.
    pub animating: bool,
}

/// Retained software renderer for one window.
pub struct Renderer {
    width: u32,
    height: u32,
    scale: f32,
    font_px: f32,
    reduced_motion: bool,
    palette: Palette,
    text: TextEngine,
    hatches: HatchTiles,
    threads: usize,
    items: Arc<Vec<Item>>,
    chrome: Chrome,
    card: Option<Card>,
    index: HashMap<u32, usize>,
    front: Pixmap,
    stub: Pixmap,
    pool: Vec<Pixmap>,
    labels_drawn: bool,
    hover: Option<u32>,
    selected: Vec<u32>,
    dim: Option<u8>,
    pulse: f32,
    dirty: Option<IRect>,
    full_dirty: bool,
    anim: Option<Anim>,
    timings: PaintTimings,
}

/// Where the time of the latest `paint` went, for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaintTimings {
    /// Repainting the retained picture (tiled), in microseconds.
    pub render_micros: u64,
    /// Copying the picture into the target buffer, in microseconds.
    pub copy_micros: u64,
}

impl Renderer {
    /// Creates a renderer. Returns `None` for a zero-sized window.
    pub fn new(
        width: u32,
        height: u32,
        scale: f32,
        font_px: f32,
        palette: Palette,
        text: TextEngine,
    ) -> Option<Self> {
        let front = Pixmap::new(width, height)?;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16);
        Some(Self {
            width,
            height,
            scale,
            font_px,
            reduced_motion: false,
            palette,
            text,
            hatches: HatchTiles::default(),
            threads,
            items: Arc::new(Vec::new()),
            chrome: Chrome::default(),
            card: None,
            index: HashMap::new(),
            front,
            stub: Pixmap::new(1, 1)?,
            pool: Vec::new(),
            labels_drawn: false,
            hover: None,
            selected: Vec::new(),
            dim: None,
            pulse: 1.0,
            dirty: None,
            full_dirty: true,
            anim: None,
            timings: PaintTimings::default(),
        })
    }

    /// Timing breakdown of the latest `paint`.
    pub fn timings(&self) -> PaintTimings {
        self.timings
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads.max(1);
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn set_reduced_motion(&mut self, on: bool) {
        self.reduced_motion = on;
    }

    pub fn text(&self) -> &TextEngine {
        &self.text
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        self.finish_zoom();
        self.invalidate();
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Changes the window size or scale factor. The caller supplies a fresh
    /// item list afterwards; caches are keyed by scale, so nothing stale is
    /// reused.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        if let Some(front) = Pixmap::new(width, height) {
            self.front = front;
            self.width = width;
            self.height = height;
            self.scale = scale;
            // The animation's images have the old size; drop them with it.
            self.pool.clear();
            self.anim = None;
            self.items = Arc::new(Vec::new());
            self.index.clear();
            self.invalidate();
        }
    }

    pub fn style(&self, labels: bool) -> SceneStyle {
        let mut st = SceneStyle::new(self.scale, self.font_px, labels);
        st.dim_except = self.dim;
        st.pulse = self.pulse;
        st
    }

    /// Jumps a running zoom to its end, rendering the new view at full quality.
    pub fn finish_animation(&mut self) {
        self.finish_zoom();
    }

    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    /// The items of the view on screen.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    fn invalidate(&mut self) {
        self.full_dirty = true;
        self.labels_drawn = false;
    }

    fn take_pixmap(&mut self) -> Option<Pixmap> {
        match self.pool.pop() {
            Some(p) if p.width() == self.width && p.height() == self.height => Some(p),
            _ => Pixmap::new(self.width, self.height),
        }
    }

    fn set_view(&mut self, items: Arc<Vec<Item>>) {
        self.index = items.iter().enumerate().map(|(i, it)| (it.id, i)).collect();
        self.items = items;
        self.hover = None;
        self.selected.clear();
    }

    /// Sets the chrome bars drawn over the map and repaints everything.
    pub fn set_bars(&mut self, bars: Vec<Bar>) {
        self.chrome = Chrome {
            bars,
            widgets: Vec::new(),
        };
        self.invalidate();
    }

    /// Replaces the window chrome. When only some widgets differ from the
    /// current ones, only their rectangles are repainted.
    /// Returns whether anything changed.
    pub fn set_chrome(&mut self, chrome: Chrome) -> bool {
        if self.chrome == chrome {
            return false;
        }
        let regions = self.chrome.changed_regions(&chrome);
        if self.anim.is_some() {
            // The bars do not zoom, so a change meant for them lands in both
            // images of the animation, and the frame being composed shows it.
            let mut rects: Vec<Rect> = regions.clone().unwrap_or_else(|| {
                let all = |c: &Chrome| {
                    c.bars
                        .iter()
                        .map(|b| b.rect)
                        .chain(c.widgets.iter().map(Widget::rect))
                        .collect::<Vec<_>>()
                };
                let mut v = all(&self.chrome);
                v.extend(all(&chrome));
                v
            });
            self.chrome = chrome;
            rects.retain(|r| r.w > 0.0 && r.h > 0.0);
            self.repaint_bars_in_animation(&rects);
            return true;
        }
        self.chrome = chrome;
        let partial = regions.filter(|_| !self.full_dirty);
        match partial {
            Some(regions) => {
                for r in regions {
                    self.repaint_region(r);
                }
            }
            None => self.invalidate(),
        }
        true
    }

    /// Shows the hover card, moves it, or removes it with `None`. Only the
    /// rectangles of the old and new card repaint. Returns whether anything
    /// changed.
    pub fn set_card(&mut self, card: Option<Card>) -> bool {
        if self.card == card {
            return false;
        }
        let scale = self.scale;
        let region = [&self.card, &card]
            .into_iter()
            .flatten()
            .map(|c| c.dirty_rect(scale))
            .reduce(|a, b| a.union(&b));
        self.card = card;
        if self.anim.is_none()
            && !self.full_dirty
            && let Some(region) = region
        {
            self.repaint_region(region);
        }
        true
    }

    /// The hover card on screen.
    pub fn card(&self) -> Option<&Card> {
        self.card.as_ref()
    }

    /// Draws the bars again in `rects` in both images of the running zoom.
    fn repaint_bars_in_animation(&mut self, rects: &[Rect]) {
        let Some(mut anim) = self.anim.take() else {
            return;
        };
        let style = self.style(self.text.is_ready());
        for pm in [&mut anim.big, &mut anim.small] {
            let mut canvas = SkiaCanvas::new(pm.as_mut(), self.text.clone(), self.hatches.clone());
            for &r in rects {
                let Some(r) =
                    r.intersect(&Rect::new(0.0, 0.0, self.width as f32, self.height as f32))
                else {
                    continue;
                };
                let r = r.outer_pixels().to_rect();
                canvas.push_clip(r);
                canvas_fill_bg(&mut canvas, r, &self.palette);
                draw_chrome(&mut canvas, &self.chrome, &self.palette, &style, Some(r));
                canvas.pop_clip();
            }
        }
        self.anim = Some(anim);
    }

    /// The chrome on screen.
    pub fn chrome(&self) -> &Chrome {
        &self.chrome
    }

    /// Dims every file and group outside `category` (the legend is hovered),
    /// or clears the dimming. Repaints the map without laying anything out
    /// again.
    pub fn set_dim(&mut self, category: Option<u8>) {
        if self.dim == category {
            return;
        }
        self.dim = category;
        if self.anim.is_some() || self.full_dirty {
            // The picture is painted again anyway.
            self.invalidate();
            return;
        }
        let Some(region) = self.items.iter().map(|i| i.rect).reduce(|a, b| a.union(&b)) else {
            return;
        };
        self.repaint_region(region);
    }

    /// Sets how solid the title text of folders still being counted is, and
    /// repaints just those title bars.
    pub fn set_pulse(&mut self, pulse: f32) {
        if (self.pulse - pulse).abs() < 0.001 {
            return;
        }
        self.pulse = pulse;
        if self.anim.is_some() || self.full_dirty {
            return;
        }
        let st = self.style(self.labels_drawn);
        let bands: Vec<Rect> = self
            .items
            .iter()
            .filter(|i| {
                i.kind == ItemKind::FolderTitled
                    && i.label.is_some()
                    && i.flags.contains(Flags::IN_PROGRESS)
            })
            .map(|i| title_band(i.rect, &st))
            .collect();
        for band in bands {
            self.repaint_region(band);
        }
    }

    /// Replaces the view without animation and repaints everything.
    pub fn set_items(&mut self, items: Arc<Vec<Item>>) {
        self.finish_zoom();
        self.set_view(items);
        self.invalidate();
    }

    /// Paints `items` into `pixmap` with horizontal bands on several threads.
    fn render_tiled(&self, pixmap: &mut Pixmap, items: &[Item], labels: bool) {
        let (w, h) = (self.width, self.height);
        let style = self.style(labels);
        let stride = w as usize * 4;
        let band_rows = (h as usize / (self.threads * 4)).clamp(16, 256);
        let pal = &self.palette;
        let chrome = &self.chrome;
        parallel_bands(
            pixmap.data_mut(),
            stride,
            band_rows,
            self.threads,
            |row0, band| {
                let rows = band.len() / stride;
                let Some(band_map) = PixmapMut::from_bytes(band, w, rows as u32) else {
                    return;
                };
                let mut canvas = SkiaCanvas::band(
                    band_map,
                    (0, row0 as i32),
                    (w, h),
                    self.text.clone(),
                    self.hatches.clone(),
                );
                let region = Rect::new(0.0, row0 as f32, w as f32, rows as f32);
                canvas_fill_bg(&mut canvas, region, pal);
                draw_items(&mut canvas, items, pal, &style, Some(region));
                draw_chrome(&mut canvas, chrome, pal, &style, Some(region));
            },
        );
    }

    /// Repaints the picture in full, with labels when fonts are available.
    fn render_front(&mut self) {
        let mut pm = std::mem::replace(&mut self.front, self.stub.clone());
        let labels = self.text.is_ready();
        let items = Arc::clone(&self.items);
        self.render_tiled(&mut pm, &items, labels);
        self.front = pm;
        self.labels_drawn = labels;
        self.full_dirty = true;
        self.paint_overlays(None);
    }

    /// Outlines to draw: `(rect, hovered, selected)`.
    fn overlay_rects(&self) -> Vec<(Rect, bool, bool)> {
        let mut out = Vec::new();
        for &id in &self.selected {
            if let Some(r) = self.rect_of(Some(id)) {
                out.push((r, self.hover == Some(id), true));
            }
        }
        if let Some(id) = self.hover
            && !self.selected.contains(&id)
            && let Some(r) = self.rect_of(Some(id))
        {
            out.push((r, true, false));
        }
        out
    }

    fn paint_overlays(&mut self, region: Option<Rect>) {
        let overlays = self.overlay_rects();
        let pal = self.palette.clone();
        let scale = self.scale;
        let style = self.style(self.labels_drawn);
        let mut canvas =
            SkiaCanvas::new(self.front.as_mut(), self.text.clone(), self.hatches.clone());
        if let Some(r) = region {
            canvas.push_clip(r);
        }
        for (rect, hover, selected) in overlays {
            if region.is_none_or(|r| r.intersects(&rect.inflate(2.0 * scale))) {
                draw_overlay(&mut canvas, rect, &pal, hover, selected, scale);
            }
        }
        if let Some(card) = &self.card {
            draw_card(&mut canvas, card, &pal, &style, region);
        }
    }

    /// Redraws only `region` of the retained picture: the items under it, then
    /// the outlines.
    fn repaint_region(&mut self, region: Rect) {
        let Some(region) =
            region.intersect(&Rect::new(0.0, 0.0, self.width as f32, self.height as f32))
        else {
            return;
        };
        let region = region.outer_pixels().to_rect();
        let style = self.style(self.labels_drawn);
        {
            let mut canvas =
                SkiaCanvas::new(self.front.as_mut(), self.text.clone(), self.hatches.clone());
            canvas.push_clip(region);
            canvas_fill_bg(&mut canvas, region, &self.palette);
            draw_items(
                &mut canvas,
                &self.items,
                &self.palette,
                &style,
                Some(region),
            );
            draw_chrome(
                &mut canvas,
                &self.chrome,
                &self.palette,
                &style,
                Some(region),
            );
        }
        self.paint_overlays(Some(region));
        let ir = region.outer_pixels();
        self.dirty = Some(self.dirty.map_or(ir, |d| d.union(&ir)));
    }

    fn rect_of(&self, id: Option<u32>) -> Option<Rect> {
        id.and_then(|id| self.index.get(&id))
            .and_then(|&i| self.items.get(i))
            .map(|it| it.rect)
    }

    /// Repaints what an outline on `rect` touched. A hover outline only covers
    /// the box's edge, so only four thin strips repaint; a selection also tints
    /// the whole box, so it repaints in full.
    fn repaint_outline(&mut self, rect: Rect, whole_box: bool) {
        if self.full_dirty {
            return;
        }
        if whole_box {
            self.repaint_region(rect);
            return;
        }
        let t = (2.0 * self.scale).round().max(1.0) + 1.0;
        let r = rect.snapped();
        if r.w <= 2.0 * t || r.h <= 2.0 * t {
            self.repaint_region(r);
            return;
        }
        self.repaint_region(Rect::new(r.x, r.y, r.w, t));
        self.repaint_region(Rect::new(r.x, r.bottom() - t, r.w, t));
        self.repaint_region(Rect::new(r.x, r.y + t, t, r.h - 2.0 * t));
        self.repaint_region(Rect::new(r.right() - t, r.y + t, t, r.h - 2.0 * t));
    }

    /// Moves the hover outline. Only the old and new boxes' edges repaint.
    pub fn set_hover(&mut self, id: Option<u32>) {
        if self.hover == id {
            return;
        }
        if self.anim.is_some() {
            // The outline is painted when the zoom ends.
            self.hover = id;
            return;
        }
        let selected = |s: &Self, id: Option<u32>| id.is_some_and(|i| s.selected.contains(&i));
        let old = self.rect_of(self.hover);
        let old_selected = selected(self, self.hover);
        self.hover = id;
        let new = self.rect_of(id);
        let new_selected = selected(self, id);
        if let Some(r) = old {
            self.repaint_outline(r, old_selected);
        }
        if let Some(r) = new {
            self.repaint_outline(r, new_selected);
        }
    }

    /// Moves the selection outline to one box, or to none.
    pub fn set_selected(&mut self, id: Option<u32>) {
        let ids: Vec<u32> = id.into_iter().collect();
        self.set_selection(&ids);
    }

    /// Selects the boxes in `ids`. Only the boxes whose state changed repaint.
    pub fn set_selection(&mut self, ids: &[u32]) {
        if self.selected == ids {
            return;
        }
        let old = std::mem::replace(&mut self.selected, ids.to_vec());
        if self.anim.is_some() {
            return;
        }
        let changed: Vec<u32> = old
            .iter()
            .filter(|i| !ids.contains(i))
            .chain(ids.iter().filter(|i| !old.contains(i)))
            .copied()
            .collect();
        for id in changed {
            if let Some(r) = self.rect_of(Some(id)) {
                self.repaint_outline(r, true);
            }
        }
    }

    /// Starts a zoom to `items`, the new view. `child` is the folder's box in
    /// the larger of the two views: the current one when zooming in, the new
    /// one when zooming out. The target view is rendered once, without labels;
    /// the animation scales and crossfades the two cached images, and the
    /// full-quality view replaces them at the end.
    pub fn begin_zoom(
        &mut self,
        direction: ZoomDirection,
        items: Arc<Vec<Item>>,
        child: Rect,
        now: Duration,
    ) {
        // A zoom already running ends first, so the new one starts from a real
        // picture of the view on screen.
        self.finish_zoom();
        if self.reduced_motion {
            self.set_items(items);
            return;
        }
        if self.full_dirty || !self.labels_drawn && self.text.is_ready() {
            self.render_front();
        }
        let Some(mut other) = self.take_pixmap() else {
            self.set_items(items);
            return;
        };
        self.render_tiled(&mut other, &items, false);
        let current = std::mem::replace(&mut self.front, self.stub.clone());
        let target = other;
        let (big, small) = match direction {
            ZoomDirection::In => (current, target),
            ZoomDirection::Out => (target, current),
        };
        let region = zoom_region(&items, self.width, self.height);
        self.set_view(items);
        self.prewarm_labels();
        self.anim = Some(Anim {
            direction,
            big,
            small,
            child,
            region,
            start: now,
        });
    }

    /// Shapes the new view's labels on a worker thread while the animation
    /// runs, so the full-quality frame at the end only blits cached labels.
    /// One thread, on purpose: on a four-core machine more threads slowed the
    /// animation frames running beside them more than they sped the shaping up.
    fn prewarm_labels(&self) {
        if !self.text.is_ready() {
            return;
        }
        let items = Arc::clone(&self.items);
        let pal = self.palette.clone();
        let style = self.style(true);
        let text = self.text.clone();
        let spawned = std::thread::Builder::new()
            .name("labels".into())
            .spawn(move || {
                let mut warm = crate::warm::WarmCanvas::new(text);
                draw_items(&mut warm, &items, &pal, &style, None);
                draw_bars(&mut warm, &[], &pal, &style, None);
            });
        drop(spawned);
    }

    /// Progress of the running animation, `0.0..=1.0` in time.
    fn anim_time(&self, now: Duration) -> Option<f32> {
        self.anim.as_ref().map(|a| {
            (now.saturating_sub(a.start).as_secs_f32() / ZOOM_DURATION.as_secs_f32()).min(1.0)
        })
    }

    /// Writes the current picture into `target` (`width × height` native pixel
    /// words) and reports what changed.
    ///
    /// `whole` forces a full copy; pass it when the buffer's contents are not
    /// the previous frame's.
    pub fn paint(&mut self, target: &mut [u32], now: Duration, whole: bool) -> Painted {
        self.timings = PaintTimings::default();
        let full = IRect::new(0, 0, self.width as i32, self.height as i32);
        if target.len() < (self.width * self.height) as usize {
            return Painted {
                damage: Vec::new(),
                animating: false,
            };
        }
        if let Some(t) = self.anim_time(now) {
            if t < 1.0 {
                self.paint_zoom_frame(target, t);
                return Painted {
                    damage: vec![full],
                    animating: true,
                };
            }
            self.finish_zoom();
        }
        self.ensure_front();
        if self.full_dirty || (!self.labels_drawn && self.text.is_ready()) {
            let t = std::time::Instant::now();
            self.render_front();
            self.timings.render_micros = t.elapsed().as_micros() as u64;
        }
        if self.full_dirty || whole {
            let t = std::time::Instant::now();
            self.copy_front(target, full);
            self.timings.copy_micros = t.elapsed().as_micros() as u64;
            self.full_dirty = false;
            self.dirty = None;
            return Painted {
                damage: vec![full],
                animating: false,
            };
        }
        match self.dirty.take() {
            Some(d) => {
                self.copy_front(target, d);
                Painted {
                    damage: vec![d],
                    animating: false,
                }
            }
            None => Painted {
                damage: Vec::new(),
                animating: false,
            },
        }
    }

    fn paint_zoom_frame(&self, target: &mut [u32], t: f32) {
        let Some(a) = &self.anim else {
            return;
        };
        let eased = 1.0 - (1.0 - t).powi(3);
        let p = match a.direction {
            ZoomDirection::In => eased,
            ZoomDirection::Out => 1.0 - eased,
        };
        // The folder's box is in window coordinates; the zoom works in the
        // map's own.
        let child = Rect::new(
            a.child.x - a.region.x as f32,
            a.child.y - a.region.y as f32,
            a.child.w,
            a.child.h,
        );
        let frame: ZoomFrame = zoom_geometry(a.region.w as f32, a.region.h as f32, child, p);
        let (w, h) = (self.width as usize, self.height as usize);
        compose_zoom(
            target,
            (w, h),
            a.region,
            a.big.data(),
            a.small.data(),
            &frame,
            self.threads,
        );
    }

    /// Ends a running animation, if any, and puts a real picture back in
    /// `front`. Every path that keeps the renderer's size ends a zoom here, so
    /// the placeholder swapped in while the images were animating never outlives
    /// it. The one exception is `resize`, which replaces `front` and drops the
    /// animation's images together, because they have the old size. The new view
    /// is repainted at full quality, with labels, on the next `paint`.
    fn finish_zoom(&mut self) {
        let Some(anim) = self.anim.take() else {
            return;
        };
        self.pool.push(anim.big);
        self.pool.push(anim.small);
        self.front = self.take_pixmap().unwrap_or_else(|| self.stub.clone());
        self.invalidate();
    }

    /// Guards against a picture of the wrong size (it cannot happen through
    /// the public calls, but a wrong size would otherwise be an out-of-range
    /// slice).
    fn ensure_front(&mut self) {
        if self.front.width() != self.width || self.front.height() != self.height {
            if let Some(pm) = Pixmap::new(self.width, self.height) {
                self.front = pm;
            }
            self.invalidate();
        }
    }

    fn copy_front(&self, target: &mut [u32], region: IRect) {
        let Some(r) = region.intersect(&IRect::new(0, 0, self.width as i32, self.height as i32))
        else {
            return;
        };
        let w = self.width as usize;
        let data = self.front.data();
        let rows = &mut target[r.y as usize * w..r.bottom() as usize * w];
        let (x0, x1) = (r.x as usize, r.right() as usize);
        let first = r.y as usize;
        let src_for = |row: usize| &data[(first + row) * w * 4..][..w * 4];
        if r.h >= 256 {
            let band_rows = (r.h as usize / (self.threads * 2)).max(16);
            parallel_bands(rows, w, band_rows, self.threads, |row0, band| {
                for (i, line) in band.chunks_exact_mut(w).enumerate() {
                    copy_words(&mut line[x0..x1], &src_for(row0 + i)[x0 * 4..x1 * 4]);
                }
            });
        } else {
            for (i, line) in rows.chunks_exact_mut(w).enumerate() {
                copy_words(&mut line[x0..x1], &src_for(i)[x0 * 4..x1 * 4]);
            }
        }
    }

    /// The current picture as straight RGBA, for PNG export. Renders first if
    /// anything is pending.
    pub fn snapshot_rgba(&mut self) -> Vec<u8> {
        self.finish_zoom();
        self.ensure_front();
        if self.full_dirty || (!self.labels_drawn && self.text.is_ready()) {
            self.render_front();
            self.full_dirty = false;
        }
        pixel::to_png_rgba(self.front.data())
    }

    /// Writes the current picture to a PNG file.
    pub fn save_png(&mut self, path: &std::path::Path) -> Result<(), String> {
        let rgba = self.snapshot_rgba();
        let size = tiny_skia::IntSize::from_wh(self.width, self.height)
            .ok_or_else(|| "empty picture".to_string())?;
        let pixmap = Pixmap::from_vec(rgba, size).ok_or_else(|| "bad picture size".to_string())?;
        pixmap.save_png(path).map_err(|e| e.to_string())
    }

    /// Hit test: the topmost item containing the point.
    pub fn item_at(&self, x: f32, y: f32) -> Option<&Item> {
        self.items.iter().rev().find(|it| it.rect.contains(x, y))
    }
}

/// The part of the window that zooms: the root box of the view (the map),
/// or the whole window when the view has no such box.
fn zoom_region(items: &[Item], width: u32, height: u32) -> IRect {
    let full = IRect::new(0, 0, width as i32, height as i32);
    items
        .first()
        .filter(|i| i.depth == 0)
        .and_then(|i| i.rect.outer_pixels().intersect(&full))
        .filter(|r| r.w > 0 && r.h > 0)
        .unwrap_or(full)
}

fn canvas_fill_bg(canvas: &mut SkiaCanvas<'_>, region: Rect, pal: &Palette) {
    canvas.fill_rect(region, pal.bg);
}

fn copy_words(dst: &mut [u32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
        *d = u32::from_le_bytes(*s);
    }
}
