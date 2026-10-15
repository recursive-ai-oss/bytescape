//! The geometry of the things that sit over the map: the confirmation
//! dialogs, the context menu and the toast. Each builder returns the card to
//! draw and where its controls are, in device pixels.

use bs_render::{Card, CardPart, CardRole, Popup, Rect};

use crate::card::{Metrics, wrap};

/// One button of a dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    /// Acts only while held down.
    pub hold: bool,
}

/// What a dialog says and offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogSpec {
    /// An explanation that comes before the question.
    pub lead: Option<String>,
    pub title: String,
    pub lines: Vec<(String, CardRole)>,
    /// Positions in `lines` that are drawn whatever else the dialog holds
    /// (the warning, how to confirm); the rest share the room that is left.
    pub pinned: Vec<usize>,
    /// Most lines drawn besides the pinned ones; 0 means the usual limit.
    pub rows: usize,
    pub buttons: Vec<Button>,
}

/// A built popup and where its controls are.
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    pub popup: Popup,
    /// The clickable controls and their index (button or row).
    pub hits: Vec<(Rect, usize)>,
}

impl Built {
    /// The control under a point.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, i)| *i)
    }
}

/// How state changes the look of a dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DialogState {
    /// The button with keyboard focus or under the pointer.
    pub focus: usize,
    /// How much of a hold button is filled, 0 to 1.
    pub progress: f32,
}

/// Lines in the dialog are limited so a hostile text cannot fill the window.
const MAX_LINES: usize = 14;
/// The most a dialog may ask for, whatever the caller wants.
const MOST_LINES: usize = 60;

/// Builds a dialog centered in `window`.
pub fn dialog(spec: &DialogSpec, window: Rect, m: Metrics<'_>, state: DialogState) -> Built {
    let em = m.em;
    let pad = (em * 1.0).round();
    let line_h = (em * 1.5).round();
    let title_h = (em * 1.9).round();
    let button_h = (em * 2.2).round();
    let gap = (em * 0.6).round();
    let margin = (12.0 * m.scale).round();
    let width = (em * 34.0)
        .min((window.w - 2.0 * margin).max(em * 12.0))
        .round();
    let inner = width - 2.0 * pad;
    let text_w = |s: &str| {
        m.measure
            .width(s)
            .unwrap_or_else(|| s.chars().count() as f32 * em * 0.56)
    };

    let mut parts: Vec<CardPart> = Vec::new();
    let mut y = pad;
    let mut rows = 0usize;
    // The pinned lines keep their room; everything else shares the rest.
    let reserved: usize = spec
        .pinned
        .iter()
        .filter_map(|&i| spec.lines.get(i))
        .map(|(t, _)| wrap(t, inner, 4, m.measure, em).len())
        .sum();
    let wanted = match spec.rows {
        0 => MAX_LINES,
        n => n.min(MOST_LINES),
    };
    let limit = wanted.saturating_sub(reserved).max(1);
    let mut push_lines =
        |parts: &mut Vec<CardPart>, y: &mut f32, text: &str, role, h: f32, pinned: bool| {
            let mut wrapped = wrap(text, inner, 4, m.measure, em);
            // A blank line stays a blank line.
            if wrapped.is_empty() && text.trim().is_empty() {
                wrapped.push(String::new());
            }
            for l in wrapped {
                if !pinned && rows >= limit {
                    break;
                }
                if !pinned {
                    rows += 1;
                }
                parts.push(CardPart::Line {
                    rect: Rect::new(pad, *y, inner, h),
                    text: l.into(),
                    role,
                });
                *y += h;
            }
        };
    if let Some(lead) = &spec.lead {
        push_lines(&mut parts, &mut y, lead, CardRole::Normal, line_h, false);
        y += (gap / 2.0).round();
    }
    push_lines(
        &mut parts,
        &mut y,
        &spec.title,
        CardRole::Title,
        title_h,
        false,
    );
    for (i, (text, role)) in spec.lines.iter().enumerate() {
        let pinned = spec.pinned.contains(&i);
        push_lines(&mut parts, &mut y, text, *role, line_h, pinned);
    }
    y += gap;

    // Buttons side by side, right-aligned. When they do not fit in one row
    // they continue on the next one, so a long label never leaves the dialog.
    let btn_pad = (em * 1.2).round();
    let widths: Vec<f32> = spec
        .buttons
        .iter()
        .map(|b| (text_w(&b.label).ceil() + 2.0 * btn_pad).min(inner))
        .collect();
    let mut lines_of_buttons: Vec<Vec<usize>> = vec![Vec::new()];
    let mut row_w = 0.0f32;
    for (i, w) in widths.iter().enumerate() {
        let add = if row_w == 0.0 { *w } else { gap + *w };
        if row_w > 0.0 && row_w + add > inner {
            lines_of_buttons.push(Vec::new());
            row_w = *w;
        } else {
            row_w += add;
        }
        if let Some(last) = lines_of_buttons.last_mut() {
            last.push(i);
        }
    }
    let mut hits = Vec::new();
    let last_row = lines_of_buttons.len().saturating_sub(1);
    for (n, row) in lines_of_buttons.iter().enumerate() {
        let total: f32 = row.iter().filter_map(|&i| widths.get(i)).sum::<f32>()
            + gap * row.len().saturating_sub(1) as f32;
        let mut x = pad + (inner - total).max(0.0);
        for &i in row {
            let (Some(b), Some(w)) = (spec.buttons.get(i), widths.get(i)) else {
                continue;
            };
            let r = Rect::new(x, y, *w, button_h);
            let focused = state.focus == i;
            parts.push(if b.hold {
                CardPart::Hold {
                    rect: r,
                    label: b.label.clone().into(),
                    progress: if focused { state.progress } else { 0.0 },
                    hot: focused,
                }
            } else {
                CardPart::Button {
                    rect: r,
                    label: b.label.clone().into(),
                    enabled: true,
                    hot: focused,
                }
            });
            hits.push((r, i));
            x += w + gap;
        }
        if n < last_row {
            y += button_h + gap;
        }
    }
    y += button_h + pad;

    let rect = Rect::new(
        (window.x + (window.w - width) / 2.0).round(),
        (window.y + (window.h - y) / 3.0).round().max(window.y),
        width,
        y,
    );
    shift(&mut parts, &mut hits, rect.x, rect.y);
    Built {
        popup: Popup {
            card: Card { rect, parts },
            scrim: true,
        },
        hits,
    }
}

