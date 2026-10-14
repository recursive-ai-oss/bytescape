//! Text shaping and the label cache.
//!
//! Font discovery is slow compared with a first-frame budget, so the font
//! system is built on a worker thread. Until it is ready, measuring and drawing
//! text are no-ops and the caller draws again once [`TextEngine::is_ready`]
//! turns true.
//!
//! A label is shaped and rasterized once into a small premultiplied bitmap and
//! then blitted. The cache is keyed by text, size, scale, color and available
//! width, so a zoom or a repaint that shows the same label again costs a
//! hash lookup.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use cosmic_text::{
    Attrs, Buffer, Ellipsize, EllipsizeHeightLimit, Family, FontSystem, Metrics, Shaping,
    SwashCache, SwashContent, Wrap,
};
use tiny_skia::Pixmap;

use crate::canvas::TextStyle;
use crate::pixel;

/// Line height as a multiple of the font size.
const LINE_HEIGHT: f32 = 1.25;
/// Entries kept before the cache is cleared and refilled by the next frame.
const CACHE_LIMIT: usize = 30_000;

/// Text, size bits and scale bits.
type WidthKey = (Box<str>, u32, u32);

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    text: Box<str>,
    size_bits: u32,
    scale_bits: u32,
    color: u32,
    width: u32,
}

/// A shaped and rasterized line of text.
pub struct Label {
    /// Premultiplied pixels in the backend's channel order.
    pub pixmap: Pixmap,
    /// Offset of the bitmap from the pen origin (left, top), device pixels.
    pub origin: (i32, i32),
    /// Advance width of the shaped line.
    pub width: f32,
}

struct Fonts {
    system: FontSystem,
    swash: SwashCache,
}

struct Shared {
    fonts: Mutex<Option<Fonts>>,
    ready: AtomicBool,
    cache: RwLock<HashMap<Key, Option<Arc<Label>>>>,
    widths: RwLock<HashMap<WidthKey, f32>>,
    hits: AtomicU64,
    misses: AtomicU64,
    font_init_micros: AtomicU64,
}

