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
    let orientation = decoder.exif_data().map_or(1, exif_orientation);
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
    Ok(Arc::new(orient(Image { width: w, height: h, rgba }, orientation)))
}

/// The orientation EXIF data (from its TIFF header on) gives a photo: 1 to 8 as
/// TIFF numbers them, 1 (upright) when it doesn't say or can't be read.
pub fn exif_orientation(tiff: &[u8]) -> u16 {
    const ORIENTATION: u16 = 0x0112;
    const SHORT: u16 = 3;
    let big = match tiff.get(..2) {
        Some(b"MM") => true,
        Some(b"II") => false,
        _ => return 1,
    };
    let u16_at = |at: usize| {
        tiff.get(at..at + 2)
            .map(|b| if big { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    };
    let u32_at = |at: usize| {
        tiff.get(at..at + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }
        })
    };
    if u16_at(2) != Some(42) {
        return 1;
    }
    let Some(ifd) = u32_at(4).map(|o| o as usize) else { return 1 };
    let count = u16_at(ifd).unwrap_or(0) as usize;
    (0..count)
        .map(|i| ifd + 2 + i * 12)
        .find(|&e| u16_at(e) == Some(ORIENTATION) && u16_at(e + 2) == Some(SHORT))
        .and_then(|e| u16_at(e + 8))
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1)
}

/// `img` turned and flipped as EXIF `orientation` says, so it shows upright:
/// 2 mirrored, 3 turned half round, 4 flipped, 5 transposed, 6 turned a quarter
/// clockwise, 7 transversed, 8 turned a quarter anticlockwise.
pub fn orient(img: Image, orientation: u16) -> Image {
    if !(2..=8).contains(&orientation) {
        return img;
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let sideways = orientation >= 5;
    let (ow, oh) = if sideways { (h, w) } else { (w, h) };
    let mut rgba = vec![0; img.rgba.len()];
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                _ => (y, w - 1 - x),
            };
            let (from, to) = ((y * w + x) * 4, (dy * ow + dx) * 4);
            rgba[to..to + 4].copy_from_slice(&img.rgba[from..from + 4]);
        }
    }
    Image { width: ow as u32, height: oh as u32, rgba }
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

    /// `jpeg` with an EXIF block saying `orientation`, as cameras write it
    /// (big-endian if `big`).
    fn with_orientation(jpeg: &[u8], orientation: u16, big: bool) -> Vec<u8> {
        let u16b = |v: u16| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut tiff: Vec<u8> = if big { b"MM".to_vec() } else { b"II".to_vec() };
        tiff.extend(u16b(42));
        tiff.extend(u32b(8));
        tiff.extend(u16b(2)); // two entries: something else first
        for (tag, value) in [(0x010f_u16, 0_u16), (0x0112, orientation)] {
            tiff.extend(u16b(tag));
            tiff.extend(u16b(3));
            tiff.extend(u32b(1));
            tiff.extend(u16b(value));
            tiff.extend([0, 0]);
        }
        tiff.extend(u32b(0));
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = vec![0xff, 0xd8, 0xff, 0xe1];
        out.extend(((app1.len() + 2) as u16).to_be_bytes());
        out.extend(app1);
        // The rest, without any EXIF it had (the decoder keeps the last).
        let mut at = 2;
        while jpeg[at] == 0xff && jpeg[at + 1] != 0xda {
            let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
            if jpeg[at + 1] != 0xe1 {
                out.extend(&jpeg[at..at + 2 + len]);
            }
            at += 2 + len;
        }
        out.extend(&jpeg[at..]);
        out
    }

    #[test]
    fn photos_are_turned_upright_as_their_exif_says() {
        // COLOR is red on the left, blue on the right.
        let red = [255, 0, 0];
        // Orientation, the size it shows at, and where the red half is.
        type Point = (u32, u32);
        let cases: [(u16, Point, Point); 8] = [
            (1, (32, 16), (3, 8)),
            (2, (32, 16), (28, 8)),
            (3, (32, 16), (28, 8)),
            (4, (32, 16), (3, 8)),
            (5, (16, 32), (8, 3)),
            (6, (16, 32), (8, 3)),
            (7, (16, 32), (8, 28)),
            (8, (16, 32), (8, 28)),
        ];
        for (o, size, red_at) in cases {
            for big in [false, true] {
                let img = decode(&with_orientation(COLOR, o, big)).unwrap();
                assert_eq!((img.width, img.height), size, "orientation {o}");
                assert!(near(at(&img, red_at.0, red_at.1), red), "orientation {o}: red at {red_at:?}");
            }
        }
        // Flips and turns that look alike on two colours, told apart on a 2x2.
        let px = |rgba: &[u8]| rgba.chunks(4).map(|p| p[0]).collect::<Vec<_>>();
        let img = || Image {
            width: 2,
            height: 2,
            rgba: [1, 2, 3, 4].iter().flat_map(|&v| [v, 0, 0, 255]).collect(),
        };
        assert_eq!(px(&orient(img(), 4).rgba), [3, 4, 1, 2], "flipped top to bottom");
        assert_eq!(px(&orient(img(), 3).rgba), [4, 3, 2, 1], "half round");
        assert_eq!(px(&orient(img(), 5).rgba), [1, 3, 2, 4], "transposed");
        assert_eq!(px(&orient(img(), 6).rgba), [3, 1, 4, 2], "a quarter clockwise");
        assert_eq!(px(&orient(img(), 7).rgba), [4, 2, 3, 1], "transversed");
        assert_eq!(px(&orient(img(), 8).rgba), [2, 4, 1, 3], "a quarter anticlockwise");
    }

    #[test]
    fn exif_that_says_nothing_sensible_leaves_a_photo_upright() {
        assert_eq!(exif_orientation(b""), 1);
        assert_eq!(exif_orientation(b"XX\0\x2a\0\0\0\x08"), 1, "not a TIFF header");
        assert_eq!(exif_orientation(b"II\x2a\0\xff\xff\0\0"), 1, "IFD past the end");
        let mut cut = with_orientation(COLOR, 6, false);
        cut.truncate(30); // inside the IFD
        assert_eq!(exif_orientation(&cut[12..]), 1);
        let img = decode(&with_orientation(COLOR, 9, false)).unwrap();
        assert_eq!((img.width, img.height), (32, 16), "no orientation 9");
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
