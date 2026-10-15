//! Rectangles waiting to be drawn, and the pass that draws them.

use std::sync::Mutex;

use super::GpuContext;
use super::atlas::Atlas;

/// Bytes per rectangle: rect, texture rect, color and parameters.
pub(crate) const INSTANCE_BYTES: usize = 52;

/// Which texture a run of rectangles samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bind {
    Atlas(usize),
    Image(usize),
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    bind: Bind,
    /// Index of the first rectangle of the run.
    start: u32,
}

/// Kinds of rectangle, as the shader reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Solid = 0,
    Image = 1,
    HatchRising = 2,
    HatchFalling = 3,
}

/// A list of rectangles in drawing order.
#[derive(Default)]
pub struct Batch {
    data: Vec<u8>,
    segments: Vec<Segment>,
    count: u32,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Rectangles recorded so far.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.segments.clear();
        self.count = 0;
    }

    pub(crate) fn push(
        &mut self,
        rect: [f32; 4],
        uv: [f32; 4],
        color: [u8; 4],
        kind: Kind,
        params: [f32; 2],
        bind: Option<Bind>,
    ) {
        // Rectangles that sample nothing ride along with whatever texture is
        // bound; the first run starts with the first label page.
        let bind = bind.or(self.segments.is_empty().then_some(Bind::Atlas(0)));
        if let Some(bind) = bind {
            let current = self.segments.last().map(|s| s.bind);
            if current != Some(bind) {
                self.segments.push(Segment {
                    bind,
                    start: self.count,
                });
            }
        }
        let d = &mut self.data;
        for v in rect.iter().chain(uv.iter()) {
            d.extend_from_slice(&v.to_ne_bytes());
        }
        d.extend_from_slice(&color);
        for v in [kind as u32 as f32, params[0], params[1], 0.0] {
            d.extend_from_slice(&v.to_ne_bytes());
        }
        self.count += 1;
    }

    /// Draws the rectangles into `view`, clearing it first when `clear` is
    /// given.
    pub(crate) fn encode(
        &self,
        gpu: &GpuContext,
        target: &Target<'_>,
        atlas: &Atlas,
        images: &[&wgpu::BindGroup],
        encoder: &mut wgpu::CommandEncoder,
        clear: Option<wgpu::Color>,
    ) {
        if self.count == 0 && clear.is_none() {
            return;
        }
        let pipeline = gpu.pipeline(target.format);
        let slice = (self.count > 0).then(|| gpu.upload(&self.data));
        let load = match clear {
            Some(c) => wgpu::LoadOp::Clear(c),
            None => wgpu::LoadOp::Load,
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("bytescape draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let Some((buffer, offset)) = slice else {
            return;
        };
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, target.view_bind, &[]);
        pass.set_vertex_buffer(0, buffer.slice(offset..offset + self.data.len() as u64));
        for (i, seg) in self.segments.iter().enumerate() {
            let end = self.segments.get(i + 1).map_or(self.count, |n| n.start);
            if end <= seg.start {
                continue;
            }
            let group = match seg.bind {
                Bind::Atlas(p) => atlas.bind_group(p),
                Bind::Image(i) => images.get(i).copied(),
            };
            let Some(group) = group else {
                continue;
            };
            pass.set_bind_group(1, group, &[]);
            pass.draw(0..4, seg.start..end);
        }
    }
}

/// Where a pass draws.
pub(crate) struct Target<'a> {
    pub view: &'a wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    /// The bind group holding the target's size.
    pub view_bind: &'a wgpu::BindGroup,
}

/// Per-frame buffer that rectangles are copied into. Reset after each submit.
pub(crate) struct Arena {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    used: u64,
}

impl Arena {
    pub fn new() -> Mutex<Self> {
        Mutex::new(Self {
            buffer: None,
            capacity: 0,
            used: 0,
        })
    }

    pub fn reset(&mut self) {
        self.used = 0;
    }

    pub fn alloc(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &[u8],
    ) -> (wgpu::Buffer, u64) {
        let need = data.len() as u64;
        if self.used + need > self.capacity {
            // Passes already recorded keep the buffer they were given.
            self.capacity = (self.used + need).max(self.capacity * 2).max(1 << 20);
            self.buffer = None;
            self.used = 0;
        }
        let capacity = self.capacity;
        let buffer = self.buffer.get_or_insert_with(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bytescape rectangles"),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        let offset = self.used;
        queue.write_buffer(buffer, offset, data);
        self.used += need.next_multiple_of(4);
        (buffer.clone(), offset)
    }
}