/// One row of a context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuRow {
    pub label: String,
    /// A shortcut when the row works, or why it does not.
    pub hint: String,
    pub enabled: bool,
    /// A line between groups instead of a row.
    pub separator: bool,
}

/// Builds the menu with its corner near `at`, kept inside `bounds`. `hot` is
/// the row under the pointer or the keyboard.
pub fn menu(
    rows: &[MenuRow],
    at: (f32, f32),
    bounds: Rect,
    m: Metrics<'_>,
    hot: Option<usize>,
) -> Built {
    let em = m.em;
    let row_h = (em * 1.9).round();
    let sep_h = (em * 0.7).round();
    let pad = (em * 0.7).round();
    let text_w = |s: &str| {
        m.measure
            .width(s)
            .unwrap_or_else(|| s.chars().count() as f32 * em * 0.56)
    };
    let widest = rows
        .iter()
        .filter(|r| !r.separator)
        .map(|r| text_w(&r.label).ceil() + text_w(&r.hint).ceil() + 3.0 * pad)
        .fold(em * 12.0, f32::max);
    let edge = m.scale.round().max(1.0);
    let width = (widest + 2.0 * edge + 2.0)
        .min((bounds.w - 8.0).max(em * 8.0))
        .round();
    let height: f32 = rows
        .iter()
        .map(|r| if r.separator { sep_h } else { row_h })
        .sum::<f32>()
        + 2.0 * edge;

    let mut parts = Vec::new();
    let mut hits = Vec::new();
    let mut y = edge;
    for (i, r) in rows.iter().enumerate() {
        if r.separator {
            parts.push(CardPart::Rule {
                rect: Rect::new(pad, y + (sep_h / 2.0).round(), width - 2.0 * pad, edge),
            });
            y += sep_h;
            continue;
        }
        let rect = Rect::new(edge, y, width - 2.0 * edge, row_h);
        parts.push(CardPart::Row {
            rect,
            label: r.label.clone().into(),
            hint: r.hint.clone().into(),
            enabled: r.enabled,
            hot: hot == Some(i),
        });
        hits.push((rect, i));
        y += row_h;
    }
    let x = at.0.min(bounds.right() - width).max(bounds.x);
    let top = at.1.min(bounds.bottom() - height).max(bounds.y);
    let rect = Rect::new(x.round(), top.round(), width, height);
    shift(&mut parts, &mut hits, rect.x, rect.y);
    Built {
        popup: Popup {
            card: Card { rect, parts },
            scrim: false,
        },
        hits,
    }
}

/// What a toast says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastSpec {
    pub lines: Vec<(String, CardRole)>,
    /// A button after the first line.
    pub button: Option<String>,
}

/// A built toast: the card and where its button is.
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltToast {
    pub card: Card,
    pub button: Option<Rect>,
}

