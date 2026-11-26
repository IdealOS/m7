//! wgpu renderer for a [`DrawList`]: instanced SDF quads, textured images, and
//! glyphon text, in that order within each layer.
//!
//! The renderer knows nothing about windows or backends. It records into a
//! command encoder targeting any texture view, so the hosted window, the KMS
//! backend and headless screenshots all share it.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Buffer, Cache, Color, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport,
};
use protocol::NodeId;
use wgpu::util::DeviceExt;

use crate::scene::{DrawList, ImageItem, Rect, Rgba, TextItem};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    rect: [f32; 4],
    color: [f32; 4],
    border_color: [f32; 4],
    params: [f32; 4],
    clip: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageInstance {
    rect: [f32; 4],
    params: [f32; 4],
    clip: [f32; 4],
}

/// An image uploaded to the GPU, kept while it's drawn.
struct GpuImage {
    bind_group: wgpu::BindGroup,
    used: bool,
}

/// A shaped text buffer, reused across frames until its inputs change.
struct CachedText {
    buffer: Buffer,
    /// What it was shaped from: text, font, width and alignment.
    key: (String, crate::style::Font, u32, bool),
    used: bool,
}

pub struct Renderer {
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    instance_capacity: usize,

    image_pipeline: wgpu::RenderPipeline,
    image_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    image_instances: wgpu::Buffer,
    image_capacity: usize,
    /// Uploaded images by blob hash.
    images: HashMap<String, GpuImage>,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    /// One text renderer per layer, so text draws in stacking order.
    text_renderers: Vec<TextRenderer>,
    texts: HashMap<(NodeId, u32), CachedText>,
}

