//! Label images packed into textures, so a label is uploaded once and drawn
//! as a quad afterwards.

use std::collections::HashMap;
use std::sync::Arc;

use crate::text::Label;

const PAGE_W: u32 = super::ATLAS_W;
const PAGE_H: u32 = super::ATLAS_H;
/// Past this many pages the atlas starts over at the next full repaint.
const TRIM_PAGES: usize = 4;

struct Shelf {
    y: u32,
    height: u32,
    x: u32,
}

struct Page {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    /// A copy of the texture, so many small labels reach the GPU in one write.
    pixels: Vec<u8>,
    shelves: Vec<Shelf>,
    next_y: u32,
    dirty: Option<(u32, u32)>,
}

/// Where a label sits in the atlas.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slot {
    pub page: usize,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub(crate) struct Atlas {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pages: Vec<Page>,
    /// The cache keeps each label alive, so its address stays unique.
    slots: HashMap<usize, (Arc<Label>, Option<Slot>)>,
}

impl Atlas {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        layout: wgpu::BindGroupLayout,
        sampler: wgpu::Sampler,
    ) -> Self {
        let mut atlas = Self {
            device,
            queue,
            layout,
            sampler,
            pages: Vec::new(),
            slots: HashMap::new(),
        };
        atlas.add_page();
        atlas
    }

    fn add_page(&mut self) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bytescape labels"),
            size: wgpu::Extent3d {
                width: PAGE_W,
                height: PAGE_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bytescape labels"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.pages.push(Page {
            texture,
            bind,
            pixels: vec![0; PAGE_W as usize * PAGE_H as usize * 4],
            shelves: Vec::new(),
            next_y: 0,
            dirty: None,
        });
    }

    pub fn bind_group(&self, page: usize) -> Option<&wgpu::BindGroup> {
        self.pages.get(page).map(|p| &p.bind)
    }

    /// Starts over when the atlas has grown large. Call only when nothing
    /// recorded still points into it.
    pub fn trim(&mut self) {
        if self.pages.len() > TRIM_PAGES || self.slots.len() > 30_000 {
            self.pages.truncate(1);
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.slots.clear();
        for p in &mut self.pages {
            p.shelves.clear();
            p.next_y = 0;
            p.dirty = None;
        }
    }

    /// The place of `label`, uploading it on first use. `None` when the label
    /// is larger than a page.
    pub fn slot(&mut self, label: &Arc<Label>) -> Option<Slot> {
        let key = Arc::as_ptr(label) as usize;
        if let Some((_, slot)) = self.slots.get(&key) {
            return *slot;
        }
        let slot = self.place(label);
        self.slots.insert(key, (Arc::clone(label), slot));
        slot
    }

    fn place(&mut self, label: &Label) -> Option<Slot> {
        let (w, h) = (label.pixmap.width(), label.pixmap.height());
        if w == 0 || h == 0 || w > PAGE_W || h > PAGE_H {
            return None;
        }
        let bucket = h.next_multiple_of(8);
        let found = self.pages.iter_mut().enumerate().find_map(|(i, page)| {
            let (x, y) = Self::room(page, w, h, bucket)?;
            Some((i, x, y))
        });
        let (page, x, y) = match found {
            Some(f) => f,
            None => {
                self.add_page();
                let i = self.pages.len() - 1;
                let (x, y) = Self::room(&mut self.pages[i], w, h, bucket)?;
                (i, x, y)
            }
        };
        let p = &mut self.pages[page];
        // The label's bytes are blue, green, red, alpha; the texture is red,
        // green, blue, alpha.
        let src = label.pixmap.data();
        for row in 0..h as usize {
            let from = &src[row * w as usize * 4..][..w as usize * 4];
            let to = &mut p.pixels[((y as usize + row) * PAGE_W as usize + x as usize) * 4..]
                [..w as usize * 4];
            for (d, s) in to
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(from.as_chunks::<4>().0)
            {
                *d = [s[2], s[1], s[0], s[3]];
            }
        }
        p.dirty = Some(match p.dirty {
            Some((a, b)) => (a.min(y), b.max(y + h)),
            None => (y, y + h),
        });
        Some(Slot { page, x, y, w, h })
    }

    fn room(page: &mut Page, w: u32, h: u32, bucket: u32) -> Option<(u32, u32)> {
        for s in &mut page.shelves {
            if s.height == bucket && s.x + w <= PAGE_W {
                let at = (s.x, s.y);
                s.x += w;
                return Some(at);
            }
        }
        if page.next_y + bucket.max(h) <= PAGE_H {
            let y = page.next_y;
            page.next_y += bucket;
            page.shelves.push(Shelf {
                y,
                height: bucket,
                x: w,
            });
            return Some((0, y));
        }
        None
    }

    /// Sends labels added since the last call to the GPU.
    pub fn sync(&mut self) {
        for p in &mut self.pages {
            let Some((y0, y1)) = p.dirty.take() else {
                continue;
            };
            let row = PAGE_W as usize * 4;
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &p.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &p.pixels[y0 as usize * row..y1 as usize * row],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row as u32),
                    rows_per_image: Some(y1 - y0),
                },
                wgpu::Extent3d {
                    width: PAGE_W,
                    height: y1 - y0,
                    depth_or_array_layers: 1,
                },
            );
        }
    }
}
