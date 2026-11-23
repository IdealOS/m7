//! wgpu renderer for a [`DrawList`]: instanced SDF quads plus glyphon text.
//!
//! The renderer knows nothing about windows or backends. It records into a
//! command encoder targeting any texture view, so the hosted window, the KMS
//! backend and headless screenshots all share it.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use protocol::NodeId;
use wgpu::util::DeviceExt;

use crate::scene::{DrawList, Rect, Rgba, TextItem};

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

/// A shaped text buffer, reused across frames until its inputs change.
struct CachedText {
    buffer: Buffer,
    key: (String, u32, u32, bool, bool),
    used: bool,
}

pub struct Renderer {
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    instance_capacity: usize,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    /// One text renderer per layer, so text draws in stacking order.
    text_renderers: Vec<TextRenderer>,
    texts: HashMap<NodeId, CachedText>,
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
    ) -> FrameStats {
        let (w, h) = size;
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals { viewport: [w as f32, h as f32], _pad: [0.0; 2] }),
        );

        // Quads for every layer go into one instance buffer; each layer draws a range.
        let mut instances = Vec::new();
        let mut ranges = Vec::with_capacity(list.layers.len());
        for layer in &list.layers {
            let start = instances.len() as u32;
            for q in &layer.quads {
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
            for t in &layer.texts {
                self.shape(t);
            }
            text_count += layer.texts.len();
            let areas = layer.texts.iter().map(|t| {
                let clip = phys(t.rect.intersect(&t.clip), scale);
                TextArea {
                    buffer: &self.texts[&t.node].buffer,
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
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r as f64,
                            g: g as f64,
                            b: b as f64,
                            a: a as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            for (i, range) in ranges.into_iter().enumerate() {
                if !range.is_empty() {
                    // glyphon binds its own pipeline, bind group and vertex buffer
                    // when it draws a layer's text, so rebind ours for every layer.
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &self.bind_group, &[]);
                    pass.set_vertex_buffer(0, self.instances.slice(..));
                    pass.draw(0..4, range);
                }
                if let Err(e) = self.text_renderers[i].render(&self.atlas, &self.viewport, &mut pass) {
                    log::error!("text render failed: {e}");
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
        let key = (t.text.clone(), t.size.to_bits(), t.rect.w.to_bits(), t.bold, t.center);
        if let Some(c) = self.texts.get_mut(&t.node)
            && c.key == key
        {
            c.used = true;
            return;
        }
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(t.size, t.size * 1.3));
        let width = (t.rect.w > 0.0).then_some(t.rect.w);
        buffer.set_size(width, None);
        let mut attrs = Attrs::new().family(Family::SansSerif);
        if t.bold {
            attrs = attrs.weight(Weight::SEMIBOLD);
        }
        let align = t.center.then_some(glyphon::cosmic_text::Align::Center);
        buffer.set_text(&t.text, &attrs, Shaping::Advanced, align);
        buffer.shape_until_scroll(&mut self.font_system, false);
        self.texts.insert(t.node, CachedText { buffer, key, used: true });
    }
}

fn phys(r: Rect, scale: f32) -> [f32; 4] {
    [r.x * scale, r.y * scale, r.w * scale, r.h * scale]
}
