//! Offscreen rendering: draw a scene into a texture and read it back.
//!
//! Used for screenshots (`compositor --screenshot out.png`) and, later, for the
//! golden-image tests described in the plan's testing strategy.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::render::{FrameStats, Renderer};
use crate::scene::{DrawList, Rect, Rgba};

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 (sRGB).
    pub pixels: Vec<u8>,
}

impl Image {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2], self.pixels[i + 3]]
    }

    pub fn save_png(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut enc = png::Encoder::new(file, self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&self.pixels)?;
        Ok(())
    }
}

/// Render `list` at `width` x `height` logical pixels times `scale`. Take
/// the list from [`crate::desktop::Desktop::draw_list`], as the backends do, so
/// it's laid out and carries images' pixels.
pub fn render(list: &DrawList, width: u32, height: u32, scale: f32) -> Result<Image, String> {
    let (pw, ph) = ((width as f32 * scale) as u32, (height as f32 * scale) as u32);
    let mut target = Offscreen::new(pw, ph, wgpu::TextureFormat::Rgba8UnormSrgb)?;
    let mut pixels = Vec::with_capacity((pw * ph * 4) as usize);
    target.render(list, scale, Rgba::BLACK, None, |row| pixels.extend_from_slice(row))?;
    Ok(Image { width: pw, height: ph, pixels })
}

/// A long-lived offscreen render target with a CPU readback buffer.
///
/// The KMS backend's copy path (lavapipe, or any GPU without dmabuf export) renders
/// here each frame and copies the rows into a scanout dumb buffer. Pick
/// `Bgra8UnormSrgb` for that: its byte order is DRM's `XRGB8888` on little-endian.
pub struct Offscreen {
    pub adapter: wgpu::AdapterInfo,
    /// Where the last [`Offscreen::render`] or [`Offscreen::render_to`] spent its time.
    pub timing: FrameTiming,
    /// The device can import dma-bufs, so it can render into scanout buffers.
    pub dmabuf_import: bool,
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    renderer: Renderer,
    width: u32,
    height: u32,
    padded_row: u32,
}

/// The three parts of one offscreen frame on the copy path.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTiming {
    /// The CPU recording the frame's GPU commands, text shaping included.
    pub encode: Duration,
    /// From submitting them until the GPU has drawn the frame, copied it into the
    /// readback buffer, and the buffer is mapped.
    pub gpu: Duration,
    /// The CPU reading the mapped buffer, row by row, into the caller.
    pub read: Duration,
}

impl Offscreen {
    /// `width` and `height` are physical pixels.
    pub fn new(width: u32, height: u32, format: wgpu::TextureFormat) -> Result<Offscreen, String> {
        pollster::block_on(Self::new_async(width, height, format))
    }