pub struct FrameStats {
    pub quads: usize,
    pub texts: usize,
    pub layers: usize,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(include_str!("quad.wgsl").into()),
        });
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::bytes_of(&Globals { viewport: [1.0, 1.0], _pad: [0.0; 2] }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad globals"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let attributes = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<QuadInstance>() as u64,
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
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let instance_capacity = 256;
        let instances = Self::instance_buffer(device, instance_capacity);

        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image texture"),
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
        let image_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image"),
            bind_group_layouts: &[Some(&bgl), Some(&image_layout)],
            immediate_size: 0,
        });
        let image_attributes = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &image_attributes,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let image_capacity = 16;
        let image_instances = Self::image_buffer(device, image_capacity);

        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let atlas = TextAtlas::new(device, queue, &cache, format);

        Renderer {
            format,
            pipeline,
            globals,
            bind_group,
            instances,
            instance_capacity,
            image_pipeline,
            image_layout,
            sampler,
            image_instances,
            image_capacity,
            images: HashMap::new(),
            font_system: crate::fonts::system(),
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderers: Vec::new(),
            texts: HashMap::new(),
        }
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    fn image_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image instances"),
            size: (capacity * std::mem::size_of::<ImageInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Upload `item`'s pixels unless they're already on the GPU. False if it has none.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, item: &ImageItem) -> bool {
        if let Some(gpu) = self.images.get_mut(&item.blob) {
            gpu.used = true;
            return true;
        }
        let Some(img) = &item.pixels else { return false };
        let size = wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &img.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * img.width),
                rows_per_image: Some(img.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.image_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.images.insert(item.blob.clone(), GpuImage { bind_group, used: true });
        true
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad instances"),
            size: (capacity * std::mem::size_of::<QuadInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Record one frame. `size` is the target size in physical pixels and
    /// `scale` the logical-to-physical factor.
    ///
    /// `damage` (logical pixels, see [`crate::damage`]) limits the frame to those
    /// rectangles: the target keeps its contents and only the damage is redrawn, each
    /// rectangle filled with the background and every layer drawn clipped to it. The
    /// target must hold the frame the damage was worked out against. `None` redraws
    /// everything.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: (u32, u32),
        scale: f32,
        list: &DrawList,
        clear: Rgba,
        damage: Option<&[Rect]>,
    ) -> FrameStats {
        let (w, h) = size;
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals { viewport: [w as f32, h as f32], _pad: [0.0; 2] }),
        );

        // A plain full-screen background becomes the clear colour, which a tiled GPU
        // applies for free, instead of a quad shaded on every pixel. On a Pi 4 at
        // 3440x1440 that quad cost ~27 ms a frame.
        let background = background_fill(list, size, scale);
        let clear = background.unwrap_or(clear);
        // Scissor rectangles (physical pixels) for a partial frame.
        let scissors: Option<Vec<[u32; 4]>> = damage
            .filter(|_| !list.layers.is_empty())
            .map(|rects| rects.iter().filter_map(|r| scissor(*r, scale, size)).collect());
        // In a partial frame, a layer nowhere near the damage is neither prepared nor
        // drawn: each draw call costs, even where the scissor discards every pixel.
        // The first layer always draws, since it starts with the background fill.
        let touches: Vec<Vec<bool>> = match (&scissors, damage) {
            (Some(_), Some(rects)) => list
                .layers
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let bounds = layer_bounds(l);
                    rects
                        .iter()
                        .map(|r| i == 0 || bounds.is_some_and(|b| !b.intersect(r).is_empty()))
                        .collect()
                })
                .collect(),
            _ => vec![vec![true]; list.layers.len()],
        };
        let used: Vec<bool> = touches.iter().map(|t| t.iter().any(|&x| x)).collect();

        // Quads for every layer go into one instance buffer; each layer draws a range.
        let mut instances = Vec::new();
        let mut ranges = Vec::with_capacity(list.layers.len());
        if scissors.is_some() {
            // A partial frame keeps the old contents, so the damage is first filled
            // with the background, as a clear would: a quad over the whole target,
            // which the scissors limit to the damage. It starts the first layer.
            instances.push(QuadInstance {
                rect: [0.0, 0.0, w as f32, h as f32],
                color: clear.to_linear(),
                border_color: clear.to_linear(),
                params: [0.0; 4],
                clip: phys(Rect::INFINITE, 1.0),
            });
        }
        for (i, layer) in list.layers.iter().enumerate() {
            let start = if i == 0 { 0 } else { instances.len() as u32 };
            let skip = usize::from(i == 0 && background.is_some());
            for q in &layer.quads[skip..] {
                instances.push(QuadInstance {
                    rect: phys(q.rect, scale),
                    color: q.color.to_linear(),
                    border_color: q.border_color.to_linear(),
                    params: [q.radius * scale, q.border_width * scale, 0.0, 0.0],
                    clip: phys(q.clip, scale),
                });
            }
            ranges.push(start..instances.len() as u32);
        }
        if instances.len() > self.instance_capacity {
            self.instance_capacity = instances.len().next_power_of_two();
            self.instances = Self::instance_buffer(device, self.instance_capacity);
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        }

        // Images likewise: one instance each, drawn after each layer's quads.
        for gpu in self.images.values_mut() {
            gpu.used = false;
        }
        let mut image_instances = Vec::new();
        let mut image_ranges: Vec<Vec<(u32, String)>> = Vec::with_capacity(list.layers.len());
        for layer in &list.layers {
            let mut drawn = Vec::new();
            for item in &layer.images {
                if self.upload(device, queue, item) {
                    drawn.push((image_instances.len() as u32, item.blob.clone()));
                    image_instances.push(ImageInstance {
                        rect: phys(item.rect, scale),
                        params: [item.radius * scale, 0.0, 0.0, 0.0],
                        clip: phys(item.clip, scale),
                    });
                }
            }
            image_ranges.push(drawn);
        }
        if image_instances.len() > self.image_capacity {
            self.image_capacity = image_instances.len().next_power_of_two();
            self.image_instances = Self::image_buffer(device, self.image_capacity);
        }
        if !image_instances.is_empty() {
            queue.write_buffer(&self.image_instances, 0, bytemuck::cast_slice(&image_instances));
        }
        self.images.retain(|_, gpu| gpu.used);

        // Shape (or reuse) text, then prepare one text renderer per layer.
        self.viewport.update(queue, Resolution { width: w, height: h });
        for cached in self.texts.values_mut() {
            cached.used = false;
        }
        while self.text_renderers.len() < list.layers.len() {
            self.text_renderers.push(TextRenderer::new(
                &mut self.atlas,
                device,
                wgpu::MultisampleState::default(),
                None,
            ));
        }
        let mut text_count = 0;
        for (i, layer) in list.layers.iter().enumerate() {
            if !used[i] {
                // Keep its shaped text cached for the next frame that draws it.
                for t in &layer.texts {
                    if let Some(c) = self.texts.get_mut(&(t.node, t.slot)) {
                        c.used = true;
                    }
                }
                continue;
            }
            for t in &layer.texts {
                self.shape(t);
            }
            text_count += layer.texts.len();
            let areas = layer.texts.iter().map(|t| {
                let clip = phys(text_bounds(t), scale);
                TextArea {
                    buffer: &self.texts[&(t.node, t.slot)].buffer,
                    left: t.rect.x * scale,
                    top: t.rect.y * scale,
                    scale,
                    bounds: TextBounds {
                        left: clip[0].floor() as i32,
                        top: clip[1].floor() as i32,
                        right: (clip[0] + clip[2]).ceil() as i32,
                        bottom: (clip[1] + clip[3]).ceil() as i32,
                    },
                    default_color: Color::rgba(t.color.0[0], t.color.0[1], t.color.0[2], t.color.0[3]),
                    custom_glyphs: &[],
                }
            });
            if let Err(e) = self.text_renderers[i].prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
            ) {
                log::error!("text prepare failed: {e}");
            }
        }
        self.texts.retain(|_, c| c.used);

        let [r, g, b, a] = clear.to_linear();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if scissors.is_some() {
                            wgpu::LoadOp::Load
                        } else {
                            wgpu::LoadOp::Clear(wgpu::Color {
                                r: r as f64,
                                g: g as f64,
                                b: b as f64,
                                a: a as f64,
                            })
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // The whole frame once, or every layer once per damaged rectangle.
            let passes = scissors.unwrap_or_else(|| vec![[0, 0, w, h]]);
            for (n, [sx, sy, sw, sh]) in passes.into_iter().enumerate() {
                pass.set_scissor_rect(sx, sy, sw, sh);
                for (i, range) in ranges.iter().enumerate() {
                    if !touches[i].get(n).copied().unwrap_or(true) {
                        continue;
                    }
                    if !range.is_empty() {
                        // glyphon binds its own pipeline, bind group and vertex buffer
                        // when it draws a layer's text, so rebind ours for every layer.
                        pass.set_pipeline(&self.pipeline);
                        pass.set_bind_group(0, &self.bind_group, &[]);
                        pass.set_vertex_buffer(0, self.instances.slice(..));
                        pass.draw(0..4, range.clone());
                    }
                    if !image_ranges[i].is_empty() {
                        pass.set_pipeline(&self.image_pipeline);
                        pass.set_bind_group(0, &self.bind_group, &[]);
                        pass.set_vertex_buffer(0, self.image_instances.slice(..));
                        for (at, blob) in &image_ranges[i] {
                            pass.set_bind_group(1, &self.images[blob].bind_group, &[]);
                            pass.draw(0..4, *at..*at + 1);
                        }
                    }
                    if let Err(e) = self.text_renderers[i].render(&self.atlas, &self.viewport, &mut pass) {
                        log::error!("text render failed: {e}");
                    }
                }
            }
        }
        self.atlas.trim();

        FrameStats { quads: instances.len(), texts: text_count, layers: list.layers.len() }
    }

    /// Shaping is the expensive part of text, so buffers are cached per node and
    /// only reshaped when the string, size, width or weight changes. Shaping is in
    /// logical pixels; glyphon applies the display scale at prepare time.
    fn shape(&mut self, t: &TextItem) {
        let key = (t.text.clone(), t.font.clone(), t.rect.w.to_bits(), t.center);
        if let Some(c) = self.texts.get_mut(&(t.node, t.slot))
            && c.key == key
        {
            c.used = true;
            return;
        }
        let size = t.font.size;
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(size, size * 1.3));
        let width = (t.rect.w > 0.0).then_some(t.rect.w);
        buffer.set_size(width, None);
        let attrs = crate::fonts::attrs(self.font_system.db(), &t.font);
        let align = t.center.then_some(glyphon::cosmic_text::Align::Center);
        buffer.set_text(&t.text, &attrs, Shaping::Advanced, align);
        buffer.shape_until_scroll(&mut self.font_system, false);
        self.texts.insert((t.node, t.slot), CachedText { buffer, key, used: true });
    }
}

