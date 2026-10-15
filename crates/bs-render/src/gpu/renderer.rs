//! The GPU renderer: the same calls as the software [`crate::Renderer`], with
//! the retained picture, the two zoom images and the label atlas living in
//! GPU memory.
//!
//! Hover, selection, the hover card and the toast still repaint only their own
//! rectangles into the retained picture. A zoom renders the target view once
//! and then draws the two cached images scaled and blended, one pass per
//! frame.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::card::{Card, Popup, draw_card, draw_popup};
use crate::chrome::{Chrome, Widget, draw_chrome};
use crate::geom::{IRect, Rect};
use crate::renderer::{PaintTimings, Painted, ZOOM_DURATION, ZoomDirection};
use crate::scene::{
    Bar, Flags, Item, ItemKind, Palette, SceneStyle, draw_bars, draw_items, draw_overlay,
    title_band,
};
use crate::text::TextEngine;
use crate::zoom::zoom_geometry;

use super::atlas::Atlas;
use super::batch::{Batch, Bind, Kind, Target};
use super::canvas::GpuCanvas;
use super::image::{IMAGE_FORMAT, Image, read_rgba};
use super::{FailStage, GpuContext, GpuError, GpuOptions};

struct Anim {
    direction: ZoomDirection,
    big: Image,
    small: Image,
    child: Rect,
    region: IRect,
    start: Duration,
}

/// The size and scale of the picture a renderer draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Size in device pixels.
    pub width: u32,
    pub height: u32,
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// The UI font size in logical pixels.
    pub font_px: f32,
}

/// Retained renderer for one window, drawing on the GPU.
pub struct GpuRenderer {
    gpu: Arc<GpuContext>,
    width: u32,
    height: u32,
    scale: f32,
    font_px: f32,
    reduced_motion: bool,
    palette: Palette,
    text: TextEngine,
    atlas: Atlas,
    view_bind: wgpu::BindGroup,
    items: Arc<Vec<Item>>,
    chrome: Chrome,
    card: Option<Card>,
    toast: Option<Card>,
    popup: Option<Popup>,
    index: HashMap<u32, usize>,
    front: Option<Image>,
    offscreen: Option<Image>,
    pool: Vec<Image>,
    /// Repaints of single regions of the retained picture, waiting for the
    /// next frame.
    pending: Batch,
    labels_drawn: bool,
    hover: Option<u32>,
    selected: Vec<u32>,
    dim: Option<u8>,
    pulse: f32,
    full_dirty: bool,
    anim: Option<Anim>,
    timings: PaintTimings,
}

fn view_bind(gpu: &GpuContext, width: u32, height: u32) -> wgpu::BindGroup {
    let mut bytes = Vec::with_capacity(16);
    for v in [width as f32, height as f32, 0.0, 0.0] {
        bytes.extend_from_slice(&v.to_ne_bytes());
    }
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bytescape view"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    gpu.queue.write_buffer(&buffer, 0, &bytes);
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bytescape view"),
        layout: &gpu.view_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    })
}

fn clear_color(c: crate::geom::Color) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(c.r) / 255.0,
        g: f64::from(c.g) / 255.0,
        b: f64::from(c.b) / 255.0,
        a: 1.0,
    }
}

impl GpuRenderer {
    /// Creates a renderer. Fails for an empty window or one the device cannot
    /// hold.
    pub fn new(
        gpu: Arc<GpuContext>,
        geometry: Geometry,
        palette: Palette,
        text: TextEngine,
        options: &GpuOptions,
    ) -> Result<Self, GpuError> {
        options.check(FailStage::Renderer)?;
        let Geometry {
            width,
            height,
            scale,
            font_px,
        } = geometry;
        let max = gpu.max_dimension();
        if width == 0 || height == 0 || width > max || height > max {
            return Err(GpuError::Size { width, height, max });
        }
        let atlas = Atlas::new(
            gpu.device.clone(),
            gpu.queue.clone(),
            gpu.image_layout.clone(),
            gpu.nearest.clone(),
        );
        let view_bind = view_bind(&gpu, width, height);
        let front = Image::new(&gpu, width, height);
        Ok(Self {
            gpu,
            width,
            height,
            scale,
            font_px,
            reduced_motion: false,
            palette,
            text,
            atlas,
            view_bind,
            items: Arc::new(Vec::new()),
            chrome: Chrome::default(),
            card: None,
            toast: None,
            popup: None,
            index: HashMap::new(),
            front: Some(front),
            offscreen: None,
            pool: Vec::new(),
            pending: Batch::new(),
            labels_drawn: false,
            hover: None,
            selected: Vec::new(),
            dim: None,
            pulse: 1.0,
            full_dirty: true,
            anim: None,
            timings: PaintTimings::default(),
        })
    }

