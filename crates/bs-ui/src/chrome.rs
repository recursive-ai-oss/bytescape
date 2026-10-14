//! Window chrome: the navigation bar on top, the status bar below, and the
//! map area between them.

use bs_render::{Bar, Rect};

/// Full text of the reserved link slot.
pub const LINK_TEXT: &str = "Find more free software at RecursiveAI.org";
/// Shortened text used in narrow windows.
pub const LINK_TEXT_SHORT: &str = "RecursiveAI.org";
/// Logical width under which the link text shortens.
pub const NARROW_WIDTH: f32 = 640.0;

const NAV_HEIGHT: f32 = 30.0;
const STATUS_HEIGHT: f32 = 26.0;

/// The bars for a window of `width × height` device pixels, and the map
/// rectangle they leave.
pub fn layout(
    width: f32,
    height: f32,
    scale: f32,
    breadcrumb: &str,
    info: &str,
) -> (Vec<Bar>, Rect) {
    let nav_h = (NAV_HEIGHT * scale).round();
    let status_h = (STATUS_HEIGHT * scale).round();
    let link = if width / scale < NARROW_WIDTH {
        LINK_TEXT_SHORT
    } else {
        LINK_TEXT
    };
    let nav = Bar {
        rect: Rect::new(0.0, 0.0, width, nav_h),
        left: (!breadcrumb.is_empty()).then(|| breadcrumb.into()),
        right: None,
        border_on_top: false,
    };
    let status = Bar {
        rect: Rect::new(0.0, height - status_h, width, status_h),
        left: (!info.is_empty()).then(|| info.into()),
        right: Some(link.into()),
        border_on_top: true,
    };
    let map = Rect::new(0.0, nav_h, width, (height - nav_h - status_h).max(0.0));
    (vec![nav, status], map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_sits_between_the_bars() {
        let (bars, map) = layout(1000.0, 700.0, 1.0, "C:", "");
        assert_eq!(bars.len(), 2);
        assert_eq!(map, Rect::new(0.0, 30.0, 1000.0, 644.0));
        assert_eq!(bars[1].rect.bottom(), 700.0);
    }

    #[test]
    fn link_shortens_only_in_narrow_windows() {
        let (wide, _) = layout(1000.0, 700.0, 1.0, "", "");
        assert_eq!(wide[1].right.as_deref(), Some(LINK_TEXT));
        let (narrow, _) = layout(600.0, 700.0, 1.0, "", "");
        assert_eq!(narrow[1].right.as_deref(), Some(LINK_TEXT_SHORT));
        // The threshold is in logical pixels: 1200 device px at 2x is narrow.
        let (hidpi, _) = layout(1200.0, 700.0, 2.0, "", "");
        assert_eq!(hidpi[1].right.as_deref(), Some(LINK_TEXT_SHORT));
    }

    #[test]
    fn bars_scale_with_the_display() {
        let (_, map1) = layout(1000.0, 700.0, 1.0, "", "");
        let (_, map2) = layout(2000.0, 1400.0, 2.0, "", "");
        assert_eq!(map2.y, map1.y * 2.0);
    }
}
