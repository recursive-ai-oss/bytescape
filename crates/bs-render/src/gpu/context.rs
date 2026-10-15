//! The GPU device, its queue, and what every renderer shares.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use super::batch::{Arena, INSTANCE_BYTES};

/// Why the GPU backend could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuError {
    /// No adapter that can draw was found.
    NoAdapter(String),
    /// The device could not be created.
    Device(String),
    /// The window surface could not be created or configured.
    Surface(String),
    /// A shader or pipeline was rejected.
    Pipeline(String),
    /// The picture is larger than the device allows, or empty.
    Size { width: u32, height: u32, max: u32 },
    /// The GPU reported an error while drawing, or the device was lost.
    Lost(String),
    /// The GPU path panicked inside the driver stack.
    Panicked(String),
    /// A failure requested by a test.
    Injected(FailStage),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter(m) => write!(f, "no usable graphics adapter: {m}"),
            Self::Device(m) => write!(f, "graphics device: {m}"),
            Self::Surface(m) => write!(f, "window surface: {m}"),
            Self::Pipeline(m) => write!(f, "graphics pipeline: {m}"),
            Self::Size { width, height, max } => {
                write!(f, "{width}x{height} does not fit the device limit {max}")
            }
            Self::Lost(m) => write!(f, "graphics device lost: {m}"),
            Self::Panicked(m) => write!(f, "graphics stack panicked: {m}"),
            Self::Injected(s) => write!(f, "forced failure at {s:?}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// A step of starting the GPU backend, so a test can make each one fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailStage {
    Instance,
    Adapter,
    Device,
    Pipeline,
    Surface,
    Renderer,
    /// Fails the first frame after everything started.
    FirstFrame,
}

/// Which adapter to ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdapterChoice {
    /// The system's default pick.
    #[default]
    Default,
    /// Only a software adapter (such as llvmpipe).
    Software,
}

/// How to start the GPU backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuOptions {
    pub adapter: AdapterChoice,
    /// Makes the named step fail, for tests.
    pub fail_at: Option<FailStage>,
}

impl FailStage {
    /// The stage named `name` (`instance`, `adapter`, `device`, `pipeline`,
    /// `surface`, `renderer`, `first-frame`).
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "instance" => Self::Instance,
            "adapter" => Self::Adapter,
            "device" => Self::Device,
            "pipeline" => Self::Pipeline,
            "surface" => Self::Surface,
            "renderer" => Self::Renderer,
            "first-frame" => Self::FirstFrame,
            _ => return None,
        })
    }
}

impl GpuOptions {
    /// Reads two diagnostic variables: `BYTESCAPE_GPU_ADAPTER=software` asks
    /// for a software adapter, and `BYTESCAPE_GPU_FAIL_AT=<stage>` makes that
    /// start-up step fail so the fall back to software drawing can be seen.
    pub fn from_env() -> Self {
        let adapter = match std::env::var("BYTESCAPE_GPU_ADAPTER").as_deref() {
            Ok("software") => AdapterChoice::Software,
            _ => AdapterChoice::Default,
        };
        let fail_at = std::env::var("BYTESCAPE_GPU_FAIL_AT")
            .ok()
            .and_then(|v| FailStage::parse(&v));
        Self { adapter, fail_at }
    }

    pub(crate) fn check(&self, stage: FailStage) -> Result<(), GpuError> {
        if self.fail_at == Some(stage) {
            Err(GpuError::Injected(stage))
        } else {
            Ok(())
        }
    }
}

/// Runs a future to completion on this thread. The GPU calls that are
/// asynchronous finish without waiting on anything else, so no executor is
/// needed.
pub(crate) fn block_on<F: Future>(fut: F) -> F::Output {
    struct Wakeup(Thread);
    impl Wake for Wakeup {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Wakeup(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => thread::park(),
        }
    }
}

/// The device and everything built on it that renderers share.
pub struct GpuContext {
    _instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    info: wgpu::AdapterInfo,
    max_dimension: u32,
    failure: Arc<Mutex<Option<String>>>,
    lost: Arc<AtomicBool>,
    shader: wgpu::ShaderModule,
    pub(crate) view_layout: wgpu::BindGroupLayout,
    pub(crate) image_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: Mutex<HashMap<wgpu::TextureFormat, Arc<wgpu::RenderPipeline>>>,
    pub(crate) nearest: wgpu::Sampler,
    pub(crate) linear: wgpu::Sampler,
    arena: Mutex<Arena>,
}

