//! Channel order of the software backend.
//!
//! Presenting through `softbuffer` expects each 32-bit pixel as
//! `0x00RRGGBB`, which in memory is the bytes B, G, R, A. The software
//! canvas draws with `tiny-skia`, whose pixmap bytes are R, G, B, A. Rather
//! than convert every presented frame, every color that reaches a pixmap has
//! its red and blue swapped on the way in. The pixmap's bytes then read as a
//! native `0xAARRGGBB` word and can be copied or composed straight into the
//! window buffer; only PNG export swaps back.

use crate::geom::Color;

/// Pixmap bytes `[b, g, r, a]` for a straight-alpha color, premultiplied.
pub fn premultiplied(c: Color, alpha: u8) -> [u8; 4] {
    let a = alpha as u32;
    let p = |v: u8| ((v as u32 * a + 127) / 255) as u8;
    [p(c.b), p(c.g), p(c.r), alpha]
}

/// The native pixel word of an opaque color.
pub fn word(c: Color) -> u32 {
    0xFF00_0000 | (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32
}

/// A `tiny-skia` color with red and blue swapped.
pub fn skia_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.b, c.g, c.r, c.a)
}

/// Converts pixmap bytes to straight RGBA for PNG export.
pub fn to_png_rgba(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.as_chunks::<4>().0 {
        let [b, g, r, a] = *px;
        if a == 0 || a == 255 {
            out.extend_from_slice(&[r, g, b, a]);
        } else {
            let un = |v: u8| ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            out.extend_from_slice(&[un(r), un(g), un(b), a]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_matches_native_layout() {
        assert_eq!(word(Color::rgb(0x12, 0x34, 0x56)), 0xFF12_3456);
        let bytes = premultiplied(Color::rgb(0x12, 0x34, 0x56), 255);
        assert_eq!(u32::from_le_bytes(bytes), 0xFF12_3456);
    }

    #[test]
    fn png_export_swaps_back() {
        let rgba = to_png_rgba(&[0x56, 0x34, 0x12, 0xFF]);
        assert_eq!(rgba, vec![0x12, 0x34, 0x56, 0xFF]);
    }
}
