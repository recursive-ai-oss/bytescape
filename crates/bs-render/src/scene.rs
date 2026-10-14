//! Palettes and the painter that turns a flat list of draw items into canvas
//! calls.
//!
//! [`Item`] is this crate's own description of one box. The layout code builds
//! its draw list in its own types; a small adapter maps them to `Item`, so
//! neither side depends on the other.

use crate::canvas::{Canvas, Hatch, HatchAngle, Stroke, TextStyle};
use crate::geom::{Color, Rect};

/// What a box is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// A folder with a title bar and room for children.
    FolderTitled,
    /// A folder too small to nest: one solid labeled block.
    FolderBlock,
    FileTile,
    /// Several small files merged into one block.
    Group,
    Free,
    Unreadable,
    Reserved,
    RecycleBin,
}

/// State bits of an item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u8);

impl Flags {
    pub const HOVER: Flags = Flags(1);
    pub const SELECTED: Flags = Flags(2);
    pub const DELETED: Flags = Flags(4);
    pub const IN_PROGRESS: Flags = Flags(8);
    pub const DIMMED: Flags = Flags(16);

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

/// The text of a box: name on the left, size on the right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemLabel {
    pub name: Box<str>,
    pub size: Box<str>,
}

/// One box to draw, in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub rect: Rect,
    pub kind: ItemKind,
    pub depth: u8,
    pub label: Option<Box<ItemLabel>>,
    pub id: u32,
    /// Index into [`Palette::categories`].
    pub category: u8,
    pub flags: Flags,
}

/// Number of file categories, including "Other".
pub const CATEGORY_COUNT: usize = 9;

/// Category names in palette order.
pub const CATEGORY_NAMES: [&str; CATEGORY_COUNT] = [
    "Video",
    "Images",
    "Audio",
    "Documents",
    "Archives & disk images",
    "Apps & system",
    "Code & dev",
    "Cache & temp",
    "Other",
];

/// Base color of each category: the Okabe-Ito set, with a plum for code and a
/// light gray for other files, chosen so no two categories merge under simulated
/// colorblindness (see the palette tests).
pub const CATEGORY_BASES: [u32; CATEGORY_COUNT] = [
    0xD55E00, 0x009E73, 0xCC79A7, 0x56B4E9, 0xE69F00, 0x0072B2, 0x7B4C80, 0xF0E442, 0xBEBEBE,
];

/// One of the two palettes, chosen from the OS light/dark setting.
#[derive(Clone, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color,
    pub bar: Color,
    pub border: Color,
    pub title: [Color; 4],
    pub text: Color,
    pub text_secondary: Color,
    pub accent: Color,
    /// Tinted fill of each category.
    pub categories: [Color; CATEGORY_COUNT],
    pub unreadable: Color,
    pub deleted_hatch: Color,
}

impl Palette {
    pub fn light() -> Self {
        Self::build(false, Color::hex(0x3B82F6))
    }

    pub fn dark() -> Self {
        Self::build(true, Color::hex(0x3B82F6))
    }

