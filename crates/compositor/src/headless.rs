//! Offscreen rendering: draw a scene into a texture and read it back.
//!
//! Used for screenshots (`compositor --screenshot out.png`) and, later, for the
//! golden-image tests described in the plan's testing strategy.

use std::path::Path;

use crate::render::Renderer;
use crate::scene::{Rgba, Scene};

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
    pollster::block_on(render_async(scene, width, height, scale))
}

async fn render_async(scene: &Scene, width: u32, height: u32, scale: f32) -> Result<Image, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .map_err(|e| format!("no GPU adapter: {e}"))?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .map_err(|e| format!("request_device: {e}"))?;

    let (pw, ph) = ((width as f32 * scale) as u32, (height as f32 * scale) as u32);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut renderer = Renderer::new(&device, &queue, format);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    renderer.render(&device, &queue, &mut encoder, &view, (pw, ph), scale, &scene.draw_list(), Rgba::BLACK);

    // Rows in a buffer copy must be padded to 256 bytes.
    let unpadded = pw * 4;
    let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * ph) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(ph),
            },
        },
        wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .map_err(|e| format!("poll: {e}"))?;
    rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("map: {e}"))?;

    let data = slice.get_mapped_range().map_err(|e| format!("map range: {e:?}"))?;
    let mut pixels = Vec::with_capacity((unpadded * ph) as usize);
    for row in data.chunks(padded as usize) {
        pixels.extend_from_slice(&row[..unpadded as usize]);
    }
    Ok(Image { width: pw, height: ph, pixels })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{NodeKind, SceneOp};
    use serde_json::json;

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
