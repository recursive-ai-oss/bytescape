//! The hover card: a small panel with the facts about one box. The UI crate
//! decides what it says and where every line sits; this module only draws it,
//! and gives the renderer the rectangle to repaint when it appears, moves or
//! goes away.

use crate::canvas::{Canvas, Stroke, TextStyle};
use crate::geom::Rect;
use crate::scene::{Palette, SceneStyle};

/// How a line of the card reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardRole {
    /// The box's name.
    Title,
    /// A fact.
    Normal,
    /// A secondary fact or a reason.
    Secondary,
}

/// One piece of the card, positioned in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum CardPart {
    /// A line of text.
    Line {
        rect: Rect,
        text: Box<str>,
        role: CardRole,
    },
    /// The path: click to copy. `copied` swaps in the confirmation.
    Path {
        rect: Rect,
        text: Box<str>,
        hot: bool,
        copied: bool,
    },
    /// An action button.
    Button {
        rect: Rect,
        label: Box<str>,
        enabled: bool,
        hot: bool,
    },
    /// A thin rule between groups of lines.
    Rule { rect: Rect },
}

/// The hover card.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    /// The whole panel.
    pub rect: Rect,
    pub parts: Vec<CardPart>,
}

/// The word shown beside the path after it was copied.
pub const COPIED_TEXT: &str = "Copied";

/// How far the panel's soft edge reaches past its rectangle, in device pixels
/// at scale 1. The renderer repaints this much more than the panel.
pub const CARD_EDGE: f32 = 2.0;

impl Card {
    /// The rectangle to repaint when this card changes.
    pub fn dirty_rect(&self, scale: f32) -> Rect {
        self.rect.inflate((CARD_EDGE * scale).ceil())
    }
}

fn style(st: &SceneStyle, color: crate::geom::Color) -> TextStyle {
    TextStyle {
        size: st.em / st.scale,
        scale: st.scale,
        color,
    }
}

fn text_y(r: Rect, st: &SceneStyle) -> f32 {
    r.y + ((r.h - st.em * 1.25) / 2.0).round().max(0.0)
}

/// Paints the card if it touches `region`.
pub fn draw_card<C: Canvas>(
    canvas: &mut C,
    card: &Card,
    pal: &Palette,
    st: &SceneStyle,
    region: Option<Rect>,
) {
    if let Some(region) = region
        && !card.dirty_rect(st.scale).intersects(&region)
    {
        return;
    }
    let r = card.rect.snapped();
    // A faint edge lifts the panel off whatever is behind it.
    let edge = st.scale.round().max(1.0);
    canvas.stroke_rect(
        Rect::new(r.x - edge, r.y - edge, r.w + 2.0 * edge, r.h + 2.0 * edge),
        Stroke::solid(edge, pal.text.with_opacity(0.14)),
    );
    canvas.fill_rect(r, pal.bar);
    canvas.stroke_rect(r, Stroke::solid(edge, pal.border));
    for part in &card.parts {
        draw_part(canvas, part, pal, st);
    }
}