    pub fn for_scheme(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// Replaces the accent, for example with the desktop's accent color.
    pub fn with_accent(mut self, accent: Color) -> Self {
        self.accent = accent;
        self
    }

    fn build(dark: bool, accent: Color) -> Self {
        let (bg, bar, border, title) = if dark {
            (
                Color::hex(0x1B1B1E),
                Color::hex(0x232327),
                Color::hex(0x3A3A40),
                [0x2A2A2F, 0x313137, 0x38383F, 0x2E2E34].map(Color::hex),
            )
        } else {
            (
                Color::hex(0xF7F7F8),
                Color::hex(0xFFFFFF),
                Color::hex(0xD9D9DE),
                [0xECECEF, 0xE3E3E8, 0xDADAE0, 0xE8E8EC].map(Color::hex),
            )
        };
        let mut categories = [Color::rgb(0, 0, 0); CATEGORY_COUNT];
        for (slot, base) in categories.iter_mut().zip(CATEGORY_BASES) {
            let base = Color::hex(base);
            *slot = if dark {
                base.mix(Color::hex(0x1B1B1E), 0.45)
            } else {
                base.mix(Color::rgb(255, 255, 255), 0.55)
            };
        }
        Self {
            dark,
            bg,
            bar,
            border,
            title,
            text: if dark {
                Color::hex(0xE8E8EC)
            } else {
                Color::hex(0x1F1F23)
            },
            text_secondary: if dark {
                Color::hex(0xA0A0A8)
            } else {
                Color::hex(0x5E5E66)
            },
            accent,
            categories,
            unreadable: if dark {
                Color::hex(0x6E6350)
            } else {
                Color::hex(0xB8A98A)
            },
            deleted_hatch: Color::hex(0x8A8A93),
        }
    }

    /// Fill of a category, falling back to "Other" for an unknown index.
    pub fn category_fill(&self, category: u8) -> Color {
        self.categories
            .get(category as usize)
            .copied()
            .unwrap_or(self.categories[CATEGORY_COUNT - 1])
    }

    /// Text color for a label on a category fill.
    pub fn category_label(&self, category: u8) -> Color {
        self.category_fill(category).readable_text()
    }
}

/// A strip of window chrome (navigation or status bar) with optional text on
/// the left and right.
#[derive(Clone, Debug, PartialEq)]
pub struct Bar {
    pub rect: Rect,
    pub left: Option<Box<str>>,
    pub right: Option<Box<str>>,
    /// Draw the separator line on the top edge instead of the bottom.
    pub border_on_top: bool,
}

/// Sizes and switches for one painting pass.
#[derive(Clone, Copy, Debug)]
pub struct SceneStyle {
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// The system UI font size in device pixels.
    pub em: f32,
    /// Draw labels. Zoom animation frames draw without them.
    pub labels: bool,
    /// While set, every file and group outside this category is dimmed, and so
    /// is everything that has no category (the legend is being hovered).
    pub dim_except: Option<u8>,
    /// Opacity of the title text of folders still being counted; 1.0 is solid.
    pub pulse: f32,
}

impl SceneStyle {
    pub fn new(scale: f32, font_logical_px: f32, labels: bool) -> Self {
        Self {
            scale,
            em: font_logical_px * scale,
            labels,
            dim_except: None,
            pulse: 1.0,
        }
    }

    /// Whether the legend hover dims this item.
    pub fn dims(&self, item: &Item) -> bool {
        match self.dim_except {
            None => false,
            Some(c) => match item.kind {
                ItemKind::FileTile | ItemKind::Group => item.category != c,
                _ => true,
            },
        }
    }

    /// Height of a folder's title bar.
    pub fn title_height(&self) -> f32 {
        (self.em * 1.6).round()
    }

    fn font(&self) -> f32 {
        self.em / self.scale
    }