    async fn new_async(width: u32, height: u32, format: wgpu::TextureFormat) -> Result<Offscreen, String> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = backends(wgpu::Backends::from_env());
        let instance = wgpu::Instance::new(desc);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|e| format!("no GPU adapter: {e}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                // Rendering straight into KMS scanout buffers (direct scanout) needs
                // dma-buf import; ask for it wherever the driver has it.
                required_features: adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF,
                // lavapipe, V3DV and virgl all need the conservative limits.
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("request_device: {e}"))?;
        let (texture, view, readback, padded_row) = Self::target(&device, width, height, format);
        let renderer = Renderer::new(&device, &queue, format);
        Ok(Offscreen {
            adapter: adapter.get_info(),
            timing: FrameTiming::default(),
            dmabuf_import: device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
            device,
            queue,
            texture,
            view,
            readback,
            renderer,
            width,
            height,
            padded_row,
        })
    }

    /// Render at `width` x `height` from now on. Keeps the device, so it costs only
    /// the new texture and readback buffer.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (texture, view, readback, padded_row) =
            Self::target(&self.device, width, height, self.texture.format());
        (self.texture, self.view, self.readback, self.padded_row) = (texture, view, readback, padded_row);
        (self.width, self.height) = (width, height);
    }

    /// The texture to render into, and the buffer its rows are read back through.
    fn target(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (wgpu::Texture, wgpu::TextureView, wgpu::Buffer, u32) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // Rows in a buffer copy must be padded to 256 bytes.
        let padded_row =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        (texture, view, readback, padded_row)
    }

    /// Wrap a dma-buf, such as a KMS scanout buffer, as a texture to render into. The
    /// buffer is linear (no tiling), `stride` bytes per row, in this target's format.
    /// The texture view keeps the buffer imported for as long as it lives.
    #[cfg(target_os = "linux")]
    pub fn import_dmabuf(
        &self,
        fd: std::os::fd::OwnedFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<wgpu::TextureView, String> {
        use wgpu::hal;
        if !self.dmabuf_import {
            return Err("this GPU's driver can't import dma-bufs (VK_EXT_external_memory_dma_buf)".into());
        }
        /// DRM_FORMAT_MOD_LINEAR: rows one after another, as dumb buffers are.
        const LINEAR: u64 = 0;
        let format = self.texture.format();
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let hal_desc = hal::TextureDescriptor {
            label: Some("scanout"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::COLOR_TARGET,
            memory_flags: hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        // SAFETY: the device was opened with VULKAN_EXTERNAL_MEMORY_DMA_BUF (checked
        // above), and the fd is a single-plane linear buffer of `stride` bytes per row,
        // big enough for `width` x `height` pixels of `format`.
        let hal_texture = unsafe {
            let device = self.device.as_hal::<hal::api::Vulkan>().ok_or("not a Vulkan device")?;
            device.texture_from_dmabuf_fd(fd, &hal_desc, LINEAR, u64::from(stride), 0)
        }
        .map_err(|e| format!("import dma-buf as a {width}x{height} {format:?} texture: {e}"))?;
        let desc = wgpu::TextureDescriptor {
            label: Some("scanout"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        };
        // SAFETY: `hal_texture` came from this device and matches `desc`; its
        // contents are undefined, which UNINITIALIZED states.
        let texture = unsafe {
            self.device.create_texture_from_hal::<hal::api::Vulkan>(
                hal_texture,
                &desc,
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
    }

    /// Render one frame straight into `view` (an imported scanout buffer of `size`)
    /// and wait until the GPU has finished, so the buffer can be shown. No readback.
    /// `damage`: see [`Renderer::render`]; the buffer must hold the frame it was
    /// worked out against.
    pub fn render_to(
        &mut self,
        view: &wgpu::TextureView,
        size: (u32, u32),
        list: &DrawList,
        scale: f32,
        clear: Rgba,
        damage: Option<&[Rect]>,
    ) -> Result<FrameStats, String> {
        let started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let stats = self.renderer.render(
            &self.device,
            &self.queue,
            &mut encoder,
            view,
            size,
            scale,
            list,
            clear,
            damage,
        );
        let encoded = Instant::now();
        let index = self.queue.submit(Some(encoder.finish()));
        self.device
            .poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None })
            .map_err(|e| format!("poll: {e}"))?;
        self.timing = FrameTiming { encode: encoded - started, gpu: encoded.elapsed(), read: Duration::ZERO };
        Ok(stats)
    }

    /// Render one frame and hand each row of tightly packed pixels to `row`, top to bottom.
    /// `damage`: see [`Renderer::render`]; the offscreen texture holds the last frame
    /// rendered here, so damage worked out against that frame applies.
    pub fn render(
        &mut self,
        list: &DrawList,
        scale: f32,
        clear: Rgba,
        damage: Option<&[Rect]>,
        mut row: impl FnMut(&[u8]),
    ) -> Result<FrameStats, String> {
        let (w, h) = (self.width, self.height);
        let started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let stats = self.renderer.render(
            &self.device,
            &self.queue,
            &mut encoder,
            &self.view,
            (w, h),
            scale,
            list,
            clear,
            damage,
        );
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let encoded = Instant::now();
        self.queue.submit(Some(encoder.finish()));

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .map_err(|e| format!("poll: {e}"))?;
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("map: {e}"))?;
        let drawn = Instant::now();
        {
            let data = slice.get_mapped_range().map_err(|e| format!("map range: {e:?}"))?;
            for r in data.chunks(self.padded_row as usize) {
                row(&r[..(w * 4) as usize]);
            }
        }
        self.readback.unmap();
        self.timing = FrameTiming { encode: encoded - started, gpu: drawn - encoded, read: drawn.elapsed() };
        Ok(stats)
    }
}

/// The wgpu backends to try: `WGPU_BACKEND` if set, otherwise Vulkan, Metal and DX12.
/// OpenGL is left out: the renderer doesn't need it, starting it prints EGL warnings
/// where there is no GL driver (plain QEMU), and where virgl's GL driver exists (UTM)
/// tearing it down aborted the process at exit.
fn backends(from_env: Option<wgpu::Backends>) -> wgpu::Backends {
    from_env.unwrap_or(wgpu::Backends::PRIMARY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;
    use protocol::{NodeKind, SceneOp};
    use serde_json::json;

    #[test]
    fn opengl_is_off_unless_asked_for() {
        let default = backends(None);
        assert!(default.contains(wgpu::Backends::VULKAN | wgpu::Backends::METAL));
        assert!(!default.contains(wgpu::Backends::GL));
        // WGPU_BACKEND still wins, GL included.
        assert_eq!(backends(Some(wgpu::Backends::GL)), wgpu::Backends::GL);
    }

    /// Renders a real frame on whatever GPU (or software adapter) is present.
    /// Skips quietly where no adapter exists, such as a bare CI runner.
    #[test]
    fn renders_a_rounded_rect() {
        let mut scene = Scene::new();
        let props = json!({"x": 10, "y": 10, "w": 80, "h": 40, "radius": 8, "color": "#ff0000"});
        scene.apply_all([
            SceneOp::Create { id: 1, kind: NodeKind::Rect, props: props.as_object().unwrap().clone() },
            SceneOp::Insert { parent: 0, child: 1, index: 0 },
        ]);
        let Some(img) = render_or_skip(&scene.draw_list()) else { return };
        assert_eq!(img.pixel(50, 30), [255, 0, 0, 255], "center is the fill");
        assert_eq!(img.pixel(2, 2), [0, 0, 0, 255], "outside is the clear color");
        assert_eq!(img.pixel(10, 10)[0], 0, "the rounded corner leaves the corner pixel clear");
    }

    /// Region damage must be invisible: redrawing only what changed, over the last
    /// frame, gives exactly the pixels of drawing the new frame from scratch.
    #[test]
    fn a_partial_redraw_matches_a_full_one() {
        use crate::damage::DamageTracker;
        let (w, h) = (1200u32, 800u32);
        let screen = (w as f32, h as f32);
        let mut scene = Scene::new();
        scene.apply_all(crate::demo::build(screen.0, screen.1, 4));
        let mut target = match Offscreen::new(w, h, wgpu::TextureFormat::Rgba8UnormSrgb) {
            Ok(t) => t,
            Err(e) if e.starts_with("no GPU adapter") => return eprintln!("skipping: {e}"),
            Err(e) => panic!("{e}"),
        };
        let mut tracker = DamageTracker::new(1);
        let first = scene.draw_list();
        assert_eq!(tracker.next_frame(&first, screen), None);
        target.render(&first, 1.0, Rgba::BLACK, None, |_| {}).unwrap();
        tracker.drew(0);

        // Move the frontmost window (just under the top bar) and change the readout.
        let top = scene.top_level();
        let props = json!({"x": 600, "y": 400}).as_object().unwrap().clone();
        scene.apply(SceneOp::Set { id: top[top.len() - 2], props });
        scene.apply(crate::demo::hud("partial redraw"));
        let second = scene.draw_list();
        let damage = tracker.next_frame(&second, screen).expect("a partial redraw");
        assert!(!damage.is_empty());
        let region = tracker.region_for(0, screen).expect("a partial redraw");
        let area: f32 = region.iter().map(Rect::area).sum();
        assert!(area < 0.6 * screen.0 * screen.1, "the damage is part of the screen: {region:?}");
        let mut partial = Vec::new();
        target.render(&second, 1.0, Rgba::BLACK, Some(&region), |r| partial.extend_from_slice(r)).unwrap();

        let full = render(&second, w, h, 1.0).unwrap();
        let differ: Vec<usize> = (0..partial.len() / 4)
            .filter(|&i| partial[i * 4..i * 4 + 4] != full.pixels[i * 4..i * 4 + 4])
            .collect();
        assert!(
            differ.is_empty(),
            "{} pixels differ, first at ({}, {}); damage {region:?}",
            differ.len(),
            differ[0] % w as usize,
            differ[0] / w as usize
        );
    }

    fn render_or_skip(list: &DrawList) -> Option<Image> {
        match render(list, 100, 60, 1.0) {
            Ok(img) => Some(img),
            Err(e) if e.starts_with("no GPU adapter") => {
                eprintln!("skipping: {e}");
                None
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn renders_an_image() {
        // A one-pixel green image stretched over a square.
        let pixels =
            std::sync::Arc::new(crate::image::Image { width: 1, height: 1, rgba: vec![0, 255, 0, 255] });
        let mut layer = crate::scene::Layer::default();
        layer.images.push(crate::scene::ImageItem {
            node: 1,
            blob: "ab".repeat(32),
            pixels: Some(pixels),
            rect: crate::scene::Rect { x: 20.0, y: 10.0, w: 40.0, h: 40.0 },
            clip: crate::scene::Rect { x: 0.0, y: 0.0, w: 100.0, h: 60.0 },
            radius: 0.0,
        });
        let list = DrawList { layers: vec![layer] };
        let Some(img) = render_or_skip(&list) else { return };
        assert_eq!(img.pixel(40, 30), [0, 255, 0, 255], "the image");
        assert_eq!(img.pixel(80, 30), [0, 0, 0, 255], "beside it, the clear color");
    }
}
