//! The pointer image. The KMS backend shows it on the display's cursor plane;
//! hosted windows use the system cursor.

/// A classic arrow: `X` is the black outline, `o` the white fill, space is clear.
/// The hotspot is the tip, at the top-left.
const ARROW: [&str; 17] = [
    "X",
    "XX",
    "XoX",
    "XooX",
    "XoooX",
    "XooooX",
    "XoooooX",
    "XooooooX",
    "XoooooooX",
    "XooooooooX",
    "XoooooXXXXX",
    "XooXooX",
    "XoX XooX",
    "XX  XooX",
    "X    XooX",
    "     XooX",
    "      XX",
];

/// A cursor bitmap: premultiplied ARGB8888, row by row.
#[derive(Debug)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    pub pixels: Vec<u32>,
}

pub fn arrow() -> Bitmap {
    let width = ARROW.iter().map(|row| row.len()).max().unwrap_or(0);
    let mut pixels = vec![0u32; width * ARROW.len()];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, c) in row.bytes().enumerate() {
            pixels[y * width + x] = match c {
                b'X' => 0xff00_0000,
                b'o' => 0xffff_ffff,
                _ => 0,
            };
        }
    }
    Bitmap { width: width as u32, height: ARROW.len() as u32, hotspot: (0, 0), pixels }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_fits_a_cursor_plane_and_its_tip_is_the_hotspot() {
        let a = arrow();
        assert!(a.width <= 64 && a.height <= 64);
        assert_eq!(a.pixels.len(), (a.width * a.height) as usize);
        let at = |x: u32, y: u32| a.pixels[(y * a.width + x) as usize];
        assert_eq!(at(a.hotspot.0, a.hotspot.1), 0xff00_0000, "tip is opaque");
        assert_eq!(at(2, 5), 0xffff_ffff, "inside is white");
        assert_eq!(at(a.width - 1, 0), 0, "outside is clear");
    }
}
