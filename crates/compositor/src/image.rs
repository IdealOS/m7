//! Images: PNG blobs decoded for the renderer.
//!
//! An `image` node names a blob by its hash. The compositor fetches the blob once
//! (through the bus, as a passed descriptor), decodes it here, and keeps the
//! pixels by hash; every node showing that blob shares them, and the renderer
//! uploads each to the GPU once.

use std::sync::Arc;

/// Decoded pixels: 8-bit sRGB RGBA, row by row.
#[derive(Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Largest image side the compositor shows.
pub const MAX_SIDE: u32 = 4096;
/// Largest blob it reads as an image.
pub const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Decode a PNG of any color type and bit depth into RGBA.
pub fn decode_png(bytes: &[u8]) -> Result<Arc<Image>, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| format!("not a PNG: {e}"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("{w}x{h} is too big (at most {MAX_SIDE} on a side)"));
    }
    let size = reader.output_buffer_size().ok_or("the image's size overflows")?;
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf).map_err(|e| format!("decoding: {e}"))?;
    let data = &buf[..frame.buffer_size()];
    let pixels = (w * h) as usize;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => {
            data.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
        }
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("a palette image wasn't expanded".into()),
    };
    if rgba.len() != pixels * 4 {
        return Err("the image's data is the wrong size".into());
    }
    Ok(Arc::new(Image { width: w, height: h, rgba }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
        out
    }

    #[test]
    fn pngs_of_each_color_type_decode_to_rgba() {
        let rgb = decode_png(&encode(2, 1, png::ColorType::Rgb, &[255, 0, 0, 0, 255, 0])).unwrap();
        assert_eq!((rgb.width, rgb.height), (2, 1));
        assert_eq!(rgb.rgba, [255, 0, 0, 255, 0, 255, 0, 255]);
        let gray = decode_png(&encode(1, 1, png::ColorType::Grayscale, &[7])).unwrap();
        assert_eq!(gray.rgba, [7, 7, 7, 255]);
        let rgba = decode_png(&encode(1, 1, png::ColorType::Rgba, &[1, 2, 3, 4])).unwrap();
        assert_eq!(rgba.rgba, [1, 2, 3, 4]);
        assert!(decode_png(b"not a png").is_err());
    }
}
