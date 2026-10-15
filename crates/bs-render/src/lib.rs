//! The narrow drawing interface and its software backend, with the glyph cache
//! and hatch patterns.
//!
//! * [`canvas`]: the [`Canvas`] trait and a recording implementation for tests.
//! * [`skia`]: the `tiny-skia` backend.
//! * [`text`]: font discovery, shaping and the label cache.
//! * [`card`]: the hover card.
//! * [`chrome`]: buttons, legend swatches, the capacity bar and chrome text.
//! * [`scene`]: palettes and the painter for a flat list of boxes.
//! * [`renderer`]: the retained picture, dirty regions, tiled repaints and the
//!   cached-image zoom.
//! * [`present`]: presenting a frame through `softbuffer`.

pub mod canvas;
pub mod card;
pub mod chrome;
pub mod geom;
pub mod pixel;
pub mod present;
pub mod renderer;
pub mod scene;
pub mod skia;
pub mod text;
pub mod warm;
pub mod zoom;

pub use canvas::{Canvas, Hatch, HatchAngle, RecordingCanvas, Stroke, TextStyle};
pub use card::{COPIED_TEXT, Card, CardPart, CardRole, Popup};
pub use chrome::{Chrome, Glyph, SegmentKind, TextRole, Widget};
pub use geom::{Color, IRect, Rect};
pub use renderer::{PaintTimings, Painted, Renderer, ZOOM_DURATION, ZoomDirection};
pub use scene::{Bar, Flags, Item, ItemKind, ItemLabel, Palette, SceneStyle};
pub use text::TextEngine;
