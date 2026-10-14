//! The software canvas: `tiny-skia` for the general cases and direct row
//! writes for opaque rectangles, which dominate a treemap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tiny_skia::{
    FillRule, FilterQuality, Mask, Paint, PathBuilder, Pattern, Pixmap, PixmapMut, PixmapPaint,
    SpreadMode, Stroke as SkStroke, Transform,
};

use crate::canvas::{Canvas, Hatch, HatchAngle, Stroke, TextStyle};
use crate::geom::{Color, Rect};
use crate::pixel;
use crate::text::TextEngine;

type TileKey = (HatchAngle, u32, u32, u32);

/// Hatch tiles shared between canvases (and between the threads of a tiled
/// repaint) so each pattern is rasterized once.
#[derive(Clone, Default)]
pub struct HatchTiles {
    tiles: Arc<Mutex<HashMap<TileKey, Arc<Pixmap>>>>,
}

impl HatchTiles {
    fn tile(&self, hatch: &Hatch) -> Option<Arc<Pixmap>> {
        let key: TileKey = (
            hatch.angle,
            hatch.spacing.to_bits(),
            hatch.line_width.to_bits(),
            u32::from_le_bytes([hatch.color.r, hatch.color.g, hatch.color.b, hatch.color.a]),
        );
        let mut map = self.tiles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = map.get(&key) {
            return Some(t.clone());
        }
        let s = hatch.spacing.round().max(2.0);
        let size = s as u32;
        let mut tile = Pixmap::new(size, size)?;
        let mut paint = Paint::default();
        paint.set_color(pixel::skia_color(hatch.color));
        paint.anti_alias = true;
        let stroke = SkStroke {
            width: hatch.line_width.max(0.5),
            ..SkStroke::default()
        };
        let transform = match hatch.angle {
            HatchAngle::Rising => Transform::identity(),
            HatchAngle::Falling => Transform::from_row(-1.0, 0.0, 0.0, 1.0, s, 0.0),
        };
        // Lines x + y = k·s for k = 0, 1, 2 cover the tile and its corners, so
        // repeating the tile leaves no seams.
        let mut pb = PathBuilder::new();
        for k in 0..3 {
            let k = k as f32 * s;
            pb.move_to(-s, k + s);
            pb.line_to(k + s, -s);
        }
        let path = pb.finish()?;
        tile.stroke_path(&path, &paint, &stroke, transform, None);
        let tile = Arc::new(tile);
        map.insert(key, tile.clone());
        Some(tile)
    }
}

/// Draws into a pixmap (or one horizontal band of a larger one).
///
/// `origin` is where the pixmap's top-left sits in canvas coordinates, so a
/// band can be drawn with the same coordinates as the whole canvas.
pub struct SkiaCanvas<'a> {
    pixmap: PixmapMut<'a>,
    origin: (i32, i32),
    canvas_size: (u32, u32),
    clips: Vec<Rect>,
    text: TextEngine,
    hatches: HatchTiles,
}

impl<'a> SkiaCanvas<'a> {
    /// A canvas covering all of `pixmap`.
    pub fn new(pixmap: PixmapMut<'a>, text: TextEngine, hatches: HatchTiles) -> Self {
        let size = (pixmap.width(), pixmap.height());
        Self::band(pixmap, (0, 0), size, text, hatches)
    }

    /// A canvas for a band of a `canvas_size` target, whose top-left is at
    /// `origin` in canvas coordinates.
    pub fn band(
        pixmap: PixmapMut<'a>,
        origin: (i32, i32),
        canvas_size: (u32, u32),
        text: TextEngine,
        hatches: HatchTiles,
    ) -> Self {
        let bounds = Rect::new(
            origin.0 as f32,
            origin.1 as f32,
            pixmap.width() as f32,
            pixmap.height() as f32,
        );
        Self {
            pixmap,
            origin,
            canvas_size,
            clips: vec![bounds],
            text,
            hatches,
        }
    }

    fn clip(&self) -> Rect {
        self.clips.last().copied().unwrap_or_default()
    }

    /// The part of `rect` that is inside the clip, in whole device pixels,
    /// in pixmap-local coordinates.
    fn local_pixels(&self, rect: Rect) -> Option<(usize, usize, usize, usize)> {
        let r = rect.intersect(&self.clip())?;
        let x0 = (r.x.round() as i32 - self.origin.0).max(0) as usize;
        let y0 = (r.y.round() as i32 - self.origin.1).max(0) as usize;
        let x1 = ((r.right().round() as i32 - self.origin.0).max(0) as usize)
            .min(self.pixmap.width() as usize);
        let y1 = ((r.bottom().round() as i32 - self.origin.1).max(0) as usize)
            .min(self.pixmap.height() as usize);
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
    }