    /// The GPU this renderer draws on.
    pub fn gpu(&self) -> &Arc<GpuContext> {
        &self.gpu
    }

    /// Time spent in the latest `paint`: recording the rectangles, and
    /// handing them to the GPU.
    pub fn timings(&self) -> PaintTimings {
        self.timings
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
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
    /// item list afterwards.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) -> Result<(), GpuError> {
        let max = self.gpu.max_dimension();
        if width == 0 || height == 0 || width > max || height > max {
            return Err(GpuError::Size { width, height, max });
        }
        self.width = width;
        self.height = height;
        self.scale = scale;
        self.view_bind = view_bind(&self.gpu, width, height);
        self.front = Some(Image::new(&self.gpu, width, height));
        self.offscreen = None;
        self.pool.clear();
        self.anim = None;
        self.items = Arc::new(Vec::new());
        self.index.clear();
        self.invalidate();
        Ok(())
    }

    pub fn style(&self, labels: bool) -> SceneStyle {
        let mut st = SceneStyle::new(self.scale, self.font_px, labels);
        st.dim_except = self.dim;
        st.pulse = self.pulse;
        st
    }

    /// Jumps a running zoom to its end.
    pub fn finish_animation(&mut self) {
        self.finish_zoom();
    }

    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    fn invalidate(&mut self) {
        self.full_dirty = true;
        self.labels_drawn = false;
        self.pending.clear();
    }

    fn take_image(&mut self) -> Image {
        match self.pool.pop() {
            Some(p) if p.size == (self.width, self.height) => p,
            _ => Image::new(&self.gpu, self.width, self.height),
        }
    }

    fn set_view(&mut self, items: Arc<Vec<Item>>) {
        self.index = items.iter().enumerate().map(|(i, it)| (it.id, i)).collect();
        self.items = items;
        self.hover = None;
        self.selected.clear();
    }

    pub fn set_bars(&mut self, bars: Vec<Bar>) {
        self.chrome = Chrome {
            bars,
            widgets: Vec::new(),
        };
        self.invalidate();
    }

    /// Replaces the window chrome; only widgets that differ repaint. Returns
    /// whether anything changed.
    pub fn set_chrome(&mut self, chrome: Chrome) -> bool {
        if self.chrome == chrome {
            return false;
        }
        let regions = self.chrome.changed_regions(&chrome);
        if self.anim.is_some() {
            // The bars do not zoom, so a change meant for them lands in both
            // images of the animation.
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
        self.repaint_layer(region);
        true
    }

    pub fn card(&self) -> Option<&Card> {
        self.card.as_ref()
    }

    pub fn set_toast(&mut self, toast: Option<Card>) -> bool {
        if self.toast == toast {
            return false;
        }
        let scale = self.scale;
        let region = [&self.toast, &toast]
            .into_iter()
            .flatten()
            .map(|c| c.dirty_rect(scale))
            .reduce(|a, b| a.union(&b));
        self.toast = toast;
        self.repaint_layer(region);
        true
    }

    pub fn set_popup(&mut self, popup: Option<Popup>) -> bool {
        if self.popup == popup {
            return false;
        }
        let (scale, window) = (self.scale, self.window_rect());
        let region = [&self.popup, &popup]
            .into_iter()
            .flatten()
            .map(|p| p.dirty_rect(scale, window))
            .reduce(|a, b| a.union(&b));
        self.popup = popup;
        self.repaint_layer(region);
        true
    }

    pub fn popup(&self) -> Option<&Popup> {
        self.popup.as_ref()
    }

    pub fn chrome(&self) -> &Chrome {
        &self.chrome
    }

    fn window_rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    fn repaint_layer(&mut self, region: Option<Rect>) {
        if self.anim.is_none()
            && !self.full_dirty
            && let Some(region) = region
        {
            self.repaint_region(region);
        }
    }

    /// Draws the bars again in `rects` in both images of the running zoom.
    fn repaint_bars_in_animation(&mut self, rects: &[Rect]) {
        let Some(anim) = self.anim.take() else {
            return;
        };
        let style = self.style(self.text.is_ready());
        let size = (self.width, self.height);
        let mut batch = Batch::new();
        {
            let mut canvas = GpuCanvas::new(&mut batch, &mut self.atlas, size, self.text.clone());
            for &r in rects {
                let Some(r) = r.intersect(&Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32))
                else {
                    continue;
                };
                let r = r.outer_pixels().to_rect();
                crate::canvas::Canvas::push_clip(&mut canvas, r);
                crate::canvas::Canvas::fill_rect(&mut canvas, r, self.palette.bg);
                draw_chrome(&mut canvas, &self.chrome, &self.palette, &style, Some(r));
                crate::canvas::Canvas::pop_clip(&mut canvas);
            }
        }
        self.atlas.sync();
        let mut encoder = self.encoder();
        for img in [&anim.big, &anim.small] {
            let target = Target {
                view: &img.view,
                format: IMAGE_FORMAT,
                view_bind: &self.view_bind,
            };
            batch.encode(&self.gpu, &target, &self.atlas, &[], &mut encoder, None);
        }
        self.gpu.submit(encoder);
        self.anim = Some(anim);
    }

