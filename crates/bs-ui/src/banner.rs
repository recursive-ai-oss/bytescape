//! The note shown after a crash, and the card data the shell adds to what the
//! source knows: what the desktop can do, and what a delete would free.

use std::time::Duration;

use bs_render::{Rect, TextRole, Widget};

use super::{CRASH_BUTTONS, CRASH_WORDS, CrashNotice, CrashStrip, CrashUi, Shell, ShellEffect};
use crate::card::{self, CardData};
use crate::chrome;
use crate::{InputEvent, Key, ViewId, ViewSource};

/// A part of the hover card as a screen reader needs it.
pub(super) enum CardNode {
    /// A line of text.
    Text(String, Rect),
    /// The path, which a click copies.
    Path(String, Rect),
    /// A button: label, whether it works, why not, and where it is.
    Button(String, bool, String, Rect),
}

/// What the pointer is on in the crash banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CrashHit {
    Panel,
    Button(usize),
}

impl<S: ViewSource> Shell<S> {
    /// Shows the note about a crash last time, or takes it away. Returns what
    /// the host must do because the map's rectangle changed.
    pub fn set_crash_notice(
        &mut self,
        notice: Option<CrashNotice>,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let before = self.map_rect();
        self.crash = notice.map(|notice| CrashUi { notice, hot: None });
        self.crash_focus = None;
        if self.map_rect() == before {
            return Vec::new();
        }
        self.map_moved(now)
    }