    fn fill_opaque(&mut self, rect: Rect, color: Color) {
        let Some((x0, y0, x1, y1)) = self.local_pixels(rect) else {
            return;
        };
        let stride = self.pixmap.width() as usize * 4;
        let px = pixel::word(color).to_le_bytes();
        let data = self.pixmap.data_mut();
        // Fill the first row, then copy it down: a memcpy per row is much
        // faster than writing every pixel of a large box.
        let first = y0 * stride + x0 * 4..y0 * stride + x1 * 4;
        for p in data[first.clone()].as_chunks_mut::<4>().0 {
            *p = px;
        }
        for y in y0 + 1..y1 {
            data.copy_within(first.clone(), y * stride + x0 * 4);
        }
    }

    /// A mask for the current clip, needed only when a draw can spill out of
    /// it and the cheap paths do not apply.
    fn clip_mask(&self, bounds: Rect) -> Option<Mask> {
        let clip = self.clip();
        let inside = bounds.x >= clip.x
            && bounds.y >= clip.y
            && bounds.right() <= clip.right()
            && bounds.bottom() <= clip.bottom();
        if inside {
            return None;
        }
        let mut mask = Mask::new(self.pixmap.width(), self.pixmap.height())?;
        let local = tiny_skia::Rect::from_xywh(
            clip.x - self.origin.0 as f32,
            clip.y - self.origin.1 as f32,
            clip.w.max(0.1),
            clip.h.max(0.1),
        )?;
        mask.fill_path(
            &PathBuilder::from_rect(local),
            FillRule::Winding,
            false,
            Transform::identity(),
        );
        Some(mask)
    }

    /// Opaque pixels of the canvas, for export. Valid only for a canvas that
    /// covers a whole pixmap.
    pub fn pixmap(&self) -> &PixmapMut<'a> {
        &self.pixmap
    }
}

