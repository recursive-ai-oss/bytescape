//! The narrow drawing interface the user interface draws through.
//!
//! # Backends
//!
//! Views, widgets and the item painter are generic over [`Canvas`] and never
//! name a pixel format or a rasterizer. Two backends implement it:
//!
//! * [`crate::skia::SkiaCanvas`] draws on the CPU with `tiny-skia`.
//! * [`crate::gpu::GpuCanvas`] records rectangles for one GPU pipeline: fills,
//!   strokes and hatches become instanced rectangles (the hatch is computed in
//!   the shader), and `draw_text` and `measure_text` use the same label cache
//!   as the software backend, with the label images packed into atlas textures.
//!   Clips are applied on the CPU by cropping each rectangle, so both backends
//!   round and clip alike.
//!
//! `Canvas` covers drawing only. What happens around it belongs to each
//! backend's renderer: the retained picture, dirty regions, the zoom that
//! scales and blends two cached images, and presenting. The software versions
//! are [`crate::Renderer`] and [`crate::present`]; the GPU versions are
//! [`crate::gpu::GpuRenderer`] and [`crate::gpu::GpuPresenter`], which take the
//! same calls (`set_items`, `set_hover`, `set_selected`, `begin_zoom` and so
//! on) and keep the zoom textures on the GPU, so nothing in `bs-ui` changes.
//! Only the last step differs: the software renderer fills a pixel buffer in
//! `paint`, and the GPU renderer draws into a texture in `paint_to`.
//!
//! All coordinates are device pixels. Callers snap rectangles to whole pixels
//! when they want crisp edges; backends do not re-snap.

use crate::geom::{Color, Rect};

/// Direction of the diagonal lines in a hatch. Angles are measured as they
/// appear on screen: `Rising` runs from bottom-left to top-right (45°),
/// `Falling` from top-left to bottom-right (−45°).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HatchAngle {
    Rising,
    Falling,
}

/// A repeating diagonal line pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hatch {
    pub angle: HatchAngle,
    /// Distance between neighbouring lines, measured along an axis.
    pub spacing: f32,
    pub line_width: f32,
    pub color: Color,
}

/// A rectangle outline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub color: Color,
    /// `(dash, gap)` lengths for a dashed outline.
    pub dash: Option<(f32, f32)>,
}

impl Stroke {
    pub const fn solid(width: f32, color: Color) -> Self {
        Self {
            width,
            color,
            dash: None,
        }
    }
}

/// How a run of text looks. `size` is logical; the backend multiplies by
/// `scale`, so one cache entry is keyed by both.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub scale: f32,
    pub color: Color,
}

/// The drawing interface.
pub trait Canvas {
    /// Target size in device pixels.
    fn size(&self) -> (u32, u32);

    fn fill_rect(&mut self, rect: Rect, color: Color);

    /// Outlines `rect` inside its own edges.
    fn stroke_rect(&mut self, rect: Rect, stroke: Stroke);

    /// Fills `rect` with a repeating diagonal hatch over whatever is below.
    fn fill_pattern(&mut self, rect: Rect, hatch: Hatch);

    /// Width of `text` on one line, in device pixels. Returns `None` while the
    /// font system is still loading.
    fn measure_text(&mut self, text: &str, size: f32, scale: f32) -> Option<f32>;

    /// Draws one line of text with its top-left at `(x, y)`. Text wider than
    /// `max_width` is shortened with an ellipsis in the middle, measured by the
    /// shaper. Draws nothing while the font system is still loading.
    fn draw_text(&mut self, text: &str, style: TextStyle, x: f32, y: f32, max_width: f32);

    /// Restricts drawing to the intersection of `rect` and the current clip.
    fn push_clip(&mut self, rect: Rect);

    fn pop_clip(&mut self);
}

/// Draws an outline as thin filled rectangles, solid or dashed. Backends that
/// fill rectangles on whole pixels share it, so strokes look the same on each.
pub(crate) fn stroke_with_fills<C: Canvas + ?Sized>(c: &mut C, rect: Rect, stroke: Stroke) {
    let w = stroke.width.round().max(1.0);
    if rect.w < 1.0 || rect.h < 1.0 {
        return;
    }
    let color = stroke.color;
    match stroke.dash {
        None => {
            let h = w.min(rect.h);
            let v = w.min(rect.w);
            c.fill_rect(Rect::new(rect.x, rect.y, rect.w, h), color);
            c.fill_rect(Rect::new(rect.x, rect.bottom() - h, rect.w, h), color);
            let inner = (rect.h - 2.0 * h).max(0.0);
            c.fill_rect(Rect::new(rect.x, rect.y + h, v, inner), color);
            c.fill_rect(Rect::new(rect.right() - v, rect.y + h, v, inner), color);
        }
        Some((dash, gap)) => {
            let (dash, gap) = (dash.max(1.0), gap.max(1.0));
            let mut x = rect.x;
            while x < rect.right() {
                let len = dash.min(rect.right() - x);
                c.fill_rect(Rect::new(x, rect.y, len, w), color);
                c.fill_rect(Rect::new(x, rect.bottom() - w, len, w), color);
                x += dash + gap;
            }
            let mut y = rect.y;
            while y < rect.bottom() {
                let len = dash.min(rect.bottom() - y);
                c.fill_rect(Rect::new(rect.x, y, w, len), color);
                c.fill_rect(Rect::new(rect.right() - w, y, w, len), color);
                y += dash + gap;
            }
        }
    }
}

/// One recorded drawing operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Fill(Rect, Color),
    Stroke(Rect, Stroke),
    Pattern(Rect, Hatch),
    Text {
        text: String,
        style: TextStyle,
        x: f32,
        y: f32,
        max_width: f32,
    },
    PushClip(Rect),
    PopClip,
}

/// A canvas that only remembers what it was asked to draw, so tests can check
/// kinds, rectangles and labels without comparing pixels.
#[derive(Debug, Default)]
pub struct RecordingCanvas {
    size: (u32, u32),
    pub ops: Vec<Op>,
    /// Pretend text is this wide per character (device px per `size` unit).
    pub char_width: f32,
}

impl RecordingCanvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            size: (w, h),
            ops: Vec::new(),
            char_width: 0.5,
        }
    }

    pub fn fills(&self) -> impl Iterator<Item = (&Rect, &Color)> {
        self.ops.iter().filter_map(|o| match o {
            Op::Fill(r, c) => Some((r, c)),
            _ => None,
        })
    }

    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.ops.iter().filter_map(|o| match o {
            Op::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }
}

impl Canvas for RecordingCanvas {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.ops.push(Op::Fill(rect, color));
    }

    fn stroke_rect(&mut self, rect: Rect, stroke: Stroke) {
        self.ops.push(Op::Stroke(rect, stroke));
    }

    fn fill_pattern(&mut self, rect: Rect, hatch: Hatch) {
        self.ops.push(Op::Pattern(rect, hatch));
    }

    fn measure_text(&mut self, text: &str, size: f32, scale: f32) -> Option<f32> {
        Some(text.chars().count() as f32 * size * scale * self.char_width)
    }

    fn draw_text(&mut self, text: &str, style: TextStyle, x: f32, y: f32, max_width: f32) {
        self.ops.push(Op::Text {
            text: text.to_owned(),
            style,
            x,
            y,
            max_width,
        });
    }

    fn push_clip(&mut self, rect: Rect) {
        self.ops.push(Op::PushClip(rect));
    }

    fn pop_clip(&mut self) {
        self.ops.push(Op::PopClip);
    }
}
