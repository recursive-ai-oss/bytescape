//! The GPU implementation of [`Canvas`]: every call turns into rectangles in a
//! [`Batch`], clipped and snapped on the CPU exactly as the software canvas
//! does, so the two backends draw the same pixels.

use crate::canvas::{Canvas, Hatch, HatchAngle, Stroke, TextStyle, stroke_with_fills};
use crate::geom::{Color, Rect};
use crate::text::TextEngine;

use super::atlas::Atlas;
use super::batch::{Batch, Bind, Kind};

/// Records drawing calls into a batch.
pub struct GpuCanvas<'a> {
    batch: &'a mut Batch,
    atlas: &'a mut Atlas,
    size: (u32, u32),
    clips: Vec<Rect>,
    text: TextEngine,
}

/// A color as premultiplied bytes in red, green, blue, alpha order.
pub(crate) fn premultiplied(c: Color) -> [u8; 4] {
    let a = c.a as u32;
    let p = |v: u8| ((v as u32 * a + 127) / 255) as u8;
    [p(c.r), p(c.g), p(c.b), c.a]
}

impl<'a> GpuCanvas<'a> {
    pub(crate) fn new(
        batch: &'a mut Batch,
        atlas: &'a mut Atlas,
        size: (u32, u32),
        text: TextEngine,
    ) -> Self {
        let bounds = Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32);
        Self {
            batch,
            atlas,
            size,
            clips: vec![bounds],
            text,
        }
    }

    fn clip(&self) -> Rect {
        self.clips.last().copied().unwrap_or_default()
    }

    /// The part of `rect` inside the clip, in whole device pixels.
    fn pixels(&self, rect: Rect) -> Option<Rect> {
        let r = rect.intersect(&self.clip())?;
        let (x0, y0) = (r.x.round(), r.y.round());
        let (x1, y1) = (r.right().round(), r.bottom().round());
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

impl Canvas for GpuCanvas<'_> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        if color.a == 0 {
            return;
        }
        let Some(r) = self.pixels(rect) else {
            return;
        };
        self.batch.push(
            [r.x, r.y, r.w, r.h],
            [0.0; 4],
            premultiplied(color),
            Kind::Solid,
            [0.0; 2],
            None,
        );
    }

    fn stroke_rect(&mut self, rect: Rect, stroke: Stroke) {
        stroke_with_fills(self, rect, stroke);
    }

    fn fill_pattern(&mut self, rect: Rect, hatch: Hatch) {
        let Some(r) = self.pixels(rect) else {
            return;
        };
        let kind = match hatch.angle {
            HatchAngle::Rising => Kind::HatchRising,
            HatchAngle::Falling => Kind::HatchFalling,
        };
        // The software backend rasterizes a whole-pixel tile; match its period.
        let spacing = hatch.spacing.round().max(2.0);
        self.batch.push(
            [r.x, r.y, r.w, r.h],
            [0.0; 4],
            premultiplied(hatch.color),
            kind,
            [spacing, hatch.line_width.max(0.5)],
            None,
        );
    }

    fn measure_text(&mut self, text: &str, size: f32, scale: f32) -> Option<f32> {
        self.text.measure(text, size, scale)
    }

    fn draw_text(&mut self, text: &str, style: TextStyle, x: f32, y: f32, max_width: f32) {
        let Some(label) = self.text.label(text, style, max_width) else {
            return;
        };
        let Some(slot) = self.atlas.slot(&label) else {
            return;
        };
        let lx = x.round() + label.origin.0 as f32;
        let ly = y.round() + label.origin.1 as f32;
        let full = Rect::new(lx, ly, slot.w as f32, slot.h as f32);
        let clip = self.clip();
        let (cx0, cy0) = (clip.x.round(), clip.y.round());
        let (cx1, cy1) = (clip.right().round(), clip.bottom().round());
        let (x0, y0) = (lx.max(cx0), ly.max(cy0));
        let (x1, y1) = (full.right().min(cx1), full.bottom().min(cy1));
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        // Texture coordinates of the visible part.
        let (aw, ah) = (super::ATLAS_W as f32, super::ATLAS_H as f32);
        let u0 = (slot.x as f32 + (x0 - lx)) / aw;
        let v0 = (slot.y as f32 + (y0 - ly)) / ah;
        let u1 = (slot.x as f32 + (x1 - lx)) / aw;
        let v1 = (slot.y as f32 + (y1 - ly)) / ah;
        self.batch.push(
            [x0, y0, x1 - x0, y1 - y0],
            [u0, v0, u1, v1],
            [255; 4],
            Kind::Image,
            [0.0; 2],
            Some(Bind::Atlas(slot.page)),
        );
    }

    fn push_clip(&mut self, rect: Rect) {
        let next = self.clip().intersect(&rect).unwrap_or_default();
        self.clips.push(next);
    }

    fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }
}