    fn pad(&self) -> f32 {
        (4.0 * self.scale).round()
    }
}

/// Paints `items` in order, restricted to `region` when given. Items outside
/// the region are skipped without touching the canvas.
pub fn draw_items<C: Canvas>(
    canvas: &mut C,
    items: &[Item],
    pal: &Palette,
    style: &SceneStyle,
    region: Option<Rect>,
) {
    for item in items {
        if let Some(region) = region
            && !item.rect.intersects(&region)
        {
            continue;
        }
        draw_item(canvas, item, pal, style);
    }
}

/// Paints chrome bars. The right-hand text is drawn first and is never
/// covered or truncated; the left text shortens to the space that remains.
pub fn draw_bars<C: Canvas>(
    canvas: &mut C,
    bars: &[Bar],
    pal: &Palette,
    st: &SceneStyle,
    region: Option<Rect>,
) {
    for bar in bars {
        if let Some(region) = region
            && !bar.rect.intersects(&region)
        {
            continue;
        }
        let r = bar.rect.snapped();
        canvas.fill_rect(r, pal.bar);
        let line_y = if bar.border_on_top {
            r.y
        } else {
            r.bottom() - 1.0
        };
        canvas.fill_rect(Rect::new(r.x, line_y, r.w, 1.0), pal.border);
        if !st.labels {
            continue;
        }
        let pad = st.pad() * 2.0;
        let ty = r.y + ((r.h - st.em * 1.25) / 2.0).round().max(0.0);
        let mut avail = r.w - 2.0 * pad;
        if let Some(right) = &bar.right
            && let Some(w) = canvas.measure_text(right, st.font(), st.scale)
        {
            let style = TextStyle {
                size: st.font(),
                scale: st.scale,
                color: pal.text_secondary,
            };
            canvas.draw_text(right, style, r.right() - pad - w, ty, w + 2.0);
            avail -= w + pad * 2.0;
        }
        if let Some(left) = &bar.left {
            let style = TextStyle {
                size: st.font(),
                scale: st.scale,
                color: pal.text,
            };
            canvas.draw_text(left, style, r.x + pad, ty, avail.max(1.0));
        }
    }
}

fn draw_item<C: Canvas>(canvas: &mut C, item: &Item, pal: &Palette, st: &SceneStyle) {
    let r = item.rect.snapped();
    if r.w < 1.0 || r.h < 1.0 {
        return;
    }
    let title = pal.title[item.depth as usize % 4];
    let mut label_color = pal.text;
    match item.kind {
        ItemKind::FolderTitled => {
            canvas.fill_rect(r, pal.border);
            let inner = inset(r, 1.0);
            canvas.fill_rect(inner, pal.bg);
            let band_h = st.title_height().min(inner.h);
            let band = Rect::new(inner.x, inner.y, inner.w, band_h);
            canvas.fill_rect(band, title);
            if st.labels {
                draw_title(canvas, item, band, pal, st);
            }
            finish(canvas, item, r, pal, st);
            return;
        }
        ItemKind::FolderBlock => {
            canvas.fill_rect(r, pal.border);
            canvas.fill_rect(inset(r, 1.0), title);
        }
        ItemKind::FileTile | ItemKind::Group => {
            let fill = if item.kind == ItemKind::Group && item.category as usize >= CATEGORY_COUNT {
                title
            } else {
                pal.category_fill(item.category)
            };
            canvas.fill_rect(r, fill.mix(pal.bg, 0.35).mix(pal.text, 0.12));
            if r.w >= 3.0 && r.h >= 3.0 {
                canvas.fill_rect(inset(r, 1.0), fill);
            }
            label_color = fill.readable_text();
        }
        ItemKind::Free => {
            canvas.fill_rect(r, pal.bar);
            canvas.stroke_rect(
                r,
                Stroke {
                    width: 1.0,
                    color: pal.border.mix(pal.text, 0.25),
                    dash: Some((4.0 * st.scale, 3.0 * st.scale)),
                },
            );
        }
        ItemKind::Unreadable => {
            canvas.fill_rect(r, pal.bg);
            canvas.fill_pattern(
                inset(r, 1.0),
                Hatch {
                    angle: HatchAngle::Rising,
                    spacing: (14.0 * st.scale).round(),
                    line_width: 5.0 * st.scale,
                    color: pal.unreadable.with_opacity(0.55),
                },
            );
            canvas.stroke_rect(r, Stroke::solid(1.0, pal.border));
        }
        ItemKind::Reserved => {
            canvas.fill_rect(r, pal.border);
            canvas.fill_rect(inset(r, 1.0), pal.title[1]);
            draw_lock(canvas, r, pal, st);
        }
        ItemKind::RecycleBin => {
            canvas.fill_rect(r, pal.border);
            canvas.fill_rect(inset(r, 1.0), pal.title[2]);
            draw_bin(canvas, r, pal, st);
        }
    }
    if st.labels
        && r.w >= 3.5 * st.em
        && r.h >= 1.6 * st.em
        && let Some(label) = &item.label
    {
        let style = TextStyle {
            size: st.font(),
            scale: st.scale,
            color: label_color,
        };
        draw_name(canvas, &label.name, style, r, st);
    }
    finish(canvas, item, r, pal, st);
}

/// Draws a box's name near its top-left corner, clipped to the box.
fn draw_name<C: Canvas>(canvas: &mut C, name: &str, style: TextStyle, r: Rect, st: &SceneStyle) {
    let pad = st.pad();
    let ty = r.y
        + ((r.h.min(2.0 * st.em) - st.em * 1.25) / 2.0)
            .max(1.0)
            .round();
    canvas.push_clip(inset(r, 1.0));
    canvas.draw_text(name, style, r.x + pad, ty, (r.w - 2.0 * pad).max(1.0));
    canvas.pop_clip();
}

/// State styling that sits on top of an item's own look.
fn finish<C: Canvas>(canvas: &mut C, item: &Item, r: Rect, pal: &Palette, st: &SceneStyle) {
    if item.flags.contains(Flags::DELETED) {
        canvas.fill_rect(r, pal.bg.with_opacity(0.65));
        canvas.fill_pattern(
            r,
            Hatch {
                angle: HatchAngle::Falling,
                spacing: (6.0 * st.scale).round(),
                line_width: st.scale.max(1.0),
                color: pal.deleted_hatch.with_opacity(0.8),
            },
        );
        // The label stays fully readable on top of the veil and the hatch.
        if st.labels
            && let Some(label) = &item.label
            && r.w >= 3.5 * st.em
            && r.h >= 1.6 * st.em
        {
            let style = TextStyle {
                size: st.font(),
                scale: st.scale,
                color: pal.text,
            };
            draw_name(canvas, &label.name, style, r, st);
        }
    }
    if item.flags.contains(Flags::DIMMED) || st.dims(item) {
        canvas.fill_rect(r, pal.bg.with_opacity(0.75));
    }
}

/// The title bar of a nested folder box.
pub fn title_band(rect: Rect, st: &SceneStyle) -> Rect {
    let inner = inset(rect.snapped(), 1.0);
    Rect::new(inner.x, inner.y, inner.w, st.title_height().min(inner.h))
}

fn draw_title<C: Canvas>(canvas: &mut C, item: &Item, band: Rect, pal: &Palette, st: &SceneStyle) {
    let Some(label) = &item.label else {
        return;
    };
    if band.w < 3.5 * st.em {
        return;
    }
    let pad = st.pad();
    let ty = band.y + ((band.h - st.em * 1.25) / 2.0).round().max(0.0);
    let mut avail = band.w - 2.0 * pad;
    canvas.push_clip(band);
    // A folder still being counted breathes once a second in its title text.
    let fade = if item.flags.contains(Flags::IN_PROGRESS) {
        st.pulse.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let size_style = TextStyle {
        size: st.font(),
        scale: st.scale,
        color: pal.text_secondary.with_opacity(fade),
    };
    let mut size_w = 0.0;
    if !label.size.is_empty()
        && let Some(w) = canvas.measure_text(&label.size, size_style.size, st.scale)
        && w + st.em * 3.0 < avail
    {
        size_w = w + pad;
        canvas.draw_text(&label.size, size_style, band.right() - pad - w, ty, w + 2.0);
    }
    avail -= size_w;
    let name_style = TextStyle {
        color: pal.text.with_opacity(fade),
        ..size_style
    };
    canvas.draw_text(&label.name, name_style, band.x + pad, ty, avail.max(1.0));
    canvas.pop_clip();
}

fn draw_lock<C: Canvas>(canvas: &mut C, r: Rect, pal: &Palette, st: &SceneStyle) {
    let u = (st.em * 0.5).round().max(2.0);
    if r.w < u * 4.0 || r.h < u * 4.0 {
        return;
    }
    let x = r.right() - u * 2.5;
    let y = r.y + u * 0.8;
    canvas.fill_rect(Rect::new(x, y + u, u * 1.5, u), pal.text_secondary);
    canvas.stroke_rect(
        Rect::new(x + u * 0.25, y, u, u * 1.4),
        Stroke::solid(1.0, pal.text_secondary),
    );
}

fn draw_bin<C: Canvas>(canvas: &mut C, r: Rect, pal: &Palette, st: &SceneStyle) {
    let u = (st.em * 0.5).round().max(2.0);
    if r.w < u * 4.0 || r.h < u * 4.0 {
        return;
    }
    let x = r.right() - u * 2.5;
    let y = r.y + u * 0.8;
    canvas.fill_rect(Rect::new(x, y, u * 1.5, u * 0.3), pal.text_secondary);
    canvas.stroke_rect(
        Rect::new(x + u * 0.1, y + u * 0.4, u * 1.3, u * 1.4),
        Stroke::solid(1.0, pal.text_secondary),
    );
}

fn inset(r: Rect, d: f32) -> Rect {
    Rect::new(
        r.x + d,
        r.y + d,
        (r.w - 2.0 * d).max(0.0),
        (r.h - 2.0 * d).max(0.0),
    )
}

/// The hover and selection outline, drawn after the items under it.
pub fn draw_overlay<C: Canvas>(
    canvas: &mut C,
    rect: Rect,
    pal: &Palette,
    hover: bool,
    selected: bool,
    scale: f32,
) {
    let r = rect.snapped();
    if selected {
        canvas.fill_rect(r, pal.accent.with_opacity(0.10));
    }
    if hover || selected {
        let w = (2.0 * scale).round().max(1.0);
        canvas.stroke_rect(r, Stroke::solid(w, pal.accent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{Op, RecordingCanvas};

    fn item(kind: ItemKind, rect: Rect) -> Item {
        Item {
            rect,
            kind,
            depth: 0,
            label: Some(Box::new(ItemLabel {
                name: "name".into(),
                size: "1 MB".into(),
            })),
            id: 1,
            category: 0,
            flags: Flags::default(),
        }
    }

    fn style(labels: bool) -> SceneStyle {
        SceneStyle::new(1.0, 13.0, labels)
    }

    #[test]
    fn exact_chrome_colors() {
        let l = Palette::light();
        assert_eq!(l.bg, Color::hex(0xF7F7F8));
        assert_eq!(l.bar, Color::hex(0xFFFFFF));
        assert_eq!(l.border, Color::hex(0xD9D9DE));
        assert_eq!(l.title[3], Color::hex(0xE8E8EC));
        let d = Palette::dark();
        assert_eq!(d.bg, Color::hex(0x1B1B1E));
        assert_eq!(d.title[2], Color::hex(0x38383F));
    }

    #[test]
    fn category_tints_follow_the_spec() {
        let l = Palette::light();
        // Video: #D55E00, 55% toward white.
        assert_eq!(l.categories[0], Color::rgb(236, 183, 140));
        let d = Palette::dark();
        // Video: #D55E00, 45% toward #1B1B1E.
        assert_eq!(d.categories[0], Color::rgb(129, 64, 14));
    }

    #[test]
    fn category_labels_reach_contrast_in_both_modes() {
        for pal in [Palette::light(), Palette::dark()] {
            for c in 0..CATEGORY_COUNT as u8 {
                let ratio = pal.category_fill(c).contrast(pal.category_label(c));
                assert!(ratio >= 4.5, "dark={} category {c}: {ratio:.2}", pal.dark);
            }
        }
    }

    #[test]
    fn titled_folder_draws_border_body_and_cycled_title() {
        let pal = Palette::light();
        for depth in 0..5u8 {
            let mut it = item(ItemKind::FolderTitled, Rect::new(10.0, 10.0, 200.0, 100.0));
            it.depth = depth;
            let mut c = RecordingCanvas::new(400, 400);
            draw_items(&mut c, &[it], &pal, &style(false), None);
            let fills: Vec<_> = c.fills().collect();
            assert_eq!(fills.len(), 3);
            assert_eq!(*fills[0].1, pal.border);
            assert_eq!(*fills[1].1, pal.bg);
            assert_eq!(*fills[2].1, pal.title[depth as usize % 4]);
            assert_eq!(fills[2].0.h, style(false).title_height());
        }
    }

    #[test]
    fn labels_are_drawn_only_when_enabled_and_they_fit() {
        let pal = Palette::light();
        let big = item(ItemKind::FolderTitled, Rect::new(0.0, 0.0, 300.0, 100.0));
        let mut c = RecordingCanvas::new(400, 400);
        draw_items(&mut c, std::slice::from_ref(&big), &pal, &style(true), None);
        let texts: Vec<_> = c.texts().collect();
        assert_eq!(texts, vec!["1 MB", "name"]);
        let mut c = RecordingCanvas::new(400, 400);
        draw_items(&mut c, &[big], &pal, &style(false), None);
        assert_eq!(c.texts().count(), 0);
        let tiny = item(ItemKind::FileTile, Rect::new(0.0, 0.0, 20.0, 10.0));
        let mut c = RecordingCanvas::new(400, 400);
        draw_items(&mut c, &[tiny], &pal, &style(true), None);
        assert_eq!(c.texts().count(), 0);
    }

    #[test]
    fn region_limits_what_is_drawn() {
        let pal = Palette::light();
        let a = item(ItemKind::FileTile, Rect::new(0.0, 0.0, 10.0, 10.0));
        let b = item(ItemKind::FileTile, Rect::new(100.0, 100.0, 10.0, 10.0));
        let mut c = RecordingCanvas::new(200, 200);
        draw_items(
            &mut c,
            &[a, b],
            &pal,
            &style(false),
            Some(Rect::new(95.0, 95.0, 20.0, 20.0)),
        );
        assert!(c.fills().all(|(r, _)| r.x >= 100.0));
    }

    #[test]
    fn deleted_and_unreadable_use_opposite_hatches() {
        let pal = Palette::light();
        let mut unreadable = item(ItemKind::Unreadable, Rect::new(0.0, 0.0, 100.0, 100.0));
        unreadable.label = None;
        let mut deleted = item(ItemKind::FileTile, Rect::new(0.0, 0.0, 100.0, 100.0));
        deleted.flags = Flags::DELETED;
        let angle = |it: Item| {
            let mut c = RecordingCanvas::new(200, 200);
            draw_items(&mut c, &[it], &pal, &style(false), None);
            c.ops.iter().find_map(|o| match o {
                Op::Pattern(_, h) => Some((h.angle, h.spacing)),
                _ => None,
            })
        };
        let (ua, us) = angle(unreadable).expect("unreadable hatch");
        let (da, ds) = angle(deleted).expect("deleted hatch");
        assert_eq!(ua, HatchAngle::Rising);
        assert_eq!(da, HatchAngle::Falling);
        assert!(ds < us, "deleted hatch is denser");
    }

    #[test]
    fn dimmed_items_get_a_veil() {
        let pal = Palette::light();
        let mut it = item(ItemKind::FileTile, Rect::new(0.0, 0.0, 30.0, 30.0));
        it.flags = Flags::DIMMED;
        let mut c = RecordingCanvas::new(100, 100);
        draw_items(&mut c, &[it], &pal, &style(false), None);
        let (_, last) = c.fills().last().expect("fill");
        assert_eq!(last.a, 191);
    }

    #[test]
    fn dashed_border_on_free_space() {
        let pal = Palette::dark();
        let it = item(ItemKind::Free, Rect::new(0.0, 0.0, 100.0, 60.0));
        let mut c = RecordingCanvas::new(200, 200);
        draw_items(&mut c, &[it], &pal, &style(false), None);
        assert!(
            c.ops
                .iter()
                .any(|o| matches!(o, Op::Stroke(_, s) if s.dash.is_some()))
        );
    }
}
