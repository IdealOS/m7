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

/// A double-headed arrow, left to right; the up-down one is it turned. The hotspot
/// is the middle.
const RESIZE_EW: [&str; 7] = [
    "   X         X   ",
    "  XX         XX  ",
    " XoXXXXXXXXXXXoX ",
    "XoooooooooooooooX",
    " XoXXXXXXXXXXXoX ",
    "  XX         XX  ",
    "   X         X   ",
];

/// A diagonal double-headed arrow, top left to bottom right; the other diagonal
/// is it mirrored. The hotspot is the middle.
const RESIZE_NWSE: [&str; 11] = [
    "XXXXX      ",
    "XoooX      ",
    "XooX       ",
    "XoXoX      ",
    "XX XoX     ",
    "    XoX    ",
    "     XoX XX",
    "      XoXoX",
    "       XooX",
    "      XoooX",
    "      XXXXX",
];

/// Which pointer image to show: the arrow, or one that says what dragging here
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shape {
    #[default]
    Arrow,
    /// Resize left and right: a window's side edges, a vertical divider.
    ResizeEw,
    /// Resize up and down: a window's top and bottom edges, a horizontal divider.
    ResizeNs,
    /// A window's top-left and bottom-right corners.
    ResizeNwse,
    /// A window's top-right and bottom-left corners.
    ResizeNesw,
}

/// The bitmap for `shape`, for the KMS backend's cursor plane.
pub fn bitmap(shape: Shape) -> Bitmap {
    let rows: Vec<String> = match shape {
        Shape::Arrow => return arrow(),
        Shape::ResizeEw => RESIZE_EW.iter().map(|r| r.to_string()).collect(),
        Shape::ResizeNs => transpose(&RESIZE_EW),
        Shape::ResizeNwse => RESIZE_NWSE.iter().map(|r| r.to_string()).collect(),
        Shape::ResizeNesw => RESIZE_NWSE.iter().map(|r| r.chars().rev().collect()).collect(),
    };
    let mut b = from_art(&rows);
    b.hotspot = (b.width / 2, b.height / 2);
    b
}

fn transpose(rows: &[&str]) -> Vec<String> {
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    (0..width)
        .map(|x| rows.iter().map(|r| r.as_bytes().get(x).map_or(' ', |&b| b as char)).collect())
        .collect()
}

fn from_art(rows: &[String]) -> Bitmap {
    let width = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    let mut pixels = vec![0u32; width * rows.len()];
    for (y, row) in rows.iter().enumerate() {
        for (x, c) in row.bytes().enumerate() {
            pixels[y * width + x] = match c {
                b'X' => 0xff00_0000,
                b'o' => 0xffff_ffff,
                _ => 0,
            };
        }
    }
    Bitmap { width: width as u32, height: rows.len() as u32, hotspot: (0, 0), pixels }
}

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
    fn resize_cursors_fit_and_point_both_ways_from_the_middle() {
        for shape in [Shape::ResizeEw, Shape::ResizeNs, Shape::ResizeNwse, Shape::ResizeNesw] {
            let b = bitmap(shape);
            assert!(b.width <= 64 && b.height <= 64, "{shape:?}");
            assert_eq!(b.pixels.len(), (b.width * b.height) as usize);
            let at = |x: u32, y: u32| b.pixels[(y * b.width + x) as usize];
            assert_eq!(
                at(b.hotspot.0, b.hotspot.1),
                0xffff_ffff,
                "{shape:?}: the middle is the arrow's white shaft"
            );
        }
        let ns = bitmap(Shape::ResizeNs);
        assert_eq!((ns.width, ns.height), (7, 17), "the left-right arrow turned");
    }

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