/// Shapes text and caches the result. Cheap to clone; clones share the cache.
#[derive(Clone)]
pub struct TextEngine {
    shared: Arc<Shared>,
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    /// Creates an engine without fonts. Call [`TextEngine::load_fonts`] or
    /// [`TextEngine::load_fonts_in_background`] to discover system fonts.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                fonts: Mutex::new(None),
                ready: AtomicBool::new(false),
                cache: RwLock::new(HashMap::new()),
                widths: RwLock::new(HashMap::new()),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                font_init_micros: AtomicU64::new(0),
            }),
        }
    }

    /// Discovers system fonts on the calling thread.
    pub fn load_fonts(&self) {
        let started = std::time::Instant::now();
        #[cfg(windows)]
        let system = {
            let mut system = FontSystem::new();
            system.db_mut().set_sans_serif_family("Segoe UI");
            system
        };
        #[cfg(not(windows))]
        let system = FontSystem::new();
        let fonts = Fonts {
            system,
            swash: SwashCache::new(),
        };
        let mut slot = self.shared.fonts.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(fonts);
        self.shared
            .font_init_micros
            .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.shared.ready.store(true, Ordering::Release);
    }

    /// Discovers system fonts on a worker thread.
    pub fn load_fonts_in_background(&self) {
        let engine = self.clone();
        let spawned = std::thread::Builder::new()
            .name("fonts".into())
            .spawn(move || engine.load_fonts());
        // If the thread cannot start, text stays unavailable; the map still
        // draws without labels.
        drop(spawned);
    }

    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }

    /// How long font discovery took, once it has finished.
    pub fn font_init_micros(&self) -> Option<u64> {
        self.is_ready()
            .then(|| self.shared.font_init_micros.load(Ordering::Relaxed))
    }

    /// `(hits, misses)` of the label cache since creation.
    pub fn cache_stats(&self) -> (u64, u64) {
        (
            self.shared.hits.load(Ordering::Relaxed),
            self.shared.misses.load(Ordering::Relaxed),
        )
    }

    pub fn cache_len(&self) -> usize {
        self.shared
            .cache
            .read()
            .map(|c| c.len())
            .unwrap_or_default()
    }

    /// Returns the cached label for this text, shaping it on a miss. Text wider
    /// than `max_width` is ellipsized in the middle. `None` while fonts are
    /// loading or when the text has no visible glyphs.
    pub fn label(&self, text: &str, style: TextStyle, max_width: f32) -> Option<Arc<Label>> {
        if !self.is_ready() {
            return None;
        }
        let key = Key {
            text: text.into(),
            size_bits: style.size.to_bits(),
            scale_bits: style.scale.to_bits(),
            color: u32::from_le_bytes([style.color.r, style.color.g, style.color.b, style.color.a]),
            width: max_width.max(0.0).ceil() as u32,
        };
        if let Ok(cache) = self.shared.cache.read()
            && let Some(hit) = cache.get(&key)
        {
            self.shared.hits.fetch_add(1, Ordering::Relaxed);
            return hit.clone();
        }
        self.shared.misses.fetch_add(1, Ordering::Relaxed);
        let made = self.shape(text, style, max_width).map(Arc::new);
        if let Ok(mut cache) = self.shared.cache.write() {
            if cache.len() >= CACHE_LIMIT {
                cache.clear();
            }
            cache.insert(key, made.clone());
        }
        made
    }

    /// Width of the text on one line in device pixels. Results are cached.
    pub fn measure(&self, text: &str, size: f32, scale: f32) -> Option<f32> {
        if !self.is_ready() {
            return None;
        }
        let key = (Box::<str>::from(text), size.to_bits(), scale.to_bits());
        if let Ok(widths) = self.shared.widths.read()
            && let Some(w) = widths.get(&key)
        {
            return Some(*w);
        }
        let width = {
            let mut guard = self.shared.fonts.lock().unwrap_or_else(|e| e.into_inner());
            let fonts = guard.as_mut()?;
            let px = size * scale;
            let mut buffer = Buffer::new(&mut fonts.system, Metrics::new(px, px * LINE_HEIGHT));
            buffer.set_wrap(Wrap::None);
            buffer.set_size(None, None);
            buffer.set_text(
                text,
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(&mut fonts.system, false);
            buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
        };
        if let Ok(mut widths) = self.shared.widths.write() {
            if widths.len() >= CACHE_LIMIT {
                widths.clear();
            }
            widths.insert(key, width);
        }
        Some(width)
    }

    fn shape(&self, text: &str, style: TextStyle, max_width: f32) -> Option<Label> {
        if text.is_empty() || max_width < 1.0 {
            return None;
        }
        // No label is wider than any display; this also keeps the bitmap size
        // arithmetic far from overflow.
        let max_width = max_width.min(16_384.0);
        let mut guard = self.shared.fonts.lock().unwrap_or_else(|e| e.into_inner());
        let fonts = guard.as_mut()?;
        let px = (style.size * style.scale).max(1.0);
        let line_h = (px * LINE_HEIGHT).ceil();
        let mut buffer = Buffer::new(&mut fonts.system, Metrics::new(px, line_h));
        buffer.set_wrap(Wrap::None);
        buffer.set_ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1)));
        buffer.set_size(Some(max_width), Some(line_h * 2.0));
        buffer.set_text(
            text,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut fonts.system, false);

        let width = buffer
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0_f32, f32::max)
            .min(max_width);
        let w = (width.ceil() as u32)
            .saturating_add(2)
            .min((max_width.ceil() as u32).saturating_add(2));
        let h = (line_h as u32).saturating_add(2);
        let mut pixmap = Pixmap::new(w.max(1), h.max(1))?;
        let color = style.color;
        let stride = pixmap.width() as i32;
        let rows = pixmap.height() as i32;
        let data = pixmap.data_mut();
        let mut any = false;
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((0.0, run.line_y), 1.0);
                let Some(image) = fonts.swash.get_image(&mut fonts.system, physical.cache_key)
                else {
                    continue;
                };
                let gx = physical.x + image.placement.left;
                let gy = physical.y - image.placement.top;
                let (iw, ih) = (image.placement.width as i32, image.placement.height as i32);
                for row in 0..ih {
                    let y = gy + row;
                    if y < 0 || y >= rows {
                        continue;
                    }
                    for col in 0..iw {
                        let x = gx + col;
                        if x < 0 || x >= stride {
                            continue;
                        }
                        let (src, cov) = match image.content {
                            SwashContent::Mask => {
                                let Some(&a) = image.data.get((row * iw + col) as usize) else {
                                    continue;
                                };
                                (color, a)
                            }
                            SwashContent::Color | SwashContent::SubpixelMask => {
                                let i = ((row * iw + col) * 4) as usize;
                                let Some(px) = image.data.get(i..i + 4) else {
                                    continue;
                                };
                                (crate::geom::Color::rgb(px[0], px[1], px[2]), px[3])
                            }
                        };
                        if cov == 0 {
                            continue;
                        }
                        any = true;
                        let a = (cov as u32 * color.a as u32 + 127) / 255;
                        let o = ((y * stride + x) * 4) as usize;
                        let dst = &mut data[o..o + 4];
                        let [pb, pg, pr, pa] = pixel::premultiplied(src, a as u8);
                        // Source-over onto the existing premultiplied pixel.
                        let inv = 255 - pa as u32;
                        dst[0] = (pb as u32 + (dst[0] as u32 * inv + 127) / 255).min(255) as u8;
                        dst[1] = (pg as u32 + (dst[1] as u32 * inv + 127) / 255).min(255) as u8;
                        dst[2] = (pr as u32 + (dst[2] as u32 * inv + 127) / 255).min(255) as u8;
                        dst[3] = (pa as u32 + (dst[3] as u32 * inv + 127) / 255).min(255) as u8;
                    }
                }
            }
        }
        any.then_some(Label {
            pixmap,
            origin: (0, 0),
            width,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Color;

    fn style() -> TextStyle {
        TextStyle {
            size: 13.0,
            scale: 1.0,
            color: Color::rgb(0, 0, 0),
        }
    }

    #[test]
    fn nothing_is_drawn_before_fonts_load() {
        let engine = TextEngine::new();
        assert!(!engine.is_ready());
        assert!(engine.label("hello", style(), 100.0).is_none());
        assert!(engine.measure("hello", 13.0, 1.0).is_none());
    }

    #[test]
    fn labels_are_cached_and_hostile_names_do_not_panic() {
        let engine = TextEngine::new();
        engine.load_fonts();
        let long = "x".repeat(4096);
        let hostile = [
            "plain.txt",
            "",
            "\u{202e}rtl\u{200b}\u{200d}zero\u{0301}\u{0301}\u{0301}width",
            "שלום עולם.txt",
            "\u{fffd}\u{fffd}lossy",
            long.as_str(),
        ];
        for name in hostile {
            let a = engine.label(name, style(), 120.0);
            if let Some(a) = &a {
                assert!(a.pixmap.width() <= 124, "label must respect the width");
            }
            let b = engine.label(name, style(), 120.0);
            assert_eq!(a.is_some(), b.is_some());
        }
        let (hits, misses) = engine.cache_stats();
        assert!(hits >= 5, "second lookups should hit: {hits}");
        assert!(misses >= 5);
    }

    #[test]
    fn wide_text_is_shortened_to_the_available_width() {
        let engine = TextEngine::new();
        engine.load_fonts();
        let text = "a-quite-long-file-name-that-cannot-fit-in-sixty-pixels.tar.gz";
        let full = engine
            .measure(text, 13.0, 1.0)
            .expect("fonts finished loading");
        assert!(
            full > 60.0,
            "this test needs a system font; measured {full} px"
        );
        let label = engine.label(text, style(), 60.0).expect("label");
        assert!(label.width <= 60.5, "width {} over limit", label.width);
    }
}
