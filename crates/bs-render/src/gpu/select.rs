//! Choosing between the GPU and the software backend at start-up.
//!
//! The GPU is tried first (when allowed) and any failure, including a panic
//! inside the driver stack, turns into the software backend with the reason
//! kept for the About box and the log. A caller that later sees
//! [`GpuError`] from a frame switches the same way.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::scene::Palette;
use crate::text::TextEngine;

use super::present::{GpuPresenter, PresentPacing};
use super::renderer::{Geometry, GpuRenderer};
use super::{GpuContext, GpuError, GpuOptions};

/// What the user or the environment asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendPreference {
    /// GPU when one works, else software.
    #[default]
    Auto,
    /// Always software.
    Software,
    /// GPU only; failure is reported rather than hidden by the caller.
    Gpu,
}

impl BackendPreference {
    /// Reads `BYTESCAPE_BACKEND` (`auto`, `software`, `gpu`).
    pub fn from_env() -> Self {
        match std::env::var("BYTESCAPE_BACKEND").as_deref() {
            Ok("software" | "cpu") => Self::Software,
            Ok("gpu") => Self::Gpu,
            _ => Self::Auto,
        }
    }
}

/// A GPU renderer and the surface it presents to.
pub struct GpuWindow {
    pub renderer: GpuRenderer,
    pub presenter: GpuPresenter,
}

/// The outcome of choosing a backend for a window.
pub enum WindowBackend {
    Gpu(Box<GpuWindow>),
    /// Use the software renderer. `reason` says why the GPU was not used.
    Software {
        reason: String,
    },
}

/// The outcome of choosing a backend without a window.
pub enum OffscreenBackend {
    Gpu(Arc<GpuContext>),
    Software { reason: String },
}

fn describe(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Runs `f`, turning a panic into an error.
pub fn guarded<T>(f: impl FnOnce() -> Result<T, GpuError>) -> Result<T, GpuError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => Err(GpuError::Panicked(describe(p.as_ref()))),
    }
}

/// Tries to start the GPU backend for `window`.
#[allow(clippy::too_many_arguments)]
pub fn choose_window<W>(
    window: W,
    size: (u32, u32),
    scale: f32,
    font_px: f32,
    palette: Palette,
    text: TextEngine,
    preference: BackendPreference,
    pacing: PresentPacing,
    options: &GpuOptions,
) -> WindowBackend
where
    W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
{
    if preference == BackendPreference::Software {
        return WindowBackend::Software {
            reason: "software drawing was requested".into(),
        };
    }
    let started = guarded(|| {
        let (gpu, presenter) = GpuPresenter::open(window, size, pacing, options)?;
        let renderer = GpuRenderer::new(
            gpu,
            Geometry {
                width: size.0,
                height: size.1,
                scale,
                font_px,
            },
            palette,
            text,
            options,
        )?;
        Ok(GpuWindow {
            renderer,
            presenter,
        })
    });
    match started {
        Ok(w) => WindowBackend::Gpu(Box::new(w)),
        Err(e) => WindowBackend::Software {
            reason: e.to_string(),
        },
    }
}

/// Tries to start the GPU without a window.
pub fn choose_offscreen(preference: BackendPreference, options: &GpuOptions) -> OffscreenBackend {
    if preference == BackendPreference::Software {
        return OffscreenBackend::Software {
            reason: "software drawing was requested".into(),
        };
    }
    match guarded(|| GpuContext::headless(options)) {
        Ok(g) => OffscreenBackend::Gpu(g),
        Err(e) => OffscreenBackend::Software {
            reason: e.to_string(),
        },
    }
}
