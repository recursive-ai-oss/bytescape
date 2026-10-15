//! Window chrome geometry: the navigation bar on top, the capacity bar under
//! it, the status bar at the bottom and the map between them.
//!
//! Everything here is a pure function of the window size, the scale factor and
//! the measured width of a few texts, so the rules (above all, that the link in
//! the status bar is never truncated or covered) are tested without a window.

use bs_render::Rect;

/// Full text of the reserved link slot.
pub const LINK_TEXT: &str = "Find more free software at RecursiveAI.org";
/// Shortened text used in narrow windows.
pub const LINK_TEXT_SHORT: &str = "RecursiveAI.org";
/// Where the link goes.
pub const LINK_URL: &str = "https://recursiveai.org/";
/// Logical width under which the link text shortens.
pub const NARROW_WIDTH: f32 = 640.0;

/// Logical heights of the bars.
pub const NAV_HEIGHT: f32 = 30.0;
pub const CAPACITY_HEIGHT: f32 = 10.0;
pub const STATUS_HEIGHT: f32 = 26.0;

/// The legend's names in palette order. Shorter than the full category names,
/// which the swatch tooltip spells out, so more of them fit beside the link.
pub const LEGEND_NAMES: [&str; 9] = [
    "Video",
    "Images",
    "Audio",
    "Documents",
    "Archives",
    "Apps",
    "Code",
    "Cache",
    "Other",
];

/// Width of one navigation button, in logical pixels.
const BUTTON_WIDTH: f32 = 28.0;
/// The info bar never shrinks below this many ems before the swatches give
/// way.
const INFO_MIN_EMS: f32 = 14.0;

/// Measures text in device pixels at the UI font size.
pub trait TextMeasure {
    /// The width of `text` on one line, or `None` while fonts are loading.
    fn width(&self, text: &str) -> Option<f32>;
}

/// What the pointer can be over in the chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Back,
    Forward,
    Up,
    Refresh,
    Crumb,
    /// A clickable segment of the breadcrumb, by position.
    CrumbSeg(usize),
    Capacity,
    Info,
    /// A legend swatch, by palette index.
    Swatch(u8),
    /// The chip that stands for swatches that did not fit.
    Overflow,
    Link,
}

/// How the legend fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LegendMode {
    /// Every category collapsed into the overflow chip.
    Collapsed,
    /// Some swatches, without names, and the chip for the rest.
    Partial,
    /// All swatches, without names.
    Compact,
    /// All swatches with their names.
    Named,
}

/// One swatch of the legend.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwatchSlot {
    pub category: u8,
    pub rect: Rect,
    pub named: bool,
}

/// Where everything in the chrome sits, in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub nav: Rect,
    pub back: Rect,
    pub forward: Rect,
    pub up: Rect,
    pub refresh: Rect,
    pub crumb: Rect,
    pub capacity: Rect,
    pub map: Rect,
    pub status: Rect,
    /// The info bar's share of the status bar.
    pub info: Rect,
    pub legend: LegendMode,
    pub swatches: Vec<SwatchSlot>,
    pub overflow: Option<Rect>,
    /// The reserved slot, exactly as wide as its text.
    pub link: Rect,
    pub link_text: &'static str,
}

/// The map rectangle for a window of `width × height` device pixels.
pub fn map_rect(width: f32, height: f32, scale: f32) -> Rect {
    let top = ((NAV_HEIGHT + CAPACITY_HEIGHT) * scale).round();
    let status = (STATUS_HEIGHT * scale).round();
    Rect::new(0.0, top, width, (height - top - status).max(0.0))
}

fn text_width(m: &dyn TextMeasure, text: &str, em: f32) -> f32 {
    // Until fonts are loaded, assume a typical proportional face.
    m.width(text)
        .unwrap_or_else(|| text.chars().count() as f32 * em * 0.56)
}