impl GpuContext {
    /// Starts the GPU. With a `surface` the adapter is one that can present
    /// to it.
    pub(crate) fn open(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        options: &GpuOptions,
    ) -> Result<Self, GpuError> {
        options.check(FailStage::Adapter)?;
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: options.adapter == AdapterChoice::Software,
            compatible_surface: surface,
            apply_limit_buckets: false,
        }))
        .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        let info = adapter.get_info();
        options.check(FailStage::Device)?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("bytescape"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| GpuError::Device(e.to_string()))?;
        // A driver error must not abort the program: remember it, and the
        // renderer reports the loss so the caller can switch backends.
        let failure = Arc::new(Mutex::new(None));
        let lost = Arc::new(AtomicBool::new(false));
        {
            let first = Arc::clone(&failure);
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                let mut slot = first.lock().unwrap_or_else(|p| p.into_inner());
                slot.get_or_insert_with(|| e.to_string());
            }));
            let (failure, lost) = (Arc::clone(&failure), Arc::clone(&lost));
            device.set_device_lost_callback(move |reason, message| {
                let mut slot = failure.lock().unwrap_or_else(|p| p.into_inner());
                slot.get_or_insert_with(|| format!("{reason:?}: {message}"));
                lost.store(true, Ordering::SeqCst);
            });
        }
        let max_dimension = device.limits().max_texture_dimension_2d;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bytescape draw"),
            source: wgpu::ShaderSource::Wgsl(super::shader::SOURCE.into()),
        });
        let view_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bytescape view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bytescape image"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bytescape draw"),
            bind_group_layouts: &[Some(&view_layout), Some(&image_layout)],
            immediate_size: 0,
        });
        let sampler = |filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        };
        let nearest = sampler(wgpu::FilterMode::Nearest);
        let linear = sampler(wgpu::FilterMode::Linear);
        let ctx = Self {
            _instance: instance,
            adapter,
            device,
            queue,
            info,
            max_dimension,
            failure,
            lost,
            shader,
            view_layout,
            image_layout,
            pipeline_layout,
            pipelines: Mutex::new(HashMap::new()),
            nearest,
            linear,
            arena: Arena::new(),
        };
        options.check(FailStage::Pipeline)?;
        // Build the pipeline once now, so a driver that rejects it is found
        // before the first frame rather than during it.
        let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _ = ctx.pipeline(wgpu::TextureFormat::Rgba8Unorm);
        if let Some(e) = block_on(scope.pop()) {
            return Err(GpuError::Pipeline(e.to_string()));
        }
        Ok(ctx)
    }

    /// A context with no window, for drawing into textures.
    pub fn headless(options: &GpuOptions) -> Result<Arc<Self>, GpuError> {
        options.check(FailStage::Instance)?;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        Self::open(instance, None, options).map(Arc::new)
    }

    /// The adapter in use.
    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.info
    }

    /// Whether the adapter is a CPU implementation (such as llvmpipe).
    pub fn is_software(&self) -> bool {
        self.info.device_type == wgpu::DeviceType::Cpu
    }

    /// The largest picture side the device draws.
    pub fn max_dimension(&self) -> u32 {
        self.max_dimension
    }

    /// The first error the GPU reported, if any.
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// True once the GPU reported an error or the device was lost.
    pub fn has_failed(&self) -> bool {
        self.lost.load(Ordering::SeqCst) || self.failure().is_some()
    }

    pub(crate) fn pipeline(&self, format: wgpu::TextureFormat) -> Arc<wgpu::RenderPipeline> {
        let mut map = self.pipelines.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = map.get(&format) {
            return Arc::clone(p);
        }
        let attr = |format, offset, shader_location| wgpu::VertexAttribute {
            format,
            offset,
            shader_location,
        };
        let attributes = [
            attr(wgpu::VertexFormat::Float32x4, 0, 0),
            attr(wgpu::VertexFormat::Float32x4, 16, 1),
            attr(wgpu::VertexFormat::Unorm8x4, 32, 2),
            attr(wgpu::VertexFormat::Float32x4, 36, 3),
        ];
        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("bytescape draw"),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &self.shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: INSTANCE_BYTES as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &attributes,
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let pipeline = Arc::new(pipeline);
        map.insert(format, Arc::clone(&pipeline));
        pipeline
    }

    /// Copies rectangle data into the shared per-frame buffer.
    pub(crate) fn upload(&self, data: &[u8]) -> (wgpu::Buffer, u64) {
        self.arena
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .alloc(&self.device, &self.queue, data)
    }

    /// Sends recorded work to the GPU and lets the buffer be reused.
    pub(crate) fn submit(&self, encoder: wgpu::CommandEncoder) -> wgpu::SubmissionIndex {
        let index = self.queue.submit([encoder.finish()]);
        self.arena.lock().unwrap_or_else(|p| p.into_inner()).reset();
        index
    }

    /// Waits until everything submitted so far has finished.
    pub fn wait_idle(&self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }
}