/// Builds the toast in the lower left corner of `bounds`. `rise` is how far
/// along it is, from 0 (below the window) to 1 (in place).
pub fn toast(
    spec: &ToastSpec,
    bounds: Rect,
    m: Metrics<'_>,
    hot_button: bool,
    rise: f32,
) -> BuiltToast {
    let em = m.em;
    let pad = (em * 0.8).round();
    let line_h = (em * 1.5).round();
    let button_h = (em * 2.0).round();
    let gap = (em * 0.7).round();
    let margin = (12.0 * m.scale).round();
    let text_w = |s: &str| {
        m.measure
            .width(s)
            .unwrap_or_else(|| s.chars().count() as f32 * em * 0.56)
            .ceil()
    };
    let btn_w = spec
        .button
        .as_ref()
        .map_or(0.0, |b| text_w(b) + 2.0 * (em * 0.9).round());
    let max_w = (bounds.w - 2.0 * margin).max(em * 12.0);
    let first_w = spec.lines.first().map_or(0.0, |(t, _)| text_w(t));
    let widest = spec.lines.iter().skip(1).map(|(t, _)| text_w(t)).fold(
        first_w + if btn_w > 0.0 { gap + btn_w } else { 0.0 },
        f32::max,
    );
    let width = (widest + 2.0 * pad).min(max_w).max(em * 12.0).round();
    let inner = width - 2.0 * pad;

    let mut parts = Vec::new();
    let mut y = pad;
    let mut button = None;
    for (i, (text, role)) in spec.lines.iter().enumerate() {
        let avail = if i == 0 && btn_w > 0.0 {
            (inner - btn_w - gap).max(em * 4.0)
        } else {
            inner
        };
        let wrapped = wrap(text, avail, 3, m.measure, em);
        let n = wrapped.len().max(1);
        for l in wrapped {
            parts.push(CardPart::Line {
                rect: Rect::new(pad, y, avail, line_h),
                text: l.into(),
                role: *role,
            });
            if i == 0 && btn_w > 0.0 && button.is_none() {
                let r = Rect::new(
                    pad + inner - btn_w,
                    y + ((line_h - button_h) / 2.0).round(),
                    btn_w,
                    button_h,
                );
                parts.push(CardPart::Button {
                    rect: r,
                    label: spec.button.clone().unwrap_or_default().into(),
                    enabled: true,
                    hot: hot_button,
                });
                button = Some(r);
            }
            y += line_h;
        }
        let _ = n;
    }
    let height = y + pad;
    let full = Rect::new(
        bounds.x + margin,
        bounds.bottom() - margin - height,
        width,
        height,
    );
    // It slides up from below the window edge.
    let drop = ((1.0 - rise.clamp(0.0, 1.0)) * (height + margin)).round();
    let rect = Rect::new(full.x, full.y + drop, full.w, full.h);
    let mut hits = Vec::new();
    shift(&mut parts, &mut hits, rect.x, rect.y);
    BuiltToast {
        card: Card { rect, parts },
        button: button.map(|b| Rect::new(b.x + rect.x, b.y + rect.y, b.w, b.h)),
    }
}

