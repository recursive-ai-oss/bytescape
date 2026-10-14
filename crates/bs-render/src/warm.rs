//! A canvas that draws nothing but asks for every label, to fill the label
//! cache ahead of the frame that needs it.

use crate::canvas::{Canvas, Hatch, Stroke, TextStyle};
use crate::geom::{Color, Rect};
use crate::text::TextEngine;

pub struct WarmCanvas {
    text: TextEngine,
}

impl WarmCanvas {
    pub fn new(text: TextEngine) -> Self {
        Self { text }
    }
}

impl Canvas for WarmCanvas {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }

    fn fill_rect(&mut self, _rect: Rect, _color: Color) {}

    fn stroke_rect(&mut self, _rect: Rect, _stroke: Stroke) {}

    fn fill_pattern(&mut self, _rect: Rect, _hatch: Hatch) {}

    fn measure_text(&mut self, text: &str, size: f32, scale: f32) -> Option<f32> {
        self.text.measure(text, size, scale)
    }

    fn draw_text(&mut self, text: &str, style: TextStyle, _x: f32, _y: f32, max_width: f32) {
        let _ = self.text.label(text, style, max_width);
    }

    fn push_clip(&mut self, _rect: Rect) {}

    fn pop_clip(&mut self) {}
}
