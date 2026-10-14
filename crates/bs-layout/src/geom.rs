//! Rectangles and the font-relative measures every threshold is built from.

/// A rectangle in device pixels. Layout output always has whole-pixel edges.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// A rectangle from its left, top, width and height.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// The right edge.
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    /// The bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Whether the rectangle covers no area (or is not a number).
    pub fn is_empty(&self) -> bool {
        !(self.w > 0.0 && self.h > 0.0)
    }

    /// Area in square device pixels.
    pub fn area(&self) -> f64 {
        f64::from(self.w) * f64::from(self.h)
    }

    /// Whether the point lies inside (left and top edges included).
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.right() && py < self.bottom()
    }

    /// Whether `other` lies entirely inside this rectangle.
    pub fn contains_rect(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// Whether the two rectangles share any area.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Edges are whole pixels held in `i32`; coordinates beyond this are clamped
/// so that every edge is exact in `f32`.
pub(crate) const COORD_LIMIT: f32 = 16_000_000.0;

/// A rectangle with whole-pixel edges, the form layout works in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PxRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl PxRect {
    pub fn w(&self) -> i32 {
        self.x1 - self.x0
    }

    pub fn h(&self) -> i32 {
        self.y1 - self.y0
    }

    pub fn to_rect(self) -> Rect {
        Rect::new(
            self.x0 as f32,
            self.y0 as f32,
            self.w() as f32,
            self.h() as f32,
        )
    }

    /// Rounds a viewport to whole pixels. `None` if it is not finite or
    /// rounds to nothing.
    pub fn from_viewport(r: Rect) -> Option<PxRect> {
        if !(r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite()) {
            return None;
        }
        let edge = |v: f32| v.clamp(-COORD_LIMIT, COORD_LIMIT).round() as i32;
        let x0 = edge(r.x);
        let y0 = edge(r.y);
        let x1 = edge(r.x + r.w);
        let y1 = edge(r.y + r.h);
        (x1 > x0 && y1 > y0).then_some(PxRect { x0, y0, x1, y1 })
    }
}

/// Font and display scale. Every threshold is a multiple of `em`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// The system UI font size in device pixels.
    pub em: f32,
    /// Device pixels per logical pixel.
    pub scale: f32,
}

impl Metrics {
    /// Metrics from a font size in logical pixels and a display scale.
    pub fn from_logical(font_logical_px: f32, scale: f32) -> Metrics {
        Metrics {
            em: font_logical_px * scale,
            scale,
        }
        .sanitized()
    }

    /// Replaces unusable values (zero, negative, NaN, infinite) so that
    /// layout never divides by them: a scale of 1, an em of 13 px.
    pub fn sanitized(self) -> Metrics {
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale.min(64.0)
        } else {
            1.0
        };
        let em = if self.em.is_finite() && self.em > 0.0 {
            self.em.min(1024.0)
        } else {
            13.0 * scale
        };
        Metrics { em, scale }
    }
}

/// The size thresholds of the layout, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// Width from which a folder is drawn with a title bar and children.
    pub nest_w: f32,
    /// Height from which a folder is drawn with a title bar and children.
    pub nest_h: f32,
    /// Width from which a block carries a label.
    pub label_w: f32,
    /// Height from which a block carries a label.
    pub label_h: f32,
    /// Side of the square below which a sibling is merged into a group.
    pub group_side: f32,
    /// Smallest drawn side: anything narrower or shorter is not drawn.
    pub min_tile: f32,
}

impl Thresholds {
    /// The thresholds for the given metrics.
    pub fn new(m: Metrics) -> Thresholds {
        let m = m.sanitized();
        Thresholds {
            nest_w: 5.0 * m.em,
            nest_h: 3.2 * m.em,
            label_w: 3.5 * m.em,
            label_h: 1.6 * m.em,
            group_side: 2.5 * m.em,
            min_tile: 2.0,
        }
    }
}

/// Height of a folder's title bar in device pixels.
pub fn title_height(m: Metrics) -> f32 {
    (m.sanitized().em * 1.6).round()
}

/// Padding between a folder's border and its children, in device pixels.
pub fn padding(m: Metrics) -> f32 {
    (2.0 * m.sanitized().scale).round().max(1.0)
}

/// The area inside a nested folder's box where its children go: inside the
/// 1 px border, below the title bar, and inside the padding. Empty (zero
/// size) when the box is too small to have one.
pub fn content_rect(folder: Rect, m: Metrics) -> Rect {
    px_content(
        PxRect {
            x0: folder.x.round() as i32,
            y0: folder.y.round() as i32,
            x1: folder.right().round() as i32,
            y1: folder.bottom().round() as i32,
        },
        m,
    )
    .map_or(Rect::new(folder.x, folder.y, 0.0, 0.0), PxRect::to_rect)
}

pub(crate) fn px_content(r: PxRect, m: Metrics) -> Option<PxRect> {
    let pad = padding(m) as i32;
    let title = title_height(m) as i32;
    let c = PxRect {
        x0: r.x0.checked_add(1 + pad)?,
        y0: r.y0.checked_add(1 + title + pad)?,
        x1: r.x1.checked_sub(1 + pad)?,
        y1: r.y1.checked_sub(1 + pad)?,
    };
    (c.x1 > c.x0 && c.y1 > c.y0).then_some(c)
}