fn draw_part<C: Canvas>(canvas: &mut C, part: &CardPart, pal: &Palette, st: &SceneStyle) {
    match part {
        CardPart::Rule { rect } => canvas.fill_rect(rect.snapped(), pal.border),
        CardPart::Line { rect, text, role } => {
            if !st.labels {
                return;
            }
            let color = match role {
                CardRole::Title | CardRole::Normal => pal.text,
                CardRole::Secondary => pal.text_secondary,
            };
            let r = rect.snapped();
            canvas.push_clip(r);
            canvas.draw_text(text, style(st, color), r.x, text_y(r, st), r.w.max(1.0));
            canvas.pop_clip();
        }
        CardPart::Path {
            rect,
            text,
            hot,
            copied,
        } => {
            if !st.labels {
                return;
            }
            let r = rect.snapped();
            let size = st.em / st.scale;
            let ty = text_y(r, st);
            canvas.push_clip(r);
            let mut path_w = r.w;
            if *copied {
                let cw = canvas
                    .measure_text(COPIED_TEXT, size, st.scale)
                    .unwrap_or(st.em * 3.0)
                    .ceil();
                canvas.draw_text(
                    COPIED_TEXT,
                    style(st, pal.accent),
                    r.right() - cw,
                    ty,
                    cw + 2.0,
                );
                path_w = (r.w - cw - (st.em * 0.6).round()).max(1.0);
            }
            let color = if *hot { pal.accent } else { pal.text_secondary };
            canvas.draw_text(text, style(st, color), r.x, ty, path_w);
            if *hot {
                let tw = canvas
                    .measure_text(text, size, st.scale)
                    .unwrap_or(path_w)
                    .min(path_w);
                canvas.fill_rect(
                    Rect::new(
                        r.x,
                        (ty + st.em * 1.15).round(),
                        tw.round(),
                        st.scale.round().max(1.0),
                    ),
                    pal.accent,
                );
            }
            canvas.pop_clip();
        }
        CardPart::Button {
            rect,
            label,
            enabled,
            hot,
        } => {
            let r = rect.snapped();
            let fill = if *hot && *enabled {
                pal.title[2]
            } else {
                pal.title[0]
            };
            canvas.fill_rect(r, fill);
            canvas.stroke_rect(r, Stroke::solid(st.scale.round().max(1.0), pal.border));
            if !st.labels {
                return;
            }
            let color = if *enabled {
                pal.text
            } else {
                pal.text_secondary.with_opacity(0.55)
            };
            let tw = canvas
                .measure_text(label, st.em / st.scale, st.scale)
                .unwrap_or(r.w)
                .min(r.w);
            canvas.push_clip(r);
            canvas.draw_text(
                label,
                style(st, color),
                r.x + ((r.w - tw) / 2.0).round(),
                text_y(r, st),
                r.w.max(1.0),
            );
            canvas.pop_clip();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{Op, RecordingCanvas};

    fn card() -> Card {
        Card {
            rect: Rect::new(10.0, 10.0, 200.0, 100.0),
            parts: vec![
                CardPart::Line {
                    rect: Rect::new(14.0, 14.0, 190.0, 20.0),
                    text: "Videos".into(),
                    role: CardRole::Title,
                },
                CardPart::Path {
                    rect: Rect::new(14.0, 40.0, 190.0, 20.0),
                    text: "/home/a/Videos".into(),
                    hot: false,
                    copied: false,
                },
                CardPart::Button {
                    rect: Rect::new(14.0, 70.0, 80.0, 24.0),
                    label: "Delete".into(),
                    enabled: false,
                    hot: false,
                },
            ],
        }
    }

    #[test]
    fn the_panel_and_its_texts_are_drawn() {
        let pal = Palette::light();
        let st = SceneStyle::new(1.0, 13.0, true);
        let mut c = RecordingCanvas::new(300, 200);
        draw_card(&mut c, &card(), &pal, &st, None);
        let texts: Vec<_> = c.texts().collect();
        assert_eq!(texts, ["Videos", "/home/a/Videos", "Delete"]);
        // The panel body is filled with the bar color.
        assert!(c.fills().any(|(r, col)| *col == pal.bar && r.w == 200.0));
    }

    #[test]
    fn a_copied_path_shows_the_confirmation() {
        let pal = Palette::light();
        let st = SceneStyle::new(1.0, 13.0, true);
        let mut k = card();
        if let CardPart::Path { copied, .. } = &mut k.parts[1] {
            *copied = true;
        }
        let mut c = RecordingCanvas::new(300, 200);
        draw_card(&mut c, &k, &pal, &st, None);
        assert!(c.texts().any(|t| t == COPIED_TEXT));
    }

    #[test]
    fn a_disabled_button_is_dimmed_and_without_labels_only_boxes_draw() {
        let pal = Palette::light();
        let st = SceneStyle::new(1.0, 13.0, true);
        let mut c = RecordingCanvas::new(300, 200);
        draw_card(&mut c, &card(), &pal, &st, None);
        let dimmed = c.ops.iter().any(|o| {
            matches!(o, Op::Text { text, style, .. }
                if text == "Delete" && style.color != pal.text)
        });
        assert!(dimmed);
        let st = SceneStyle::new(1.0, 13.0, false);
        let mut c = RecordingCanvas::new(300, 200);
        draw_card(&mut c, &card(), &pal, &st, None);
        assert_eq!(c.texts().count(), 0);
    }

    #[test]
    fn a_region_away_from_the_card_draws_nothing() {
        let pal = Palette::light();
        let st = SceneStyle::new(1.0, 13.0, true);
        let mut c = RecordingCanvas::new(300, 200);
        draw_card(
            &mut c,
            &card(),
            &pal,
            &st,
            Some(Rect::new(250.0, 150.0, 20.0, 20.0)),
        );
        assert!(c.ops.is_empty());
    }
}
