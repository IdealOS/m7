//! Images: PNG and JPEG blobs decoded for the renderer.
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

/// Decode a PNG or a JPEG, told apart by their first bytes.
pub fn decode(bytes: &[u8]) -> Result<Arc<Image>, String> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) { decode_jpeg(bytes) } else { decode_png(bytes) }
}

/// Decode a JPEG (baseline or progressive; grey, colour or CMYK) into RGBA. One
/// bigger than `MAX_SIDE` is scaled down by a power of two while decoding, which
/// JPEG does cheaply, rather than refused: a camera's photo or an oversized cover
/// still shows. Orientation from EXIF isn't applied.
pub fn decode_jpeg(bytes: &[u8]) -> Result<Arc<Image>, String> {
    use jpeg_decoder::{Decoder, PixelFormat};
    let mut decoder = Decoder::new(std::io::Cursor::new(bytes));
    decoder.read_info().map_err(|e| format!("not a JPEG: {e}"))?;
    let info = decoder.info().ok_or("the JPEG has no size")?;
    let (mut w, mut h) = (u32::from(info.width), u32::from(info.height));
    if w == 0 || h == 0 {
        return Err("the JPEG is empty".into());
    }
    // The smallest power of two (up to 8, as far as JPEG scales) that fits.
    let factor = [1, 2, 4, 8].into_iter().find(|f| w.max(h).div_ceil(*f) <= MAX_SIDE);
    let Some(factor) = factor else {
        return Err(format!("{w}x{h} is too big (at most {} on a side)", MAX_SIDE * 8));
    };
    if factor > 1 {
        let (sw, sh) = decoder
            .scale(w.div_ceil(factor) as u16, h.div_ceil(factor) as u16)
            .map_err(|e| format!("scaling: {e}"))?;
        (w, h) = (u32::from(sw), u32::from(sh));
    }
    let data = decoder.decode().map_err(|e| format!("decoding: {e}"))?;
    let rgba: Vec<u8> = match info.pixel_format {
        PixelFormat::RGB24 => data.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        PixelFormat::L8 => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        // Big-endian 16-bit grey: the high byte is near enough.
        PixelFormat::L16 => data.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], 255]).collect(),
        // As the decoder gives it (Adobe's inverted CMYK, the usual kind): each
        // colour is its channel times K.
        PixelFormat::CMYK32 => data
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let k = u16::from(p[3]);
                let c = |v: u8| (u16::from(v) * k / 255) as u8;
                [c(p[0]), c(p[1]), c(p[2]), 255]
            })
            .collect(),
    };
    if rgba.len() != (w * h) as usize * 4 {
        return Err("the image's data is the wrong size".into());
    }
    Ok(Arc::new(Image { width: w, height: h, rgba }))
}

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

    /// The colour of pixel (`x`, `y`).
    fn at(img: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        img.rgba[i..i + 4].try_into().unwrap()
    }

    /// Whether `got` is within JPEG's loss of `want`.
    fn near(got: [u8; 4], want: [u8; 3]) -> bool {
        got[3] == 255 && got.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 24)
    }

    // The fixtures are a 32x16 picture, red on the left and blue on the right, in
    // colour and in grey, and the same 8200x16 (made on a Mac with sips).
    const COLOR: &[u8] = include_bytes!("../testdata/color.jpg");
    const GRAY: &[u8] = include_bytes!("../testdata/gray.jpg");
    const WIDE: &[u8] = include_bytes!("../testdata/wide.jpg");

    #[test]
    fn jpegs_decode_to_rgba() {
        let img = decode(COLOR).unwrap();
        assert_eq!((img.width, img.height), (32, 16));
        assert!(near(at(&img, 3, 8), [255, 0, 0]), "red: {:?}", at(&img, 3, 8));
        assert!(near(at(&img, 28, 8), [0, 0, 255]), "blue: {:?}", at(&img, 28, 8));
        let gray = decode(GRAY).unwrap();
        assert_eq!((gray.width, gray.height), (32, 16));
        let [r, g, b, a] = at(&gray, 3, 8);
        assert!(r == g && g == b && a == 255, "grey: {:?}", [r, g, b, a]);
        assert!(r.abs_diff(at(&gray, 28, 8)[0]) > 20, "red and blue are different greys");
    }

    #[test]
    fn a_jpeg_too_big_to_show_is_scaled_down_while_decoding() {
        let img = decode(WIDE).unwrap();
        // 8200 / 2 is still over 4096; a quarter fits.
        assert_eq!((img.width, img.height), (2050, 4));
        assert!(near(at(&img, 10, 2), [255, 0, 0]));
        assert!(near(at(&img, 2040, 2), [0, 0, 255]));
    }

    #[test]
    fn decode_tells_jpeg_from_png_and_refuses_neither() {
        assert!(decode(&encode(1, 1, png::ColorType::Grayscale, &[7])).is_ok());
        assert!(decode(&COLOR[..COLOR.len() / 2]).is_err(), "a cut-off JPEG");
        assert!(decode(&[0xff, 0xd8, 0xff, 0xe0, 0, 0]).is_err());
        assert!(decode(b"").is_err());
    }
}
