//! The hover card's words and geometry: what it says about a box, how the
//! text wraps, where the buttons sit, and where the card goes so that it
//! never covers the box it describes.

use bs_knowledge::Handoff;
use bs_render::{Card, CardPart, CardRole, Rect};

use crate::action::{Action, reason};
use crate::chrome::TextMeasure;
use crate::info::percent;

/// What the card is built from. A [`crate::ViewSource`] fills it in.
#[derive(Clone, Debug, PartialEq)]
pub struct CardData {
    /// The name, converted for display.
    pub name: String,
    /// The full path, converted for display.
    pub path: String,
    /// Allocated bytes ("size on disk"), as counted so far.
    pub bytes: u64,
    /// Logical bytes, when known.
    pub logical: Option<u64>,
    /// Allocated bytes of the folder that holds it.
    pub parent_bytes: Option<u64>,
    /// Files inside.
    pub files: u64,
    /// Still being counted: the sizes are lower bounds.
    pub in_progress: bool,
    /// The scan was stopped before this was counted completely.
    pub stopped: bool,
    /// The size is an estimate.
    pub estimated: bool,
    pub is_folder: bool,
    /// Seconds since 1970 (UTC), when known.
    pub modified: Option<i64>,
    /// Seconds the user's clock is ahead of UTC at that moment, so the date
    /// shown is the one on their calendar.
    pub utc_offset: i32,
    /// The category's name.
    pub category: Option<&'static str>,
    /// What the place is and who manages it.
    pub note: Option<&'static str>,
    /// The tool that owns the place, if the table names one.
    pub handoff: Option<bs_knowledge::Handoff>,
    /// Another name of the same file shares its data.
    pub hard_link: bool,
    pub delete_state: DeleteMark,
    /// The subtree is fully counted, so actions are allowed.
    pub complete: bool,
    /// Part of the operating system: never offered for deletion.
    pub blocked: bool,
}

/// Whether a box is already gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMark {
    None,
    InBin,
    Deleted,
}

/// What the pointer can be over in the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardHit {
    Path,
    Button(Action),
}

/// What changes while the card is up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CardState {
    pub hot: Option<CardHit>,
    pub copied: bool,
}

/// A built card and where its controls are.
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    pub card: Card,
    /// Clickable parts, in device pixels.
    pub hits: Vec<(Rect, CardHit)>,
    /// The box the card describes.
    pub anchor: Rect,
}

impl Built {
    /// The control under a point.
    pub fn hit(&self, x: f32, y: f32) -> Option<CardHit> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, h)| *h)
    }
}

/// Sizes the card is drawn with.
#[derive(Clone, Copy)]
pub struct Metrics<'a> {
    /// The UI font size in device pixels.
    pub em: f32,
    pub scale: f32,
    pub measure: &'a dyn TextMeasure,
}

/// The widest the note can wrap to.
const NOTE_LINES: usize = 5;

/// The name of the bin on this system, for the words on the card.
pub fn bin_name() -> &'static str {
    if cfg!(windows) {
        "Recycle Bin"
    } else {
        "Trash"
    }
}

