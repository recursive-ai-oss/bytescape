//! Presenting a frame through `softbuffer`.

use std::num::NonZeroU32;

use softbuffer::{Context, Surface};

use crate::geom::IRect;

pub use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
pub use softbuffer::SoftBufferError;

/// A window surface that takes finished frames.
pub struct Presenter<D: HasDisplayHandle, W: HasWindowHandle> {
    surface: Surface<D, W>,
    size: (u32, u32),
}

impl<D: HasDisplayHandle + Clone, W: HasWindowHandle> Presenter<D, W> {
    pub fn new(display: D, window: W, width: u32, height: u32) -> Result<Self, SoftBufferError> {
        let context = Context::new(display)?;
        let mut surface = Surface::new(&context, window)?;
        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            surface.resize(w, h)?;
        }
        Ok(Self {
            surface,
            size: (width, height),
        })
    }

    /// Resizes the buffer. Zero sizes (a minimized window) are ignored.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), SoftBufferError> {
        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            self.surface.resize(w, h)?;
            self.size = (width, height);
        }
        Ok(())
    }

    /// Lets `fill` write the next frame into the window buffer, then presents
    /// only the regions it reports. `fill` receives the buffer and whether its
    /// contents are unknown (so everything must be written), and returns the
    /// regions it wrote.
    pub fn present_with<F>(&mut self, fill: F) -> Result<(), SoftBufferError>
    where
        F: FnOnce(&mut [u32], bool) -> Vec<IRect>,
    {
        let mut buffer = self.surface.buffer_mut()?;
        let unknown = buffer.age() != 1;
        let damage = fill(&mut buffer, unknown);
        let rects: Vec<softbuffer::Rect> = if unknown {
            Vec::new()
        } else {
            damage
                .iter()
                .filter_map(|r| {
                    Some(softbuffer::Rect {
                        x: u32::try_from(r.x).ok()?,
                        y: u32::try_from(r.y).ok()?,
                        width: NonZeroU32::new(u32::try_from(r.w).ok()?)?,
                        height: NonZeroU32::new(u32::try_from(r.h).ok()?)?,
                    })
                })
                .collect()
        };
        if unknown {
            buffer.present()
        } else {
            buffer.present_with_damage(&rects)
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}
