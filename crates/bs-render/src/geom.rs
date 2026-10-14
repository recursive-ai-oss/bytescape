//! Rectangles and colors shared by every drawing backend.

/// A rectangle in device pixels, with fractional edges allowed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        !(self.w > 0.0 && self.h > 0.0)
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.right() && py < self.bottom()
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r > x && b > y {
            Some(Rect::new(x, y, r - x, b - y))
        } else {
            None
        }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    pub fn inflate(&self, d: f32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    /// The smallest whole-pixel rectangle that covers this one.
    pub fn outer_pixels(&self) -> IRect {
        let x = self.x.floor() as i32;
        let y = self.y.floor() as i32;
        let r = self.right().ceil() as i32;
        let b = self.bottom().ceil() as i32;
        IRect::new(x, y, (r - x).max(0), (b - y).max(0))
    }

    /// The rectangle with every edge rounded to the nearest device pixel, so
    /// fills and hairlines stay crisp.
    pub fn snapped(&self) -> Rect {
        let x = self.x.round();
        let y = self.y.round();
        let r = self.right().round();
        let b = self.bottom().round();
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }
}

/// A rectangle in whole device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl IRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn intersect(&self, other: &IRect) -> Option<IRect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r > x && b > y {
            Some(IRect::new(x, y, r - x, b - y))
        } else {
            None
        }
    }

    pub fn union(&self, other: &IRect) -> IRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        IRect::new(x, y, r - x, b - y)
    }

    pub fn to_rect(&self) -> Rect {
        Rect::new(self.x as f32, self.y as f32, self.w as f32, self.h as f32)
    }
}

/// An sRGB color with straight (non-premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// `0xRRGGBB`, fully opaque.
    pub const fn hex(v: u32) -> Self {
        Self::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Opacity as a fraction, `0.0..=1.0`.
    pub fn with_opacity(self, f: f32) -> Self {
        self.with_alpha((f.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    /// Moves this color `t` of the way toward `other` (alpha is kept).
    pub fn mix(self, other: Color, t: f32) -> Color {
        let m = |a: u8, b: u8| {
            (a as f32 + (b as f32 - a as f32) * t)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Color {
            r: m(self.r, other.r),
            g: m(self.g, other.g),
            b: m(self.b, other.b),
            a: self.a,
        }
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f32 {
        fn lin(c: u8) -> f32 {
            let c = c as f32 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// WCAG contrast ratio between two colors, `1.0..=21.0`.
    pub fn contrast(self, other: Color) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Black or white, whichever reads better on this color.
    pub fn readable_text(self) -> Color {
        let black = Color::rgb(0, 0, 0);
        let white = Color::rgb(255, 255, 255);
        if self.contrast(black) >= self.contrast(white) {
            black
        } else {
            white
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_intersection_and_union() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), Some(Rect::new(5.0, 5.0, 5.0, 5.0)));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));
        assert_eq!(a.intersect(&Rect::new(10.0, 0.0, 5.0, 5.0)), None);
    }

    #[test]
    fn snapping_keeps_edges_on_pixels() {
        let r = Rect::new(1.4, 2.6, 3.2, 3.2).snapped();
        assert_eq!(r, Rect::new(1.0, 3.0, 4.0, 3.0));
    }

    #[test]
    fn contrast_is_symmetric_and_bounded() {
        let w = Color::rgb(255, 255, 255);
        let k = Color::rgb(0, 0, 0);
        assert!((w.contrast(k) - 21.0).abs() < 0.01);
        assert!((k.contrast(w) - 21.0).abs() < 0.01);
    }
}