/// The date of `secs` since 1970 as `2024-03-05`, in UTC (add the zone's
/// offset first for a local date). `None` for times
/// that cannot be a real file date.
pub fn format_date(secs: i64) -> Option<String> {
    // Between the years 1 and 9999.
    if !(-62_135_596_800..=253_402_300_799).contains(&secs) {
        return None;
    }
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Breaks `text` into lines no wider than `width`, at spaces. A word wider
/// than the line stays whole on a line of its own (drawing shortens it). More
/// than `max_lines` lines end in an ellipsis.
pub fn wrap(text: &str, width: f32, max_lines: usize, m: &dyn TextMeasure, em: f32) -> Vec<String> {
    let w = |s: &str| {
        m.width(s)
            .unwrap_or_else(|| s.chars().count() as f32 * em * 0.56)
    };
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
            continue;
        }
        let trial = format!("{cur} {word}");
        if w(&trial) <= width {
            cur = trial;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines && max_lines > 0 {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines
}

/// Why `action` is not available on the box `data` describes, if it is not.
pub fn why_not(data: &CardData, action: Action) -> Option<&'static str> {
    let gone = match data.delete_state {
        DeleteMark::InBin => Some(reason::MOVED),
        DeleteMark::Deleted => Some(reason::DELETED),
        DeleteMark::None => None,
    };
    match action {
        Action::Delete | Action::DeletePermanently => {
            if data.blocked {
                Some(reason::OPERATING_SYSTEM)
            } else if !data.complete {
                Some(if data.stopped {
                    reason::SCAN_STOPPED
                } else {
                    reason::SCANNING
                })
            } else {
                gone
            }
        }
        // Uninstall and the package lookup are not wired up yet; a button
        // that could only fail says so instead.
        Action::Handoff(Handoff::Uninstall | Handoff::Package) => Some(reason::NOT_YET),
        _ => gone,
    }
}

/// The buttons on a card, with the reason each disabled one gives.
pub fn buttons(data: &CardData) -> Vec<(Action, Option<&'static str>)> {
    let mut acts = vec![Action::Delete, Action::ShowInFileManager, Action::Open];
    if let Some(h) = data.handoff {
        acts.push(Action::Handoff(h));
    }
    acts.into_iter().map(|a| (a, why_not(data, a))).collect()
}

/// Where a card of `size` goes: beside the box when it fits there (right
/// first, then left, below, above), centered on the pointer along the side,
/// always inside `bounds`. When no side has room because the box fills the
/// window, the card goes inside the box, beside the pointer and never over it.
pub fn place(anchor: Rect, size: (f32, f32), bounds: Rect, gap: f32, pointer: (f32, f32)) -> Rect {
    let (w, h) = size;
    let (px, py) = pointer;
    let clamp_x = |x: f32| x.clamp(bounds.x, (bounds.right() - w).max(bounds.x));
    let clamp_y = |y: f32| y.clamp(bounds.y, (bounds.bottom() - h).max(bounds.y));
    let right = Rect::new(anchor.right() + gap, clamp_y(py - h / 3.0), w, h);
    let left = Rect::new(anchor.x - gap - w, clamp_y(py - h / 3.0), w, h);
    let below = Rect::new(clamp_x(px - w / 2.0), anchor.bottom() + gap, w, h);
    let above = Rect::new(clamp_x(px - w / 2.0), anchor.y - gap - h, w, h);
    let fits = |r: &Rect| {
        r.x >= bounds.x - 0.01
            && r.y >= bounds.y - 0.01
            && r.right() <= bounds.right() + 0.01
            && r.bottom() <= bounds.bottom() + 0.01
    };
    for r in [right, left, below, above] {
        if fits(&r) {
            return r;
        }
    }
    // The box fills the window. Put the card beside the pointer, on whichever
    // side has more room, so the pointer stays visible.
    let off = gap * 2.0;
    let x = if px + off + w <= bounds.right() {
        px + off
    } else {
        px - off - w
    };
    let y = if py + off + h <= bounds.bottom() {
        py + off
    } else {
        py - off - h
    };
    Rect::new(clamp_x(x), clamp_y(y), w, h)
}

/// Builds the card for `data`. `drive_total` is the size of the whole drive,
/// `anchor` the box described, `pointer` where the pointer rests and `bounds`
/// the area the card may use (the map).
pub fn build(
    data: &CardData,
    drive_total: Option<u64>,
    anchor: Rect,
    pointer: (f32, f32),
    bounds: Rect,
    m: Metrics<'_>,
    state: CardState,
) -> Built {
    let em = m.em;
    let pad = (em * 0.7).round();
    let line_h = (em * 1.5).round();
    let title_h = (em * 1.8).round();
    let button_h = (em * 2.0).round();
    let button_gap = (em * 0.5).round();
    let section = (em * 0.5).round();
    let margin = (8.0 * m.scale).round();
    let width = (em * 24.0)
        .min((bounds.w - 2.0 * margin).max(em * 10.0))
        .round();
    let inner = width - 2.0 * pad;
    let text_w = |s: &str| {
        m.measure
            .width(s)
            .unwrap_or_else(|| s.chars().count() as f32 * em * 0.56)
    };

    // Laid out in the card's own coordinates, then moved into place.
    let mut parts: Vec<CardPart> = Vec::new();
    let mut hits: Vec<(Rect, CardHit)> = Vec::new();
    let mut y = pad;
    fn line(
        parts: &mut Vec<CardPart>,
        pad: f32,
        inner: f32,
        y: &mut f32,
        text: String,
        role: CardRole,
        h: f32,
    ) {
        parts.push(CardPart::Line {
            rect: Rect::new(pad, *y, inner, h),
            text: text.into(),
            role,
        });
        *y += h;
    }

    line(
        &mut parts,
        pad,
        inner,
        &mut y,
        data.name.clone(),
        CardRole::Title,
        title_h,
    );

    let bound = if data.in_progress {
        "≥ "
    } else if data.estimated {
        "≈ "
    } else {
        ""
    };
    let mut size = format!("{bound}{} on disk", bs_layout::format_size(data.bytes));
    if let Some(l) = data.logical {
        size.push_str(&format!(" · {bound}{} logical", bs_layout::format_size(l)));
    }
    line(
        &mut parts,
        pad,
        inner,
        &mut y,
        size,
        CardRole::Normal,
        line_h,
    );

    let mut shares: Vec<String> = Vec::new();
    if let Some(p) = data.parent_bytes.and_then(|w| percent(data.bytes, w)) {
        shares.push(format!("{bound}{p} of parent"));
    }
    if let Some(p) = drive_total.and_then(|w| percent(data.bytes, w)) {
        shares.push(format!("{bound}{p} of drive"));
    }
    if data.is_folder {
        shares.push(format!(
            "{} {}",
            bs_layout::format_count(data.files),
            if data.files == 1 { "file" } else { "files" }
        ));
    }
    if !shares.is_empty() {
        line(
            &mut parts,
            pad,
            inner,
            &mut y,
            shares.join(" · "),
            CardRole::Normal,
            line_h,
        );
    }

    let mut facts: Vec<String> = Vec::new();
    if let Some(c) = data.category {
        facts.push(c.to_string());
    }
    if let Some(d) = data
        .modified
        .and_then(|m| m.checked_add(i64::from(data.utc_offset)))
        .and_then(format_date)
    {
        facts.push(format!("modified {d}"));
    }
    if !facts.is_empty() {
        let mut s = facts.join(" · ");
        // The sentence starts with a capital letter.
        let mut chars = s.chars();
        if let Some(first) = chars.next() {
            s = first.to_uppercase().chain(chars).collect();
        }
        line(
            &mut parts,
            pad,
            inner,
            &mut y,
            s,
            CardRole::Secondary,
            line_h,
        );
    }

    match data.delete_state {
        DeleteMark::InBin => line(
            &mut parts,
            pad,
            inner,
            &mut y,
            format!("Moved to {}", bin_name()),
            CardRole::Normal,
            line_h,
        ),
        DeleteMark::Deleted => line(
            &mut parts,
            pad,
            inner,
            &mut y,
            "Deleted".into(),
            CardRole::Normal,
            line_h,
        ),
        DeleteMark::None => {}
    }

    let mut notes: Vec<(String, CardRole)> = Vec::new();
    if let Some(n) = data.note {
        notes.push((n.to_string(), CardRole::Normal));
    }
    if data.in_progress {
        notes.push((
            "Still scanning: the size is what has been counted so far.".into(),
            CardRole::Secondary,
        ));
    }
    if data.estimated {
        notes.push((
            "The size on disk is an estimate on this file system.".into(),
            CardRole::Secondary,
        ));
    }
    if data.hard_link {
        notes.push((
            "Another name of this file shares its data, so deleting this name may free little or nothing."
                .into(),
            CardRole::Secondary,
        ));
    }
    for (note, role) in notes {
        y += (section / 2.0).round();
        for l in wrap(&note, inner, NOTE_LINES, m.measure, em) {
            line(&mut parts, pad, inner, &mut y, l, role, line_h);
        }
    }

    // The path, to copy.
    y += section;
    let path_rect = Rect::new(pad, y, inner, line_h);
    parts.push(CardPart::Path {
        rect: path_rect,
        text: data.path.clone().into(),
        hot: state.hot == Some(CardHit::Path),
        copied: state.copied,
    });
    hits.push((path_rect, CardHit::Path));
    y += line_h + section;

    parts.push(CardPart::Rule {
        rect: Rect::new(pad, y, inner, m.scale.round().max(1.0)),
    });
    y += section + m.scale.round().max(1.0);

    // Buttons flow left to right and wrap.
    let btns = buttons(data);
    let btn_pad = (em * 0.9).round();
    let mut x = pad;
    let mut disabled: Vec<(&'static str, &'static str)> = Vec::new();
    for (action, why) in &btns {
        let bw = (text_w(action.label()).ceil() + 2.0 * btn_pad).min(inner);
        if x > pad && x + bw > pad + inner {
            x = pad;
            y += button_h + button_gap;
        }
        let r = Rect::new(x, y, bw, button_h);
        parts.push(CardPart::Button {
            rect: r,
            label: action.label().into(),
            enabled: why.is_none(),
            hot: state.hot == Some(CardHit::Button(*action)),
        });
        if why.is_none() {
            hits.push((r, CardHit::Button(*action)));
        } else if let Some(why) = why {
            disabled.push((action.label(), why));
        }
        x += bw + button_gap;
    }
    y += button_h;

    // Every disabled button says why.
    let mut reasons: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for (label, why) in disabled {
        match reasons.iter_mut().find(|(r, _)| *r == why) {
            Some((_, labels)) => labels.push(label),
            None => reasons.push((why, vec![label])),
        }
    }
    for (why, labels) in reasons {
        y += (section / 2.0).round();
        let text = format!("{} unavailable: {why}", labels.join(", "));
        for l in wrap(&text, inner, 3, m.measure, em) {
            line(
                &mut parts,
                pad,
                inner,
                &mut y,
                l,
                CardRole::Secondary,
                line_h,
            );
        }
    }
    let height = y + pad;

    let rect = place(anchor, (width, height), bounds, margin, pointer);
    let (dx, dy) = (rect.x, rect.y);
    let shift = |r: Rect| Rect::new(r.x + dx, r.y + dy, r.w, r.h);
    for p in &mut parts {
        match p {
            CardPart::Line { rect, .. }
            | CardPart::Path { rect, .. }
            | CardPart::Button { rect, .. }
            | CardPart::Row { rect, .. }
            | CardPart::Hold { rect, .. }
            | CardPart::Rule { rect } => *rect = shift(*rect),
        }
    }
    for (r, _) in &mut hits {
        *r = shift(*r);
    }
    Built {
        card: Card { rect, parts },
        hits,
        anchor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(f32);
    impl TextMeasure for Fixed {
        fn width(&self, text: &str) -> Option<f32> {
            Some(text.chars().count() as f32 * self.0)
        }
    }

    fn data() -> CardData {
        CardData {
            name: "Videos".into(),
            path: "/home/alex/Videos".into(),
            bytes: 41_200_000_000,
            logical: Some(40_900_000_000),
            parent_bytes: Some(100_000_000_000),
            files: 1234,
            in_progress: false,
            stopped: false,
            estimated: false,
            is_folder: true,
            modified: Some(1_709_596_800),
            utc_offset: 0,
            category: Some("Video"),
            note: None,
            handoff: None,
            hard_link: false,
            delete_state: DeleteMark::None,
            complete: true,
            blocked: false,
        }
    }

    const BOUNDS: Rect = Rect {
        x: 0.0,
        y: 40.0,
        w: 1000.0,
        h: 600.0,
    };

    fn build_at(d: &CardData, anchor: Rect, pointer: (f32, f32)) -> Built {
        let measure = Fixed(13.0 * 0.56);
        build(
            d,
            Some(500_000_000_000),
            anchor,
            pointer,
            BOUNDS,
            Metrics {
                em: 13.0,
                scale: 1.0,
                measure: &measure,
            },
            CardState::default(),
        )
    }

    fn texts(b: &Built) -> Vec<String> {
        b.card
            .parts
            .iter()
            .filter_map(|p| match p {
                CardPart::Line { text, .. } | CardPart::Path { text, .. } => Some(text.to_string()),
                CardPart::Button { label, .. }
                | CardPart::Row { label, .. }
                | CardPart::Hold { label, .. } => Some(label.to_string()),
                CardPart::Rule { .. } => None,
            })
            .collect()
    }

    #[test]
    fn dates_are_correct_across_eras() {
        assert_eq!(format_date(0).as_deref(), Some("1970-01-01"));
        assert_eq!(format_date(1_709_596_800).as_deref(), Some("2024-03-05"));
        assert_eq!(format_date(951_782_400).as_deref(), Some("2000-02-29"));
        assert_eq!(format_date(-86_400).as_deref(), Some("1969-12-31"));
        assert_eq!(format_date(253_402_300_799).as_deref(), Some("9999-12-31"));
        assert_eq!(format_date(i64::MAX), None);
        assert_eq!(format_date(i64::MIN), None);
    }

    #[test]
    fn the_modified_date_is_the_one_on_the_users_calendar() {
        // 2024-03-05 00:00 UTC is still the 4th in a zone an hour behind and
        // already the 5th in one ahead.
        let mut behind = data();
        behind.utc_offset = -3600;
        let t = texts(&build_at(
            &behind,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert_eq!(t[3], "Video · modified 2024-03-04");
        let mut ahead = data();
        ahead.utc_offset = 5 * 3600;
        let t = texts(&build_at(
            &ahead,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert_eq!(t[3], "Video · modified 2024-03-05");
        // A hostile offset cannot overflow.
        let mut odd = data();
        odd.modified = Some(i64::MAX);
        odd.utc_offset = i32::MAX;
        let _ = texts(&build_at(
            &odd,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
    }

    #[test]
    fn handoffs_that_are_not_wired_up_say_so_instead_of_failing() {
        let d = data();
        for h in [Handoff::Uninstall, Handoff::Package] {
            assert_eq!(
                why_not(&d, Action::Handoff(h)),
                Some("Not available in this version"),
                "{h:?}"
            );
        }
        assert_eq!(why_not(&d, Action::Handoff(Handoff::DiskCleanup)), None);
        let mut with = data();
        with.handoff = Some(Handoff::Uninstall);
        let b = buttons(&with);
        let (_, reason) = b
            .iter()
            .find(|(a, _)| matches!(a, Action::Handoff(_)))
            .expect("the button is listed");
        assert_eq!(*reason, Some("Not available in this version"));
    }

    #[test]
    fn the_card_says_the_facts_of_a_folder() {
        let b = build_at(&data(), Rect::new(10.0, 100.0, 100.0, 100.0), (50.0, 150.0));
        let t = texts(&b);
        assert_eq!(t[0], "Videos");
        assert_eq!(t[1], "41.2 GB on disk · 40.9 GB logical");
        assert_eq!(t[2], "41% of parent · 8.2% of drive · 1,234 files");
        assert_eq!(t[3], "Video · modified 2024-03-05");
        assert!(t.contains(&"/home/alex/Videos".to_string()));
        for label in ["Delete", Action::ShowInFileManager.label(), "Open"] {
            assert!(t.iter().any(|s| s == label), "{label}");
        }
    }

    #[test]
    fn counting_and_estimates_are_never_hidden() {
        let mut d = data();
        d.in_progress = true;
        let t = texts(&build_at(
            &d,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert!(t[1].starts_with("≥ 41.2 GB on disk"), "{}", t[1]);
        assert!(t[2].contains("≥ 41% of parent"));
        assert!(t.iter().any(|s| s.starts_with("Still scanning")));
        let mut d = data();
        d.estimated = true;
        let t = texts(&build_at(
            &d,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert!(t[1].starts_with("≈ 41.2 GB"), "{}", t[1]);
    }

    #[test]
    fn the_note_and_the_hard_link_caveat_appear() {
        let mut d = data();
        d.note = Some("Thumbnails kept by the desktop. The desktop manages them.");
        d.hard_link = true;
        d.is_folder = false;
        let t = texts(&build_at(
            &d,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert!(t.iter().any(|s| s.starts_with("Thumbnails kept")));
        assert!(t.join(" ").contains("shares its data"));
        // No file count for a file.
        assert!(!t.iter().any(|s| s.contains("files")));
    }

    #[test]
    fn a_long_note_wraps_and_ends_in_an_ellipsis_past_the_limit() {
        let m = Fixed(7.0);
        let long = "word ".repeat(200);
        let lines = wrap(&long, 150.0, 5, &m, 13.0);
        assert_eq!(lines.len(), 5);
        assert!(lines[4].ends_with('…'));
        assert!(
            lines
                .iter()
                .all(|l| l.chars().count() as f32 * 7.0 <= 150.0 + 7.0)
        );
        // A word wider than the line stays whole; drawing shortens it.
        let lines = wrap("a verylongwordthatdoesnotfitanywhere b", 50.0, 5, &m, 13.0);
        assert_eq!(lines, ["a", "verylongwordthatdoesnotfitanywhere", "b"]);
        assert!(wrap("", 50.0, 5, &m, 13.0).is_empty());
        // Hostile text: zero-width and combining characters, RTL, no spaces.
        let hostile = "\u{200b}\u{0301}\u{202e}abc\u{200d}".repeat(40);
        assert!(!wrap(&hostile, 100.0, 3, &m, 13.0).is_empty());
    }

    #[test]
    fn disabled_buttons_say_why_and_cannot_be_hit() {
        let mut d = data();
        d.complete = false;
        let b = build_at(&d, Rect::new(10.0, 100.0, 100.0, 100.0), (50.0, 150.0));
        let t = texts(&b);
        assert!(
            t.iter().any(|s| s == "Delete unavailable: Still scanning"),
            "{t:?}"
        );
        assert!(
            !b.hits
                .iter()
                .any(|(_, h)| *h == CardHit::Button(Action::Delete))
        );
        assert!(
            b.hits
                .iter()
                .any(|(_, h)| *h == CardHit::Button(Action::Open))
        );

        let mut d = data();
        d.blocked = true;
        let t = texts(&build_at(
            &d,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert!(t.join(" ").contains("Part of the operating system"));

        let mut d = data();
        d.delete_state = DeleteMark::InBin;
        let t = texts(&build_at(
            &d,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert!(t.iter().any(|s| s.contains("Already moved")));
        assert!(t.iter().any(|s| s.starts_with("Moved to ")));
    }

    #[test]
    fn a_handoff_adds_a_button() {
        let mut d = data();
        d.handoff = Some(bs_knowledge::Handoff::Uninstall);
        let t = texts(&build_at(
            &d,
            Rect::new(10.0, 100.0, 100.0, 100.0),
            (50.0, 150.0),
        ));
        assert!(t.iter().any(|s| s == "Uninstall…"));
    }

    #[test]
    fn hits_find_the_path_and_the_buttons() {
        let b = build_at(&data(), Rect::new(10.0, 100.0, 100.0, 100.0), (50.0, 150.0));
        let (r, _) = b
            .hits
            .iter()
            .find(|(_, h)| *h == CardHit::Path)
            .copied()
            .expect("path");
        assert_eq!(b.hit(r.x + 1.0, r.y + 1.0), Some(CardHit::Path));
        assert_eq!(b.hit(-5.0, -5.0), None);
        for (r, h) in &b.hits {
            assert!(b.card.rect.contains(r.x + 1.0, r.y + 1.0), "{h:?} outside");
            assert!(r.right() <= b.card.rect.right() + 0.5);
        }
    }

    // ---- placement ----------------------------------------------------

    const SIZE: (f32, f32) = (300.0, 200.0);

    fn check(anchor: Rect, pointer: (f32, f32)) -> Rect {
        let r = place(anchor, SIZE, BOUNDS, 8.0, pointer);
        assert!(r.x >= BOUNDS.x && r.right() <= BOUNDS.right(), "{r:?} x");
        assert!(r.y >= BOUNDS.y && r.bottom() <= BOUNDS.bottom(), "{r:?} y");
        assert!(!r.intersects(&anchor), "{r:?} covers {anchor:?}");
        r
    }

    #[test]
    fn the_card_goes_right_of_a_box_in_the_middle() {
        let a = Rect::new(400.0, 300.0, 100.0, 80.0);
        let r = check(a, (450.0, 340.0));
        assert!(r.x >= a.right());
    }

    #[test]
    fn at_the_right_edge_the_card_flips_left() {
        let a = Rect::new(900.0, 300.0, 100.0, 80.0);
        let r = check(a, (950.0, 340.0));
        assert!(r.right() <= a.x);
    }

    #[test]
    fn at_the_left_edge_it_stays_on_the_right() {
        let a = Rect::new(0.0, 300.0, 100.0, 80.0);
        let r = check(a, (50.0, 340.0));
        assert!(r.x >= a.right());
    }

    #[test]
    fn at_the_top_and_bottom_it_slides_inside_the_bounds() {
        let top = Rect::new(400.0, 40.0, 100.0, 40.0);
        let r = check(top, (450.0, 50.0));
        assert!(r.y >= BOUNDS.y);
        let bottom = Rect::new(400.0, 600.0, 100.0, 40.0);
        let r = check(bottom, (450.0, 630.0));
        assert!(r.bottom() <= BOUNDS.bottom());
    }

    #[test]
    fn in_every_corner_the_card_stays_inside_and_off_the_box() {
        for (ax, ay) in [(0.0, 40.0), (950.0, 40.0), (0.0, 600.0), (950.0, 600.0)] {
            let a = Rect::new(ax, ay, 50.0, 40.0);
            check(a, (ax + 25.0, ay + 20.0));
        }
    }

    #[test]
    fn a_wide_box_across_the_window_puts_the_card_above_or_below() {
        let a = Rect::new(0.0, 300.0, 1000.0, 40.0);
        let r = check(a, (500.0, 320.0));
        assert!(r.bottom() <= a.y || r.y >= a.bottom());
        // At the bottom edge there is only room above.
        let a = Rect::new(0.0, 590.0, 1000.0, 50.0);
        let r = check(a, (500.0, 610.0));
        assert!(r.bottom() <= a.y);
    }

    #[test]
    fn a_box_that_fills_the_window_gets_a_card_beside_the_pointer() {
        let a = BOUNDS;
        for pointer in [(500.0, 300.0), (990.0, 630.0), (5.0, 45.0)] {
            let r = place(a, SIZE, BOUNDS, 8.0, pointer);
            assert!(r.x >= BOUNDS.x && r.right() <= BOUNDS.right());
            assert!(r.y >= BOUNDS.y && r.bottom() <= BOUNDS.bottom());
            assert!(!r.contains(pointer.0, pointer.1), "{r:?} over {pointer:?}");
        }
    }

    #[test]
    fn a_window_smaller_than_the_card_does_not_panic() {
        let tiny = Rect::new(0.0, 0.0, 120.0, 90.0);
        let r = place(
            Rect::new(10.0, 10.0, 20.0, 20.0),
            SIZE,
            tiny,
            8.0,
            (15.0, 15.0),
        );
        assert!(r.x.is_finite() && r.y.is_finite());
    }

    #[test]
    fn built_cards_stay_in_the_bounds_at_the_edges() {
        for anchor in [
            Rect::new(0.0, 40.0, 60.0, 40.0),
            Rect::new(940.0, 40.0, 60.0, 40.0),
            Rect::new(0.0, 600.0, 60.0, 40.0),
            Rect::new(940.0, 600.0, 60.0, 40.0),
            Rect::new(480.0, 300.0, 60.0, 40.0),
        ] {
            let b = build_at(&data(), anchor, (anchor.x + 5.0, anchor.y + 5.0));
            let r = b.card.rect;
            assert!(r.x >= 0.0 && r.right() <= 1000.0, "{r:?}");
            assert!(r.y >= 40.0 && r.bottom() <= 640.0, "{r:?}");
            assert!(!r.intersects(&anchor), "{r:?} covers {anchor:?}");
        }
    }
}