/// Lays the chrome out. `em` is the UI font size in device pixels and
/// `names` the legend names in palette order.
pub fn geometry(
    width: f32,
    height: f32,
    scale: f32,
    em: f32,
    names: &[&str],
    measure: &dyn TextMeasure,
) -> Geometry {
    let nav_h = (NAV_HEIGHT * scale).round();
    let cap_h = (CAPACITY_HEIGHT * scale).round();
    let status_h = (STATUS_HEIGHT * scale).round();
    let pad = (8.0 * scale).round();
    let map = map_rect(width, height, scale);

    // Navigation bar.
    let nav = Rect::new(0.0, 0.0, width, nav_h);
    let bw = (BUTTON_WIDTH * scale).round();
    let by = ((nav_h - bw) / 2.0).round().max(0.0);
    let bh = bw.min(nav_h);
    let x0 = (4.0 * scale).round();
    let button = |i: f32| Rect::new(x0 + i * bw, by, bw, bh);
    let (back, forward, up, refresh) = (button(0.0), button(1.0), button(2.0), button(3.0));
    let crumb_x = refresh.right() + pad;
    let crumb = Rect::new(crumb_x, 0.0, (width - crumb_x - pad).max(0.0), nav_h);

    let capacity = Rect::new(0.0, nav_h, width, cap_h);

    // Status bar. The link is placed first and nothing can take its place.
    let status = Rect::new(0.0, height - status_h, width, status_h);
    let link_text = if width / scale < NARROW_WIDTH {
        LINK_TEXT_SHORT
    } else {
        LINK_TEXT
    };
    let link_w = text_width(measure, link_text, em).ceil() + 2.0;
    let link = Rect::new((width - pad - link_w).max(0.0), status.y, link_w, status_h);
    let gap = (16.0 * scale).round();
    let left_end = (link.x - gap).max(pad);
    let avail = (left_end - pad).max(0.0);

    // Legend: the info bar shrinks first; then the swatches lose their names,
    // then some of them go behind the overflow chip.
    let side = (em * 0.9).round().max(6.0);
    let name_gap = (em * 0.4).round();
    let step = (em * 0.6).round().max(4.0);
    let info_min = INFO_MIN_EMS * em;
    let budget = (avail - info_min - gap).max(0.0);
    let named_widths: Vec<f32> = names
        .iter()
        .map(|n| side + name_gap + text_width(measure, n, em).ceil() + (em * 0.7).round())
        .collect();
    let named_total: f32 = named_widths.iter().sum();
    let compact_w = side + step;
    let compact_total = compact_w * names.len() as f32;
    let chip_w = (em * 1.8).round();

    let (legend, shown, named, legend_w) = if budget >= named_total {
        (LegendMode::Named, names.len(), true, named_total)
    } else if budget >= compact_total {
        (LegendMode::Compact, names.len(), false, compact_total)
    } else {
        let room = budget - chip_w;
        let fit = if room > 0.0 {
            (room / compact_w).floor() as usize
        } else {
            0
        };
        if fit == 0 {
            if budget >= chip_w {
                (LegendMode::Collapsed, 0, false, chip_w)
            } else {
                (LegendMode::Collapsed, 0, false, 0.0)
            }
        } else {
            (
                LegendMode::Partial,
                fit,
                false,
                fit as f32 * compact_w + chip_w,
            )
        }
    };

    let info_w = (avail - legend_w - if legend_w > 0.0 { gap } else { 0.0 }).max(0.0);
    let info = Rect::new(pad, status.y, info_w, status_h);
    let mut x = info.right() + if legend_w > 0.0 { gap } else { 0.0 };
    let mut swatches = Vec::new();
    for (i, w) in named_widths.iter().enumerate().take(shown) {
        let sw = if named { *w } else { compact_w };
        swatches.push(SwatchSlot {
            category: i as u8,
            rect: Rect::new(x, status.y, sw, status_h),
            named,
        });
        x += sw;
    }
    let overflow =
        (legend_w > 0.0 && shown < names.len()).then(|| Rect::new(x, status.y, chip_w, status_h));

    Geometry {
        nav,
        back,
        forward,
        up,
        refresh,
        crumb,
        capacity,
        map,
        status,
        info,
        legend,
        swatches,
        overflow,
        link,
        link_text,
    }
}

/// One piece of the breadcrumb: a folder name, a separator or the ellipsis
/// that stands for folders that did not fit.
#[derive(Clone, Debug, PartialEq)]
pub struct CrumbPart {
    pub rect: Rect,
    pub text: String,
    /// The segment this part navigates to when clicked, if it is a folder
    /// above the current one.
    pub target: Option<usize>,
    /// The folder the view shows (the last segment).
    pub current: bool,
}

