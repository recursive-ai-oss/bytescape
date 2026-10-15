//! Presenting GPU frames to a window.

use std::sync::Arc;
use std::time::Duration;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::renderer::Painted;

use super::renderer::GpuRenderer;
use super::{FailStage, GpuContext, GpuError, GpuOptions};

/// How frames wait for the display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PresentPacing {
    /// Frames wait for the display's refresh (no tearing).
    #[default]
    Vsync,
    /// Frames are shown as soon as they are ready; used to time the work
    /// itself.
    Immediate,
}

/// A window surface that takes finished frames.
pub struct GpuPresenter {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Arc<GpuContext>,
    presented: u64,
    fail_first: bool,
}

impl GpuPresenter {
    /// Creates the surface for `window` and a GPU able to draw to it.
    pub fn open<W>(
        window: W,
        size: (u32, u32),
        pacing: PresentPacing,
        options: &GpuOptions,
    ) -> Result<(Arc<GpuContext>, Self), GpuError>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        options.check(FailStage::Instance)?;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        options.check(FailStage::Surface)?;
        let surface = instance
            .create_surface(window)
            .map_err(|e| GpuError::Surface(e.to_string()))?;
        let gpu = Arc::new(GpuContext::open(instance, Some(&surface), options)?);
        let caps = surface.get_capabilities(&gpu.adapter);
        // Colors are already in the output encoding, so the format must not
        // convert them again.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .ok_or_else(|| GpuError::Surface("no linear surface format".into()))?;
        let present_mode = match pacing {
            PresentPacing::Vsync => wgpu::PresentMode::Fifo,
            PresentPacing::Immediate => [wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox]
                .into_iter()
                .find(|m| caps.present_modes.contains(m))
                .unwrap_or(wgpu::PresentMode::Fifo),
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::default(),
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: Vec::new(),
        };
        surface.configure(&gpu.device, &config);
        if let Some(message) = gpu.failure() {
            return Err(GpuError::Surface(message));
        }
        Ok((
            gpu.clone(),
            Self {
                surface,
                config,
                gpu,
                presented: 0,
                fail_first: options.fail_at == Some(FailStage::FirstFrame),
            },
        ))
    }

    /// The presentation mode in use.
    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    /// Resizes the surface. Zero sizes (a minimized window) are ignored.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.gpu.device, &self.config);
    }

    /// Draws the renderer's picture into the next window frame and shows it.
    /// A frame the display cannot take right now (minimized, resizing) is
    /// skipped and reported as nothing drawn.
    pub fn present(
        &mut self,
        renderer: &mut GpuRenderer,
        now: Duration,
    ) -> Result<Painted, GpuError> {
        if self.fail_first && self.presented == 0 {
            return Err(GpuError::Injected(FailStage::FirstFrame));
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                self.surface.configure(&self.gpu.device, &self.config);
                f
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(Painted {
                    damage: Vec::new(),
                    animating: renderer.is_animating(),
                });
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.gpu.device, &self.config);
                return Ok(Painted {
                    damage: Vec::new(),
                    animating: renderer.is_animating(),
                });
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                return Err(GpuError::Lost("the window surface was lost".into()));
            }
            other => return Err(GpuError::Surface(format!("{other:?}"))),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let painted = renderer.paint_to(&view, self.config.format, now);
        self.gpu.queue.present(frame);
        self.presented += 1;
        painted
    }
}
