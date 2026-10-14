//! Window chrome as a flat list of widgets: buttons, text, legend swatches and
//! the capacity bar. The UI crate decides where each one sits; this module
//! only draws them, and tells the renderer which rectangles changed between
//! two lists so that a new info-bar text repaints the info bar and nothing
//! else.

use crate::canvas::{Canvas, Hatch, HatchAngle, Stroke, TextStyle};
use crate::geom::{Color, Rect};
use crate::scene::{Bar, Palette, SceneStyle, draw_bars};

/// The picture on a navigation button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Back,
    Forward,
    Up,
    Refresh,
}

impl Glyph {
    fn text(self) -> &'static str {
        match self {
            Glyph::Back => "←",
            Glyph::Forward => "→",
            Glyph::Up => "↑",
            Glyph::Refresh => "↻",
        }
    }
}

/// How a piece of chrome text looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    Primary,
    Secondary,
    /// A link: secondary text that turns accent-colored and underlined when
    /// the pointer is over it.
    Link,
}

/// One part of the capacity bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    Used,
    RecycleBin,
    Unreadable,
    Reserved,
    Free,
}

/// A drawable piece of chrome.
#[derive(Clone, Debug, PartialEq)]
pub enum Widget {
    Button {
        rect: Rect,
        glyph: Glyph,
        enabled: bool,
        hot: bool,
    },
    Text {
        rect: Rect,
        text: Box<str>,
        role: TextRole,
        hot: bool,
    },
    /// A legend swatch, with the category's name beside it when there is room.
    Swatch {
        rect: Rect,
        category: u8,
        name: Option<Box<str>>,
        hot: bool,
    },
    /// Stands in for the swatches that did not fit.
    Overflow { rect: Rect, hot: bool },
    /// Parts of the capacity bar as fractions of the whole volume, in the
    /// order they are drawn from the left.
    Capacity {
        rect: Rect,
        parts: Vec<(SegmentKind, f32)>,
    },
}

impl Widget {
    /// The widget's rectangle in device pixels.
    pub fn rect(&self) -> Rect {
        match self {
            Widget::Button { rect, .. }
            | Widget::Text { rect, .. }
            | Widget::Swatch { rect, .. }
            | Widget::Overflow { rect, .. }
            | Widget::Capacity { rect, .. } => *rect,
        }
    }
}

/// Everything the renderer draws around the map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chrome {
    pub bars: Vec<Bar>,
    pub widgets: Vec<Widget>,
}

impl Chrome {
    /// The rectangles that differ between `self` and `newer`, or `None` when
    /// the two lists have a different shape and everything must repaint.
    pub fn changed_regions(&self, newer: &Chrome) -> Option<Vec<Rect>> {
        if self.bars.len() != newer.bars.len() || self.widgets.len() != newer.widgets.len() {
            return None;
        }
        let mut out = Vec::new();
        for (a, b) in self.bars.iter().zip(&newer.bars) {
            if a != b {
                out.push(a.rect.union(&b.rect));
            }
        }
        for (a, b) in self.widgets.iter().zip(&newer.widgets) {
            if a != b {
                out.push(a.rect().union(&b.rect()));
            }
        }
        Some(out)
    }
}

/// The colors of the capacity bar parts.
pub fn segment_color(kind: SegmentKind, pal: &Palette) -> Color {
    match kind {
        SegmentKind::Used => pal.text_secondary.mix(pal.bg, 0.35),
        SegmentKind::RecycleBin => pal.text_secondary.mix(pal.bg, 0.62),
        SegmentKind::Unreadable => pal.unreadable,
        SegmentKind::Reserved => pal.title[2],
        SegmentKind::Free => pal.bar,
    }
}

/// Paints the bars and widgets that intersect `region`.
pub fn draw_chrome<C: Canvas>(
    canvas: &mut C,
    chrome: &Chrome,
    pal: &Palette,
    st: &SceneStyle,
    region: Option<Rect>,
) {
    draw_bars(canvas, &chrome.bars, pal, st, region);
    for w in &chrome.widgets {
        if let Some(region) = region
            && !w.rect().intersects(&region)
        {
            continue;
        }
        draw_widget(canvas, w, pal, st);
    }
}