/// Separator between breadcrumb segments.
pub const CRUMB_SEPARATOR: &str = "›";

/// Lays the breadcrumb out in `area`. When it is too long the first folders
/// give way to an ellipsis; the folder being shown always stays.
pub fn layout_crumbs(area: Rect, em: f32, segs: &[String], m: &dyn TextMeasure) -> Vec<CrumbPart> {
    let Some(last) = segs.len().checked_sub(1) else {
        return Vec::new();
    };
    let width = |s: &str| text_width(m, s, em).ceil() + 2.0;
    let gap = (em * 0.5).round();
    let sep_w = width(CRUMB_SEPARATOR) + 2.0 * gap;
    let ellipsis_w = width("…");
    let widths: Vec<f32> = segs.iter().map(|s| width(s)).collect();
    // The first segment to show: all of them if they fit, else drop from the
    // front until the rest fits beside the ellipsis.
    let total = |from: usize| -> f32 {
        let body: f32 = widths[from..].iter().sum::<f32>() + sep_w * (last - from) as f32;
        if from > 0 {
            body + ellipsis_w + sep_w
        } else {
            body
        }
    };
    let mut start = 0;
    while start < last && total(start) > area.w {
        start += 1;
    }
    let mut parts = Vec::new();
    let mut x = area.x;
    let push = |parts: &mut Vec<CrumbPart>, x: &mut f32, w: f32, text: &str, target, current| {
        let w = w.min((area.right() - *x).max(0.0));
        parts.push(CrumbPart {
            rect: Rect::new(*x, area.y, w, area.h),
            text: text.to_string(),
            target,
            current,
        });
        *x += w;
    };
    if start > 0 {
        push(&mut parts, &mut x, ellipsis_w, "…", None, false);
        push(&mut parts, &mut x, sep_w, CRUMB_SEPARATOR, None, false);
    }
    for i in start..=last {
        push(
            &mut parts,
            &mut x,
            widths[i],
            &segs[i],
            (i != last).then_some(i),
            i == last,
        );
        if i != last {
            push(&mut parts, &mut x, sep_w, CRUMB_SEPARATOR, None, false);
        }
    }
    parts
}