    /// Dims every file and group outside `category`, or clears the dimming.
    pub fn set_dim(&mut self, category: Option<u8>) {
        if self.dim == category {
            return;
        }
        self.dim = category;
        if self.anim.is_some() || self.full_dirty {
            self.invalidate();
            return;
        }
        let Some(region) = self.items.iter().map(|i| i.rect).reduce(|a, b| a.union(&b)) else {
            return;
        };
        self.repaint_region(region);
    }

    /// Sets how solid the title text of folders still being counted is.
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

    /// Replaces the view without animation.
    pub fn set_items(&mut self, items: Arc<Vec<Item>>) {
        self.finish_zoom();
        self.set_view(items);
        self.invalidate();
    }

    /// The GPU path records a view quickly enough to do it in the frame, so
    /// this shows the view at once and returns `false`, as the software
    /// renderer does when it cannot draw in the background.
    pub fn set_items_background(&mut self, items: Arc<Vec<Item>>) -> bool {
        self.set_items(items);
        false
    }

    pub fn background_pending(&self) -> bool {
        false
    }

    pub fn poll_background(&mut self) -> bool {
        false
    }

    fn encoder(&self) -> wgpu::CommandEncoder {
        self.gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bytescape frame"),
            })
    }

    /// Records the whole picture: map, chrome and everything drawn over it.
    fn record_scene(&mut self, items: &[Item], labels: bool) -> Batch {
        let mut batch = Batch::new();
        let style = self.style(labels);
        let size = (self.width, self.height);
        let mut canvas = GpuCanvas::new(&mut batch, &mut self.atlas, size, self.text.clone());
        draw_items(&mut canvas, items, &self.palette, &style, None);
        draw_chrome(&mut canvas, &self.chrome, &self.palette, &style, None);
        batch
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

    fn paint_overlays(
        &mut self,
        batch_into_pending: bool,
        batch: &mut Batch,
        region: Option<Rect>,
    ) {
        let overlays = self.overlay_rects();
        let style = self.style(self.labels_drawn);
        let scale = self.scale;
        let size = (self.width, self.height);
        let window = self.window_rect();
        let target = if batch_into_pending {
            &mut self.pending
        } else {
            batch
        };
        let mut canvas = GpuCanvas::new(target, &mut self.atlas, size, self.text.clone());
        if let Some(r) = region {
            crate::canvas::Canvas::push_clip(&mut canvas, r);
        }
        for (rect, hover, selected) in overlays {
            if region.is_none_or(|r| r.intersects(&rect.inflate(2.0 * scale))) {
                draw_overlay(&mut canvas, rect, &self.palette, hover, selected, scale);
            }
        }
        if let Some(card) = &self.card {
            draw_card(&mut canvas, card, &self.palette, &style, region);
        }
        if let Some(toast) = &self.toast {
            draw_card(&mut canvas, toast, &self.palette, &style, region);
        }
        if let Some(popup) = &self.popup {
            draw_popup(&mut canvas, popup, window, &self.palette, &style, region);
        }
    }

    /// Redraws only `region` of the retained picture: the items under it, then
    /// the outlines.
    fn repaint_region(&mut self, region: Rect) {
        let Some(region) = region.intersect(&self.window_rect()) else {
            return;
        };
        let region = region.outer_pixels().to_rect();
        let style = self.style(self.labels_drawn);
        let size = (self.width, self.height);
        {
            let mut canvas =
                GpuCanvas::new(&mut self.pending, &mut self.atlas, size, self.text.clone());
            crate::canvas::Canvas::push_clip(&mut canvas, region);
            crate::canvas::Canvas::fill_rect(&mut canvas, region, self.palette.bg);
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
        let mut unused = Batch::new();
        self.paint_overlays(true, &mut unused, Some(region));
    }

    fn rect_of(&self, id: Option<u32>) -> Option<Rect> {
        id.and_then(|id| self.index.get(&id))
            .and_then(|&i| self.items.get(i))
            .map(|it| it.rect)
    }

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

    /// Moves the hover outline.
    pub fn set_hover(&mut self, id: Option<u32>) {
        if self.hover == id {
            return;
        }
        if self.anim.is_some() {
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

    pub fn set_selected(&mut self, id: Option<u32>) {
        let ids: Vec<u32> = id.into_iter().collect();
        self.set_selection(&ids);
    }

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

    /// Brings the retained picture up to date inside `encoder`.
    fn update_front(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if self.front.is_none() {
            self.front = Some(self.take_image());
            self.invalidate();
        }
        let labels = self.text.is_ready();
        if self.full_dirty || (!self.labels_drawn && labels) {
            self.atlas.trim();
            self.pending.clear();
            let items = Arc::clone(&self.items);
            let mut batch = self.record_scene(&items, labels);
            self.labels_drawn = labels;
            self.full_dirty = false;
            self.paint_overlays(false, &mut batch, None);
            self.atlas.sync();
            if let Some(front) = &self.front {
                let target = Target {
                    view: &front.view,
                    format: IMAGE_FORMAT,
                    view_bind: &self.view_bind,
                };
                let clear = Some(clear_color(self.palette.bg));
                batch.encode(&self.gpu, &target, &self.atlas, &[], encoder, clear);
            }
        } else if !self.pending.is_empty() {
            self.atlas.sync();
            if let Some(front) = &self.front {
                let target = Target {
                    view: &front.view,
                    format: IMAGE_FORMAT,
                    view_bind: &self.view_bind,
                };
                self.pending
                    .encode(&self.gpu, &target, &self.atlas, &[], encoder, None);
            }
            self.pending.clear();
        }
    }

    /// Starts a zoom to `items`, the new view. `child` is the folder's box in
    /// the larger of the two views. The target view is rendered once, without
    /// labels; the animation draws the two cached images scaled and blended,
    /// and the full-quality view replaces them at the end.
    pub fn begin_zoom(
        &mut self,
        direction: ZoomDirection,
        items: Arc<Vec<Item>>,
        child: Rect,
        now: Duration,
    ) {
        self.finish_zoom();
        if self.reduced_motion {
            self.set_items(items);
            return;
        }
        let mut encoder = self.encoder();
        self.update_front(&mut encoder);
        let other = self.take_image();
        let batch = self.record_scene(&items, false);
        self.atlas.sync();
        {
            let target = Target {
                view: &other.view,
                format: IMAGE_FORMAT,
                view_bind: &self.view_bind,
            };
            let clear = Some(clear_color(self.palette.bg));
            batch.encode(&self.gpu, &target, &self.atlas, &[], &mut encoder, clear);
        }
        self.gpu.submit(encoder);
        let Some(current) = self.front.take() else {
            self.set_items(items);
            return;
        };
        let (big, small) = match direction {
            ZoomDirection::In => (current, other),
            ZoomDirection::Out => (other, current),
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
    /// runs, so the full-quality frame at the end only uploads cached labels.
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

    fn anim_time(&self, now: Duration) -> Option<f32> {
        self.anim.as_ref().map(|a| {
            (now.saturating_sub(a.start).as_secs_f32() / ZOOM_DURATION.as_secs_f32()).min(1.0)
        })
    }

    fn finish_zoom(&mut self) {
        let Some(anim) = self.anim.take() else {
            return;
        };
        self.pool.push(anim.big);
        self.pool.push(anim.small);
        self.front = Some(self.take_image());
        self.invalidate();
    }

    /// One frame of the zoom: the large image everywhere, the zoomed part of
    /// it over the map, then the small image blended on top.
    fn zoom_batch(&self, a: &Anim, t: f32) -> Batch {
        let eased = 1.0 - (1.0 - t).powi(3);
        let p = match a.direction {
            ZoomDirection::In => eased,
            ZoomDirection::Out => 1.0 - eased,
        };
        let child = Rect::new(
            a.child.x - a.region.x as f32,
            a.child.y - a.region.y as f32,
            a.child.w,
            a.child.h,
        );
        let region = a.region;
        let (rw, rh) = (region.w as f32, region.h as f32);
        let frame = zoom_geometry(rw, rh, child, p);
        let (w, h) = (self.width as f32, self.height as f32);
        let (rx, ry) = (region.x as f32, region.y as f32);
        let mut batch = Batch::new();
        // The bars keep their place: the large image, unscaled, underneath.
        batch.push(
            [0.0, 0.0, w, h],
            [0.0, 0.0, 1.0, 1.0],
            [255; 4],
            Kind::Image,
            [0.0; 2],
            Some(Bind::Image(0)),
        );
        let s = frame.big_src;
        batch.push(
            [rx, ry, rw, rh],
            [
                (rx + s.x) / w,
                (ry + s.y) / h,
                (rx + s.right()) / w,
                (ry + s.bottom()) / h,
            ],
            [255; 4],
            Kind::Image,
            [0.0; 2],
            Some(Bind::Image(0)),
        );
        let dst = frame.small_dst;
        let alpha = (frame.small_alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        let x0 = dst.x.round().clamp(0.0, rw);
        let x1 = dst.right().round().clamp(0.0, rw);
        let y0 = dst.y.round().clamp(0.0, rh);
        let y1 = dst.bottom().round().clamp(0.0, rh);
        if alpha > 0 && x1 > x0 && y1 > y0 && dst.w > 0.0 && dst.h > 0.0 {
            let u = |x: f32| (rx + (x - dst.x) / dst.w * rw) / w;
            let v = |y: f32| (ry + (y - dst.y) / dst.h * rh) / h;
            batch.push(
                [rx + x0, ry + y0, x1 - x0, y1 - y0],
                [u(x0), v(y0), u(x1), v(y1)],
                [alpha; 4],
                Kind::Image,
                [0.0; 2],
                Some(Bind::Image(1)),
            );
        }
        batch
    }

    /// Draws the current picture into `view` (a window surface texture of
    /// `format`) and reports what happened. The whole target is written every
    /// frame, so the caller never has to track which part is stale.
    pub fn paint_to(
        &mut self,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        now: Duration,
    ) -> Result<Painted, GpuError> {
        let started = Instant::now();
        self.timings = PaintTimings::default();
        let mut encoder = self.encoder();
        let full = IRect::new(0, 0, self.width as i32, self.height as i32);
        let mut animating = false;
        if let Some(t) = self.anim_time(now) {
            if t >= 1.0 {
                self.finish_zoom();
            } else {
                animating = true;
            }
        }
        if animating {
            if let (Some(a), Some(t)) = (&self.anim, self.anim_time(now)) {
                let batch = self.zoom_batch(a, t);
                let images = [&a.big.bind, &a.small.bind];
                let target = Target {
                    view,
                    format,
                    view_bind: &self.view_bind,
                };
                self.timings.render_micros = started.elapsed().as_micros() as u64;
                batch.encode(&self.gpu, &target, &self.atlas, &images, &mut encoder, None);
            }
        } else {
            self.update_front(&mut encoder);
            self.timings.render_micros = started.elapsed().as_micros() as u64;
            if let Some(front) = &self.front {
                let mut batch = Batch::new();
                batch.push(
                    [0.0, 0.0, self.width as f32, self.height as f32],
                    [0.0, 0.0, 1.0, 1.0],
                    [255; 4],
                    Kind::Image,
                    [0.0; 2],
                    Some(Bind::Image(0)),
                );
                let target = Target {
                    view,
                    format,
                    view_bind: &self.view_bind,
                };
                batch.encode(
                    &self.gpu,
                    &target,
                    &self.atlas,
                    &[&front.bind],
                    &mut encoder,
                    None,
                );
            }
        }
        self.gpu.submit(encoder);
        self.timings.copy_micros =
            (started.elapsed().as_micros() as u64).saturating_sub(self.timings.render_micros);
        if let Some(message) = self.gpu.failure() {
            return Err(GpuError::Lost(message));
        }
        Ok(Painted {
            damage: vec![full],
            animating,
        })
    }

    /// Draws the current picture into a texture of its own instead of a
    /// window; used by tests and the benchmark.
    pub fn paint_offscreen(&mut self, now: Duration) -> Result<Painted, GpuError> {
        let image = match self.offscreen.take() {
            Some(i) if i.size == (self.width, self.height) => i,
            _ => Image::new(&self.gpu, self.width, self.height),
        };
        let painted = self.paint_to(&image.view, IMAGE_FORMAT, now);
        self.offscreen = Some(image);
        painted
    }

    /// The latest offscreen frame as straight RGBA.
    pub fn read_offscreen(&self) -> Result<Vec<u8>, GpuError> {
        match &self.offscreen {
            Some(i) => read_rgba(&self.gpu, &i.texture, i.size),
            None => Err(GpuError::Lost("nothing has been painted offscreen".into())),
        }
    }

    /// Draws directly with a canvas onto a cleared picture and returns it as
    /// straight RGBA. For tests and tools that exercise the drawing calls
    /// without a scene.
    pub fn render_with(
        &mut self,
        draw: impl FnOnce(&mut GpuCanvas<'_>),
    ) -> Result<Vec<u8>, GpuError> {
        self.finish_zoom();
        let mut batch = Batch::new();
        {
            let size = (self.width, self.height);
            let mut canvas = GpuCanvas::new(&mut batch, &mut self.atlas, size, self.text.clone());
            draw(&mut canvas);
        }
        self.atlas.sync();
        let image = Image::new(&self.gpu, self.width, self.height);
        let target = Target {
            view: &image.view,
            format: IMAGE_FORMAT,
            view_bind: &self.view_bind,
        };
        let clear = Some(wgpu::Color::TRANSPARENT);
        let mut encoder = self.encoder();
        batch.encode(&self.gpu, &target, &self.atlas, &[], &mut encoder, clear);
        self.gpu.submit(encoder);
        read_rgba(&self.gpu, &image.texture, image.size)
    }

    /// The current picture, fully rendered, as straight RGBA.
    pub fn snapshot_rgba(&mut self) -> Result<Vec<u8>, GpuError> {
        self.finish_zoom();
        let mut encoder = self.encoder();
        self.update_front(&mut encoder);
        self.gpu.submit(encoder);
        let Some(front) = &self.front else {
            return Err(GpuError::Lost("no picture".into()));
        };
        let out = read_rgba(&self.gpu, &front.texture, front.size)?;
        if let Some(message) = self.gpu.failure() {
            return Err(GpuError::Lost(message));
        }
        Ok(out)
    }

    /// Writes the current picture to a PNG file.
    pub fn save_png(&mut self, path: &std::path::Path) -> Result<(), String> {
        let rgba = self.snapshot_rgba().map_err(|e| e.to_string())?;
        let size = tiny_skia::IntSize::from_wh(self.width, self.height)
            .ok_or_else(|| "empty picture".to_string())?;
        let pixmap = tiny_skia::Pixmap::from_vec(rgba, size)
            .ok_or_else(|| "bad picture size".to_string())?;
        pixmap.save_png(path).map_err(|e| e.to_string())
    }

    /// Hit test: the topmost item containing the point.
    pub fn item_at(&self, x: f32, y: f32) -> Option<&Item> {
        self.items.iter().rev().find(|it| it.rect.contains(x, y))
    }
}

/// The part of the window that zooms: the root boxes of the view, or the
/// whole window when the view has no such box.
fn zoom_region(items: &[Item], width: u32, height: u32) -> IRect {
    let full = IRect::new(0, 0, width as i32, height as i32);
    items
        .iter()
        .filter(|i| i.depth == 0)
        .map(|i| i.rect)
        .reduce(|a, b| a.union(&b))
        .and_then(|r| r.outer_pixels().intersect(&full))
        .filter(|r| r.w > 0 && r.h > 0)
        .unwrap_or(full)
}