/// Moves every part (and hit rectangle) by `(dx, dy)`.
fn shift(parts: &mut [CardPart], hits: &mut [(Rect, usize)], dx: f32, dy: f32) {
    let mv = |r: Rect| Rect::new(r.x + dx, r.y + dy, r.w, r.h);
    for p in parts {
        match p {
            CardPart::Line { rect, .. }
            | CardPart::Path { rect, .. }
            | CardPart::Button { rect, .. }
            | CardPart::Rule { rect }
            | CardPart::Row { rect, .. }
            | CardPart::Hold { rect, .. } => *rect = mv(*rect),
        }
    }
    for (r, _) in hits {
        *r = mv(*r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::TextMeasure;

    struct Fixed;
    impl TextMeasure for Fixed {
        fn width(&self, text: &str) -> Option<f32> {
            Some(text.chars().count() as f32 * 7.0)
        }
    }

    fn metrics() -> Metrics<'static> {
        Metrics {
            em: 13.0,
            scale: 1.0,
            measure: &Fixed,
        }
    }

    #[test]
    fn a_dialog_sits_inside_the_window_with_its_buttons_inside_it() {
        let window = Rect::new(0.0, 0.0, 800.0, 600.0);
        let spec = DialogSpec {
            lead: None,
            title: "Permanently delete 3 items (12.3 GB)?".into(),
            lines: vec![("This can't be undone.".into(), CardRole::Secondary)],
            pinned: Vec::new(),
            rows: 0,
            buttons: vec![
                Button {
                    label: "Delete permanently".into(),
                    hold: false,
                },
                Button {
                    label: "Cancel".into(),
                    hold: false,
                },
            ],
        };
        let b = dialog(&spec, window, metrics(), DialogState::default());
        let card = b.popup.card.rect;
        assert!(card.x >= 0.0 && card.right() <= 800.0 && card.bottom() <= 600.0);
        assert_eq!(b.hits.len(), 2);
        for (r, _) in &b.hits {
            assert!(r.x >= card.x && r.right() <= card.right() && r.bottom() <= card.bottom());
        }
        assert!(b.popup.scrim);
    }

    #[test]
    fn buttons_that_do_not_fit_in_a_row_continue_on_the_next_one() {
        let window = Rect::new(0.0, 0.0, 600.0, 400.0);
        let spec = DialogSpec {
            lead: None,
            title: "About".into(),
            lines: Vec::new(),
            pinned: Vec::new(),
            rows: 0,
            buttons: [
                "Find more free software at RecursiveAI.org",
                "Check for updates",
                "Close",
            ]
            .iter()
            .map(|l| Button {
                label: (*l).into(),
                hold: false,
            })
            .collect(),
        };
        let b = dialog(&spec, window, metrics(), DialogState::default());
        let card = b.popup.card.rect;
        assert_eq!(b.hits.len(), 3);
        for (r, _) in &b.hits {
            assert!(
                r.x >= card.x && r.right() <= card.right() && r.bottom() <= card.bottom(),
                "{r:?} outside {card:?}"
            );
        }
        // The long one sits alone on the first row; the other two share the
        // second, right-aligned and in order.
        let rect = |i: usize| {
            b.hits
                .iter()
                .find(|(_, n)| *n == i)
                .map(|(r, _)| *r)
                .unwrap()
        };
        assert!(rect(1).y > rect(0).y);
        assert_eq!(rect(1).y, rect(2).y);
        assert!(rect(1).right() < rect(2).x);
        assert!((rect(2).right() - (card.right() - 13.0)).abs() < 1.5);
        // Two short buttons still share one row, as before.
        let short = DialogSpec {
            buttons: vec![
                Button {
                    label: "OK".into(),
                    hold: false,
                },
                Button {
                    label: "Cancel".into(),
                    hold: false,
                },
            ],
            ..spec
        };
        let b = dialog(&short, window, metrics(), DialogState::default());
        assert_eq!(b.hits[0].0.y, b.hits[1].0.y);
    }

    #[test]
    fn a_long_hostile_line_cannot_make_the_dialog_taller_than_the_window() {
        let window = Rect::new(0.0, 0.0, 600.0, 400.0);
        let spec = DialogSpec {
            lead: Some("z".repeat(900)),
            title: "x".repeat(5000),
            lines: (0..100)
                .map(|i| {
                    (
                        format!("line {i} {}", "y ".repeat(300)),
                        CardRole::Secondary,
                    )
                })
                .collect(),
            pinned: Vec::new(),
            rows: 0,
            buttons: vec![Button {
                label: "OK".into(),
                hold: false,
            }],
        };
        let b = dialog(&spec, window, metrics(), DialogState::default());
        assert!(b.popup.card.rect.h < 14.0 * 4.0 * 30.0);
    }

    #[test]
    fn a_menu_stays_inside_the_bounds_and_separators_are_not_hits() {
        let rows = vec![
            MenuRow {
                label: "Open".into(),
                hint: "Enter".into(),
                enabled: true,
                separator: false,
            },
            MenuRow {
                label: String::new(),
                hint: String::new(),
                enabled: false,
                separator: true,
            },
            MenuRow {
                label: "Delete".into(),
                hint: "Part of the operating system".into(),
                enabled: false,
                separator: false,
            },
        ];
        let bounds = Rect::new(0.0, 0.0, 400.0, 300.0);
        let b = menu(&rows, (390.0, 290.0), bounds, metrics(), None);
        let r = b.popup.card.rect;
        assert!(r.right() <= 400.0 && r.bottom() <= 300.0 && r.x >= 0.0);
        let ids: Vec<usize> = b.hits.iter().map(|(_, i)| *i).collect();
        assert_eq!(ids, [0, 2]);
        assert!(!b.popup.scrim);
    }

    #[test]
    fn the_toast_rises_into_the_lower_left_corner() {
        let spec = ToastSpec {
            lines: vec![("Moved to Trash · 12.3 GB".into(), CardRole::Normal)],
            button: Some("Undo".into()),
        };
        let bounds = Rect::new(0.0, 28.0, 800.0, 560.0);
        let done = toast(&spec, bounds, metrics(), false, 1.0);
        assert!(done.card.rect.x >= 0.0 && done.card.rect.bottom() <= 588.0);
        let undo = done.button.expect("an Undo button");
        assert!(undo.x >= done.card.rect.x && undo.right() <= done.card.rect.right());
        let start = toast(&spec, bounds, metrics(), false, 0.0);
        assert!(start.card.rect.y > done.card.rect.y);
    }
}