fn text_y(r: Rect, st: &SceneStyle) -> f32 {
    r.y + ((r.h - st.em * 1.25) / 2.0).round().max(0.0)
}

fn style(st: &SceneStyle, color: Color) -> TextStyle {
    TextStyle {
        size: st.em / st.scale,
        scale: st.scale,
        color,
    }
}

fn draw_widget<C: Canvas>(canvas: &mut C, w: &Widget, pal: &Palette, st: &SceneStyle) {
    match w {
        Widget::Button {
            rect,
            glyph,
            enabled,
            hot,
        } => {
            let r = rect.snapped();
            if *hot && *enabled {
                canvas.fill_rect(r, pal.title[1]);
            }
            if !st.labels {
                return;
            }
            let color = if *enabled {
                pal.text
            } else {
                pal.text_secondary.with_opacity(0.45)
            };
            let text = glyph.text();
            let tw = canvas
                .measure_text(text, st.em / st.scale, st.scale)
                .unwrap_or(st.em);
            canvas.draw_text(
                text,
                style(st, color),
                r.x + ((r.w - tw) / 2.0).round(),
                text_y(r, st),
                tw + 2.0,
            );
        }
        Widget::Text {
            rect,
            text,
            role,
            hot,
        } => {
            if !st.labels {
                return;
            }
            let r = rect.snapped();
            let color = match role {
                TextRole::Primary => pal.text,
                TextRole::Secondary => pal.text_secondary,
                TextRole::Link if *hot => pal.accent,
                TextRole::Link => pal.text_secondary,
            };
            let ty = text_y(r, st);
            canvas.push_clip(r);
            canvas.draw_text(text, style(st, color), r.x, ty, r.w.max(1.0));
            if *role == TextRole::Link && *hot {
                let tw = canvas
                    .measure_text(text, st.em / st.scale, st.scale)
                    .unwrap_or(r.w)
                    .min(r.w);
                let uy = (ty + st.em * 1.15).round();
                canvas.fill_rect(
                    Rect::new(r.x, uy, tw.round(), st.scale.round().max(1.0)),
                    pal.accent,
                );
            }
            canvas.pop_clip();
        }
        Widget::Swatch {
            rect,
            category,
            name,
            hot,
        } => {
            let r = rect.snapped();
            let side = (st.em * 0.9).round().max(6.0);
            let sq = Rect::new(r.x, r.y + ((r.h - side) / 2.0).round(), side, side);
            let fill = pal.category_fill(*category);
            canvas.fill_rect(sq, fill.mix(pal.text, 0.35));
            canvas.fill_rect(
                Rect::new(sq.x + 1.0, sq.y + 1.0, sq.w - 2.0, sq.h - 2.0),
                fill,
            );
            if *hot {
                let o = (2.0 * st.scale).round().max(1.0);
                canvas.stroke_rect(
                    Rect::new(sq.x - o, sq.y - o, sq.w + 2.0 * o, sq.h + 2.0 * o),
                    Stroke::solid(o, pal.accent),
                );
            }
            if let Some(name) = name
                && st.labels
            {
                let x = sq.right() + (st.em * 0.4).round();
                let w = (r.right() - x).max(1.0);
                canvas.push_clip(r);
                canvas.draw_text(name, style(st, pal.text_secondary), x, text_y(r, st), w);
                canvas.pop_clip();
            }
        }
        Widget::Overflow { rect, hot } => {
            if !st.labels {
                return;
            }
            let r = rect.snapped();
            if *hot {
                canvas.fill_rect(r, pal.title[1]);
            }
            let text = "…";
            let tw = canvas
                .measure_text(text, st.em / st.scale, st.scale)
                .unwrap_or(st.em);
            canvas.draw_text(
                text,
                style(st, pal.text_secondary),
                r.x + ((r.w - tw) / 2.0).round(),
                text_y(r, st),
                tw + 2.0,
            );
        }
        Widget::Capacity { rect, parts } => draw_capacity(canvas, *rect, parts, pal, st),
    }
}

