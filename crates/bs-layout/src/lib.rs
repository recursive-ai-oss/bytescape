//! Squarified layout of a size tree: nested folders with title bars, size
//! thresholds that follow the font, small-file grouping, and layouts that
//! stay put when files are deleted.
//!
//! # How it fits together
//!
//! [`layout`] takes a [`bs_tree::Snapshot`], a [`ViewTarget`] (a folder, or
//! the members of a group), the viewport in device pixels and the
//! [`Metrics`] (the font size `em` and the display scale), and returns a
//! [`Layout`]: a flat list of [`DrawItem`]s, parents before children. The
//! function is pure. A [`Layout`] is stored for as long as the view is
//! shown:
//!
//! * [`Layout::refresh`] brings states and label texts up to date after
//!   deletes, undo, or scan progress without touching any rectangle.
//! * [`Layout::relayout`] is the only way boxes move; call it on
//!   navigation, a resize, a change of scale, or a throttled update during
//!   a scan.
//! * [`Layout::enterable`] lists the folders and groups that can be zoomed
//!   into; [`ViewTable`] numbers them for the navigation code.
//!
//! # Sizes and thresholds
//!
//! Areas are proportional to allocated bytes as counted. Those never shrink
//! when something is deleted, so a delete cannot move a box. Texts show the
//! bytes still on disk and the bytes moved to the Recycle Bin or deleted.
//!
//! All thresholds are multiples of `em` (see [`Thresholds`]): a folder nests
//! from 5 em × 3.2 em, any box carries a label from 3.5 em × 1.6 em, a box
//! needs at least 2 × 2 device pixels to be drawn at all, and siblings that
//! would get less than a 2.5 em square merge into a group when there are at
//! least three. Bytes of boxes that are not drawn still count in their
//! parent.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod format;
mod geom;
mod item;
mod layout;
mod squarify;
mod view;

pub use format::{display_name, format_count, format_size};
pub use geom::{Metrics, Rect, Thresholds, content_rect, padding, title_height};
pub use item::{CATEGORY_COUNT, CategoryId, DrawItem, ItemKind, Label, StateFlags};
pub use layout::{BinName, ItemRef, Layout, Options, layout, layout_with};
pub use view::{GroupSpec, ViewId, ViewTable, ViewTarget};
