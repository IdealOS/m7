//! Offscreen rendering: draw a scene into a texture and read it back.
//!
//! Used for screenshots (`compositor --screenshot out.png`) and, later, for the
//! golden-image tests described in the plan's testing strategy.

use std::path::Path;

use crate::render::{FrameStats, Renderer};
use crate::scene::{DrawList, Rgba, Scene};

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

/// Render `scene` at `width` x `height` logical pixels times `scale`.
pub fn render(scene: &Scene, width: u32, height: u32, scale: f32) -> Result<Image, String> {
    let (pw, ph) = ((width as f32 * scale) as u32, (height as f32 * scale) as u32);
    let mut target = Offscreen::new(pw, ph, wgpu::TextureFormat::Rgba8UnormSrgb)?;
    let mut pixels = Vec::with_capacity((pw * ph * 4) as usize);
    target.render(&scene.draw_list(), scale, Rgba::BLACK, |row| pixels.extend_from_slice(row))?;
    Ok(Image { width: pw, height: ph, pixels })
}

/// A long-lived offscreen render target with a CPU readback buffer.
///
/// The KMS backend's copy path (lavapipe, or any GPU without dmabuf export) renders
/// here each frame and copies the rows into a scanout dumb buffer. Pick
/// `Bgra8UnormSrgb` for that: its byte order is DRM's `XRGB8888` on little-endian.
pub struct Offscreen {
    pub adapter: wgpu::AdapterInfo,
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
                // lavapipe, V3DV and virgl all need the conservative limits.
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("request_device: {e}"))?;
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
        let renderer = Renderer::new(&device, &queue, format);
        Ok(Offscreen {
            adapter: adapter.get_info(),
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

    /// Render one frame and hand each row of tightly packed pixels to `row`, top to bottom.
    pub fn render(
        &mut self,
        list: &DrawList,
        scale: f32,
        clear: Rgba,
        mut row: impl FnMut(&[u8]),
    ) -> Result<FrameStats, String> {
        let (w, h) = (self.width, self.height);
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
        {
            let data = slice.get_mapped_range().map_err(|e| format!("map range: {e:?}"))?;
            for r in data.chunks(self.padded_row as usize) {
                row(&r[..(w * 4) as usize]);
            }
        }
        self.readback.unmap();
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
        let img = match render(&scene, 100, 60, 1.0) {
            Ok(img) => img,
            Err(e) if e.starts_with("no GPU adapter") => {
                eprintln!("skipping: {e}");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        assert_eq!(img.pixel(50, 30), [255, 0, 0, 255], "center is the fill");
        assert_eq!(img.pixel(2, 2), [0, 0, 0, 255], "outside is the clear color");
        assert_eq!(img.pixel(10, 10)[0], 0, "the rounded corner leaves the corner pixel clear");
    }
}
