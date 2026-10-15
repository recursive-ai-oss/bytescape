//! The GPU drawing backend.
//!
//! * [`GpuCanvas`] implements [`crate::Canvas`] by recording rectangles; one
//!   pipeline draws solid fills, label images and hatches.
//! * [`GpuRenderer`] keeps the retained picture, the hover and selection
//!   repaints and the cached-image zoom on the GPU, with the same calls as the
//!   software [`crate::Renderer`].
//! * [`GpuPresenter`] shows frames in a window.
//! * [`choose_window`] and [`choose_offscreen`] try the GPU and fall back to
//!   software on any failure.

mod atlas;
mod batch;
mod canvas;
mod context;
mod image;
mod present;
mod renderer;
mod select;
mod shader;

pub use batch::Batch;
pub use canvas::GpuCanvas;
pub use context::{AdapterChoice, FailStage, GpuContext, GpuError, GpuOptions};
pub use present::{GpuPresenter, PresentPacing};
pub use renderer::{Geometry, GpuRenderer};
pub use select::{
    BackendPreference, GpuWindow, OffscreenBackend, WindowBackend, choose_offscreen, choose_window,
    guarded as guard,
};

/// Size of one label atlas page.
pub(crate) const ATLAS_W: u32 = 4096;
pub(crate) const ATLAS_H: u32 = 1024;