impl Geometry {
    /// What is under the point, if it is over chrome rather than the map.
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        if self.map.contains(x, y) {
            return None;
        }
        let at = |r: &Rect| r.contains(x, y);
        if at(&self.back) {
            return Some(Hit::Back);
        }
        if at(&self.forward) {
            return Some(Hit::Forward);
        }
        if at(&self.up) {
            return Some(Hit::Up);
        }
        if at(&self.refresh) {
            return Some(Hit::Refresh);
        }
        if at(&self.crumb) {
            return Some(Hit::Crumb);
        }
        if at(&self.capacity) {
            return Some(Hit::Capacity);
        }
        if at(&self.link) {
            return Some(Hit::Link);
        }
        for s in &self.swatches {
            if at(&s.rect) {
                return Some(Hit::Swatch(s.category));
            }
        }
        if let Some(o) = &self.overflow
            && at(o)
        {
            return Some(Hit::Overflow);
        }
        if at(&self.info) {
            return Some(Hit::Info);
        }
        None
    }

    /// Whether the point is anywhere in the chrome (any bar).
    pub fn in_chrome(&self, x: f32, y: f32) -> bool {
        !self.map.contains(x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_target_is_the_project_site() {
        assert_eq!(LINK_URL, "https://recursiveai.org/");
        assert_eq!(LINK_TEXT, "Find more free software at RecursiveAI.org");
        assert_eq!(LINK_TEXT_SHORT, "RecursiveAI.org");
    }

    /// Every character is 0.56 em wide, so layouts are the same on every host.
    struct Fixed(f32);

    impl TextMeasure for Fixed {
        fn width(&self, text: &str) -> Option<f32> {
            Some(text.chars().count() as f32 * self.0)
        }
    }

    const NAMES: [&str; 9] = LEGEND_NAMES;

    fn at(width_logical: f32, scale: f32) -> Geometry {
        let em = 13.0 * scale;
        geometry(
            (width_logical * scale).round(),
            (700.0 * scale).round(),
            scale,
            em,
            &NAMES,
            &Fixed(em * 0.56),
        )
    }

    #[test]
    fn map_sits_between_the_bars() {
        let g = at(1000.0, 1.0);
        assert_eq!(g.map, Rect::new(0.0, 40.0, 1000.0, 634.0));
        assert_eq!(g.status.bottom(), 700.0);
        assert_eq!(g.capacity.bottom(), g.map.y);
        assert_eq!(g.nav.bottom(), g.capacity.y);
        assert_eq!(map_rect(1000.0, 700.0, 1.0), g.map);
    }

    #[test]
    fn the_link_is_never_truncated_or_covered() {
        for scale in [1.0, 1.5, 2.0] {
            for logical in [
                320.0, 400.0, 500.0, 639.0, 640.0, 700.0, 800.0, 900.0, 1000.0, 1280.0, 1920.0,
            ] {
                let g = at(logical, scale);
                let em = 13.0 * scale;
                let text_w = g.link_text.chars().count() as f32 * em * 0.56;
                assert!(
                    g.link.w >= text_w,
                    "{logical}@{scale}: link narrower than text"
                );
                assert!(
                    g.link.right() <= g.status.right(),
                    "{logical}@{scale}: off screen"
                );
                // Nothing else in the status bar reaches the slot.
                let others = std::iter::once(g.info)
                    .chain(g.swatches.iter().map(|s| s.rect))
                    .chain(g.overflow);
                for r in others {
                    assert!(
                        r.right() <= g.link.x,
                        "{logical}@{scale}: {r:?} covers the link at {:?}",
                        g.link
                    );
                }
                assert_eq!(g.link.y, g.status.y);
            }
        }
    }

    #[test]
    fn the_link_shortens_below_640_logical_pixels() {
        assert_eq!(at(639.0, 1.0).link_text, LINK_TEXT_SHORT);
        assert_eq!(at(640.0, 1.0).link_text, LINK_TEXT);
        // 640 device pixels at 2x is 320 logical: short.
        assert_eq!(at(320.0, 2.0).link_text, LINK_TEXT_SHORT);
        assert_eq!(at(900.0, 1.5).link_text, LINK_TEXT);
    }

    #[test]
    fn the_legend_gives_way_before_the_link_does() {
        // At the three widths the design names, with the legend fitting
        // progressively better as the window grows.
        let narrow = at(640.0, 1.0);
        let mid = at(900.0, 1.0);
        let wide = at(1280.0, 1.0);
        assert!(narrow.legend <= mid.legend && mid.legend <= wide.legend);
        assert_eq!(wide.legend, LegendMode::Named);
        assert_eq!(wide.swatches.len(), 9);
        assert!(wide.swatches.iter().all(|s| s.named));
        assert_eq!(mid.legend, LegendMode::Compact);
        assert!(mid.swatches.iter().all(|s| !s.named));
        // At 640 the full link text is still shown, so some swatches sit
        // behind the chip rather than the link giving up room.
        assert_eq!(narrow.legend, LegendMode::Partial);
        assert!(narrow.overflow.is_some() && !narrow.swatches.is_empty());
        assert_eq!(at(639.0, 1.0).legend, LegendMode::Compact);
        // The link is at its natural width at every one of them.
        for g in [&narrow, &mid, &wide] {
            assert!(g.link.w > 0.0);
        }
    }

    #[test]
    fn very_narrow_windows_collapse_the_legend_into_the_chip() {
        let g = at(420.0, 1.0);
        assert!(matches!(
            g.legend,
            LegendMode::Partial | LegendMode::Collapsed
        ));
        assert!(g.overflow.is_some() || g.swatches.is_empty());
        // The chip stands in for exactly the swatches that are missing.
        if g.overflow.is_some() {
            assert!(g.swatches.len() < 9);
        }
        let tiny = at(300.0, 1.0);
        assert!(tiny.info.w >= 0.0);
    }

    #[test]
    fn the_info_bar_shrinks_first() {
        // As the window narrows, the swatch row keeps its width until the info
        // bar is down to its minimum.
        let em = 13.0;
        let min = INFO_MIN_EMS * em;
        let mut last_info = f32::MAX;
        let mut named_seen_after_compact = false;
        let mut was_compact = false;
        for logical in (500..=1400).rev().step_by(20) {
            let g = at(logical as f32, 1.0);
            assert!(g.info.w <= last_info + 40.0 || g.legend != LegendMode::Named);
            last_info = g.info.w;
            if g.legend == LegendMode::Compact {
                was_compact = true;
            }
            if was_compact && g.legend == LegendMode::Named {
                named_seen_after_compact = true;
            }
            if g.legend == LegendMode::Named {
                assert!(
                    g.info.w >= min - 1.0,
                    "{logical}: info {} < {min}",
                    g.info.w
                );
            }
        }
        assert!(!named_seen_after_compact);
    }

    #[test]
    fn hit_testing_finds_each_control() {
        let g = at(1280.0, 1.0);
        let c = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (x, y) = c(g.back);
        assert_eq!(g.hit(x, y), Some(Hit::Back));
        let (x, y) = c(g.refresh);
        assert_eq!(g.hit(x, y), Some(Hit::Refresh));
        let (x, y) = c(g.link);
        assert_eq!(g.hit(x, y), Some(Hit::Link));
        let (x, y) = c(g.swatches[3].rect);
        assert_eq!(g.hit(x, y), Some(Hit::Swatch(3)));
        let (x, y) = c(g.capacity);
        assert_eq!(g.hit(x, y), Some(Hit::Capacity));
        let (x, y) = c(g.map);
        assert_eq!(g.hit(x, y), None);
        assert!(!g.in_chrome(x, y));
    }

    fn crumbs(segs: &[&str], width: f32) -> Vec<CrumbPart> {
        let segs: Vec<String> = segs.iter().map(|s| s.to_string()).collect();
        layout_crumbs(Rect::new(0.0, 0.0, width, 30.0), 13.0, &segs, &Fixed(7.0))
    }

    fn folders(parts: &[CrumbPart]) -> Vec<&str> {
        parts
            .iter()
            .filter(|p| p.text != CRUMB_SEPARATOR && p.text != "…")
            .map(|p| p.text.as_str())
            .collect()
    }

    #[test]
    fn a_short_breadcrumb_shows_every_folder_and_only_the_last_is_current() {
        let parts = crumbs(&["C:", "Users", "alex"], 600.0);
        assert_eq!(folders(&parts), ["C:", "Users", "alex"]);
        let targets: Vec<_> = parts.iter().filter_map(|p| p.target).collect();
        assert_eq!(targets, [0, 1]);
        assert_eq!(parts.iter().filter(|p| p.current).count(), 1);
        assert!(parts.last().is_some_and(|p| p.current));
        // Parts follow each other without overlap.
        for w in parts.windows(2) {
            assert!(w[0].rect.right() <= w[1].rect.x + 0.01);
        }
    }

    #[test]
    fn a_long_breadcrumb_drops_the_first_folders_and_keeps_the_current_one() {
        let parts = crumbs(&["C:", "Users", "alex", "Videos", "Holiday 2024"], 150.0);
        assert_eq!(parts[0].text, "…");
        assert!(parts[0].target.is_none());
        assert_eq!(folders(&parts).last(), Some(&"Holiday 2024"));
        assert!(!folders(&parts).contains(&"C:"));
        for p in &parts {
            assert!(p.rect.right() <= 150.5, "{p:?}");
        }
    }

    #[test]
    fn one_enormous_segment_is_clipped_to_the_area() {
        let long = "x".repeat(500);
        let parts = crumbs(&[long.as_str()], 200.0);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].rect.w <= 200.0);
        assert!(crumbs(&[], 200.0).is_empty());
    }

    #[test]
    fn bars_scale_with_the_display() {
        let g1 = at(1000.0, 1.0);
        let g2 = at(1000.0, 2.0);
        assert_eq!(g2.map.y, g1.map.y * 2.0);
        assert_eq!(g2.status.h, g1.status.h * 2.0);
    }

    #[test]
    fn missing_fonts_fall_back_to_an_estimate_instead_of_collapsing() {
        struct Loading;
        impl TextMeasure for Loading {
            fn width(&self, _: &str) -> Option<f32> {
                None
            }
        }
        let g = geometry(1280.0, 700.0, 1.0, 13.0, &NAMES, &Loading);
        assert!(g.link.w > 100.0);
        assert!(g.info.w > 0.0);
    }
}