fn draw_capacity<C: Canvas>(
    canvas: &mut C,
    rect: Rect,
    parts: &[(SegmentKind, f32)],
    pal: &Palette,
    st: &SceneStyle,
) {
    let r = rect.snapped();
    if r.w < 2.0 || r.h < 2.0 {
        return;
    }
    canvas.fill_rect(r, pal.border);
    let inner = Rect::new(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0);
    canvas.fill_rect(inner, pal.bar);
    let mut edge = 0.0f32;
    for (kind, frac) in parts {
        let from = inner.x + (inner.w * edge.clamp(0.0, 1.0)).round();
        edge += frac.max(0.0);
        let to = inner.x + (inner.w * edge.clamp(0.0, 1.0)).round();
        if to - from < 1.0 {
            continue;
        }
        let seg = Rect::new(from, inner.y, to - from, inner.h);
        match kind {
            SegmentKind::Free => {}
            SegmentKind::Unreadable => {
                canvas.fill_rect(seg, pal.bg);
                canvas.fill_pattern(
                    seg,
                    Hatch {
                        angle: HatchAngle::Rising,
                        spacing: (5.0 * st.scale).round().max(3.0),
                        line_width: (2.0 * st.scale).max(1.0),
                        color: pal.unreadable.with_opacity(0.9),
                    },
                );
            }
            _ => canvas.fill_rect(seg, segment_color(*kind, pal)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::RecordingCanvas;

    fn text(rect: Rect, s: &str) -> Widget {
        Widget::Text {
            rect,
            text: s.into(),
            role: TextRole::Primary,
            hot: false,
        }
    }

    #[test]
    fn only_widgets_that_differ_are_reported() {
        let a = Chrome {
            bars: Vec::new(),
            widgets: vec![
                text(Rect::new(0.0, 0.0, 10.0, 10.0), "a"),
                text(Rect::new(20.0, 0.0, 10.0, 10.0), "b"),
            ],
        };
        let mut b = a.clone();
        b.widgets[1] = text(Rect::new(20.0, 0.0, 10.0, 10.0), "c");
        assert_eq!(
            a.changed_regions(&b),
            Some(vec![Rect::new(20.0, 0.0, 10.0, 10.0)])
        );
        assert_eq!(a.changed_regions(&a), Some(Vec::new()));
        b.widgets.pop();
        assert_eq!(a.changed_regions(&b), None);
    }

    #[test]
    fn capacity_parts_are_drawn_in_order_and_fill_the_bar() {
        let pal = Palette::light();
        let st = SceneStyle::new(1.0, 13.0, false);
        let w = Widget::Capacity {
            rect: Rect::new(0.0, 0.0, 102.0, 10.0),
            parts: vec![
                (SegmentKind::Used, 0.5),
                (SegmentKind::Unreadable, 0.2),
                (SegmentKind::Free, 0.3),
            ],
        };
        let mut c = RecordingCanvas::new(200, 20);
        draw_widget(&mut c, &w, &pal, &st);
        let fills: Vec<_> = c.fills().collect();
        // Track border, track body, used part, unreadable backdrop.
        assert_eq!(fills.len(), 4);
        assert_eq!(*fills[2].1, segment_color(SegmentKind::Used, &pal));
        assert_eq!(fills[2].0.x, 1.0);
        assert_eq!(fills[2].0.w, 50.0);
        assert!(
            c.ops
                .iter()
                .any(|o| matches!(o, crate::canvas::Op::Pattern(..)))
        );
    }

    #[test]
    fn a_link_underlines_only_when_hot() {
        let pal = Palette::light();
        let st = SceneStyle::new(1.0, 13.0, true);
        let mk = |hot| Widget::Text {
            rect: Rect::new(0.0, 0.0, 200.0, 24.0),
            text: "link".into(),
            role: TextRole::Link,
            hot,
        };
        let mut cold = RecordingCanvas::new(300, 30);
        draw_widget(&mut cold, &mk(false), &pal, &st);
        let mut hot = RecordingCanvas::new(300, 30);
        draw_widget(&mut hot, &mk(true), &pal, &st);
        assert_eq!(cold.fills().count(), 0);
        assert_eq!(hot.fills().filter(|(_, c)| **c == pal.accent).count(), 1);
    }
}