    /// The crash note being shown: its words and the labels of its buttons.
    pub fn crash_banner(&self) -> Option<(&'static str, [&'static str; 3])> {
        self.crash.as_ref().map(|_| (CRASH_WORDS, CRASH_BUTTONS))
    }

    /// The map's rectangle changed: lay the map out again for it.
    pub(super) fn map_moved(&mut self, now: Duration) -> Vec<ShellEffect> {
        let map = self.map_rect();
        let mut fx = self.map_event(InputEvent::Resized { map }, now);
        fx.push(ShellEffect::MapMoved);
        fx
    }

    /// How tall the crash note is, 0 when there is none. It is one row when
    /// the words and buttons fit across the window and two when they do not.
    pub(super) fn crash_height(&self) -> f32 {
        self.crash_strip().map_or(0.0, |s| s.panel.h)
    }

    pub(super) fn crash_strip(&self) -> Option<CrashStrip> {
        self.crash.as_ref()?;
        let em = self.font_px * self.scale;
        let pad = (em * 0.7).round();
        let m = |t: &str| {
            self.measure
                .width(t)
                .unwrap_or(t.chars().count() as f32 * em * 0.56)
                .ceil()
        };
        let full = chrome::map_rect(self.width as f32, self.height as f32, self.scale);
        let row_h = (em * 2.4).round();
        let bh = (em * 1.7).round();
        let widths = CRASH_BUTTONS.map(|t| (m(t) + em * 1.6).round());
        let words_w = m(CRASH_WORDS);
        let all_buttons: f32 = widths.iter().sum::<f32>() + 2.0 * pad;
        let one_row = pad + words_w + pad + all_buttons + pad <= full.w;
        let mut buttons = [Rect::default(); 3];
        if one_row {
            let panel = Rect::new(full.x, full.y, full.w, row_h);
            let text = Rect::new(panel.x + pad, panel.y, words_w + 2.0, row_h);
            let mut x = text.right() + pad;
            for (b, w) in buttons.iter_mut().zip(widths) {
                *b = Rect::new(x, panel.y + ((row_h - bh) / 2.0).round(), w, bh);
                x += w + pad;
            }
            Some(CrashStrip {
                panel,
                text,
                buttons,
            })
        } else {
            let second = bh + (em * 0.8).round();
            let panel = Rect::new(full.x, full.y, full.w, row_h + second);
            let text = Rect::new(panel.x + pad, panel.y, (full.w - 2.0 * pad).max(0.0), row_h);
            let mut x = panel.x + pad;
            for (b, w) in buttons.iter_mut().zip(widths) {
                *b = Rect::new(x, panel.y + row_h, w, bh);
                x += w + pad;
            }
            Some(CrashStrip {
                panel,
                text,
                buttons,
            })
        }
    }

    pub(super) fn crash_at(&self, x: f32, y: f32) -> Option<CrashHit> {
        let strip = self.crash_strip()?;
        if let Some(i) = strip.buttons.iter().position(|b| b.contains(x, y)) {
            Some(CrashHit::Button(i))
        } else {
            strip.panel.contains(x, y).then_some(CrashHit::Panel)
        }
    }

    /// The pointer is on the crash note.
    pub(super) fn crash_event(
        &mut self,
        event: InputEvent,
        hit: CrashHit,
        now: Duration,
    ) -> Vec<ShellEffect> {
        self.hide_card();
        self.card.rest = None;
        let mut fx = self.set_hot(None);
        match event {
            InputEvent::PointerMoved { .. } => {
                let hot = match hit {
                    CrashHit::Button(i) => Some(i),
                    CrashHit::Panel => None,
                };
                if let Some(c) = self.crash.as_mut() {
                    c.hot = hot;
                }
                fx.extend(self.map_event(InputEvent::PointerLeft, now));
            }
            InputEvent::Click { .. } => {
                if let CrashHit::Button(i) = hit {
                    fx.extend(self.crash_press(i, now));
                }
            }
            _ => {}
        }
        fx
    }

    /// A button of the crash note was pressed.
    fn crash_press(&mut self, button: usize, now: Duration) -> Vec<ShellEffect> {
        match button {
            0 => {
                self.open_report();
                Vec::new()
            }
            1 => match self.crash.as_ref() {
                Some(c) => vec![ShellEffect::OpenUrl(c.notice.issue_url().to_string())],
                None => Vec::new(),
            },
            _ => {
                let mut fx = vec![ShellEffect::CrashSeen];
                fx.extend(self.set_crash_notice(None, now));
                fx
            }
        }
    }

    /// Tab moves the keyboard onto the buttons of the crash note; Enter or
    /// Space presses the one it is on. `None` when the key is not for the
    /// note.
    pub(super) fn crash_key(&mut self, key: Key, now: Duration) -> Option<Vec<ShellEffect>> {
        self.crash.as_ref()?;
        let n = CRASH_BUTTONS.len();
        match key {
            Key::Tab | Key::BackTab if !self.toast_has_button() => {
                self.crash_focus = match (key, self.crash_focus) {
                    (Key::Tab, None) => Some(0),
                    (Key::Tab, Some(i)) if i + 1 < n => Some(i + 1),
                    (Key::BackTab, None) => Some(n - 1),
                    (Key::BackTab, Some(i)) if i > 0 => Some(i - 1),
                    _ => None,
                };
                Some(Vec::new())
            }
            Key::Enter | Key::Space if self.crash_focus.is_some() => {
                let i = self.crash_focus?;
                Some(self.crash_press(i, now))
            }
            Key::Escape if self.crash_focus.is_some() => {
                self.crash_focus = None;
                Some(Vec::new())
            }
            _ => {
                self.crash_focus = None;
                None
            }
        }
    }

    /// The widgets of the crash note.
    pub(super) fn crash_widgets(&self) -> Vec<Widget> {
        let Some(strip) = self.crash_strip() else {
            return Vec::new();
        };
        let hot = self.crash.as_ref().and_then(|c| c.hot);
        let mut widgets = vec![
            Widget::Panel { rect: strip.panel },
            Widget::Text {
                rect: strip.text,
                text: CRASH_WORDS.into(),
                role: TextRole::Primary,
                hot: false,
            },
        ];
        for (i, rect) in strip.buttons.iter().enumerate() {
            widgets.push(Widget::Pill {
                rect: *rect,
                text: CRASH_BUTTONS[i].into(),
                accent: i == 0,
                hot: hot == Some(i) || self.crash_focus == Some(i),
            });
        }
        widgets
    }

    /// Everything the card says about box `id` of `view`: what the source
    /// knows, what the desktop can do, and with `with_sharing` what a delete
    /// would free (a lookup that is only made for the box a card describes).
    pub(super) fn card_data(&self, view: ViewId, id: u32, with_sharing: bool) -> Option<CardData> {
        let source = self.app.as_ref()?.source();
        let mut data = source.card(view, id)?;
        data.software_center = self.acts.software_center;
        data.scan_holds = self.offer == crate::elevation::Offer::Running;
        if with_sharing {
            data.sharing = source.sharing(view, id);
        }
        Some(data)
    }

    /// The lookup of program and package owners has new answers: build the
    /// card on screen again from them.
    pub fn owners_changed(&mut self) {
        let Some((view, id)) = self.card.shown.as_ref().map(|s| (s.view, s.id)) else {
            return;
        };
        if let Some(data) = self.card_data(view, id, true) {
            if let Some(s) = self.card.shown.as_mut() {
                s.data = data;
            }
            self.rebuild_card();
        }
    }

    /// The hover card on screen as a screen reader needs it: where it is, its
    /// name, and its parts in reading order.
    pub(super) fn card_summary(&self) -> Option<(Rect, String, Vec<CardNode>)> {
        let shown = self.card.shown.as_ref()?;
        let reasons = card::buttons(&shown.data);
        let mut nodes = Vec::new();
        for part in &shown.built.card.parts {
            match part {
                bs_render::CardPart::Line { rect, text, .. } => {
                    nodes.push(CardNode::Text(text.to_string(), *rect));
                }
                bs_render::CardPart::Path { rect, text, .. } => {
                    nodes.push(CardNode::Path(text.to_string(), *rect));
                }
                bs_render::CardPart::Button {
                    rect,
                    label,
                    enabled,
                    ..
                } => {
                    // A disabled button says why; a working one says what it
                    // does.
                    let why = match reasons.iter().find(|(a, _)| a.label() == &**label) {
                        Some((_, Some(why))) => *why,
                        Some((a, None)) => a.describe(),
                        None => "",
                    };
                    nodes.push(CardNode::Button(
                        label.to_string(),
                        *enabled,
                        why.to_string(),
                        *rect,
                    ));
                }
                _ => {}
            }
        }
        Some((shown.built.card.rect, shown.data.name.clone(), nodes))
    }
}