impl Canvas for SkiaCanvas<'_> {
    fn size(&self) -> (u32, u32) {
        self.canvas_size
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        if color.a == 255 {
            self.fill_opaque(rect, color);
            return;
        }
        if color.a == 0 {
            return;
        }
        let Some((x0, y0, x1, y1)) = self.local_pixels(rect) else {
            return;
        };
        let Some(r) = tiny_skia::Rect::from_ltrb(x0 as f32, y0 as f32, x1 as f32, y1 as f32) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(pixel::skia_color(color));
        paint.anti_alias = false;
        self.pixmap
            .fill_rect(r, &paint, Transform::identity(), None);
    }

    fn stroke_rect(&mut self, rect: Rect, stroke: Stroke) {
        let w = stroke.width.round().max(1.0);
        if rect.w < 1.0 || rect.h < 1.0 {
            return;
        }
        let c = stroke.color;
        match stroke.dash {
            None => {
                let h = w.min(rect.h);
                let v = w.min(rect.w);
                self.fill_rect(Rect::new(rect.x, rect.y, rect.w, h), c);
                self.fill_rect(Rect::new(rect.x, rect.bottom() - h, rect.w, h), c);
                let inner = (rect.h - 2.0 * h).max(0.0);
                self.fill_rect(Rect::new(rect.x, rect.y + h, v, inner), c);
                self.fill_rect(Rect::new(rect.right() - v, rect.y + h, v, inner), c);
            }
            Some((dash, gap)) => {
                let (dash, gap) = (dash.max(1.0), gap.max(1.0));
                let mut x = rect.x;
                while x < rect.right() {
                    let len = dash.min(rect.right() - x);
                    self.fill_rect(Rect::new(x, rect.y, len, w), c);
                    self.fill_rect(Rect::new(x, rect.bottom() - w, len, w), c);
                    x += dash + gap;
                }
                let mut y = rect.y;
                while y < rect.bottom() {
                    let len = dash.min(rect.bottom() - y);
                    self.fill_rect(Rect::new(rect.x, y, w, len), c);
                    self.fill_rect(Rect::new(rect.right() - w, y, w, len), c);
                    y += dash + gap;
                }
            }
        }
    }

    fn fill_pattern(&mut self, rect: Rect, hatch: Hatch) {
        let Some((x0, y0, x1, y1)) = self.local_pixels(rect) else {
            return;
        };
        let Some(tile) = self.hatches.tile(&hatch) else {
            return;
        };
        let Some(r) = tiny_skia::Rect::from_ltrb(x0 as f32, y0 as f32, x1 as f32, y1 as f32) else {
            return;
        };
        // The pattern is anchored to canvas coordinates, so neighbouring
        // bands and repeated repaints line up.
        let shader = Pattern::new(
            tile.as_ref().as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Nearest,
            1.0,
            Transform::from_translate(-(self.origin.0 as f32), -(self.origin.1 as f32)),
        );
        let paint = Paint {
            shader,
            anti_alias: false,
            ..Paint::default()
        };
        self.pixmap
            .fill_rect(r, &paint, Transform::identity(), None);
    }

    fn measure_text(&mut self, text: &str, size: f32, scale: f32) -> Option<f32> {
        self.text.measure(text, size, scale)
    }

    fn draw_text(&mut self, text: &str, style: TextStyle, x: f32, y: f32, max_width: f32) {
        let Some(label) = self.text.label(text, style, max_width) else {
            return;
        };
        let lx = x.round() as i32 + label.origin.0;
        let ly = y.round() as i32 + label.origin.1;
        let bounds = Rect::new(
            lx as f32,
            ly as f32,
            label.pixmap.width() as f32,
            label.pixmap.height() as f32,
        );
        if !bounds.intersects(&self.clip()) {
            return;
        }
        let mask = self.clip_mask(bounds);
        self.pixmap.draw_pixmap(
            lx - self.origin.0,
            ly - self.origin.1,
            label.pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            mask.as_ref(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_over(pm: &mut Pixmap) -> SkiaCanvas<'_> {
        SkiaCanvas::new(pm.as_mut(), TextEngine::new(), HatchTiles::default())
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pm.width() + x) * 4) as usize;
        [
            pm.data()[i],
            pm.data()[i + 1],
            pm.data()[i + 2],
            pm.data()[i + 3],
        ]
    }

    #[test]
    fn opaque_fill_writes_native_words() {
        let mut pm = Pixmap::new(8, 8).expect("pixmap");
        let mut c = canvas_over(&mut pm);
        c.fill_rect(Rect::new(2.0, 2.0, 4.0, 4.0), Color::hex(0x336699));
        assert_eq!(u32::from_le_bytes(px(&pm, 3, 3)), 0xFF33_6699);
        assert_eq!(px(&pm, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn clip_restricts_fills() {
        let mut pm = Pixmap::new(10, 10).expect("pixmap");
        let mut c = canvas_over(&mut pm);
        c.push_clip(Rect::new(0.0, 0.0, 5.0, 5.0));
        c.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::rgb(255, 0, 0));
        c.pop_clip();
        assert_ne!(px(&pm, 4, 4)[3], 0);
        assert_eq!(px(&pm, 6, 6)[3], 0);
    }

    #[test]
    fn band_draws_with_canvas_coordinates() {
        let mut pm = Pixmap::new(10, 4).expect("pixmap");
        let mut c = SkiaCanvas::band(
            pm.as_mut(),
            (0, 20),
            (10, 100),
            TextEngine::new(),
            HatchTiles::default(),
        );
        c.fill_rect(Rect::new(0.0, 21.0, 10.0, 2.0), Color::rgb(0, 255, 0));
        assert_eq!(px(&pm, 5, 0)[3], 0);
        assert_ne!(px(&pm, 5, 1)[3], 0);
        assert_ne!(px(&pm, 5, 2)[3], 0);
        assert_eq!(px(&pm, 5, 3)[3], 0);
    }

    #[test]
    fn hatch_draws_lines_in_both_directions() {
        for angle in [HatchAngle::Rising, HatchAngle::Falling] {
            let mut pm = Pixmap::new(32, 32).expect("pixmap");
            let mut c = canvas_over(&mut pm);
            c.fill_rect(Rect::new(0.0, 0.0, 32.0, 32.0), Color::rgb(255, 255, 255));
            c.fill_pattern(
                Rect::new(0.0, 0.0, 32.0, 32.0),
                Hatch {
                    angle,
                    spacing: 8.0,
                    line_width: 2.0,
                    color: Color::rgb(0, 0, 0),
                },
            );
            let dark = pm
                .data()
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] < 128)
                .count();
            assert!(dark > 100 && dark < 900, "{angle:?}: {dark} dark pixels");
        }
    }

    #[test]
    fn hatch_directions_differ() {
        let render = |angle| {
            let mut pm = Pixmap::new(16, 16).expect("pixmap");
            let mut c = canvas_over(&mut pm);
            c.fill_pattern(
                Rect::new(0.0, 0.0, 16.0, 16.0),
                Hatch {
                    angle,
                    spacing: 8.0,
                    line_width: 2.0,
                    color: Color::rgb(0, 0, 0),
                },
            );
            pm.data().to_vec()
        };
        assert_ne!(render(HatchAngle::Rising), render(HatchAngle::Falling));
    }
}