/// Where text item `t` may draw: its rect within its clip. A zero width means
/// the text doesn't wrap (a single-line field), so then only the clip bounds it
/// across.
fn text_bounds(t: &TextItem) -> Rect {
    let area = if t.rect.w > 0.0 { t.rect } else { Rect { x: t.clip.x, w: t.clip.w, ..t.rect } };
    area.intersect(&t.clip)
}

/// Everything a layer can draw on (logical pixels), as damage tracking sees it.
fn layer_bounds(l: &crate::scene::Layer) -> Option<Rect> {
    let mut acc: Option<Rect> = None;
    let mut add = |r: Rect| {
        if !r.is_empty() {
            acc = Some(acc.map_or(r, |a| a.union(&r)));
        }
    };
    l.quads.iter().for_each(|q| add(q.rect.expand(2.0).intersect(&q.clip)));
    l.texts.iter().for_each(|t| add(text_bounds(t).expand(2.0)));
    l.images.iter().for_each(|i| add(i.rect.expand(2.0).intersect(&i.clip)));
    acc
}

/// A damage rectangle (logical) as a scissor in whole physical pixels, rounded
/// outwards and clipped to the target; `None` if nothing of it is on the target.
fn scissor(r: Rect, scale: f32, (w, h): (u32, u32)) -> Option<[u32; 4]> {
    let x0 = (r.x * scale).floor().clamp(0.0, w as f32) as u32;
    let y0 = (r.y * scale).floor().clamp(0.0, h as f32) as u32;
    let x1 = ((r.x + r.w) * scale).ceil().clamp(0.0, w as f32) as u32;
    let y1 = ((r.y + r.h) * scale).ceil().clamp(0.0, h as f32) as u32;
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

fn phys(r: Rect, scale: f32) -> [f32; 4] {
    [r.x * scale, r.y * scale, r.w * scale, r.h * scale]
}

/// The colour of a plain background the frame can be cleared to, in place of its quad:
/// the very first quad (nothing is drawn beneath it), when it is opaque, square,
/// borderless and covers the whole target, clip included.
fn background_fill(list: &DrawList, (w, h): (u32, u32), scale: f32) -> Option<Rgba> {
    let q = list.layers.first()?.quads.first()?;
    let covers = |r: Rect| {
        r.x * scale <= 0.0
            && r.y * scale <= 0.0
            && (r.x + r.w) * scale >= w as f32
            && (r.y + r.h) * scale >= h as f32
    };
    let plain = q.color.0[3] == 255 && q.radius <= 0.0 && q.border_width <= 0.0;
    (plain && covers(q.rect) && covers(q.clip)).then_some(q.color)
}

#[cfg(test)]
mod background_tests {
    use super::*;
    use crate::scene::{Layer, QuadItem};

    fn quad(rect: Rect, color: Rgba) -> QuadItem {
        QuadItem { rect, color, border_color: color, border_width: 0.0, radius: 0.0, clip: Rect::INFINITE }
    }
    fn list(quads: Vec<QuadItem>) -> DrawList {
        DrawList { layers: vec![Layer { quads, ..Layer::default() }] }
    }
    const GREY: Rgba = Rgba([223, 229, 236, 255]);
    const SCREEN: Rect = Rect { x: 0.0, y: 0.0, w: 1720.0, h: 720.0 };

    #[test]
    fn a_plain_full_screen_background_becomes_the_clear() {
        // 1720x720 logical at scale 2 is 3440x1440 physical.
        assert_eq!(background_fill(&list(vec![quad(SCREEN, GREY)]), (3440, 1440), 2.0), Some(GREY));
        let bigger = Rect { x: -10.0, y: -10.0, w: 2000.0, h: 900.0 };
        assert_eq!(background_fill(&list(vec![quad(bigger, GREY)]), (3440, 1440), 2.0), Some(GREY));
    }

    #[test]
    fn anything_else_is_drawn_as_a_quad() {
        let size = (3440, 1440);
        let translucent = quad(SCREEN, Rgba([223, 229, 236, 200]));
        let rounded = QuadItem { radius: 8.0, ..quad(SCREEN, GREY) };
        let bordered = QuadItem { border_width: 1.0, ..quad(SCREEN, GREY) };
        let clipped = QuadItem { clip: Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 }, ..quad(SCREEN, GREY) };
        let short = quad(Rect { h: 700.0, ..SCREEN }, GREY);
        for q in [translucent, rounded, bordered, clipped, short] {
            assert_eq!(background_fill(&list(vec![q.clone()]), size, 2.0), None, "{q:?}");
        }
        // Only the first quad: one drawn over another can't become the clear.
        let window = quad(Rect { x: 10.0, y: 10.0, w: 100.0, h: 100.0 }, GREY);
        assert_eq!(background_fill(&list(vec![window, quad(SCREEN, GREY)]), size, 2.0), None);
        assert_eq!(background_fill(&DrawList::default(), size, 2.0), None);
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;

    fn item(rect: Rect, clip: Rect) -> TextItem {
        TextItem {
            node: 1,
            slot: 0,
            text: "Grace".into(),
            font: crate::style::Font::default(),
            color: Rgba::TRANSPARENT,
            center: false,
            rect,
            clip,
        }
    }

    #[test]
    fn unwrapped_text_is_bounded_by_its_clip_not_its_zero_width() {
        // A single-line text field's text: no width (it doesn't wrap), clipped
        // to the field. It used to clip to nothing, so fields drew no text.
        let field = Rect { x: 84.0, y: 576.0, w: 472.0, h: 24.0 };
        let b = text_bounds(&item(Rect { x: 84.0, y: 579.0, w: 0.0, h: 19.0 }, field));
        assert_eq!(b, Rect { x: 84.0, y: 579.0, w: 472.0, h: 19.0 });
        // Wrapped text keeps to its own rect.
        let b = text_bounds(&item(Rect { x: 90.0, y: 579.0, w: 100.0, h: 19.0 }, field));
        assert_eq!(b, Rect { x: 90.0, y: 579.0, w: 100.0, h: 19.0 });
    }
}
